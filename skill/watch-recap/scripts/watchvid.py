#!/usr/bin/env python3
"""watchvid — resolve a recap link and read the recording behind it.

Standard library only. Needs `ffmpeg`/`ffprobe` on PATH, and `GEMINI_API_KEY`
for transcribe/ask. Every subcommand prints the paths it wrote, one per line.

A recap link points at a manifest, not a video. One recording is several files:
one video per screen, the microphone on its own track, and everything the
machine played on another. The split matters. The microphone is one known
person, so its transcript needs no diarization at all. The system track may
carry several remote participants and still does.

  watchvid info   <link>                        what the recording contains
  watchvid fetch  <link> [-o dir]               download every part
  watchvid probe  <video>                       duration, resolution, codecs
  watchvid transcribe <link|media> [--slice 90] merged, speaker-labelled transcript
  watchvid sheets <link|video> <outdir>         contact sheets, whole-video coverage
  watchvid at     <link|video> <time> <out.jpg> one full-res frame
  watchvid ats    <link|video> <outdir> <t>...  several targeted frames
  watchvid zoom   <link|video> <t> <w:h:x:y> <o>  crop + upscale a region
  watchvid burst  <link|video> <time> <outdir>  consecutive frames around a moment
  watchvid ask    <link|video> "question"       ask Gemini about the video
"""
import argparse, glob, json, os, re, shutil, subprocess, sys, tempfile, time
import urllib.parse, urllib.request
from concurrent.futures import ThreadPoolExecutor

GEMINI = "https://generativelanguage.googleapis.com"
# Measured defaults. See SKILL.md for the experiments behind each one.
FPS = 0.05                       # raising this REDUCES identifier accuracy on screen capture
MEDIA_RES = "MEDIA_RESOLUTION_HIGH"   # base resolution fabricates ticket IDs at the same cost
FRAME_W = 1536                   # code and UI labels legible; 1024 is not
DIARIZE_MODEL = "gemini-3.5-flash"
ASK_MODEL = "gemini-3.1-pro-preview"
SLICE = 90                       # seconds per request; the clock drifts with distance into one
OVERLAP = 15                     # seconds shared by neighbours, enough to pin speaker labels


def die(msg):
    sys.exit(f"error: {msg}")


def need(*bins):
    for b in bins:
        if not shutil.which(b):
            die(f"{b} not found on PATH")


def key():
    k = os.environ.get("GEMINI_API_KEY", "")
    if not k:
        die("GEMINI_API_KEY is not set")
    return k


def http(url, data=None, headers=None, retries=4):
    for attempt in range(1, retries + 1):
        try:
            req = urllib.request.Request(url, data=data, headers=headers or {})
            with urllib.request.urlopen(req) as r:
                return r.headers, r.read()
        except urllib.error.HTTPError as e:
            if e.code in (429, 500, 503) and attempt < retries:
                time.sleep(20 * attempt)
                continue
            raise
    raise RuntimeError("unreachable")


def run(cmd):
    p = subprocess.run(cmd, capture_output=True, text=True)
    if p.returncode != 0:
        tail = (p.stderr or "").strip().splitlines()
        die(f"{cmd[0]} failed: {tail[-1] if tail else p.returncode}")
    return p.stdout


def to_sec(v):
    v = str(v).strip()
    if ":" not in v:
        return float(v)
    total = 0.0
    for part in v.split(":"):
        total = total * 60 + float(part)
    return total


def hms(t):
    t = max(0, int(t))
    return f"{t // 3600:02d}:{(t % 3600) // 60:02d}:{t % 60:02d}"


def short(t):
    t = max(0, int(t))
    return f"{t // 60:02d}:{t % 60:02d}"


# ---------------------------------------------------------------- manifests

def is_link(s):
    return str(s).startswith(("http://", "https://"))


MANIFEST_IN_PAGE = re.compile(
    rb'<script[^>]+id="recap-manifest"[^>]*>(.*?)</script>', re.S)


