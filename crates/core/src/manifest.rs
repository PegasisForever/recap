//! The bundle format that one shareable link resolves to.
//!
//! A presigned URL signs exactly one object, so a recording made of several
//! files needs an index. That index is this struct, uploaded as
//! `manifest.json`, and its presigned URL is the link the user copies.
//! Every other part is presigned inside it, so whoever holds the link can
//! reach the whole recording and nothing else in the bucket.

use serde::{Deserialize, Serialize};

pub const MANIFEST_VERSION: u32 = 1;
pub const MANIFEST_NAME: &str = "manifest.json";
/// The page the shared link opens. Holds the player and the manifest.
pub const PLAYER_NAME: &str = "index.html";

/// What the microphone track is attributed to in a transcript. The person who
/// pressed Record is reading their own recording back, so a name adds nothing
/// a pronoun does not already say.
pub const LOCAL_SPEAKER: &str = "Me";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub id: String,
    /// Unix seconds when the recording started.
    pub created: u64,
    /// Longest part, in seconds.
    pub duration: f64,
    /// Who the microphone belongs to. That track needs no diarization.
    pub local_speaker: String,
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PartKind {
    /// One screen. There is one of these per monitor recorded.
    Video,
    /// The local microphone. Exactly one speaker.
    Mic,
    /// Everything the machine played, which may carry several remote people.
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Part {
    pub kind: PartKind,
    /// Human label, e.g. "DP-1" or "Microphone".
    pub label: String,
    /// Object key inside the bucket, relative to the recording prefix.
    pub key: String,
    /// Presigned GET. Expires with the manifest link.
    pub url: String,
    /// Milliseconds this part starts after the earliest part.
    ///
    /// Each stream is captured by its own process, so they do not begin at the
    /// same instant. Subtracting this puts every part on one clock.
    #[serde(default)]
    pub offset_ms: i64,
    pub bytes: u64,
    #[serde(default)]
    pub duration: f64,
}

impl Manifest {
    pub fn videos(&self) -> impl Iterator<Item = &Part> {
        self.parts.iter().filter(|p| p.kind == PartKind::Video)
    }

    pub fn mic(&self) -> Option<&Part> {
        self.parts.iter().find(|p| p.kind == PartKind::Mic)
    }

    pub fn system(&self) -> Option<&Part> {
        self.parts.iter().find(|p| p.kind == PartKind::System)
    }

    /// The screen to sample frames from unless the caller names another.
    pub fn primary_video(&self) -> Option<&Part> {
        self.videos().next()
    }
}

impl Part {
    pub fn filename(&self) -> &str {
        self.key.rsplit('/').next().unwrap_or(&self.key)
    }
}
