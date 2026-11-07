//! Sweeping the folders a caller names for files that changed during a session, compared with the
//! last clean session.
//!
//! This is deliberately not a keyword scanner. It reports one kind of fact: this text or log file
//! is new or different since the last clean session. Bounds are stated when they stop the walk, so
//! "not seen" is never mistaken for "clean".

use super::source::is_safe_artifact_name;
use super::{unix_ms, SessionWindow};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

/// One folder to sweep. The name is the key used in a baseline, so it must stay stable between
/// sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepRoot {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepLimits {
    pub max_files: usize,
    pub max_bytes: u64,
    pub max_ms: u64,
    pub max_depth: usize,
}

impl Default for SweepLimits {
    fn default() -> Self {
        Self {
            max_files: 250_000,
            max_bytes: 4 * 1024 * 1024 * 1024,
            max_ms: 30_000,
            max_depth: 32,
        }
    }
}

/// A file's identity in a baseline. Size and modification time are enough to notice the ordinary
/// append-and-rotate behaviour of a logger without reading every file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaselineEntry {
    pub size: u64,
    pub modified_unix_ms: i64,
}

/// What the last clean session looked like. Serialize it beside the session record.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Baseline {
    /// `root name/relative/path`, always with forward slashes.
    pub files: BTreeMap<String, BaselineEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweepChange {
    New,
    Changed,
}

