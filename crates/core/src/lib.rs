//! Shared guts of recap: settings, capture, upload, and the bundle format that
//! one shareable link resolves to.
//!
//! Reading a recording back is deliberately not here. That lives in
//! `watchvid.py`, kept to the Python standard library so the agent that reads
//! recordings can edit its own tooling without a rebuild.

pub mod check;
pub mod config;
pub mod manifest;
pub mod player;
pub mod progress;
pub mod record;
pub mod s3;
pub mod video;

pub use config::Config;
pub use manifest::{Manifest, Part, PartKind};
