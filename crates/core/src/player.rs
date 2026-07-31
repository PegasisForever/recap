//! The page a recap link opens.
//!
//! A recording is several files that were captured at once and have to be
//! played back that way. This generates a self-contained page holding one
//! `<video>` per monitor and one `<audio>` per track, driven by a single
//! transport: play, pause and one seek bar for all of them.
//!
//! The same page is the machine-readable form. The manifest is embedded in a
//! `<script type="application/json">` block, so `watchvid.py` can be handed the
//! page URL and find every part from it without a second request.

use crate::manifest::Manifest;

/// `</script>` inside the JSON would end the block early, whatever it is
/// escaped as in JSON terms.
fn embed_json(m: &Manifest) -> String {
    serde_json::to_string(m)
        .unwrap_or_else(|_| "{}".into())
        .replace("</", r"<\/")
}

pub fn render(m: &Manifest) -> String {
    let videos: String = m
        .videos()
        .map(|p| {
            format!(
                // An inline SVG rather than a glyph: U+26F6 and friends are
                // missing from plenty of system fonts and render as a box.
                r#"<figure><video data-part playsinline preload="metadata" src="{}"></video>
<figcaption><span>{}</span><button class="fs" title="Full screen" aria-label="Full screen">
<svg viewBox="0 0 16 16" width="15" height="15" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><path d="M2 6V2h4M14 6V2h-4M2 10v4h4M14 10v4h-4"/></svg>
</button></figcaption></figure>"#,
                html_attr(&p.url),
                html_text(&p.label)
            )
        })
        .collect();

    let audios: String = m
        .parts
        .iter()
        .filter(|p| p.kind != crate::manifest::PartKind::Video)
        .map(|p| {
            format!(
                r#"<audio data-part preload="metadata" src="{}" title="{}"></audio>"#,
                html_attr(&p.url),
                html_text(&p.label)
            )
        })
        .collect();

    // offset_ms per part, in the same order the elements appear.
    let offsets: Vec<String> = m
        .videos()
        .chain(m.parts.iter().filter(|p| p.kind != crate::manifest::PartKind::Video))
        .map(|p| p.offset_ms.to_string())
        .collect();

    let title = format!("Recap {}", &m.id[..m.id.len().min(8)]);
    let grid = if m.videos().count() > 1 { "two" } else { "one" };

    format!(
        r#"<!doctype html>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>{title}</title>
<script type="application/json" id="recap-manifest">{manifest}</script>
<style>
:root {{ color-scheme: light dark; --bg:#fff; --fg:#1b1b1b; --dim:#6a6a6a; --line:#e0e0e0; --accent:#3584e4; }}
@media (prefers-color-scheme: dark) {{
  :root {{ --bg:#1b1b1b; --fg:#f2f2f2; --dim:#a0a0a0; --line:#3a3a3a; }}
}}
* {{ box-sizing: border-box; }}
body {{ margin:0; background:var(--bg); color:var(--fg);
       font:15px/1.5 system-ui,-apple-system,"Segoe UI",sans-serif; }}
header {{ padding:14px 18px; border-bottom:1px solid var(--line); }}
h1 {{ margin:0; font-size:15px; font-weight:600; }}
header p {{ margin:2px 0 0; color:var(--dim); font-size:13px; }}
main {{ padding:18px; }}
.screens {{ display:grid; gap:14px; }}
.screens.two {{ grid-template-columns: repeat(auto-fit, minmax(420px, 1fr)); }}
figure {{ margin:0; }}
video {{ width:100%; background:#000; border-radius:8px; display:block; }}
figcaption {{ color:var(--dim); font-size:13px; padding-top:6px;
              display:flex; align-items:center; justify-content:space-between; gap:8px; }}
.fs {{ background:none; color:var(--dim); width:28px; height:28px;
       border-radius:6px; cursor:pointer; display:grid; place-items:center; }}
.fs:hover {{ background:var(--line); color:var(--fg); }}
.transport {{ position:sticky; bottom:0; background:var(--bg); border-top:1px solid var(--line);
              display:flex; align-items:center; gap:14px; padding:12px 18px; }}

/* Full screen gives the whole screen to the picture. The transport and the
   label float on top of it rather than taking a strip of their own, because
   the reason to enlarge one monitor is to read what is on it. */
figure:fullscreen {{ background:#000; position:relative; }}
figure:fullscreen video {{ position:absolute; inset:0; width:100%; height:100%;
                           border-radius:0; object-fit:contain; }}
/* Nothing sits over the picture except the transport, and leaving is a button
   on that bar rather than a second overlay of its own. */
figure:fullscreen figcaption {{ display:none; }}
#exitfs {{ display:none; }}
figure:fullscreen #exitfs {{ display:grid; place-items:center; color:#fff; }}
figure:fullscreen #exitfs:hover {{ background:#ffffff2e; }}
figure:fullscreen .transport {{ position:absolute; z-index:2; left:24px; right:24px; bottom:24px;
                                background:#000000b8; border:0; border-radius:999px;
                                padding:10px 16px; color:#fff;
                                backdrop-filter:blur(10px); -webkit-backdrop-filter:blur(10px);
                                transition:opacity .25s ease; }}
figure:fullscreen .time {{ color:#ccc; }}
/* Out of the way while playing and untouched, back on any mouse movement.
   Never hidden while paused, because then there is nothing to watch and
   nothing to signal that moving the mouse would bring the controls back. */
figure:fullscreen.idle .transport {{ opacity:0; pointer-events:none; }}
figure:fullscreen.idle {{ cursor:none; }}
button {{ font:inherit; border:0; border-radius:999px; background:var(--accent); color:#fff;
          width:46px; height:46px; cursor:pointer; flex:none; font-size:17px; }}
button:disabled {{ opacity:.5; cursor:default; }}
input[type=range] {{ flex:1; accent-color:var(--accent); }}
.time {{ font-variant-numeric:tabular-nums; color:var(--dim); font-size:13px; flex:none; }}
</style>

<header>
  <h1>{title}</h1>
  <p id="meta"></p>
</header>

<main>
  <div class="screens {grid}">{videos}</div>
  {audios}
</main>

<div class="transport" id="transport">
  <button id="play" aria-label="Play">&#9654;</button>
  <input id="seek" type="range" min="0" max="1000" value="0" step="1" aria-label="Seek">
  <span class="time"><span id="now">0:00</span> / <span id="total">0:00</span></span>
  <button class="fs" id="exitfs" title="Exit full screen" aria-label="Exit full screen">
<svg viewBox="0 0 16 16" width="15" height="15" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><path d="M6 2v4H2M10 2v4h4M6 14v-4H2M10 14v-4h4"/></svg>
  </button>
</div>

<script>
const M = JSON.parse(document.getElementById("recap-manifest").textContent);
const OFFSETS = {offsets};
const els = [...document.querySelectorAll("[data-part]")];
const playBtn = document.getElementById("play");
const seek = document.getElementById("seek");
const nowEl = document.getElementById("now");
const totalEl = document.getElementById("total");

// Every part starts at a slightly different instant, because each one is
// captured by its own process. Subtracting its offset puts them on one clock.
const startOf = i => (OFFSETS[i] || 0) / 1000;
const endOf = i => startOf(i) + (els[i].duration || 0);
// The recording lasts until the last track finishes on the shared clock. The
// monitors usually stop before the microphone does, so taking the longest file
// would cut the end off.
const duration = () => Math.max(M.duration || 0, ...els.map((_, i) => endOf(i)), 0);

const fmt = s => {{
  s = Math.max(0, Math.floor(s));
  const h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60), x = s % 60;
  return h ? `${{h}}:${{String(m).padStart(2,"0")}}:${{String(x).padStart(2,"0")}}`
           : `${{m}}:${{String(x).padStart(2,"0")}}`;
}};

// Sync thresholds, in seconds.
const IN_SYNC = 0.04;      // close enough that nobody can see it
const HARD_RESEEK = 0.5;   // too far to close by nudging, so jump
const NUDGE = 0.04;        // 4% rate change, well under the pitch-shift a listener notices

let master = 0;      // seconds on the shared timeline
let playing = false;
let raf = null;
let t0 = 0;          // performance.now() corresponding to master === 0

function seekAll(t) {{
  master = Math.min(Math.max(0, t), duration());
  // Rebase the clock, or the next frame would compute the old position back.
  t0 = performance.now() - master * 1000;
  els.forEach((e, i) => {{
    const local = master - startOf(i);
    // A part that has not begun yet parks at zero and stays silent.
    if (local < 0) {{ e.currentTime = 0; e.muted = true; }}
    else if (local > (e.duration || Infinity)) {{ e.muted = true; }}
    else {{ e.muted = false; if (Math.abs(e.currentTime - local) > 0.2) e.currentTime = local; }}
  }});
  paint();
}}

function paint() {{
  const d = duration() || 1;
  seek.value = Math.round((master / d) * 1000);
  nowEl.textContent = fmt(master);
  totalEl.textContent = fmt(d);
}}

// Wall time drives the clock, not any one element. A video that ends early
// must not be able to stall the transport, and the tracks here routinely have
// different lengths: the monitors stop when you press Stop, the microphone
// runs a little longer.
function tick() {{
  master = (performance.now() - t0) / 1000;

  // If something is buffering, hold the clock back to it rather than running
  // ahead and silently desyncing every other track.
  let lag = 0;
  els.forEach((e, i) => {{
    const local = master - startOf(i);
    if (!e.paused && local > 0 && local < (e.duration || 0)) {{
      lag = Math.max(lag, local - e.currentTime);
    }}
  }});
  if (lag > 0.5) {{ t0 += lag * 1000; master -= lag; }}

  els.forEach((e, i) => {{
    const local = master - startOf(i);
    if (local >= 0 && local <= (e.duration || Infinity)) {{
      if (e.paused && playing) e.play().catch(() => {{}});
      e.muted = false;
      // Two monitors side by side make even a fifth of a second obvious, but
      // seeking to fix it is a visible jump. So: seek only when badly out,
      // otherwise run slightly fast or slow until the gap closes on its own.
      const drift = e.currentTime - local;
      if (Math.abs(drift) > HARD_RESEEK) {{
        e.currentTime = local;
        e.playbackRate = 1;
      }} else if (Math.abs(drift) > IN_SYNC) {{
        e.playbackRate = drift > 0 ? 1 - NUDGE : 1 + NUDGE;
      }} else if (e.playbackRate !== 1) {{
        e.playbackRate = 1;
      }}
    }} else {{
      e.muted = true;
      e.playbackRate = 1;
      if (!e.paused) e.pause();
    }}
  }});
  paint();
  if (master >= duration() - 0.05) {{ stop(); master = duration(); paint(); return; }}
  raf = requestAnimationFrame(tick);
}}

function start() {{
  if (master >= duration() - 0.05) master = 0;   // replay after it has ended
  playing = true;
  t0 = performance.now() - master * 1000;
  playBtn.innerHTML = "&#10073;&#10073;";
  playBtn.setAttribute("aria-label", "Pause");
  els.forEach((e, i) => {{
    const local = master - startOf(i);
    if (local >= 0 && local <= (e.duration || Infinity)) {{
      if (Math.abs(e.currentTime - local) > 0.2) e.currentTime = local;
      e.play().catch(() => {{}});
    }}
  }});
  raf = requestAnimationFrame(tick);
  wake();   // arms the idle countdown
}}

function stop() {{
  playing = false;
  playBtn.innerHTML = "&#9654;";
  playBtn.setAttribute("aria-label", "Play");
  els.forEach(e => e.pause());
  if (raf) cancelAnimationFrame(raf);
  wake();   // paused means the controls stay put
}}

playBtn.onclick = () => (playing ? stop() : start());
seek.oninput = () => {{ const was = playing; if (was) stop(); seekAll(seek.value / 1000 * duration()); if (was) start(); }};

document.addEventListener("keydown", e => {{
  if (e.target.tagName === "INPUT") return;
  if (e.code === "Space") {{ e.preventDefault(); playing ? stop() : start(); }}
  if (e.code === "ArrowRight") seekAll(master + 5);
  if (e.code === "ArrowLeft") seekAll(master - 5);
}});

// Full screen one monitor while still driving everything from one transport.
// The bar is moved into the fullscreened figure, because anything outside the
// fullscreen element is not rendered at all.
const transport = document.getElementById("transport");
document.querySelectorAll("figure .fs").forEach(btn => {{
  btn.onclick = () => btn.closest("figure").requestFullscreen().catch(() => {{}});
}});
document.getElementById("exitfs").onclick = () => document.exitFullscreen();

const IDLE_MS = 2500;
let idleTimer = null;
function wake() {{
  const fig = document.fullscreenElement;
  clearTimeout(idleTimer);
  if (!fig) return;
  fig.classList.remove("idle");
  if (playing) idleTimer = setTimeout(() => fig.classList.add("idle"), IDLE_MS);
}}
document.addEventListener("mousemove", wake);
document.addEventListener("keydown", wake);

document.addEventListener("fullscreenchange", () => {{
  const fig = document.fullscreenElement;
  clearTimeout(idleTimer);
  document.querySelectorAll("figure.idle").forEach(f => f.classList.remove("idle"));
  if (fig && fig.tagName === "FIGURE") {{
    fig.appendChild(transport);
    wake();
  }} else {{
    document.body.appendChild(transport);
  }}
}});

document.getElementById("meta").textContent =
  `${{new Date(M.created * 1000).toLocaleString()}} · ${{M.parts.length}} tracks · ${{fmt(M.duration)}}`;

Promise.all(els.map(e => e.readyState >= 1 ? Promise.resolve()
  : new Promise(r => e.addEventListener("loadedmetadata", r, {{ once: true }})))).then(paint);
paint();
</script>
"#,
        title = html_text(&title),
        manifest = embed_json(m),
        videos = videos,
        audios = audios,
        offsets = format!("[{}]", offsets.join(",")),
        grid = grid,
    )
}

fn html_text(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn html_attr(s: &str) -> String {
    html_text(s).replace('"', "&quot;")
}
