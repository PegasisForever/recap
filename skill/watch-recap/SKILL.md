---
name: watch-recap
description: Watch a screen recording, meeting, or demo and distill it into a written artifact with decisions, action items, open questions, on-screen artifacts and timestamps. Use for recap links, local video files, or any recording where the user asks what happened, what was decided, or to summarize or take notes. Handles video far longer than fits in context by reading the transcript first and sampling frames only where they add information.
license: MIT
---

# watch-recap

Watching a video means reading two channels, what was **said** and what was **shown**, and
fusing them on a shared clock. Neither channel alone is enough:

- Transcript alone loses every file path, command, error message and diagram. In a dev
  walkthrough these are spoken as *"this thing here"* and exist only on screen.
- Frames alone lose all reasoning: why a decision was made, what was rejected, what is
  still open.

The whole method is: **read cheap text first, form questions, then spend pixels answering them.**

## The one rule that matters

**Never extract frames blindly and read them in order.** A 50-minute recording holds ~183,000
frames. Even deduplicated it is ~600, which is millions of visual tokens and mostly redundant.
Frame extraction is *targeted retrieval*, driven by the transcript. Get the transcript first,
always, even when the user hands you a bare video file.

## What a recap link is

A link is a **manifest**, not a video. One recording is several files:

| Part | What it is | Diarization |
|---|---|---|
| `video` | One per screen recorded. There may be several. | n/a |
| `mic` | The local microphone, labelled "Me". Exactly one person. | **Never.** The speaker is known. |
| `system` | Everything the machine played. | **Yes.** May carry several remote people. |

That split is the single biggest advantage this format has. The person at the keyboard is on
their own track, so their words are attributed with certainty rather than guessed from voice
timbre. Only the remote side needs diarizing, and overlapping speech stays separable because
the two sides never share a waveform.

### One clock, and why offsets exist

Every part carries an `offset_ms`, because each stream is captured by its own process and they
do not begin at the same instant. **These are not small.** Restoring a desktop portal session
and negotiating a PipeWire stream takes seconds, and no two monitors take the same amount:

```
video   Monitor 1     offset +0ms
video   Monitor 2     offset +2045ms
mic     Microphone    offset +1916ms
system  System audio  offset +1938ms
```

**Every timestamp you read or write is on the shared clock**, the one the transcript uses.
Each video file starts at its own zero, so a frame pulled at a transcript timestamp would be
late by that monitor's offset. `watchvid.py` subtracts it for you: `at`, `ats`, `zoom`, `burst`
and `sheets` all seek in the file's own timeline while burning the shared-clock time into the
pixels. So a stamp read off a contact sheet can be handed straight back to `at`, and a frame
cited next to a quote really is the frame from that moment.

Two consequences worth holding on to:

- **Never mix a raw ffmpeg seek with a transcript timestamp.** Running `ffmpeg -ss 14:05` on a
  downloaded `monitor-1.mp4` lands two seconds early. Go through `watchvid.py`, or subtract the
  offset yourself.
- **Accuracy is about 50ms across all tracks.** Measured by checking that every part ends at
  the same point on the shared clock, since they all stop on one signal.

## Step 0: See what you have

```bash
scripts/watchvid.py info <link>
```

```
id:         abc0061d-d5af-48b4-85f5-c80099fdcd72
duration:   00:42 (42.0s)
local mic:  Me
parts:
  video   Left                40.2s     3.3 MB  offset +0ms
  video   Right               40.2s     3.0 MB  offset +1ms
  mic     Microphone          42.0s     0.4 MB  offset +1ms
  system  System audio        41.9s     0.4 MB  offset +29ms
```

Read this before anything else. It tells you how many screens exist, whether there is a
microphone track at all, and who the microphone belongs to.

Every other subcommand takes the link directly and caches the download, so you rarely need
`fetch`. Use it when you want the files somewhere specific:

```bash
scripts/watchvid.py fetch <link> -o recording/
```