def load_manifest(link):
    """Fetch the part list a recap link points at.

    The link normally opens the player page. That page carries the manifest in
    a `<script type="application/json">` block precisely so this can read it
    without a second signed request. A direct manifest.json URL also works.
    """
    try:
        _, body = http(link)
    except Exception as e:
        die(f"could not fetch the link ({e}). Presigned links expire, ask for a new one.")

    raw = body
    if b"recap-manifest" in body:
        m = MANIFEST_IN_PAGE.search(body)
        if not m:
            die("that page has a recap-manifest tag but nothing readable inside it")
        # The page escapes `</` so the JSON cannot close the script tag early.
        raw = m.group(1).replace(rb"<\/", b"</")

    try:
        manifest = json.loads(raw)
    except Exception:
        die("that link is neither a recap page nor a recap manifest")
    if "parts" not in manifest:
        die("that JSON is not a recap manifest")
    return manifest


def download(url, out):
    os.makedirs(os.path.dirname(os.path.abspath(out)) or ".", exist_ok=True)
    with urllib.request.urlopen(url) as r, open(out, "wb") as f:
        while chunk := r.read(1 << 20):
            f.write(chunk)
    return out


def fetch_bundle(link, outdir):
    """Pull every part of a recording into one directory."""
    m = load_manifest(link)
    os.makedirs(outdir, exist_ok=True)
    with open(os.path.join(outdir, "manifest.json"), "w") as f:
        json.dump(m, f, indent=1)
    for p in m["parts"]:
        dest = os.path.join(outdir, os.path.basename(p["key"]))
        if not os.path.exists(dest) or os.path.getsize(dest) != p.get("bytes", -1):
            download(p["url"], dest)
        p["local"] = dest
    return m


def resolve(target, cache=None):
    """Accept a link or a local path and return (manifest_or_None, local_dir_or_path).

    A link is downloaded once into a cache directory so several subcommands in
    a row do not re-fetch the same recording.
    """
    if not is_link(target):
        return None, target
    m = load_manifest(target)
    d = cache or os.path.join(tempfile.gettempdir(), f"watchvid-{m['id']}")
    os.makedirs(d, exist_ok=True)
    for p in m["parts"]:
        dest = os.path.join(d, os.path.basename(p["key"]))
        if not os.path.exists(dest) or os.path.getsize(dest) != p.get("bytes", -1):
            print(f"# downloading {p['label']}…", file=sys.stderr)
            download(p["url"], dest)
        p["local"] = dest
    return m, d


def pick_video(m, which=None):
    """The monitor to sample frames from, and how far it lags the shared clock."""
    vids = [p for p in m["parts"] if p["kind"] == "video"]
    if not vids:
        die("this recording has no video track")
    if which:
        for p in vids:
            if p["label"] == which or os.path.basename(p["key"]) == which:
                return p
        die(f"no monitor called {which!r}. Have: {', '.join(p['label'] for p in vids)}")
    return vids[0]


def media_arg(target, which=None, cache=None):
    """Turn a link-or-path into (video file, offset seconds).

    Every timestamp in a transcript is on the shared clock, but each video file
    starts at its own zero. A monitor that began capturing 1.1s after the
    earliest track needs that subtracted, or every frame you pull is late by
    exactly that much. Offsets used to reach three seconds before the recorder
    started stamping its real first frame, and they still run to about a second.
    """
    m, d = resolve(target, cache)
    if not m:
        return d, 0.0
    p = pick_video(m, which)
    return p["local"], p.get("offset_ms", 0) / 1000.0


# ---------------------------------------------------------------- ffmpeg

def probe(path):
    return json.loads(run(["ffprobe", "-v", "error", "-show_format",
                           "-show_streams", "-of", "json", path]))


def duration(path):
    out = run(["ffprobe", "-v", "error", "-show_entries", "format=duration",
               "-of", "default=noprint_wrappers=1:nokey=1", path])
    try:
        return float(out.strip())
    except ValueError:
        return 0.0


def stamp(t, size=30):
    """Burn the true wall-clock time into the pixels.

    Seeking before -i resets the presentation timestamp to zero, so ffmpeg's
    own %{pts} would print 00:00:00 on every seeked frame. Passing the known
    time as literal text is the only way to get a citable stamp."""
    label = hms(t).replace(":", r"\:")
    return (f"drawtext=text='{label}':x=8:y=8:fontsize={size}:fontcolor=yellow"
            f":box=1:boxcolor=black@0.75:boxborderw=6")


