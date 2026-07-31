//! ffmpeg and ffprobe wrappers.
//!
//! Frame extraction is targeted retrieval, never a blind sweep. A 50 minute
//! screen recording holds around 183,000 frames, so the transcript decides
//! which handful are worth spending tokens on.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Code identifiers and UI labels stay legible at this width. 1024 does not.
pub const FRAME_WIDTH: u32 = 1536;

pub fn have(bin: &str) -> bool {
    Command::new("which")
        .arg(bin)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn require(bins: &[&str]) -> Result<()> {
    for b in bins {
        if !have(b) {
            bail!("{b} not found on PATH");
        }
    }
    Ok(())
}

fn run(cmd: &mut Command) -> Result<String> {
    let out = cmd.output().context("running ffmpeg/ffprobe")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("{:?} failed: {}", cmd.get_program(), err.lines().last().unwrap_or("").trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn duration(path: &Path) -> Result<f64> {
    let out = run(Command::new("ffprobe").args([
        "-v", "error", "-show_entries", "format=duration",
        "-of", "default=noprint_wrappers=1:nokey=1",
    ]).arg(path))?;
    Ok(out.trim().parse().unwrap_or(0.0))
}

#[derive(Debug, Clone)]
pub struct Info {
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub video_codec: String,
    pub audio_codec: String,
}

pub fn probe(path: &Path) -> Result<Info> {
    let out = run(Command::new("ffprobe").args([
        "-v", "error",
        "-show_entries", "format=duration",
        "-show_entries", "stream=codec_type,codec_name,width,height",
        "-of", "default=noprint_wrappers=1",
    ]).arg(path))?;
    let mut info = Info {
        duration: 0.0, width: 0, height: 0,
        video_codec: String::new(), audio_codec: String::new(),
    };
    let mut this_codec = String::new();
    for line in out.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        match k {
            "duration" => info.duration = v.parse().unwrap_or(0.0),
            "codec_name" => this_codec = v.to_string(),
            "width" => info.width = v.parse().unwrap_or(0),
            "height" => info.height = v.parse().unwrap_or(0),
            "codec_type" if v == "video" => info.video_codec = std::mem::take(&mut this_codec),
            "codec_type" if v == "audio" && info.audio_codec.is_empty() => {
                info.audio_codec = std::mem::take(&mut this_codec)
            }
            _ => {}
        }
    }
    Ok(info)
}

/// Accepts `90`, `1:30` or `1:02:03`.
pub fn to_secs(v: &str) -> Result<f64> {
    let v = v.trim();
    if !v.contains(':') {
        return v.parse().context("bad timestamp");
    }
    let mut total = 0.0;
    for part in v.split(':') {
        total = total * 60.0 + part.parse::<f64>().context("bad timestamp")?;
    }
    Ok(total)
}

pub fn hms(t: f64) -> String {
    let t = t.max(0.0) as u64;
    format!("{:02}:{:02}:{:02}", t / 3600, (t % 3600) / 60, t % 60)
}

pub fn ms(t: f64) -> String {
    let t = t.max(0.0) as u64;
    format!("{:02}:{:02}", t / 60, t % 60)
}

/// Burn the true wall-clock time into the pixels.
///
/// Seeking before `-i` resets the presentation timestamp to zero, so ffmpeg's
/// own `%{pts}` would print 00:00:00 on every seeked frame. Passing the known
/// time as literal text is the only way to get a citable stamp.
fn stamp(t: f64, size: u32) -> String {
    let label = hms(t).replace(':', r"\:");
    format!(
        "drawtext=text='{label}':x=8:y=8:fontsize={size}:fontcolor=yellow\
         :box=1:boxcolor=black@0.75:boxborderw=6"
    )
}

/// One frame. `crop` is `w:h:x:y` in source pixels.
pub fn grab(video: &Path, t: f64, out: &Path, width: u32, crop: Option<&str>) -> Result<()> {
    let mut vf = String::new();
    if let Some(c) = crop {
        vf.push_str(&format!("crop={c},"));
    }
    vf.push_str(&format!("scale={width}:-2,{}", stamp(t, 30)));
    crate::config::ensure_parent(out)?;
    run(Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-ss", &format!("{t}")])
        .arg("-i").arg(video)
        .args(["-frames:v", "1", "-vf", &vf, "-y"])
        .arg(out))?;
    Ok(())
}

/// Tile the whole recording into a few grids so the reader can build a map of
/// which application was on screen when. A cell costs roughly 168 visual
/// tokens against 777 for a full frame.
pub fn sheets(video: &Path, outdir: &Path, count: u32, grid: &str) -> Result<Vec<PathBuf>> {
    let (cols, rows) = grid
        .split_once('x')
        .and_then(|(a, b)| Some((a.parse::<u32>().ok()?, b.parse::<u32>().ok()?)))
        .context("grid must look like 3x3")?;
    let per = cols * rows;
    let dur = duration(video)?;
    if dur <= 0.0 {
        bail!("could not read duration of {}", video.display());
    }
    std::fs::create_dir_all(outdir)?;
    let tmp = outdir.join(".cells");
    std::fs::create_dir_all(&tmp)?;

    // Seeking per cell beats `fps=1/N`, which decodes the entire file.
    let step = dur / count as f64;
    let cells: Vec<(usize, f64)> = (0..count)
        .map(|i| (i as usize, (i as f64 + 0.5) * step))
        .collect();
    let handles: Vec<_> = cells
        .chunks(8)
        .map(|chunk| {
            let chunk: Vec<_> = chunk.to_vec();
            let video = video.to_path_buf();
            let tmp = tmp.clone();
            std::thread::spawn(move || {
                for (i, t) in chunk {
                    let out = tmp.join(format!("c{i:04}.jpg"));
                    let _ = grab(&video, t, &out, 640, None);
                }
            })
        })
        .collect();
    for h in handles {
        let _ = h.join();
    }

    let mut written = Vec::new();
    let mut sheet = 1;
    let mut idx = 0;
    while idx < count as usize {
        let group: Vec<PathBuf> = (idx..(idx + per as usize).min(count as usize))
            .map(|i| tmp.join(format!("c{i:04}.jpg")))
            .filter(|p| p.exists())
            .collect();
        if group.is_empty() {
            break;
        }
        let out = outdir.join(format!("sheet_{sheet:02}.jpg"));
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-nostdin", "-v", "error"]);
        for g in &group {
            cmd.arg("-i").arg(g);
        }
        let filter = format!(
            "{}{}tile={cols}x{rows}:padding=4:color=black",
            (0..group.len()).map(|i| format!("[{i}:v]")).collect::<String>(),
            if group.len() > 1 { format!("concat=n={}:v=1:a=0,", group.len()) } else { String::new() }
        );
        cmd.args(["-filter_complex", &filter, "-frames:v", "1", "-y"]).arg(&out);
        run(&mut cmd)?;
        written.push(out);
        idx += per as usize;
        sheet += 1;
    }
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(written)
}

/// Several frames a couple of seconds apart, for transitions and scrolls where
/// a single frame lands mid-motion.
pub fn burst(video: &Path, t: f64, outdir: &Path, n: u32, gap: f64) -> Result<Vec<PathBuf>> {
    std::fs::create_dir_all(outdir)?;
    let mut out = Vec::new();
    for i in 0..n {
        let at = t + i as f64 * gap;
        let p = outdir.join(format!("b_{:02}_{}.jpg", i + 1, hms(at).replace(':', "")));
        grab(video, at, &p, FRAME_WIDTH, None)?;
        out.push(p);
    }
    Ok(out)
}

pub fn extract_audio(media: &Path, out: &Path) -> Result<()> {
    crate::config::ensure_parent(out)?;
    run(Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error"])
        .arg("-i").arg(media)
        .args(["-vn", "-ac", "1", "-ar", "16000", "-c:a", "libopus", "-b:a", "32k", "-y"])
        .arg(out))?;
    Ok(())
}

/// Cut `[start, start+len)` out of an audio file, re-encoding so the cut lands
/// exactly where asked rather than on the nearest keyframe.
pub fn slice_audio(src: &Path, start: f64, len: f64, out: &Path) -> Result<()> {
    crate::config::ensure_parent(out)?;
    run(Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-ss", &format!("{start}"), "-t", &format!("{len}")])
        .arg("-i").arg(src)
        .args(["-vn", "-ac", "1", "-ar", "16000", "-c:a", "libopus", "-b:a", "32k", "-y"])
        .arg(out))?;
    Ok(())
}

/// Shift a track in time so every part of a bundle shares one clock.
pub fn apply_offset(src: &Path, offset_ms: i64, out: &Path) -> Result<()> {
    crate::config::ensure_parent(out)?;
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-nostdin", "-v", "error"]);
    if offset_ms > 0 {
        // Track starts late, so pad the front with silence.
        cmd.arg("-i").arg(src).args([
            "-af",
            &format!("adelay={offset_ms}:all=1"),
            "-c:a", "libopus", "-y",
        ]);
    } else if offset_ms < 0 {
        cmd.args(["-ss", &format!("{}", -offset_ms as f64 / 1000.0)])
            .arg("-i").arg(src)
            .args(["-c:a", "libopus", "-y"]);
    } else {
        cmd.arg("-i").arg(src).args(["-c", "copy", "-y"]);
    }
    run(cmd.arg(out))?;
    Ok(())
}

/// Turn a captured WAV into Opus once nothing is writing to it.
pub fn compress_audio(src: &Path, out: &Path) -> Result<()> {
    crate::config::ensure_parent(out)?;
    run(Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error"])
        .arg("-i").arg(src)
        .args(["-c:a", "libopus", "-b:a", "96k", "-ac", "2", "-y"])
        .arg(out))?;
    Ok(())
}

/// Move the MP4 index to the front of the file.
///
/// gpu-screen-recorder writes `moov` after `mdat`, which is fine for a local
/// player and useless over HTTP: the browser has to fetch the entire file
/// before it can show frame one. Relocating the atom is a remux, so it costs
/// no quality and only as long as a copy takes.
pub fn faststart(src: &Path, out: &Path) -> Result<()> {
    crate::config::ensure_parent(out)?;
    run(Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error"])
        .arg("-i").arg(src)
        .args(["-c", "copy", "-movflags", "+faststart", "-y"])
        .arg(out))?;
    Ok(())
}

/// True when a browser can decode this without transcoding.
///
/// Every current browser handles H.264 in 4:2:0. Anything else, including the
/// 4:4:4 a GPU encoder may produce, has to be re-encoded first.
pub fn browser_ready(path: &Path) -> bool {
    let Ok(out) = run(Command::new("ffprobe").args([
        "-v", "error", "-select_streams", "v:0",
        "-show_entries", "stream=codec_name,pix_fmt",
        "-of", "default=noprint_wrappers=1:nokey=1",
    ]).arg(path)) else {
        return false;
    };
    let mut lines = out.lines();
    matches!(lines.next(), Some("h264"))
        && matches!(lines.next(), Some("yuv420p") | Some("yuvj420p"))
}

/// Re-encode into something a browser will play. Only called when
/// `browser_ready` says no, because it is far slower than a remux.
pub fn to_browser_h264(src: &Path, out: &Path) -> Result<()> {
    crate::config::ensure_parent(out)?;
    run(Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error"])
        .arg("-i").arg(src)
        .args([
            "-c:v", "libx264", "-preset", "veryfast", "-crf", "23",
            "-pix_fmt", "yuv420p", "-profile:v", "high",
            "-c:a", "copy", "-movflags", "+faststart", "-y",
        ])
        .arg(out))?;
    Ok(())
}

/// Run an ffmpeg job and report how far through the media it has got.
///
/// ffmpeg only reveals its position if asked: `-progress pipe:1` makes it emit
/// `out_time_us=` lines as it goes. Without this a two-hour transcode is a
/// frozen window with no way to tell work from a hang.
fn run_with_progress(
    cmd: &mut Command,
    stage: &crate::progress::Stage,
    total_secs: f64,
    report: crate::progress::Reporter,
) -> Result<()> {
    use crate::progress::Progress;
    use std::io::{BufRead, BufReader};

    let mut child = cmd
        .args(["-progress", "pipe:1", "-nostats"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("spawning ffmpeg")?;

    let total_us = (total_secs * 1_000_000.0) as u64;
    if let Some(out) = child.stdout.take() {
        for line in BufReader::new(out).lines().map_while(std::result::Result::ok) {
            let Some((k, v)) = line.split_once('=') else { continue };
            // Older builds report out_time_ms, which is really microseconds.
            let us = match k {
                "out_time_us" | "out_time_ms" => v.trim().parse::<u64>().ok(),
                _ => None,
            };
            if let Some(us) = us {
                report(Progress::new(stage, us.min(total_us), total_us));
            }
        }
    }
    let status = child.wait()?;
    if !status.success() {
        let mut err = String::new();
        if let Some(mut e) = child.stderr.take() {
            use std::io::Read;
            let _ = e.read_to_string(&mut err);
        }
        bail!("ffmpeg failed: {}", err.lines().last().unwrap_or("").trim());
    }
    report(Progress::new(stage, total_us, total_us));
    Ok(())
}

/// Turn a captured WAV into Opus, reporting progress.
pub fn compress_audio_reporting(
    src: &Path,
    out: &Path,
    stage: &crate::progress::Stage,
    report: crate::progress::Reporter,
) -> Result<()> {
    crate::config::ensure_parent(out)?;
    let secs = duration(src).unwrap_or(0.0);
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-nostdin", "-v", "error"])
        .arg("-i").arg(src)
        .args(["-c:a", "libopus", "-b:a", "96k", "-ac", "2", "-y"])
        .arg(out);
    run_with_progress(&mut cmd, stage, secs, report)
}

/// Relocate the MP4 index to the front, reporting progress.
pub fn faststart_reporting(
    src: &Path,
    out: &Path,
    stage: &crate::progress::Stage,
    report: crate::progress::Reporter,
) -> Result<()> {
    crate::config::ensure_parent(out)?;
    let secs = duration(src).unwrap_or(0.0);
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-nostdin", "-v", "error"])
        .arg("-i").arg(src)
        .args(["-c", "copy", "-movflags", "+faststart", "-y"])
        .arg(out);
    run_with_progress(&mut cmd, stage, secs, report)
}

/// Re-encode for the browser, reporting progress. Far slower than a remux, so
/// this is the one stage where a fraction really matters.
pub fn to_browser_h264_reporting(
    src: &Path,
    out: &Path,
    stage: &crate::progress::Stage,
    report: crate::progress::Reporter,
) -> Result<()> {
    crate::config::ensure_parent(out)?;
    let secs = duration(src).unwrap_or(0.0);
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-nostdin", "-v", "error"])
        .arg("-i").arg(src)
        .args([
            "-c:v", "libx264", "-preset", "veryfast", "-crf", "23",
            "-pix_fmt", "yuv420p", "-profile:v", "high",
            "-c:a", "copy", "-movflags", "+faststart", "-y",
        ])
        .arg(out);
    run_with_progress(&mut cmd, stage, secs, report)
}

/// Bytes free on the filesystem holding `path`.
pub fn free_space(path: &Path) -> Option<u64> {
    let out = Command::new("df").args(["-B1", "--output=avail"]).arg(path).output().ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .nth(1)?
        .trim()
        .parse()
        .ok()
}