**Local file or other URL** works everywhere a link does. Start from `probe`:

```bash
scripts/watchvid.py probe rec.mp4
```

## Step 1: Build the transcript

```bash
scripts/watchvid.py transcribe <link> -o transcript.txt
scripts/watchvid.py transcribe <link> --json -o transcript.json   # for the Step 2 checks
```

Given a link this does the right thing on its own: the microphone track is transcribed as
"Me" with diarization switched off, the system track is
diarized, and the two are merged on the shared clock. Roughly 40k tokens per 10 minutes.

Without a `GEMINI_API_KEY`, fall back to `whisper` or `faster-whisper` (`--model small` is
enough for meeting speech, request segment timestamps). If no transcription tool is available,
say so and fall back to frames. Do not silently produce a frames-only summary and present it
as complete.

### Why it slices, and why the timestamps used to be wrong

**Gemini's audio clock runs long, and the error grows with distance into one request.** On a
26-minute recording transcribed in 10-minute chunks, a sentence whose true position was 14:05
came back at 16:41. Three independent runs placed the *same* sentence at 10:09, 22:21 and
25:00. One run emitted a final segment at 28:34, past the end of the file. Long chunks also
drop audio silently: three minutes of one chunk vanished with no error and no gap, so the
transcript jumped from 17:04 to 20:00 as though nothing was said.

Measured against 40-second control slices on 16 anchor phrases:

| Config | median error | p90 | max |
|---|---|---|---|
| 600s slices | 42s | 279s | 359s |
| **90s slices** | **3s** | **15s** | **18s** |
| One 26-minute request, uncompressed WAV | 71s | 159s | 163s |
| 90s slices, uncompressed WAV | 5s | 16s | 18s |

**Request length is the cause, not the codec and not the container.** Three things rule out a
timing defect in the file: the source audio's declared duration matched its decoded sample
count to within 67ms over 26 minutes, re-encoding through `aresample=async=1:first_pts=0`
changed nothing, and uncompressed WAV drifts just as badly once the request is long.

One genuine codec effect exists at medium length only. On the same 185-second window, WAV
landed an anchor exactly while Opus put it 49 seconds late. That difference vanishes at 90-second
slices, so Opus stays because it is 20x smaller to upload.

Two mechanisms in the script do the correcting:

1. **Short slices**, 90 seconds with 15 of overlap, bound how far the clock can drift.
2. **Per-slice compression.** A slice reporting more speech than its own file contains gets
   squeezed back proportionally. The reverse is never applied, because a slice reporting less
   than its length is just quiet at the end.

**Verify before you cite.** Cut a 40-second slice around any moment you plan to quote and
transcribe it alone. A slice that short cannot drift far, so it is your ground truth:

```bash
ffmpeg -ss 830 -to 870 -i rec.mp4 -vn -ac 1 -ar 16000 -c:a libopus probe.ogg -y
scripts/watchvid.py transcribe probe.ogg --offset 13:50
```

## Step 2: Assess the transcript before trusting it

```bash
jq -r '.[].speaker' transcript.json | sort | uniq -c | sort -rn
jq -r '.[].text' transcript.json | grep -cEi '^ *(yeah|yes|okay|ok|mm-hmm)[.,! ]*$'
```

Three failure modes, each with a different response:

| Symptom | Diagnosis | Response |
|---|---|---|
| Only the local speaker appears | No system track, or nobody else spoke | Check `info`. Say which you believe |
| Long runs of bare "Yeah." on one track | The other side of a call was never captured | See below. Do not summarize the "Yeah"s as content |
| Transcript thin but video long | Screen-heavy demo | Shift budget to frames, sample more densely |

**A one-sided capture is common and easy to misread.** The recording then looks like a
monologue punctuated by agreement noises, but those are *replies to questions you cannot hear*.
Two consequences:

- A run of "Yeah. Yeah. Yes." marks an **inaudible exchange**, not agreement about nothing.
  Where the reply is substantive, you can reconstruct the question from the answer. Say
  explicitly that you did.