def grab(video, t, out, width=FRAME_W, crop=None, size=30, label=None):
    """`t` seeks in this file's own timeline. `label` is what gets burned in,
    which is the shared-clock time when the two differ."""
    stamped = t if label is None else label
    vf = (f"crop={crop}," if crop else "") + f"scale={width}:-2,{stamp(stamped, size)}"
    os.makedirs(os.path.dirname(os.path.abspath(out)) or ".", exist_ok=True)
    run(["ffmpeg", "-nostdin", "-v", "error", "-ss", str(t), "-i", video,
         "-frames:v", "1", "-vf", vf, "-y", out])
    return out


def extract_audio(media, out):
    run(["ffmpeg", "-nostdin", "-v", "error", "-i", media, "-vn", "-ac", "1",
         "-ar", "16000", "-c:a", "libopus", "-b:a", "32k", "-y", out])
    return out


def slice_audio(src, start, length, out):
    run(["ffmpeg", "-nostdin", "-v", "error", "-ss", str(start), "-t", str(length),
         "-i", src, "-vn", "-ac", "1", "-ar", "16000", "-c:a", "libopus",
         "-b:a", "32k", "-y", out])
    return out


# ---------------------------------------------------------------- gemini

def upload(path, mime):
    size = os.path.getsize(path)
    h, _ = http(f"{GEMINI}/upload/v1beta/files?key={key()}",
                data=json.dumps({"file": {"display_name": os.path.basename(path)}}).encode(),
                headers={"X-Goog-Upload-Protocol": "resumable",
                         "X-Goog-Upload-Command": "start",
                         "X-Goog-Upload-Header-Content-Length": str(size),
                         "X-Goog-Upload-Header-Content-Type": mime,
                         "Content-Type": "application/json"})
    # Hand the open file to urllib rather than its contents. http.client sends a
    # file object in blocks, so a multi-gigabyte video never lands in memory.
    with open(path, "rb") as f:
        _, body = http(h["x-goog-upload-url"], data=f,
                       headers={"X-Goog-Upload-Offset": "0",
                                "X-Goog-Upload-Command": "upload, finalize",
                                "Content-Length": str(size)})
    info = json.loads(body)["file"]
    while info["state"] == "PROCESSING":
        time.sleep(3)
        _, b = http(f"{GEMINI}/v1beta/{info['name']}?key={key()}")
        info = json.loads(b)
    if info["state"] != "ACTIVE":
        die(f"upload ended in state {info['state']}")
    return info["uri"]


def generate(model, parts, cfg):
    _, out = http(f"{GEMINI}/v1beta/models/{model}:generateContent?key={key()}",
                  data=json.dumps({"contents": [{"role": "user", "parts": parts}],
                                   "generationConfig": cfg}).encode(),
                  headers={"Content-Type": "application/json"})
    d = json.loads(out)
    if "candidates" not in d:
        die(f"no candidates returned: {json.dumps(d)[:300]}")
    text = "".join(p.get("text", "") for p in d["candidates"][0]["content"]["parts"])
    return text, d.get("usageMetadata", {})


# Requiring `speaker` and ordering it BEFORE `text` is the whole diarization
# mechanism: it forces the attribution decision before the words are generated.
# The prompt never mentions speakers.
DIARIZED = {"type": "ARRAY", "items": {"type": "OBJECT", "properties": {
    "speaker": {"type": "STRING", "description": 'Speaker identifier (e.g. "Speaker 1")'},
    "start": {"type": "NUMBER", "description": "Start time of this segment in seconds"},
    "text": {"type": "STRING", "description": "Transcribed text for this segment"}},
    "required": ["speaker", "start", "text"],
    "propertyOrdering": ["speaker", "start", "text"]}}

# The microphone is one known person. Asking for labels there only invites the
# model to split one voice into several, so we do not ask.
PLAIN = {"type": "ARRAY", "items": {"type": "OBJECT", "properties": {
    "start": {"type": "NUMBER", "description": "Start time of this segment in seconds"},
    "text": {"type": "STRING", "description": "Transcribed text for this segment"}},
    "required": ["start", "text"],
    "propertyOrdering": ["start", "text"]}}

PROMPT = ("Generate a transcript in English for this file. "
          "Group similar text together rather than timestamping every line.")


