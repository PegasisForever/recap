//! Capture: one monitor per process, audio on its own tracks.
//!
//! The xdg-desktop-portal picker hands back exactly one display per grant, and
//! gpu-screen-recorder asks the portal for `multiple: false` and drops every
//! stream but the last. So recording N monitors means N processes, each with
//! its own saved restore token. That is why `Source` in the config carries a
//! token path: grant once, then record forever without a dialog.
//!
//! Audio is captured separately from video, and the microphone is kept apart
//! from the system mix. The mic is one known person so its transcript needs no
//! diarization at all. The system track may carry several remote participants
//! and still does.

use crate::config::{Config, Source};
use crate::manifest::PartKind;
use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Which PipeWire node to record audio from.
///
/// Everything here is a `node.name`, never an object id. `pw-record --target`
/// takes "a serial or name", and an object id is neither. Passing one does not
/// fail: it matches nothing, so the stream falls back to `auto` and silently
/// records the default device instead of the one that was asked for. Two tracks
/// aimed at different ids then come back byte for byte identical.
#[derive(Debug, Clone)]
pub enum AudioTarget {
    /// Whatever PipeWire currently has set as the default.
    DefaultSink,
    DefaultSource,
    /// An explicit node name, for virtual or non-default devices.
    Named(String),
    /// Do not capture this track.
    None,
}

impl AudioTarget {
    fn resolve(&self) -> Result<Option<String>> {
        let key = match self {
            AudioTarget::None => return Ok(None),
            AudioTarget::Named(name) => return Ok(Some(name.clone())),
            AudioTarget::DefaultSink => "default.audio.sink",
            AudioTarget::DefaultSource => "default.audio.source",
        };
        default_node(key)
    }
}

/// Which encoder gpu-screen-recorder settled on for a monitor.
///
/// It decides this at startup, after the capture resolution is known, and says
/// so only in its log. There is no flag that forces the answer, because a GPU
/// that cannot encode at the capture resolution falls back the same way one
/// with no driver does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Gpu,
    Cpu,
}

impl Encoding {
    pub fn label(&self) -> &'static str {
        match self {
            Encoding::Gpu => "GPU",
            Encoding::Cpu => "CPU",
        }
    }
}

/// Nothing records above this. Monitor content changes slowly, extra frames
/// buy nothing a reader can use, and they cost encode time and upload size.
pub const MAX_FPS: u32 = 30;

/// Milliseconds between starting one monitor and the next.
const SPAWN_GAP_MS: u64 = 600;

pub struct RecordOptions {
    pub outdir: PathBuf,
    /// Clamped to [1, MAX_FPS] before it reaches the encoder.
    pub fps: u32,
    pub mic: AudioTarget,
    pub system: AudioTarget,
    /// gpu-screen-recorder cannot find a render node without /dev/dri/cardX,
    /// which happens on headless and virtualised machines.
    pub allow_cpu_encoding: bool,
}

impl Default for RecordOptions {
    fn default() -> Self {
        Self {
            outdir: PathBuf::from("."),
            fps: 30,
            mic: AudioTarget::DefaultSource,
            system: AudioTarget::DefaultSink,
            allow_cpu_encoding: true,
        }
    }
}

struct Track {
    kind: PartKind,
    label: String,
    path: PathBuf,
    child: Child,
    /// Wall clock at spawn, used to line the tracks up afterwards.
    started_ms: i64,
    /// Set for audio, which lands as WAV and is compressed once the capture
    /// has stopped. `path` is the WAV, this is where the Opus should go.
    compress_to: Option<PathBuf>,
    /// Video only, and None until the recorder says which encoder it took.
    encoding: Arc<Mutex<Option<Encoding>>>,
}

pub struct Recording {
    tracks: Vec<Track>,
    pub started: u64,
}

impl Recording {
    /// One entry per monitor, in the order they were started. None means the
    /// recorder has not reached its first frame yet, which takes a second or
    /// two while the portal session is restored.
    pub fn encodings(&self) -> Vec<Option<Encoding>> {
        self.tracks
            .iter()
            .filter(|t| t.kind == PartKind::Video)
            .map(|t| *t.encoding.lock().unwrap())
            .collect()
    }
}