- Never attribute a decision to a named person on that evidence alone.

## Step 3: Read the transcript fully and mark the map

Read all of it, in chunks if long. While reading, mark:

1. **Topic boundaries.** These become sections.
2. **Decisions.** "let's do X", "get rid of Y", "that's fine", reversals.
3. **Action items.** Commitments, with owner and date if stated.
4. **Open questions.** Raised and not resolved.
5. **Deictic cues.** Timestamps where speech points at the screen. These are your frame targets.

Deictic cues are the highest-value signal for frame selection:

```bash
grep -nEi '\b(let me show|look at (this|the)|you can see|right here|over here|this (file|function|line|page|chart|diagram|error|test)|scroll (down|up)|click(ing)? on|check this out|i wrote (it|this) down|demo)\b' transcript.txt
```

Any line whose words alone do not identify the referent is a line whose meaning lives on
screen. Expect few of these, around 7 clusters in a 50-minute recording. Necessary but not
sufficient, so combine with Step 4.

## Step 4: Get uniform coverage with contact sheets

```bash
scripts/watchvid.py sheets <link> frames/ -n 72 --grid 3x3
scripts/watchvid.py sheets <link> frames/ -n 72 --screen Right   # a specific screen
```

72 cells across 9 sheets is one frame every 42s of a 50-minute video, at ~168 visual tokens
per cell instead of ~777. You are not reading code off these. You are building a **map**: which
application is on screen when, and where the visual topic changes.

Scale to duration: ~40 cells under 20 min, ~72 for 30 to 60 min, ~100 for longer. Above ~120
cells, tell the user the cost first.

**On a recording hours long the default is far too sparse.** 72 cells across three hours is one
frame every 2.5 minutes, which will miss whole topics. Either raise the count to ~150 and accept
~35k tokens, or sheet only the stretches the transcript says matter and skip the rest.

**With several screens, sheet the primary one first.** Only sheet the others if the transcript
suggests something happened there. Two screens doubles the cost for what is often a static
second monitor.

## Step 5: Spend full resolution only where it pays

```bash
scripts/watchvid.py ats  <link> frames/ 23:03 24:39 26:45     # several targeted frames
scripts/watchvid.py at   <link> 23:03 statechart.jpg          # one frame
scripts/watchvid.py zoom <link> 39:54 700:420:160:440 checks.jpg   # crop w:h:x:y, upscale
scripts/watchvid.py burst <link> 24:39 frames/ -n 5 --gap 2   # 5 frames, 2s apart
```

Seeking is fast, around 0.4s even 50 minutes in, so pull frames on demand and iterate. Do not
pre-extract "just in case". All of these take `--screen <label>`.

Use `zoom` for terminal output, stack traces and small UI text, because a 1920x1080 frame
downscaled to 1536 loses roughly 8px type. Use `burst` for transitions, animations, and any
moment where a single frame lands mid-scroll.

**Resolution guide**, measured on 1920x1080 screen capture:

| Width | Verdict |
|---|---|
| 1536 | Code identifiers, PR titles, diagram labels all legible. **Default.** |
| 1024 | Headings and layout clear, body text marginal. Fine for the map, not for reading code |
| 640 | Layout only. Contact-sheet cells |

## Step 6: Fuse and write

Build the artifact from both channels, anchored on timestamps. The headers are extraction
instructions to yourself, so keep them specific:

```markdown
# <Title>, <date>, <duration>

**In one line:** <what this was for and what came out of it>

## Decisions
- **<decision>**. <rationale in the speaker's own terms> `[mm:ss]`

## Action items
- [ ] **<owner or UNASSIGNED>**. <action> `[mm:ss]`

## Open questions
- <question, and what blocks answering it> `[mm:ss]`

## Walkthrough
### <topic> `[mm:ss to mm:ss]`
<what was said, and what was on screen, fused>

## On-screen artifacts
- `[mm:ss]` <PR / ticket / diagram / error, with its identifier>

## Gaps
- `[mm:ss to mm:ss]` <inaudible or unclear stretches, and what was lost>
```