def norm_speaker(label):
    """The model sometimes returns the label with spaces inside it: "S pea ker 1",
    "Speaker 1 ". Left alone those become extra speakers in the output."""
    flat = re.sub(r"\s+", "", label or "")
    m = re.fullmatch(r"(?i)speaker(\d+)", flat)
    return f"Speaker {m.group(1)}" if m else ((label or "").strip() or "Speaker ?")


def transcribe_one(path, model, offset, span, diarize):
    uri = upload(path, "audio/ogg")
    text, usage = generate(model,
                           [{"fileData": {"mimeType": "audio/ogg", "fileUri": uri}},
                            {"text": PROMPT}],
                           {"responseMimeType": "application/json",
                            "responseJsonSchema": DIARIZED if diarize else PLAIN})
    try:
        raw = json.loads(text)
    except json.JSONDecodeError:
        raw = []
    segs = [{"speaker": norm_speaker(s.get("speaker", "Speaker 1")),
             "start": float(s.get("start", 0) or 0),
             "text": (s.get("text") or "").strip()}
            for s in raw if (s.get("text") or "").strip()]
    segs.sort(key=lambda s: s["start"])
    # Squeeze an over-running slice back into its own length. Never stretch the
    # other way: a slice reporting less than its duration is just quiet at the end.
    hi = segs[-1]["start"] if segs else 0
    k = span / hi if span and hi > span else 1.0
    for s in segs:
        s["start"] = s["start"] * k + offset
    return segs, usage.get("totalTokenCount", 0)


def similarity(a, b):
    """Character-bigram overlap, standing in for difflib without the import."""
    ga = [a.lower()[i:i + 2] for i in range(len(a) - 1)]
    gb = [b.lower()[i:i + 2] for i in range(len(b) - 1)]
    if not ga or not gb:
        return 1.0 if a.strip() == b.strip() else 0.0
    pool, hits = list(gb), 0
    for g in ga:
        if g in pool:
            pool.remove(g)
            hits += 1
    return 2 * hits / (len(ga) + len(gb))


def stitch(slices):
    """Gemini labels each request independently, so slice 4's "Speaker 1" has
    nothing to do with slice 3's. Neighbouring slices transcribe the same
    OVERLAP seconds twice, and an utterance landing in both pins one label set
    to the other. Where nothing matches, the boundary is reported rather than
    guessed, because silence there is unfixable from audio alone."""
    live = [s for s in slices if s]
    if not live:
        return [], []
    out, unresolved = list(live[0]), []
    for cur in live[1:]:
        best = (0.0, None, None)
        for a in out[-8:]:
            if len(a["text"].split()) < 5:
                continue
            for b in cur[:8]:
                r = similarity(a["text"], b["text"])
                if r > best[0]:
                    best = (r, a["speaker"], b["speaker"])
        seen = sorted({s["speaker"] for s in out})
        here = sorted({s["speaker"] for s in cur})
        table = {}
        if best[0] >= 0.55:
            _, anchor_seen, anchor_here = best
            table[anchor_here] = anchor_seen
            spare = [x for x in seen if x != anchor_seen]
            for lbl in (x for x in here if x != anchor_here):
                if spare:
                    table[lbl] = spare.pop(0)
        else:
            unresolved.append(cur[0]["start"])
            if len(here) == len(seen):
                table = dict(zip(here, seen))
        for s in cur:
            s["speaker"] = table.get(s["speaker"], s["speaker"])
        out += cur
    out.sort(key=lambda s: s["start"])
    return out, unresolved


