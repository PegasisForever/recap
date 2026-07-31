//! On-disk settings, written entirely by the Settings window.
//!
//! Lives at `~/.config/recap/config.toml`. Nothing here needs hand-editing.
//! Storage fields also accept an environment override, for scripts and CI.
//!
//! Nothing about transcription lives here. Recordings are usually read back on
//! a different machine from the one that made them, so `watchvid.py` takes its
//! Gemini key from `GEMINI_API_KEY` and nowhere else.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub s3: S3Config,
    /// Monitors the user has already granted through the portal. Every one of
    /// them is recorded. Removing a monitor is how you exclude it.
    #[serde(default)]
    pub sources: Vec<Source>,
    /// PipeWire `node.name` of the chosen microphone. None means the system
    /// default, and an empty string means record no microphone at all.
    ///
    /// A name rather than a node id, because ids are assigned per session and
    /// a saved id points at a different device, or nothing, after a reboot.
    #[serde(default)]
    pub mic_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct S3Config {
    pub bucket: String,
    /// Set for MinIO or any other S3-compatible endpoint. Empty means real AWS.
    #[serde(default)]
    pub endpoint: String,
    #[serde(default = "default_region")]
    pub region: String,
    #[serde(default)]
    pub access_key: String,
    #[serde(default)]
    pub secret_key: String,
    /// MinIO needs path-style addressing unless you have wildcard DNS.
    #[serde(default = "default_true")]
    pub path_style: bool,
    /// Presigned URL lifetime. SigV4 caps this at 7 days.
    #[serde(default = "default_expiry")]
    pub link_expiry_secs: u64,
}

/// One monitor the user has granted us, remembered between runs.
///
/// The portal picker only ever returns a single display, so recording several
/// monitors means holding several independent grants. Each one keeps its own
/// restore token file, which is what makes "pick once, record forever" work.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    /// Stable id, also the token filename.
    pub id: String,
    /// What the user sees in the monitor list.
    pub label: String,
    /// Read off the PipeWire stream when the grant was made. Zero when the
    /// grant predates this field or the negotiation line could not be parsed.
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
}

impl Source {
    pub fn resolution(&self) -> String {
        if self.width == 0 || self.height == 0 {
            "resolution unknown".into()
        } else {
            format!("{}x{}", self.width, self.height)
        }
    }
}

fn default_true() -> bool {
    true
}
fn default_region() -> String {
    "us-east-1".into()
}
fn default_expiry() -> u64 {
    7 * 24 * 3600
}

impl Default for Config {
    fn default() -> Self {
        Self {
            s3: S3Config {
                region: default_region(),
                path_style: true,
                link_expiry_secs: default_expiry(),
                ..Default::default()
            },
            sources: Vec::new(),
            mic_name: None,
        }
    }
}

impl Config {
    pub fn dir() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("recap")
    }

    pub fn path() -> PathBuf {
        Self::dir().join("config.toml")
    }

    /// Where a source's portal restore token lives.
    pub fn token_path(id: &str) -> PathBuf {
        Self::dir().join("tokens").join(id)
    }

    /// Where capture lands before it is uploaded.
    ///
    /// Deliberately not the temp directory. Inside a Flatpak sandbox `/tmp` is
    /// a tmpfs sized at a fraction of RAM (1.6 GB on a 16 GB machine), and
    /// Fedora and Arch mount the real `/tmp` as tmpfs too. An hour of two
    /// monitors plus two uncompressed audio tracks is well over that, so
    /// staging there turns a long recording into an out-of-memory failure.
    /// The cache directory is on real disk everywhere.
    /// Created on demand, because the free-space check measures this path and
    /// `df` on a directory that does not exist yet reports nothing at all.
    pub fn staging_dir() -> PathBuf {
        let dir = dirs::cache_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("recap");
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    pub fn load() -> Result<Self> {
        let path = Self::path();
        let mut cfg = if path.exists() {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?
        } else {
            Self::default()
        };
        cfg.apply_env();
        Ok(cfg)
    }

    /// Environment wins over the file, so scripts can point at a different
    /// bucket without editing anything.
    fn apply_env(&mut self) {
        let set = |slot: &mut String, key: &str| {
            if let Ok(v) = std::env::var(key) {
                if !v.is_empty() {
                    *slot = v;
                }
            }
        };
        set(&mut self.s3.bucket, "RECAP_S3_BUCKET");
        set(&mut self.s3.endpoint, "RECAP_S3_ENDPOINT");
        set(&mut self.s3.region, "RECAP_S3_REGION");
        set(&mut self.s3.access_key, "AWS_ACCESS_KEY_ID");
        set(&mut self.s3.secret_key, "AWS_SECRET_ACCESS_KEY");
        if let Ok(v) = std::env::var("RECAP_LINK_EXPIRY_SECS") {
            if let Ok(n) = v.parse() {
                self.s3.link_expiry_secs = n;
            }
        }
    }

    pub fn save(&self) -> Result<()> {
        let dir = Self::dir();
        std::fs::create_dir_all(dir.join("tokens"))?;
        let text = toml::to_string_pretty(self)?;
        std::fs::write(Self::path(), text)
            .with_context(|| format!("writing {}", Self::path().display()))?;
        Ok(())
    }

    /// True once a recording could actually be uploaded somewhere.
    pub fn storage_ready(&self) -> bool {
        !self.s3.bucket.is_empty() && !self.s3.access_key.is_empty()
    }
}

/// SigV4 refuses to sign anything longer than a week.
pub const MAX_PRESIGN_SECS: u64 = 7 * 24 * 3600;

pub fn clamp_expiry(secs: u64) -> u64 {
    secs.min(MAX_PRESIGN_SECS).max(60)
}

pub fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    Ok(())
}