/// A finished capture, before upload.
pub struct FinishedPart {
    pub kind: PartKind,
    pub label: String,
    pub path: PathBuf,
    pub offset_ms: i64,
    pub bytes: u64,
    pub duration: f64,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Start every granted monitor plus the two audio tracks.
pub fn start(cfg: &Config, opts: &RecordOptions) -> Result<Recording> {
    let sources = &cfg.sources;
    if sources.is_empty() {
        anyhow::bail!("no monitors added yet");
    }
    std::fs::create_dir_all(&opts.outdir)?;
    let started = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut tracks = Vec::new();
    for (i, src) in sources.iter().enumerate() {
        // Each monitor opens its own portal session, and two Start calls landing
        // together makes the compositor drop one of them. The loser produces no
        // file at all. A short gap between spawns is enough to serialise the
        // handshakes, and the delay it adds is recorded in offset_ms anyway.
        if i > 0 {
            std::thread::sleep(std::time::Duration::from_millis(SPAWN_GAP_MS));
        }
        match spawn_monitor(src, i, opts) {
            Ok(t) => tracks.push(t),
            Err(e) => {
                stop_all(&mut tracks);
                return Err(e);
            }
        }
    }

    // Both PipeWire defaults can name the same node, and then these two are not
    // two tracks at all: the same audio gets captured twice, uploaded twice,
    // and half of it is labelled "Microphone" while being nothing of the kind.
    // Record it once and say so, rather than shipping a convincing duplicate.
    let mic_node = opts.mic.resolve()?;
    let system_node = opts.system.resolve()?;
    let mic_node = if mic_node.is_some() && mic_node == system_node {
        eprintln!(
            "recap: the default microphone and the default output are the same \
             PipeWire node ({}), so there is only one thing to record. Keeping \
             it as the system track.",
            system_node.clone().unwrap_or_default()
        );
        None
    } else {
        mic_node
    };

    for (node, kind, label, name) in [
        (mic_node, PartKind::Mic, "Microphone", "mic.opus"),
        (system_node.clone(), PartKind::System, "System audio", "system.opus"),
    ] {
        match spawn_audio(node, kind, label, &opts.outdir.join(name)) {
            Ok(Some(t)) => tracks.push(t),
            // No such device is not fatal. A machine with no microphone still
            // records its screen and its system audio.
            Ok(None) => eprintln!("recap: no {label} device, skipping that track"),
            Err(e) => {
                stop_all(&mut tracks);
                return Err(e);
            }
        }
    }

    if !tracks.iter().any(|t| t.kind == PartKind::Video) {
        stop_all(&mut tracks);
        anyhow::bail!("no monitor track started");
    }
    Ok(Recording { tracks, started })
}

fn spawn_monitor(src: &Source, index: usize, opts: &RecordOptions) -> Result<Track> {
    let path = opts.outdir.join(format!("monitor-{index}.mp4"));
    let token = Config::token_path(&src.id);
    crate::config::ensure_parent(&token)?;

    let mut cmd = Command::new("gpu-screen-recorder");
    cmd.args(["-w", "portal"])
        .args(["-restore-portal-session", "yes"])
        .arg("-portal-session-token-filepath")
        .arg(&token)
        .args(["-f", &opts.fps.clamp(1, MAX_FPS).to_string()])
        .args(["-cursor", "yes"])
        // Spawning is not starting. Restoring the portal session and
        // negotiating the PipeWire stream takes a variable few hundred
        // milliseconds, and two monitors never take the same amount. This
        // makes the recorder stamp the moment its first frame is actually
        // encoded, which is the only honest reference for lining them up.
        .args(["-write-first-frame-ts", "yes"]);
    if opts.allow_cpu_encoding {
        cmd.args(["-fallback-cpu-encoding", "yes"]);
    }
    cmd.arg("-o")
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let started_ms = now_ms();
    let mut child = cmd
        .spawn()
        .context("spawning gpu-screen-recorder, is it installed?")?;
    let encoding = watch_log(&mut child);
    Ok(Track {
        kind: PartKind::Video,
        label: src.label.clone(),
        path,
        child,
        started_ms,
        compress_to: None,
        encoding,
    })
}

/// Read gpu-screen-recorder's log for as long as it runs, and report which
/// encoder it chose.
///
/// The reading is not optional. Its `-v` defaults to on, so it writes a status
/// line to stderr every second, and a pipe holds 64 KB. Left undrained that
/// fills after about 35 minutes, at which point the recorder blocks on the
/// write and capture stops. Nothing reports an error, the file simply stops
/// growing.
fn watch_log(child: &mut Child) -> Arc<Mutex<Option<Encoding>>> {
    let found = Arc::new(Mutex::new(None));
    let Some(err) = child.stderr.take() else {
        return found;
    };
    let out = found.clone();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(err).lines().map_while(Result::ok) {
            if line.contains("cpu encoding instead") {
                *out.lock().unwrap() = Some(Encoding::Cpu);
            } else if line.starts_with("update fps:") {
                // Encoder setup is done by the time frames are counted, so the
                // absence of the fallback warning by now means it got the GPU.
                let mut slot = out.lock().unwrap();
                if slot.is_none() {
                    *slot = Some(Encoding::Gpu);
                }
            }
        }
    });
    found
}