def transcribe_track(media, model, base_offset, slice_secs, speaker=None):
    """One audio file in, segments out.

    `speaker` fixes every segment to one name and skips diarization, which is
    what the microphone wants. None diarizes, which the system track needs.
    """
    diarize = speaker is None
    tmp = tempfile.mkdtemp(prefix="wv_")
    try:
        src = media
        if not src.lower().endswith((".m4a", ".mp3", ".wav", ".aac", ".ogg", ".opus", ".flac")):
            src = extract_audio(src, os.path.join(tmp, "audio.opus"))
        dur = duration(src)
        if dur <= 0:
            die(f"{media} has no audio")

        jobs = []
        if slice_secs and dur > slice_secs:
            i, start = 0, 0.0
            while start < dur:
                span = min(slice_secs + OVERLAP, dur - start)
                part = slice_audio(src, start, span, os.path.join(tmp, f"s{i:03d}.opus"))
                jobs.append((part, base_offset + start, span))
                start += slice_secs
                i += 1
        else:
            jobs = [(src, base_offset, dur)]

        slices, tokens = [], 0
        with ThreadPoolExecutor(max_workers=min(4, len(jobs))) as ex:
            for segs, tk in ex.map(
                    lambda j: transcribe_one(j[0], model, j[1], j[2], diarize), jobs):
                slices.append(segs)
                tokens += tk
    finally:
        shutil.rmtree(tmp, ignore_errors=True)

    if diarize:
        segs, unresolved = stitch(slices)
    else:
        segs = sorted([s for sl in slices for s in sl], key=lambda s: s["start"])
        unresolved = []
        for s in segs:
            s["speaker"] = speaker

    # The overlap makes neighbouring slices transcribe the same seconds twice.
    kept = []
    for s in segs:
        if any(abs(k["start"] - s["start"]) <= OVERLAP + 10
               and similarity(k["text"], s["text"]) > 0.8 for k in kept[-6:]):
            continue
        kept.append(s)
    return kept, tokens, unresolved


def cmd_transcribe(a):
    need("ffmpeg", "ffprobe")
    m, local = resolve(a.target)
    tokens, unresolved, segs = 0, [], []

    if m:
        # Two tracks, two different jobs. The mic is one known person and needs
        # no diarization. The system mix may hold several remote participants.
        base = a.offset and to_sec(a.offset) or 0.0
        todo = []
        for p in m["parts"]:
            if p["kind"] == "mic":
                todo.append((p, m.get("local_speaker") or "Me"))
            elif p["kind"] == "system":
                todo.append((p, None))
        if not todo:
            die("this recording has no audio tracks")
        for p, speaker in todo:
            off = base + p.get("offset_ms", 0) / 1000.0
            print(f"# {p['label']}: "
                  f"{'fixed speaker ' + speaker if speaker else 'diarizing'}…",
                  file=sys.stderr)
            s, tk, un = transcribe_track(p["local"], a.model, off, a.slice, speaker)
            segs += s
            tokens += tk
            unresolved += un
        segs.sort(key=lambda s: s["start"])
    else:
        segs, tokens, unresolved = transcribe_track(
            local, a.model, to_sec(a.offset), a.slice, a.speaker)

    speakers = sorted({s["speaker"] for s in segs})
    print(f"# segments={len(segs)} speakers={speakers} tokens={tokens}", file=sys.stderr)
    if len(speakers) == 1:
        print("# NOTE: one speaker. Either genuinely one voice, or a capture where "
              "other participants were never recorded. These are indistinguishable "
              "from audio alone. Say which you believe and why.", file=sys.stderr)
    if unresolved:
        at = ", ".join(short(t) for t in unresolved[:8])
        print(f"# WARNING: {len(unresolved)} slice boundary(s) had no shared utterance "
              f"long enough to match, so labels either side may be swapped: {at}. "
              "Re-derive those from content before you attribute anything.", file=sys.stderr)

    out = sys.stdout if a.output in (None, "-") else open(a.output, "w")
    try:
        if a.json:
            json.dump(segs, out, indent=1)
            out.write("\n")
        else:
            for s in segs:
                out.write(f"[{short(s['start'])}] {s['speaker']}: {s['text']}\n")
    finally:
        if out is not sys.stdout:
            out.close()
            print(a.output)


def cmd_ask(a):
    video, _ = media_arg(a.target, a.screen)
    # `ask` sends the whole file. On a recording hours long that is both a slow
    # upload and a very large prompt, so say so rather than appear to hang.
    size = os.path.getsize(video)
    if size > 2 * 1024**3:
        die(f"{video} is {size / 1024**3:.1f} GB. Cut a slice with ffmpeg and ask about that.")
    if size > 512 * 1024**2:
        print(f"# uploading {size / 1024**2:.0f} MB, this takes a while…", file=sys.stderr)
    uri = upload(video, "video/mp4")
    part = {"fileData": {"mimeType": "video/mp4", "fileUri": uri},
            "videoMetadata": {"fps": a.fps}}
    guard = ("Answer ONLY from what is legibly visible on screen or audible. "
             "If a value is not readable, write UNREADABLE — do not guess.\n\n")
    text, usage = generate(a.model, [part, {"text": guard + a.question}],
                           {"mediaResolution": a.media_resolution})
    print(f"# model={a.model} fps={a.fps} res={a.media_resolution} "
          f"prompt_tokens={usage.get('promptTokenCount')}", file=sys.stderr)
    print(text)