/// One file that is new or changed since the clean session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepFact {
    pub root: String,
    pub path: String,
    pub change: SweepChange,
    pub size: u64,
    pub modified_unix_ms: i64,
    pub previous: Option<BaselineEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaselineComparison {
    Compared,
    /// There was no clean session to compare with. This is not "nothing changed".
    Missing {
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweepStop {
    FileCount,
    Bytes,
    Time,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweepProblemKind {
    MissingRoot,
    UnreadableRoot,
    UnreadableEntry,
    ReparseSkipped,
    TooDeep,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepProblem {
    pub root: String,
    pub path: PathBuf,
    pub kind: SweepProblemKind,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepResult {
    pub facts: Vec<SweepFact>,
    /// The state now, for the next comparison. Only use it when `baseline_complete` is true.
    pub baseline: Baseline,
    pub baseline_complete: bool,
    pub comparison: BaselineComparison,
    /// Present when a bound stopped the walk. An absent file is not evidence of a clean session.
    pub stopped_early: Option<SweepStop>,
    pub files_seen: u64,
    pub problems: Vec<SweepProblem>,
}

fn is_text_log_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".log", ".txt", ".crash"]
        .iter()
        .any(|extension| lower.ends_with(extension))
}

/// Walk every named folder, bounded by file count, bytes and time, and report the text and log
/// files changed inside the session window.
pub fn sweep(
    roots: &[SweepRoot],
    window: SessionWindow,
    baseline: Option<&Baseline>,
    limits: SweepLimits,
) -> SweepResult {
    let comparison = match baseline {
        Some(_) => BaselineComparison::Compared,
        None => BaselineComparison::Missing {
            reason: "there is no clean session to compare with".to_string(),
        },
    };
    let can_compare = baseline.is_some();
    let mut result = SweepResult {
        facts: Vec::new(),
        baseline: Baseline::default(),
        baseline_complete: false,
        comparison,
        stopped_early: None,
        files_seen: 0,
        problems: Vec::new(),
    };

    let start = Instant::now();
    let mut bytes_seen = 0u64;
    let mut entries_seen = 0usize;

    'roots: for root in roots {
        if let Err(error) = std::fs::read_dir(&root.path) {
            let kind = if error.kind() == std::io::ErrorKind::NotFound {
                SweepProblemKind::MissingRoot
            } else {
                SweepProblemKind::UnreadableRoot
            };
            result.problems.push(SweepProblem {
                root: root.name.clone(),
                path: root.path.clone(),
                kind,
                detail: error.to_string(),
            });
            continue;
        }

        let mut pending = vec![(root.path.clone(), 0usize)];
        while let Some((folder, depth)) = pending.pop() {
            let entries = match std::fs::read_dir(&folder) {
                Ok(entries) => entries,
                Err(error) => {
                    result.problems.push(SweepProblem {
                        root: root.name.clone(),
                        path: folder,
                        kind: SweepProblemKind::UnreadableEntry,
                        detail: error.to_string(),
                    });
                    continue;
                }
            };
            for entry in entries {
                entries_seen += 1;
                if entries_seen > limits.max_files {
                    result.stopped_early = Some(SweepStop::FileCount);
                    break 'roots;
                }
                if limits.max_ms > 0 && start.elapsed().as_millis() as u64 > limits.max_ms {
                    result.stopped_early = Some(SweepStop::Time);
                    break 'roots;
                }
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        result.problems.push(SweepProblem {
                            root: root.name.clone(),
                            path: folder.clone(),
                            kind: SweepProblemKind::UnreadableEntry,
                            detail: error.to_string(),
                        });
                        continue;
                    }
                };
                let path = entry.path();
                let metadata = match entry.metadata() {
                    Ok(metadata) => metadata,
                    Err(error) => {
                        result.problems.push(SweepProblem {
                            root: root.name.clone(),
                            path: path.clone(),
                            kind: SweepProblemKind::UnreadableEntry,
                            detail: error.to_string(),
                        });
                        continue;
                    }
                };
                if crate::content_store::is_reparse_point_or_symlink(&metadata) {
                    result.problems.push(SweepProblem {
                        root: root.name.clone(),
                        path,
                        kind: SweepProblemKind::ReparseSkipped,
                        detail: "a junction or symlink is never followed".to_string(),
                    });
                    continue;
                }
                if metadata.is_dir() {
                    if depth >= limits.max_depth {
                        result.problems.push(SweepProblem {
                            root: root.name.clone(),
                            path,
                            kind: SweepProblemKind::TooDeep,
                            detail: "the folder is nested deeper than the sweep limit".to_string(),
                        });
                        continue;
                    }
                    pending.push((path, depth + 1));
                    continue;
                }
                if !metadata.is_file() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                if !is_text_log_name(&name) || !is_safe_artifact_name(&name) {
                    continue;
                }

                let size = metadata.len();
                if bytes_seen.saturating_add(size) > limits.max_bytes {
                    result.stopped_early = Some(SweepStop::Bytes);
                    break 'roots;
                }
                bytes_seen = bytes_seen.saturating_add(size);
                result.files_seen += 1;

                let Some(modified) = metadata.modified().ok() else {
                    result.problems.push(SweepProblem {
                        root: root.name.clone(),
                        path,
                        kind: SweepProblemKind::UnreadableEntry,
                        detail: "the file has no modification time".to_string(),
                    });
                    continue;
                };
                let modified_unix_ms = unix_ms(modified);
                let Some(relative) = relative_path(&root.path, &path) else {
                    continue;
                };
                let key = format!("{}/{}", root.name, relative);
                let entry = BaselineEntry {
                    size,
                    modified_unix_ms,
                };
                result.baseline.files.insert(key.clone(), entry);

                if !window.contains(modified) {
                    continue;
                }
                if !can_compare {
                    continue;
                }
                let previous = baseline.and_then(|baseline| baseline.files.get(&key).copied());
                let change = match previous {
                    None => Some(SweepChange::New),
                    Some(previous) if previous != entry => Some(SweepChange::Changed),
                    Some(_) => None,
                };
                if let Some(change) = change {
                    result.facts.push(SweepFact {
                        root: root.name.clone(),
                        path: relative,
                        change,
                        size,
                        modified_unix_ms,
                        previous,
                    });
                }
            }
        }
    }

    // A sweep with any problem or early stop is incomplete. It must never replace the last good
    // baseline, or a folder Agora could not read would look clean next time.
    result.baseline_complete = result.stopped_early.is_none() && result.problems.is_empty();
    result
        .facts
        .sort_by(|left, right| (&left.root, &left.path).cmp(&(&right.root, &right.path)));
    result
}

fn relative_path(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let parts: Vec<String> = relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}
