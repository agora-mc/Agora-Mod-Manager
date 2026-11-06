//! Finding the files a pack's crash sources name, and tying them to one session.

use super::{modified_rfc3339, rfc3339, SessionWindow};
use agora_game_api::crash::{
    ArtifactUnit, ChildPolicy, CopyPolicy, CrashSource, MatchedBy, Privacy, SourceBind, SourceMatch,
};
use agora_game_api::{fold_path_case, GamePath};
use regex::Regex;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

/// What is known about the session whose evidence is being collected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashSession {
    pub session_id: String,
    /// The game process's id, when the watcher held it.
    pub pid: Option<u32>,
    /// The working folder the game reported, when one was seen.
    pub working_dir: Option<PathBuf>,
    pub window: SessionWindow,
}

/// Turns a pack's [`GamePath`] into a real folder. The game plugin owns the roots; a pack never
/// holds an absolute path.
pub trait SourceRoots {
    fn resolve(&self, location: &GamePath) -> Result<PathBuf, String>;
}

/// The `core.process` facts a parser reported for one artifact. The collector uses them only to
/// decide whether the artifact belongs to this session; the parser itself later reads the copy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcessFacts {
    pub pid: Option<u32>,
    pub working_dir: Option<PathBuf>,
}

pub trait ArtifactFacts {
    fn process_facts(&self, artifact: &Path) -> Option<ProcessFacts>;
}

/// A caller that has no parser facts (yet) uses this.
pub struct NoArtifactFacts;

impl ArtifactFacts for NoArtifactFacts {
    fn process_facts(&self, _artifact: &Path) -> Option<ProcessFacts> {
        None
    }
}

/// One source, with the plugin it came from so every refusal names its author.
#[derive(Debug, Clone, Copy)]
pub struct SourceSpec<'a> {
    pub plugin_id: &'a str,
    pub source: &'a CrashSource,
}

/// A file or folder that matched a source and was tied to the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundArtifact {
    pub plugin_id: String,
    pub source_id: String,
    pub name: String,
    pub path: PathBuf,
    pub unit: ArtifactUnit,
    pub matched_by: MatchedBy,
    pub copy: CopyPolicy,
    pub privacy: Privacy,
    pub children: Vec<ChildPolicy>,
    pub format: Option<String>,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

/// Why a matching file is not this crash's evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectedReason {
    OutsideSessionWindow,
    PidMismatch { found: Option<u32> },
    WorkingDirMismatch,
    NoBindDeclared,
    NoBindMatched,
    ReparsePoint,
    ReservedName,
    NotFinished,
    NotAnArtifact,
    FolderArtifactExpected,
}

/// A file collection saw and refused to bind, kept so the reason is shown rather than the file
/// silently vanishing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeenArtifact {
    pub plugin_id: String,
    pub source_id: String,
    pub name: String,
    pub path: PathBuf,
    pub reason: RejectedReason,
    pub modified_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderState {
    Empty,
    Missing,
    Unreadable,
    /// The folder was read. It may hold files that no source matched; that is not the same fact as
    /// an empty folder.
    Read {
        entries: usize,
    },
}

/// What one source's folder turned out to be. Empty, missing and unreadable are three different
/// facts: "there is nothing there" must not be confused with "Agora could not look".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderObservation {
    pub plugin_id: String,
    pub source_id: String,
    pub path: PathBuf,
    pub state: FolderState,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceProblemKind {
    Resolve,
    PathEscapes,
    InvalidRegex,
    UnreadableEntry,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceProblem {
    pub plugin_id: String,
    pub source_id: String,
    pub kind: SourceProblemKind,
    pub detail: String,
}

/// Everything collection found, good and bad.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceCollection {
    pub bound: Vec<FoundArtifact>,
    pub seen: Vec<SeenArtifact>,
    pub folders: Vec<FolderObservation>,
    pub problems: Vec<SourceProblem>,
}

/// A Windows device name (`NUL`, `COM1`, and the like) must never be treated as a file: opening it
/// reaches a device, not the disk.
pub fn is_reserved_artifact_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    crate::content_store::is_windows_device_name(stem)
}