Rules for writing it:

- **Timestamp every claim.** A claim with no timestamp cannot be checked, and unverifiable
  claims are where hallucinations hide. If you cannot cite a time, drop the claim.
- **Omission is the failure mode, not invention.** Studies of LLM meeting summaries put missing
  information near-universal and hallucination far behind it. After drafting, re-scan the
  transcript for content your summary does not cover, and ask what you dropped.
- **Quote the load-bearing lines verbatim.** Rationale paraphrased into corporate neutral loses
  the actual argument. Keep the speaker's terms, including hedges and frustration, which carry
  real information about confidence.
- **Never invent names.** Use a name only on explicit evidence: self-identification, direct
  address, or an on-screen name label. "Me" on the microphone track means the person who
  pressed Record, which is not a name. Mark unowned actions `UNASSIGNED`.
- **Separate observed from inferred.** "The presenter said X" against "this implies Y".
- **Record the gaps.** A summary that hides its blind spots is worse than one that names them.

## How good is the speaker labelling

**On a two-sided 26-minute call, about 97% on substantive turns.** Scored by reading every
segment of 10 words or more, 117 of them, and judging the speaker from content alone: two were
wrong and a third was doubtful. Both confirmed errors were re-checked against a 40-second
control slice, which split what the full run had merged.

The failure mode is not voice confusion. It is a **cross-speaker merge**, where one segment
glues the end of one person's sentence to the start of the other's reply. It never confused two
voices in a sustained way, and it never invented a third speaker.

Four limits:

- **Labels are per request, not per recording.** Slice 4's "Speaker 1" is unrelated to slice
  3's. The script stitches them using the 15-second overlap, and it resolved 16 of 17
  boundaries on the test call. **Read the stderr warning.** Any boundary it could not resolve is
  printed with its timestamp, and the labels either side may be swapped.
- **Silence defeats the stitcher, and that is unfixable from audio.** The one boundary it failed
  on sat inside a 1m53s gap where nobody spoke. That is exactly where the labels flipped.
- **"Speaker 1" is positional, not an identity.** Map labels to people only on explicit evidence.
- **One speaker on the system track means one of two things**: genuinely one remote voice, or a
  second the capture never got. Say which you believe and why.

**Run it twice to find the weak segments.** Two runs over the same audio agreed on the speaker
for every substantive turn and disagreed only on short fragments and on segments at a slice
edge. Disagreement marks the segments not to build an attribution on. Agreement is not proof,
but disagreement is proof of trouble.

**Do not try to use a video-call app's active-speaker highlight as ground truth.** Teams draws a
blue ring around the main tile and it looks like a speaking indicator. Tested second by second
across a 26-minute call it agreed 48% of the time, which is chance. One participant's
uninterrupted closing monologue scored 0% ring.

## Asking Gemini about the video directly

```bash
scripts/watchvid.py ask <link> "List every ticket ID visible on the board"
```

**Set `mediaResolution: MEDIA_RESOLUTION_HIGH` and pay for it by dropping `fps` to 0.05.**
These two knobs trade against each other and the trade is overwhelmingly worth making: 20x
fewer frames funds 3.8x more detail per frame at the *same* total cost. Audio is a fixed floor
of 32 tokens/second you cannot reduce, unaffected by either knob. Both are already the defaults.

Measured on the test video, asking for on-screen ticket IDs and CI check names:

