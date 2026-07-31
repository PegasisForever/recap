# recap

Record your screens on Linux, get one link, and hand that link to an agent to
read the recording back for you.

Two pieces that share a file format:

- **`recap`**, a GTK4 app that records every monitor you have granted plus your
  microphone and system audio, uploads them to S3, and puts one link on your
  clipboard. The link opens a page with a single transport that drives every
  track at once.
- **`watch-recap`**, a Claude Code skill that takes the same link and turns the
  recording into a written artifact: decisions, action items, open questions,
  on-screen artifacts, all with timestamps.

## Why the recording is several files

A screen recorder that produces one muxed video makes two things impossible.

**Separate audio tracks give exact speaker attribution.** The microphone is one
known person, so its transcript needs no diarization at all. Only the system
track, which carries the far side of a call, has to be guessed at. Overlapping
speech stays separable because the two sides never share a waveform.

**Separate video files let you record several monitors.** The desktop portal
hands back exactly one display per grant, so N monitors means N capture
processes, each with its own saved grant.

The link therefore points at a manifest rather than a video. It lists every
part with a presigned URL and a sync offset, and it is embedded in the player
page so one request serves both a person and a program.

## Install

Needs a Wayland or X11 session with a desktop portal, plus these on `PATH`:

| Program | For |
|---|---|
| [`gpu-screen-recorder`](https://git.dec05eba.com/gpu-screen-recorder/about/) | capturing each monitor |
| `pw-record`, `pw-dump`, `wpctl` | audio capture and device listing |
| `ffmpeg`, `ffprobe` | audio compression, stream preparation |
| `gdbus` | asking the desktop what monitors exist |

The app checks all of them at startup and says which are missing.

```bash
cargo build --release          # target/release/recap, about 23 MB
cp -r skill/watch-recap ~/.claude/skills/
```

Everything else is configured in the window: monitors, microphone, bucket and
keys. No config file editing, no command line.

The skill reads its Gemini key from `GEMINI_API_KEY` and nowhere else, because
recordings are usually read back on a different machine from the one that made
them.

## What was measured

The defaults in here are not guesses. The ones worth knowing:

- **Transcription is sliced into 90-second requests.** Gemini's audio clock runs
  long, and the error grows with distance into one request. Ten-minute chunks
  put a sentence whose true position was 14:05 at 16:41, and dropped three
  minutes of audio with no error. Slicing moved the median error from 42s to 3s.
- **Frames are sampled at `fps=0.05` with `MEDIA_RESOLUTION_HIGH`.** Raising the
  frame rate makes on-screen identifiers *less* accurate, not more, because a
  mostly static screen dilutes attention across near-identical images.
- **Sync offsets come from the real first frame**, not from process spawn.
  Restoring a portal session takes seconds and no two monitors take the same
  amount. Using spawn time left tracks 2.1s apart; the fix brings every track
  within about 50ms.
- **Video is remuxed with `+faststart` before upload.** The recorder writes the
  MP4 index at the end of the file, so without this a browser downloads the
  whole recording before showing a frame.

`skill/watch-recap/SKILL.md` has the experiments behind each of these, including
the things that do not work and can be skipped.

## Layout

```
crates/core     capture, upload, manifest, player page
crates/gui      the window
skill/          the watch-recap skill and its stdlib-only Python tool
```

The reading side is deliberately Python with no dependencies beyond ffmpeg, so
the agent that reads recordings can edit its own tooling without a rebuild.

## License

MIT.
