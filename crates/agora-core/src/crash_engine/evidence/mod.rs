//! The crash doctor's file-facing evidence stage (MASTER_SPEC §30.4, §30.11).
//!
//! Three jobs, each kept out of the pure evaluator:
//!
//! - **Collect** what a pack's `sources` name, and tie each file to the session. A file whose bind
//!   does not work is reported as seen and rejected, never presented as this crash's evidence.
//! - **Copy** what was bound into Agora's own store at once, before a logger rotates it away.
//!   Parsers then read the copy through ids, never paths.
//! - **Sweep** the instance folders the caller names for text and log files changed during the
//!   session, compared with the last clean session. It reports "new since then", nothing more.
//!
//! The store keeps a retention limit. It only ever deletes inside itself, and never follows a
//! reparse point out of the store or out of a scanned folder.

mod source;
mod store;
mod sweep;

pub use source::*;
pub(crate) use source::{folder_below_trusted_root, marker_present};
pub use store::*;
pub use sweep::*;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The wall-clock span a game session covered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionWindow {
    pub started: SystemTime,
    pub ended: SystemTime,
}

impl SessionWindow {
    pub fn new(started: SystemTime, ended: SystemTime) -> Self {
        Self { started, ended }
    }

    /// The same window, widened on each side by the slop a source declares.
    pub fn widened(self, before: Duration, after: Duration) -> Self {
        Self {
            started: self.started.checked_sub(before).unwrap_or(self.started),
            ended: self.ended.checked_add(after).unwrap_or(self.ended),
        }
    }

    pub fn contains(self, time: SystemTime) -> bool {
        time >= self.started && time <= self.ended
    }
}

pub(crate) fn unix_ms(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_millis() as i64,
        Err(error) => -(error.duration().as_millis() as i64),
    }
}

pub(crate) fn rfc3339(time: SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(time).to_rfc3339()
}

pub(crate) fn modified_rfc3339(path: &std::path::Path) -> Option<String> {
    std::fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .map(rfc3339)
}

#[cfg(test)]
mod tests;