fn spawn_audio(
    node: Option<String>,
    kind: PartKind,
    label: &str,
    path: &Path,
) -> Result<Option<Track>> {
    let Some(node) = node else {
        return Ok(None);
    };
    // pw-record can write to stdout, but what comes out is not a RIFF stream
    // ffmpeg will accept over a pipe. Landing a real WAV and compressing after
    // the fact costs disk for the length of the recording and nothing else.
    let wav = path.with_extension("wav");
    let started_ms = now_ms();
    let child = Command::new("pw-record")
        .arg("--target")
        .arg(&node)
        .arg(&wav)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // Discarded rather than piped. Nothing here parses pw-record's output,
        // and a pipe with no reader is a stall waiting to happen.
        .stderr(Stdio::null())
        .spawn()
        .context("spawning pw-record, is pipewire installed?")?;
    Ok(Some(Track {
        kind,
        label: label.to_string(),
        path: wav,
        child,
        started_ms,
        encoding: Arc::new(Mutex::new(None)),
        compress_to: Some(path.to_path_buf()),
    }))
}

/// SIGINT, because both gpu-screen-recorder and ffmpeg finalise their
/// container on interrupt. SIGKILL leaves an unplayable file.
fn interrupt(child: &Child) {
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
}

fn stop_all(tracks: &mut Vec<Track>) {
    for t in tracks.iter() {
        interrupt(&t.child);
    }
    for t in tracks.iter_mut() {
        let _ = t.child.wait();
    }
    tracks.clear();
}

impl Recording {
    pub fn is_running(&mut self) -> bool {
        self.tracks
            .iter_mut()
            .any(|t| matches!(t.child.try_wait(), Ok(None)))
    }

    /// Interrupt everything, wait for clean container finalisation, then work
    /// out how far each track lags the earliest one.
    /// How many reported stages `stop` will produce, so a caller can number a
    /// continuous progress bar across stopping and uploading together.
    pub fn stop_stages(&self) -> u32 {
        1 + self.tracks.iter().filter(|t| t.compress_to.is_some()).count() as u32
    }

    /// Stages in the whole finish, stopping and uploading together. A bar
    /// numbered against this sweeps once from empty to full instead of
    /// refilling, and never sits at 100% waiting for the next stage to speak.
    pub fn finish_stages(&self) -> u32 {
        let videos = self.tracks.iter().filter(|t| t.kind == PartKind::Video).count() as u32;
        let parts = self.tracks.len() as u32;
        // closing, compress each audio, prepare each video, upload each part,
        // then the index and the player page.
        self.stop_stages() + videos + parts + 2
    }