| Config | Prompt tokens | Identifiers |
|---|---|---|
| **HIGH + `fps=0.05`** | **116,867** | **correct, and hedges when a value is cut off** |
| HIGH + `fps=0.05` (repeat) | 116,830 | **correct**, agrees with the run above |
| base + `fps=0.2` | 116,801 | **fabricated**: 6 invented ticket IDs, wrong check name, inverted result |
| HIGH + `fps=0.1` (3 runs) | 156,983 | **2 of 3 fabricated** a PR number on one cut-off card, a *different* fake each time |
| HIGH + `fps=0.5` | 479,602 | **wrong**, invented a ticket ID that `fps=0.05` read correctly |
| HIGH + `fps=1.0` | 882,730 | correct, 7.5x the cost for no gain |
| HIGH + `fps=1.0` (repeat) | 882,694 | **incomplete**, returned 1 of 3 cards |

**Raising `fps` does not help, even when cost is no object, and it can hurt.** More frames of a
mostly static screen dilute attention across near-identical images. `fps=2.0` is rejected with a
400, so 1.0 is the API ceiling. **Stay at 0.05.**

**The diagnostic that catches fabrication: run it twice and compare.** Every config reads the
*easy* values identically. The differences appear only at the edge of legibility, and there a
model reading real pixels gives the same answer twice while a model filling a gap invents a
*different* plausible value each time. Treat any identifier that changes as unread.

**Timestamps from `ask` are unreliable at every resolution.** A quote whose true position was
46:24 was placed at 26:27, at 46:28, and once at 1:06:27, a hallucinated hour on a 50-minute
video. Anchor timestamps with `at`, never with `ask` alone. A correct fact under a wrong
timestamp is still a broken citation.

## Delegating to subagents

For a long or dense video, farm frame-reading out to subagents. They read images and return
**text only**, keeping tens of megabytes of pixels out of the main context.

Give each subagent the video path, its time range, the transcript slice for that range, and a
specific question. Ask for a structured text report. Batch ~10 frames per subagent, because
attention thins beyond that. Synthesize in the main context from the text reports.

Worth it above roughly 30 minutes of video, or when the user wants exhaustive coverage.

## What does not work (tested, so you can skip it)

- **`select='gt(scene,X)'` scene detection.** The standard advice, and wrong for screen
  recordings. ffmpeg's scene score is bimodal on screen content: a full window switch scores
  ~1.0 while a scroll or a new line of code scores ~0.001. No threshold separates signal from
  noise. Measured on a 50-minute recording: 16 frames at 0.4, 24 at 0.3, 78 at 0.1.
- **`mpdecimate` as the primary filter.** On a real screen share the cursor, caret and taskbar
  clock move constantly, so almost nothing dedupes: 633 of 761 keyframes survived.
- **`fps=1/N` on a long file.** Decodes every frame, 2m42s for a 50-minute video against 31s
  using input seek. The script seeks per frame in parallel, which brings a 72-cell sheet to ~10s.
- **OCR as a preprocessing step.** Vision reads rendered UI text well, and OCR discards layout,
  which pane the text was in and what changed. That layout is most of the signal.

## Cost

| Approach | 50-min video | Verdict |
|---|---|---|
| Transcript only | ~10k tokens | Always do this first |
| + 9 contact sheets (72 cells) | ~22k | **Default.** Full coverage |
| + 20 targeted 1536px frames | ~53k | Deep read of a specific PR or demo |
| Every deduped frame individually | ~495k | Never |

That is what lands in your context. Transcription is billed separately against the Gemini key,
roughly 100k prompt tokens per 26 minutes of audio, about 75 seconds of wall clock.

Tell the user the estimate before an expensive pass, and let them choose depth.

## Checks before delivering

1. Does every decision and action item carry a timestamp? For the two or three that matter most,
   cut a 40-second control slice around each and re-transcribe it to confirm the time.
2. Did you read the *whole* transcript, or stop at the first chunk?
3. Are inaudible stretches named in **Gaps**, not silently skipped?
4. Is any name in the output supported by explicit evidence?
5. **Every identifier, ticket ID, PR number, version, filename, error string, must come from a
   frame you actually read**, not from speech and not from a model's recall. These are the values
   a reader will copy-paste, and they are exactly where fabrication happens.
6. Pick two claims at random and grep the transcript for them. If either fails, re-check the rest.
