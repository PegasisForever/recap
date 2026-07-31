//! How long-running work reports back.
//!
//! Every stage between pressing Stop and getting a link can take minutes on a
//! recording hours long: compressing two audio tracks, relocating the index in
//! each video, then pushing gigabytes to S3. Without a fraction to show, all of
//! that looks identical to a hang.

/// One stage of the finish, and where it sits in the whole run.
///
/// Stages are numbered so a bar can show a single sweep from empty to full. Per
/// stage fractions look wrong: each one fills, then sits at 100% until the next
/// stage produces its first measurement, which reads as a stall.
#[derive(Debug, Clone)]
pub struct Stage {
    pub phase: String,
    /// 1-based position, and how many stages the whole finish has.
    pub step: u32,
    pub steps: u32,
}

impl Stage {
    pub fn new(phase: impl Into<String>, step: u32, steps: u32) -> Self {
        Self { phase: phase.into(), step, steps }
    }
}

/// One update from a stage that takes a while.
#[derive(Debug, Clone)]
pub struct Progress {
    /// What is happening, e.g. "Uploading Monitor 1".
    pub phase: String,
    /// Work done and work total within this stage, in whatever unit suits it:
    /// bytes for an upload, microseconds of media for a transcode.
    pub done: u64,
    pub total: u64,
    pub step: u32,
    pub steps: u32,
}

impl Progress {
    pub fn new(stage: &Stage, done: u64, total: u64) -> Self {
        Self {
            phase: stage.phase.clone(),
            done,
            total,
            step: stage.step,
            steps: stage.steps,
        }
    }

    /// A stage with nothing measurable to report, only a name.
    pub fn spinner(stage: &Stage) -> Self {
        Self::new(stage, 0, 0)
    }

    /// Progress within this stage alone, or None when it cannot say.
    pub fn fraction(&self) -> Option<f64> {
        if self.total == 0 {
            None
        } else {
            Some((self.done as f64 / self.total as f64).clamp(0.0, 1.0))
        }
    }

    /// Progress across the whole finish, which is what a bar should show.
    ///
    /// A stage with no measurable total still counts as started, so the bar
    /// advances to the stage boundary and waits there rather than at 100%.
    pub fn overall(&self) -> Option<f64> {
        if self.steps == 0 {
            return None;
        }
        let inner = self.fraction().unwrap_or(0.0);
        let done = self.step.saturating_sub(1) as f64 + inner;
        Some((done / self.steps as f64).clamp(0.0, 1.0))
    }
}

/// Callbacks are `FnMut` because they update a widget, and boxed as `dyn` so
/// the same signature works from a thread, a task and a test.
pub type Reporter<'a> = &'a mut dyn FnMut(Progress);

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}