    /// `first_step` and `total_steps` place these stages inside the whole
    /// finish. Pass 1 and `stop_stages()` if nothing follows.
    pub fn stop(
        mut self,
        first_step: u32,
        total_steps: u32,
        report: crate::progress::Reporter,
    ) -> Result<Vec<FinishedPart>> {
        use crate::progress::{Progress, Stage};

        let mut step = first_step;
        report(Progress::spinner(&Stage::new(
            "Closing the capture files",
            step,
            total_steps,
        )));
        step += 1;
        for t in &self.tracks {
            interrupt(&t.child);
        }
        // Every capture stops on the same signal within microseconds of the
        // others, so this instant is the one thing every track genuinely has in
        // common. Startup, by contrast, varies by seconds between them.
        let stopped_ms = now_ms();
        for t in &mut self.tracks {
            let _ = t.child.wait();
        }
        // Audio landed as WAV so pw-record could finalise it on the signal.
        // An hour of it is about 640 MB per track, so compressing is not
        // instant and has to say how far along it is.
        for t in &mut self.tracks {
            let Some(dest) = t.compress_to.clone() else { continue };
            let stage = Stage::new(format!("Compressing {}", t.label), step, total_steps);
            step += 1;
            match crate::video::compress_audio_reporting(&t.path, &dest, &stage, report) {
                Ok(()) => {
                    let _ = std::fs::remove_file(&t.path);
                    t.path = dest;
                }
                Err(e) => eprintln!("recap: could not compress {}: {e}", t.label),
            }
        }

        // Spawning a capture is not the same as it starting. Restoring a portal
        // session takes seconds, and pw-record spends its own time attaching to
        // a node. Measured on a two-monitor session, the spawn stamps were out
        // by 2.1 s against the audio, which is plainly visible on playback.
        //
        // Two better references, both on CLOCK_REALTIME so they mix:
        //   video: the recorder stamps its own first encoded frame
        //   audio: work backwards from the shared stop instant and the duration
        for t in &mut self.tracks {
            if t.kind == PartKind::Video {
                if let Some(ms) = read_first_frame_ms(&t.path) {
                    t.started_ms = ms;
                }
            } else {
                let secs = crate::video::duration(&t.path).unwrap_or(0.0);
                if secs > 0.0 {
                    t.started_ms = stopped_ms - (secs * 1000.0) as i64;
                }
            }
        }

        let base = self
            .tracks
            .iter()
            .map(|t| t.started_ms)
            .min()
            .ok_or_else(|| anyhow!("no tracks"))?;

        let mut out = Vec::new();
        for t in self.tracks {
            if !t.path.exists() {
                eprintln!("recap: {} produced no file, dropping it", t.label);
                continue;
            }
            let bytes = std::fs::metadata(&t.path).map(|m| m.len()).unwrap_or(0);
            if bytes == 0 {
                eprintln!("recap: {} is empty, dropping it", t.label);
                continue;
            }
            out.push(FinishedPart {
                kind: t.kind,
                label: t.label,
                duration: crate::video::duration(&t.path).unwrap_or(0.0),
                path: t.path,
                offset_ms: t.started_ms - base,
                bytes,
            });
        }
        if out.is_empty() {
            anyhow::bail!("every track failed, nothing to upload");
        }
        Ok(out)
    }
}

/// Ask GNOME what screens exist, so the picker can label things usefully
/// before the portal dialog has ever run.
pub fn list_monitors() -> Vec<String> {
    let out = Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.gnome.Mutter.DisplayConfig",
            "--object-path",
            "/org/gnome/Mutter/DisplayConfig",
            "--method",
            "org.gnome.Mutter.DisplayConfig.GetCurrentState",
        ])
        .output();
    let Ok(out) = out else { return Vec::new() };
    let text = String::from_utf8_lossy(&out.stdout);
    // Connector names look like ('DP-1', 'MetaVendor', 'Virtual remote monitor', ...)
    let mut names = Vec::new();
    for part in text.split("('") {
        if let Some(name) = part.split('\'').next() {
            let ok = name.len() >= 3
                && name.len() < 24
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                && name.chars().any(|c| c.is_ascii_digit());
            if ok && !names.contains(&name.to_string()) {
                names.push(name.to_string());
            }
        }
    }
    names
}