/// Find, filter and bind every artifact the given sources name.
pub fn collect_sources(
    specs: &[SourceSpec<'_>],
    roots: &dyn SourceRoots,
    facts: &dyn ArtifactFacts,
    session: &CrashSession,
) -> SourceCollection {
    let mut collection = SourceCollection::default();

    for spec in specs {
        let source = spec.source;
        let folder = match roots.resolve(&source.location) {
            Ok(folder) => folder,
            Err(detail) => {
                collection.problems.push(SourceProblem {
                    plugin_id: spec.plugin_id.to_string(),
                    source_id: source.id.clone(),
                    kind: SourceProblemKind::Resolve,
                    detail,
                });
                continue;
            }
        };

        if has_parent_component(&folder) {
            collection.problems.push(SourceProblem {
                plugin_id: spec.plugin_id.to_string(),
                source_id: source.id.clone(),
                kind: SourceProblemKind::PathEscapes,
                detail: format!("the resolved folder `{}` contains `..`", folder.display()),
            });
            continue;
        }

        let entries = match std::fs::read_dir(&folder) {
            Ok(entries) => entries,
            Err(error) => {
                let state = if error.kind() == std::io::ErrorKind::NotFound {
                    FolderState::Missing
                } else {
                    FolderState::Unreadable
                };
                collection.folders.push(FolderObservation {
                    plugin_id: spec.plugin_id.to_string(),
                    source_id: source.id.clone(),
                    path: folder,
                    state,
                    detail: Some(error.to_string()),
                });
                continue;
            }
        };

        let mut entries_read = 0usize;
        let mut saw_error = false;
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    saw_error = true;
                    collection.problems.push(SourceProblem {
                        plugin_id: spec.plugin_id.to_string(),
                        source_id: source.id.clone(),
                        kind: SourceProblemKind::UnreadableEntry,
                        detail: error.to_string(),
                    });
                    continue;
                }
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            entries_read += 1;

            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(error) => {
                    saw_error = true;
                    collection.problems.push(SourceProblem {
                        plugin_id: spec.plugin_id.to_string(),
                        source_id: source.id.clone(),
                        kind: SourceProblemKind::UnreadableEntry,
                        detail: format!("`{name}`: {error}"),
                    });
                    continue;
                }
            };

            if crate::content_store::is_reparse_point_or_symlink(&metadata) {
                // Never follow a junction or symlink out of the folder. Note it, then leave it
                // alone; a swept folder had the same rule.
                let modified_at = modified_rfc3339(&path);
                collection.seen.push(SeenArtifact {
                    plugin_id: spec.plugin_id.to_string(),
                    source_id: source.id.clone(),
                    name,
                    path,
                    reason: RejectedReason::ReparsePoint,
                    modified_at,
                });
                continue;
            }

            if !glob_matches(&source.matching.glob, &name) {
                continue;
            }

            if is_reserved_artifact_name(&name) {
                collection.seen.push(SeenArtifact {
                    plugin_id: spec.plugin_id.to_string(),
                    source_id: source.id.clone(),
                    name,
                    path,
                    reason: RejectedReason::ReservedName,
                    modified_at: None,
                });
                continue;
            }

            let unit_kind = match source.unit {
                ArtifactUnit::File => {
                    if !metadata.is_file() {
                        collection.seen.push(SeenArtifact {
                            plugin_id: spec.plugin_id.to_string(),
                            source_id: source.id.clone(),
                            name,
                            path,
                            reason: RejectedReason::NotAnArtifact,
                            modified_at: None,
                        });
                        continue;
                    }
                    ArtifactUnit::File
                }
                ArtifactUnit::Folder => {
                    if !metadata.is_dir() {
                        collection.seen.push(SeenArtifact {
                            plugin_id: spec.plugin_id.to_string(),
                            source_id: source.id.clone(),
                            name,
                            path,
                            reason: RejectedReason::FolderArtifactExpected,
                            modified_at: None,
                        });
                        continue;
                    }
                    ArtifactUnit::Folder
                }
            };

            if !marker_present(&path, &source.matching, unit_kind == ArtifactUnit::Folder) {
                collection.seen.push(SeenArtifact {
                    plugin_id: spec.plugin_id.to_string(),
                    source_id: source.id.clone(),
                    name,
                    path,
                    reason: RejectedReason::NotFinished,
                    modified_at: None,
                });
                continue;
            }

            let modified = metadata.modified().ok();
            match bind(&path, &name, source, session, facts) {
                Ok(matched_by) => collection.bound.push(FoundArtifact {
                    plugin_id: spec.plugin_id.to_string(),
                    source_id: source.id.clone(),
                    name,
                    path,
                    unit: unit_kind,
                    matched_by,
                    copy: source.copy,
                    privacy: source.privacy,
                    children: source.children.clone(),
                    format: source.format.clone(),
                    size: metadata.len(),
                    modified,
                }),
                Err(failure) => {
                    if let Some(detail) = failure.invalid {
                        collection.problems.push(SourceProblem {
                            plugin_id: spec.plugin_id.to_string(),
                            source_id: source.id.clone(),
                            kind: SourceProblemKind::InvalidRegex,
                            detail,
                        });
                    }
                    collection.seen.push(SeenArtifact {
                        plugin_id: spec.plugin_id.to_string(),
                        source_id: source.id.clone(),
                        name,
                        path,
                        reason: failure.reason,
                        modified_at: modified.map(rfc3339),
                    });
                }
            }
        }

        collection.folders.push(FolderObservation {
            plugin_id: spec.plugin_id.to_string(),
            source_id: source.id.clone(),
            path: folder,
            state: if entries_read > 0 {
                FolderState::Read {
                    entries: entries_read,
                }
            } else if saw_error {
                FolderState::Unreadable
            } else {
                FolderState::Empty
            },
            detail: None,
        });
    }

    // read_dir order is not defined; make the result reproducible.
    collection
        .bound
        .sort_by(|left, right| (&left.source_id, &left.name).cmp(&(&right.source_id, &right.name)));
    collection
        .seen
        .sort_by(|left, right| (&left.source_id, &left.name).cmp(&(&right.source_id, &right.name)));
    collection
}