# ---------------------------------------------------------------- frames

def cmd_sheets(a):
    """Tile the whole recording into a few grids. A cell costs roughly 168
    visual tokens against 777 for a full frame, so this is how you get uniform
    coverage without spending the budget on redundant screens."""
    need("ffmpeg", "ffprobe")
    video, skew = media_arg(a.target, a.screen)
    cols, rows = (int(x) for x in a.grid.lower().split("x"))
    per = cols * rows
    dur = duration(video)
    if dur <= 0:
        die("could not read the duration")
    os.makedirs(a.outdir, exist_ok=True)
    tmp = tempfile.mkdtemp(prefix="wv_cells_")
    try:
        step = dur / a.count
        # Seeking per cell beats fps=1/N, which decodes the entire file.
        # Cells are stamped with the shared-clock time, so a label read off a
        # sheet can be handed straight back to `at` or quoted next to the
        # transcript.
        with ThreadPoolExecutor(max_workers=8) as ex:
            list(ex.map(lambda i: grab(video, (i + 0.5) * step,
                                       os.path.join(tmp, f"c{i:04d}.jpg"), 640,
                                       size=22, label=(i + 0.5) * step + skew),
                        range(a.count)))
        cells = sorted(glob.glob(os.path.join(tmp, "c*.jpg")))
        written = []
        for n, i in enumerate(range(0, len(cells), per), start=1):
            group = cells[i:i + per]
            out = os.path.join(a.outdir, f"sheet_{n:02d}.jpg")
            cmd = ["ffmpeg", "-nostdin", "-v", "error"]
            for g in group:
                cmd += ["-i", g]
            streams = "".join(f"[{j}:v]" for j in range(len(group)))
            concat = f"concat=n={len(group)}:v=1:a=0," if len(group) > 1 else ""
            cmd += ["-filter_complex",
                    f"{streams}{concat}tile={cols}x{rows}:padding=4:color=black",
                    "-frames:v", "1", "-y", out]
            run(cmd)
            written.append(out)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    for w in written:
        print(w)


def cmd_at(a):
    need("ffmpeg")
    video, skew = media_arg(a.target, a.screen)
    print(grab(video, to_sec(a.time) - skew, a.output, a.width, label=to_sec(a.time)))


def cmd_ats(a):
    need("ffmpeg")
    video, skew = media_arg(a.target, a.screen)
    os.makedirs(a.outdir, exist_ok=True)
    for i, t in enumerate(a.times, start=1):
        sec = to_sec(t)
        out = os.path.join(a.outdir, f"at_{i:02d}_{hms(sec).replace(':', '')}.jpg")
        print(grab(video, sec - skew, out, FRAME_W, label=sec))


def cmd_zoom(a):
    """Terminal output, stack traces and small UI text survive a crop and
    upscale where a downscaled full frame loses them."""
    need("ffmpeg")
    video, skew = media_arg(a.target, a.screen)
    print(grab(video, to_sec(a.time) - skew, a.output, FRAME_W,
               crop=a.region, label=to_sec(a.time)))


def cmd_burst(a):
    """For transitions, animations and any moment where one frame lands
    mid-scroll."""
    need("ffmpeg")
    video, skew = media_arg(a.target, a.screen)
    os.makedirs(a.outdir, exist_ok=True)
    base = to_sec(a.time)
    for i in range(a.count):
        t = base + i * a.gap
        out = os.path.join(a.outdir, f"b_{i + 1:02d}_{hms(t).replace(':', '')}.jpg")
        print(grab(video, t - skew, out, FRAME_W, label=t))


# ---------------------------------------------------------------- info

def cmd_info(a):
    m = load_manifest(a.link)
    when = time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(m.get("created", 0)))
    print(f"id:         {m['id']}")
    print(f"created:    {when}")
    print(f"duration:   {short(m.get('duration', 0))} ({m.get('duration', 0):.1f}s)")
    print(f"local mic:  {m.get('local_speaker', 'unknown')}")
    print("parts:")
    for p in m["parts"]:
        print(f"  {p['kind']:<7} {p['label']:<16} {p.get('duration', 0):>7.1f}s "
              f"{p.get('bytes', 0) / 1048576:>7.1f} MB  offset {p.get('offset_ms', 0):+}ms")