/// Walk the user through granting one monitor, and keep the grant.
///
/// There is no way to ask xdg-desktop-portal for a token without starting a
/// real capture, so this starts one, waits for the token file to land, then
/// stops immediately. The recording it makes is thrown away. Everything after
/// this runs with no dialog at all.
pub fn add_source(id: &str, timeout_secs: u64) -> Result<(u32, u32)> {
    use std::io::Read;

    let token = Config::token_path(id);
    crate::config::ensure_parent(&token)?;
    let _ = std::fs::remove_file(&token);
    let scratch = crate::config::Config::staging_dir().join(format!("grant-{id}.mp4"));

    let mut child = Command::new("gpu-screen-recorder")
        .args(["-w", "portal"])
        .arg("-portal-session-token-filepath")
        .arg(&token)
        .args(["-fallback-cpu-encoding", "yes", "-f", "30"])
        .arg("-o")
        .arg(&scratch)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning gpu-screen-recorder for the grant")?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut granted = false;
    while std::time::Instant::now() < deadline {
        if token.exists() && std::fs::metadata(&token).map(|m| m.len()).unwrap_or(0) > 0 {
            granted = true;
            break;
        }
        if let Ok(Some(_)) = child.try_wait() {
            break; // user pressed Cancel
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    // Let the PipeWire stream finish negotiating so its size reaches stderr.
    if granted {
        std::thread::sleep(std::time::Duration::from_millis(700));
    }
    interrupt(&child);
    let mut log = String::new();
    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_string(&mut log);
    }
    let _ = child.wait();
    let _ = std::fs::remove_file(&scratch);

    if !granted {
        anyhow::bail!("no monitor was granted");
    }
    Ok(parse_size(&log).unwrap_or((0, 0)))
}

/// gpu-screen-recorder prints the negotiated stream geometry as
/// `gsr info: pipewire:    Size: 2560x1440`. That is the screen's real
/// resolution, and reading it costs nothing extra.
fn parse_size(log: &str) -> Option<(u32, u32)> {
    for line in log.lines() {
        let Some(rest) = line.split("Size:").nth(1) else { continue };
        let token = rest.trim().split_whitespace().next()?;
        let (w, h) = token.split_once('x')?;
        if let (Ok(w), Ok(h)) = (w.trim().parse(), h.trim().parse()) {
            return Some((w, h));
        }
    }
    None
}

/// The `node.name` PipeWire currently has as `default.audio.sink` or
/// `default.audio.source`.
///
/// This is what `wpctl inspect @DEFAULT_AUDIO_SINK@` resolves, done against
/// `pw-dump` so wireplumber is not a dependency. The defaults live in a
/// metadata object named `default`, and already hold a name rather than an id:
///
/// ```text
/// default.audio.sink = {"name": "alsa_output.pci-0000_00_1f.3.analog-stereo"}
/// ```
///
/// so the name is taken straight from there and handed to `pw-record --target`.
/// Returns None when no default is set.
fn default_node(key: &str) -> Result<Option<String>> {
    let out = Command::new("pw-dump")
        .output()
        .context("running pw-dump, is pipewire installed?")?;
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).context("parsing pw-dump output")?;

    for o in v.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        if o["type"].as_str() != Some("PipeWire:Interface:Metadata")
            || o["props"]["metadata.name"].as_str() != Some("default")
        {
            continue;
        }
        for entry in o["metadata"].as_array().into_iter().flatten() {
            if entry["key"].as_str() == Some(key) {
                return Ok(entry["value"]["name"].as_str().map(str::to_owned));
            }
        }
    }
    Ok(None)
}

/// The node both the microphone and the system track would land on, if they
/// collide. None when they are distinct, which is the normal case.
///
/// Reads the same defaults `start` will, so what the startup check reports and
/// what recording actually does cannot drift apart.
pub fn colliding_audio_node(cfg: &Config) -> Option<String> {
    let mic = match &cfg.mic_name {
        None => AudioTarget::DefaultSource,
        Some(n) if n.is_empty() => return None, // microphone off, nothing to collide
        Some(n) => AudioTarget::Named(n.clone()),
    };
    let mic = mic.resolve().ok().flatten()?;
    let system = AudioTarget::DefaultSink.resolve().ok().flatten()?;
    (mic == system).then_some(mic)
}

/// Microphones and other capture devices PipeWire currently knows about.
///
/// Monitor sources are deliberately excluded. Those are loopbacks of an output
/// and belong to the system track, not the microphone one.
pub fn list_audio_sources() -> Vec<(String, String)> {
    let Ok(out) = Command::new("pw-dump").output() else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for node in v.as_array().into_iter().flatten() {
        if node["type"].as_str() != Some("PipeWire:Interface:Node") {
            continue;
        }
        let props = &node["info"]["props"];
        let class = props["media.class"].as_str().unwrap_or("");
        if !class.starts_with("Audio/Source") {
            continue;
        }
        let Some(node_name) = props["node.name"].as_str() else {
            continue;
        };
        let shown = props["node.description"]
            .as_str()
            .or_else(|| props["node.nick"].as_str())
            .unwrap_or(node_name);
        found.push((node_name.to_string(), shown.to_string()));
    }
    found.sort_by(|a, b| a.1.cmp(&b.1));
    found
}

