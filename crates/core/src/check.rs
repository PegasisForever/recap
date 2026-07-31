//! What has to be true before Record can work.
//!
//! Every one of these fails at a different time otherwise: a missing
//! `pw-record` only shows up as a silent audio track, an unset bucket only
//! shows up after a recording has already been made. Checking at startup puts
//! all of it in front of the user while it is still cheap to fix.

use crate::config::Config;

pub struct Issue {
    pub title: String,
    pub detail: String,
}

/// A program that must be on PATH, and what stops working without it.
const NEEDED: &[(&str, &str)] = &[
    ("gpu-screen-recorder", "Records the monitors. Build it from git.dec05eba.com or install the Flatpak."),
    ("pw-record", "Captures the microphone and system audio. Part of pipewire."),
    ("pw-dump", "Lists which microphones exist. Part of pipewire."),
    ("ffmpeg", "Compresses the audio tracks after capture."),
    ("ffprobe", "Reads how long each track turned out."),
    ("gdbus", "Asks the desktop what monitors exist. Part of glib."),
];

use crate::video::have as on_path;

/// Everything wrong right now, worst first. Empty means ready to record.
pub fn run(cfg: &Config) -> Vec<Issue> {
    let mut out = Vec::new();

    for (bin, why) in NEEDED {
        if !on_path(bin) {
            out.push(Issue {
                title: format!("{bin} is not installed"),
                detail: why.to_string(),
            });
        }
    }

    // Screen capture goes through xdg-desktop-portal, which needs a Wayland or
    // X11 session with a portal backend. A bare TTY or an SSH shell has neither.
    if std::env::var("WAYLAND_DISPLAY").is_err() && std::env::var("DISPLAY").is_err() {
        out.push(Issue {
            title: "No desktop session".into(),
            detail: "Neither WAYLAND_DISPLAY nor DISPLAY is set, so no screen can be captured."
                .into(),
        });
    }

    if cfg.s3.bucket.is_empty() {
        out.push(Issue {
            title: "No bucket set".into(),
            detail: "Recordings have nowhere to go. Set one in Settings.".into(),
        });
    } else if cfg.s3.access_key.is_empty() || cfg.s3.secret_key.is_empty() {
        out.push(Issue {
            title: "Storage keys are missing".into(),
            detail: "Uploads will be rejected. Add the access and secret key in Settings.".into(),
        });
    }

    if cfg.sources.is_empty() {
        out.push(Issue {
            title: "No monitors added".into(),
            detail: "Press Add monitor and pick a display.".into(),
        });
    }

    // When both PipeWire defaults name one node there is only one thing to
    // record, and the recording ends up with two identical tracks unless this
    // is caught. Silent to discover afterwards, obvious to fix beforehand.
    if let Some(node) = crate::record::colliding_audio_node(cfg) {
        out.push(Issue {
            title: "Microphone and system audio are the same device".into(),
            detail: format!(
                "Both default to PipeWire node {node}, so only one track can be \
                 recorded. Pick a real microphone above, or set a different \
                 default output in your sound settings."
            ),
        });
    }

    // Capture lands in the temp directory before it is uploaded, and a long
    // session eats a lot of it: roughly 220 MB per monitor per hour of mostly
    // static screen, plus 640 MB per hour for each audio track, which is
    // written uncompressed and only shrunk once recording stops.
    let tmp = Config::staging_dir();
    if let Some(free) = crate::video::free_space(&tmp) {
        let per_hour = cfg.sources.len() as u64 * 250 * 1024 * 1024 + 2 * 640 * 1024 * 1024;
        let hours = free / per_hour.max(1);
        if hours < 1 {
            out.push(Issue {
                title: "Almost no disk space left".into(),
                detail: format!(
                    "{} free in {}. That is under an hour of recording.",
                    crate::progress::human_bytes(free),
                    tmp.display()
                ),
            });
        }
    }

    out
}