def cmd_fetch(a):
    m = fetch_bundle(a.link, a.outdir)
    for p in m["parts"]:
        print(p["local"])


def cmd_probe(a):
    info = probe(media_arg(a.target)[0])
    fmt = info["format"]
    print(f"duration: {short(float(fmt['duration']))} ({float(fmt['duration']):.0f}s)")
    for s in info["streams"]:
        if s["codec_type"] == "video":
            print(f"video:    {s['width']}x{s['height']} {s.get('r_frame_rate')} {s['codec_name']}")
        elif s["codec_type"] == "audio":
            print(f"audio:    {s['codec_name']} {s.get('sample_rate')}Hz {s.get('channels')}ch")


def main():
    p = argparse.ArgumentParser(prog="watchvid", description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)

    s = sub.add_parser("info", help="what a recap link contains")
    s.add_argument("link"); s.set_defaults(fn=cmd_info)

    s = sub.add_parser("fetch", help="download every part of a recording")
    s.add_argument("link"); s.add_argument("-o", "--outdir", default="recording")
    s.set_defaults(fn=cmd_fetch)

    s = sub.add_parser("probe", help="duration, resolution, codecs")
    s.add_argument("target"); s.set_defaults(fn=cmd_probe)

    s = sub.add_parser("transcribe", help="merged, speaker-labelled transcript")
    s.add_argument("target"); s.add_argument("-o", "--output")
    s.add_argument("--model", default=DIARIZE_MODEL)
    s.add_argument("--slice", type=int, default=SLICE,
                   help=f"seconds per request (default {SLICE}, 0=off). Longer slices "
                        "drift: timestamps stretch with distance into a slice")
    s.add_argument("--offset", default="0",
                   help="where this media starts in the original recording (e.g. 20:00)")
    s.add_argument("--speaker", default=None,
                   help="fix every segment to this name and skip diarization")
    s.add_argument("--json", action="store_true")
    s.set_defaults(fn=cmd_transcribe)

    def screen(sp):
        sp.add_argument("--screen", default=None,
                        help="which screen, by label, when the link has several")

    s = sub.add_parser("sheets", help="contact sheets over the whole video")
    s.add_argument("target"); s.add_argument("outdir")
    s.add_argument("-n", "--count", type=int, default=72)
    s.add_argument("--grid", default="3x3"); screen(s); s.set_defaults(fn=cmd_sheets)

    s = sub.add_parser("at", help="one full-res frame")
    s.add_argument("target"); s.add_argument("time"); s.add_argument("output")
    s.add_argument("-w", "--width", type=int, default=FRAME_W); screen(s)
    s.set_defaults(fn=cmd_at)

    s = sub.add_parser("ats", help="several targeted frames")
    s.add_argument("target"); s.add_argument("outdir"); s.add_argument("times", nargs="+")
    screen(s); s.set_defaults(fn=cmd_ats)

    s = sub.add_parser("zoom", help="crop w:h:x:y then upscale")
    s.add_argument("target"); s.add_argument("time"); s.add_argument("region")
    s.add_argument("output"); screen(s); s.set_defaults(fn=cmd_zoom)

    s = sub.add_parser("burst", help="consecutive frames around a moment")
    s.add_argument("target"); s.add_argument("time"); s.add_argument("outdir")
    s.add_argument("-n", "--count", type=int, default=5)
    s.add_argument("--gap", type=float, default=2.0); screen(s)
    s.set_defaults(fn=cmd_burst)

    s = sub.add_parser("ask", help="ask Gemini about the video")
    s.add_argument("target"); s.add_argument("question")
    s.add_argument("--fps", type=float, default=FPS)
    s.add_argument("--media-resolution", default=MEDIA_RES)
    s.add_argument("--model", default=ASK_MODEL); screen(s)
    s.set_defaults(fn=cmd_ask)

    a = p.parse_args()
    if a.cmd in ("probe", "sheets", "at", "ats", "zoom", "burst", "transcribe"):
        need("ffmpeg", "ffprobe")
    a.fn(a)


if __name__ == "__main__":
    main()