fn has_parent_component(path: &Path) -> bool {
    path.components()
        .any(|component| component == Component::ParentDir)
}

fn marker_present(path: &Path, matching: &SourceMatch, folder: bool) -> bool {
    let Some(marker) = matching.marker.as_deref() else {
        return true;
    };
    let base = if folder {
        path.to_path_buf()
    } else {
        match path.parent() {
            Some(parent) => parent.to_path_buf(),
            None => return false,
        }
    };
    base.join(marker.replace('\\', "/")).is_file()
}

struct BindFailure {
    reason: RejectedReason,
    invalid: Option<String>,
}

impl BindFailure {
    fn reason(reason: RejectedReason) -> Self {
        Self {
            reason,
            invalid: None,
        }
    }
}

/// Evaluate every declared bind and use the strongest that works. The order in the pack is the
/// author's preference between equally strong binds; strength itself is fixed here so a pack
/// cannot make a time guess outrank a process id.
fn bind(
    path: &Path,
    name: &str,
    source: &CrashSource,
    session: &CrashSession,
    facts: &dyn ArtifactFacts,
) -> Result<MatchedBy, BindFailure> {
    if source.bind.is_empty() {
        return Err(BindFailure::reason(RejectedReason::NoBindDeclared));
    }

    let mut best: Option<(u8, MatchedBy)> = None;
    let mut failures = Vec::new();
    let mut invalid = None;

    for candidate in &source.bind {
        match evaluate_bind(candidate, path, name, session, facts) {
            Ok(Some(matched_by)) => {
                let strength = bind_strength(candidate);
                if best.is_none_or(|(current, _)| strength > current) {
                    best = Some((strength, matched_by));
                }
            }
            Ok(None) => failures.push(RejectedReason::NoBindMatched),
            Err(failure) => {
                if failure.invalid.is_some() {
                    invalid = failure.invalid.clone();
                }
                failures.push(failure.reason);
            }
        }
    }

    match best {
        Some((_, matched_by)) => Ok(matched_by),
        None => Err(BindFailure {
            reason: strongest_failure(&failures),
            invalid,
        }),
    }
}