/// Read the sidecar gpu-screen-recorder writes next to its output when
/// `-write-first-frame-ts yes` is set. Format:
///
/// ```text
/// monotonic_microsec	realtime_microsec
/// 12345678	1785499087746378
/// ```
///
/// The second column is CLOCK_REALTIME, the same clock `now_ms` samples, so
/// the two can be compared directly. Returns None when the file is missing,
/// which happens if the recorder never produced a frame.
fn read_first_frame_ms(video: &Path) -> Option<i64> {
    let sidecar = PathBuf::from(format!("{}.ts", video.display()));
    let text = std::fs::read_to_string(&sidecar).ok()?;
    let realtime_us: u64 = text.lines().nth(1)?.split('\t').nth(1)?.trim().parse().ok()?;
    let _ = std::fs::remove_file(&sidecar);
    Some((realtime_us / 1000) as i64)
}

#[cfg(test)]
mod tests {
    use super::AudioTarget;

    /// `default_node` replaced a `wpctl inspect` call. Where wireplumber is
    /// still installed the two must name the same node, so this maps the name
    /// back to an id through pw-dump and compares against wpctl's answer.
    #[test]
    fn default_node_agrees_with_wpctl() {
        if !crate::video::have("wpctl") || !crate::video::have("pw-dump") {
            eprintln!("skipping: wpctl or pw-dump missing");
            return;
        }
        for (target, alias) in [
            (AudioTarget::DefaultSink, "@DEFAULT_AUDIO_SINK@"),
            (AudioTarget::DefaultSource, "@DEFAULT_AUDIO_SOURCE@"),
        ] {
            let name = target.resolve().expect("pw-dump lookup should not error");
            let mine = name.as_deref().and_then(node_id_of);

            let out = std::process::Command::new("wpctl")
                .args(["inspect", alias])
                .output()
                .expect("wpctl should run");
            let theirs: Option<u32> = String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .and_then(|l| l.strip_prefix("id "))
                .and_then(|l| l.split(',').next())
                .and_then(|l| l.trim().parse().ok());

            assert_eq!(mine, theirs, "disagreement on {alias} (name {name:?})");
            eprintln!("{alias}: {name:?} -> id {mine:?}, wpctl says {theirs:?}");
        }
    }

    fn node_id_of(name: &str) -> Option<u32> {
        let out = std::process::Command::new("pw-dump").output().ok()?;
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
        for o in v.as_array()?.iter() {
            if o["type"].as_str() == Some("PipeWire:Interface:Node")
                && o["info"]["props"]["node.name"].as_str() == Some(name)
            {
                return o["id"].as_u64().map(|i| i as u32);
            }
        }
        None
    }

    /// The bug this guards against: `pw-record --target` takes a serial or a
    /// name, so an object id matches nothing and the stream silently falls back
    /// to the default device. Recording two different targets then produced two
    /// identical tracks. Every target recap builds must be a real `node.name`.
    #[test]
    fn resolved_targets_are_node_names_not_ids() {
        if !crate::video::have("pw-dump") {
            eprintln!("skipping: pw-dump missing");
            return;
        }
        for target in [AudioTarget::DefaultSink, AudioTarget::DefaultSource] {
            let Some(name) = target.resolve().expect("resolve should not error") else {
                continue;
            };
            assert!(
                name.parse::<u32>().is_err(),
                "target {name:?} is a bare number, which --target reads as a serial"
            );
            assert!(
                node_id_of(&name).is_some(),
                "target {name:?} does not match any node.name in pw-dump"
            );
            eprintln!("{target:?} -> {name:?} (a real node.name)");
        }
    }

    /// End to end through the real spawn path: recap must capture from the node
    /// it names, not from whatever the default happens to be.
    ///
    /// Runs against a source whose object id and object.serial disagree, which
    /// is the exact shape that hid the bug. Passing the id there silently
    /// captured the default instead, so two tracks aimed at different devices
    /// came back byte for byte identical.
    #[test]
    fn spawn_audio_captures_the_named_node() {
        if !crate::video::have("pw-record") || !crate::video::have("pw-dump") {
            eprintln!("skipping: pipewire tools missing");
            return;
        }
        let Some((name, id, serial)) = source_with_mismatched_ids() else {
            eprintln!("skipping: no source where id and serial differ");
            return;
        };
        eprintln!("targeting {name:?} (id {id}, serial {serial})");

        let dir = std::env::temp_dir().join("recap-target-test");
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("probe.opus");
        let mut track = super::spawn_audio(Some(name.clone()), crate::manifest::PartKind::Mic, "probe", &out)
            .expect("spawn should succeed")
            .expect("a named target is never None");

        std::thread::sleep(std::time::Duration::from_millis(2500));
        let captured = capture_peers();
        let _ = track.child.kill();
        let _ = track.child.wait();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            captured.contains(&name),
            "recap captured from {captured:?}, not from the requested {name:?}"
        );
    }

    /// A source node whose id differs from its object.serial, plus both numbers.
    fn source_with_mismatched_ids() -> Option<(String, u64, u64)> {
        let out = std::process::Command::new("pw-dump").output().ok()?;
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
        for o in v.as_array()?.iter() {
            if o["type"].as_str() != Some("PipeWire:Interface:Node") {
                continue;
            }
            let p = &o["info"]["props"];
            if !p["media.class"].as_str().unwrap_or("").starts_with("Audio/Source") {
                continue;
            }
            let (Some(id), Some(serial), Some(name)) = (
                o["id"].as_u64(),
                p["object.serial"].as_u64(),
                p["node.name"].as_str(),
            ) else {
                continue;
            };
            if id != serial {
                return Some((name.to_string(), id, serial));
            }
        }
        None
    }

    /// Which nodes the running pw-record capture stream is linked to.
    fn capture_peers() -> Vec<String> {
        let Ok(out) = std::process::Command::new("pw-dump").output() else {
            return Vec::new();
        };
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
            return Vec::new();
        };
        let objs = v.as_array().map(|a| a.as_slice()).unwrap_or(&[]);
        let name_of = |id: u64| -> Option<String> {
            objs.iter()
                .find(|o| o["id"].as_u64() == Some(id))
                .and_then(|o| o["info"]["props"]["node.name"].as_str())
                .map(str::to_owned)
        };
        let mine: Vec<u64> = objs
            .iter()
            .filter(|o| {
                o["type"].as_str() == Some("PipeWire:Interface:Node")
                    && o["info"]["props"]["media.class"].as_str() == Some("Stream/Input/Audio")
            })
            .filter_map(|o| o["id"].as_u64())
            .collect();
        let mut peers = Vec::new();
        for o in objs {
            if o["type"].as_str() != Some("PipeWire:Interface:Link") {
                continue;
            }
            let p = &o["info"]["props"];
            if let (Some(inn), Some(outn)) =
                (p["link.input.node"].as_u64(), p["link.output.node"].as_u64())
            {
                if mine.contains(&inn) {
                    if let Some(n) = name_of(outn) {
                        if !peers.contains(&n) {
                            peers.push(n);
                        }
                    }
                }
            }
        }
        peers
    }

    /// The startup check and the recording path must agree about whether the
    /// two audio defaults collide, or the window says one thing and the files
    /// say another.
    #[test]
    fn collision_check_matches_what_recording_would_do() {
        if !crate::video::have("pw-dump") {
            eprintln!("skipping: pw-dump missing");
            return;
        }
        let cfg = crate::config::Config::default();
        let collision = super::colliding_audio_node(&cfg);
        let mic = AudioTarget::DefaultSource.resolve().unwrap();
        let sink = AudioTarget::DefaultSink.resolve().unwrap();
        eprintln!("source -> {mic:?}, sink -> {sink:?}, collision -> {collision:?}");
        match collision {
            Some(n) => {
                assert_eq!(mic.as_deref(), Some(n.as_str()));
                assert_eq!(sink.as_deref(), Some(n.as_str()));
            }
            None => assert!(
                mic.is_none() || sink.is_none() || mic != sink,
                "check says no collision but both resolved to {mic:?}"
            ),
        }
    }
}