fn bind_strength(bind: &SourceBind) -> u8 {
    match bind {
        SourceBind::PidInName { .. } | SourceBind::PidInFacts => 3,
        SourceBind::WorkingDirInFacts => 2,
        SourceBind::Time { .. } => 1,
    }
}

fn strongest_failure(failures: &[RejectedReason]) -> RejectedReason {
    let rank = |reason: &RejectedReason| match reason {
        RejectedReason::OutsideSessionWindow => 5,
        RejectedReason::PidMismatch { .. } => 4,
        RejectedReason::WorkingDirMismatch => 3,
        RejectedReason::NoBindMatched => 2,
        _ => 1,
    };
    failures
        .iter()
        .max_by_key(|reason| rank(reason))
        .cloned()
        .unwrap_or(RejectedReason::NoBindMatched)
}

fn evaluate_bind(
    bind: &SourceBind,
    path: &Path,
    name: &str,
    session: &CrashSession,
    facts: &dyn ArtifactFacts,
) -> Result<Option<MatchedBy>, BindFailure> {
    match bind {
        SourceBind::PidInName { regex } => {
            let regex = Regex::new(regex).map_err(|error| BindFailure {
                reason: RejectedReason::NoBindMatched,
                invalid: Some(format!("`{regex}`: {error}")),
            })?;
            let Some(captures) = regex.captures(name) else {
                return Ok(None);
            };
            let found = captures
                .get(1)
                .and_then(|group| group.as_str().parse::<u32>().ok());
            match (found, session.pid) {
                (Some(found), Some(pid)) if found == pid => Ok(Some(MatchedBy::Pid)),
                (found, _) => Err(BindFailure::reason(RejectedReason::PidMismatch { found })),
            }
        }
        SourceBind::PidInFacts => {
            let found = facts.process_facts(path).and_then(|facts| facts.pid);
            match (found, session.pid) {
                (None, _) => Ok(None),
                (Some(found), Some(pid)) if found == pid => Ok(Some(MatchedBy::Pid)),
                (found, _) => Err(BindFailure::reason(RejectedReason::PidMismatch { found })),
            }
        }
        SourceBind::WorkingDirInFacts => {
            let found = facts
                .process_facts(path)
                .and_then(|facts| facts.working_dir);
            let Some(found) = found else {
                return Ok(None);
            };
            let Some(expected) = &session.working_dir else {
                return Ok(None);
            };
            if same_folder(&found, expected) {
                Ok(Some(MatchedBy::WorkingDir))
            } else {
                Err(BindFailure::reason(RejectedReason::WorkingDirMismatch))
            }
        }
        SourceBind::Time { before_s, after_s } => {
            let Some(modified) = std::fs::metadata(path)
                .ok()
                .and_then(|metadata| metadata.modified().ok())
            else {
                return Ok(None);
            };
            let window = session.window.widened(
                std::time::Duration::from_secs(*before_s as u64),
                std::time::Duration::from_secs(*after_s as u64),
            );
            if window.contains(modified) {
                Ok(Some(MatchedBy::Time))
            } else {
                Err(BindFailure::reason(RejectedReason::OutsideSessionWindow))
            }
        }
    }
}

fn same_folder(left: &Path, right: &Path) -> bool {
    let normalize = |path: &Path| {
        let text = path.to_string_lossy().replace('\\', "/");
        let text = text.trim_end_matches('/');
        if cfg!(windows) {
            fold_path_case(text)
        } else {
            text.to_string()
        }
    };
    normalize(left) == normalize(right)
}

/// Case-insensitive `*` and `?`, over the name only, as `SourceMatch::glob` specifies.
pub fn glob_matches(pattern: &str, name: &str) -> bool {
    let pattern = pattern.to_lowercase();
    let name = name.to_lowercase();
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    let (mut p, mut n) = (0usize, 0usize);
    let (mut star, mut resume) = (None, 0usize);
    while n < name.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == name[n]) {
            p += 1;
            n += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            resume = n;
            p += 1;
        } else if let Some(star_at) = star {
            p = star_at + 1;
            resume += 1;
            n = resume;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}
