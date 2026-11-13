//! Finding the files a pack's crash sources name, and tying them to one session.

use super::{modified_rfc3339, rfc3339, SessionWindow};
use agora_game_api::crash::{
    ArtifactUnit, ChildPolicy, CopyPolicy, CrashSource, MatchedBy, Privacy, SourceBind, SourceMatch,
};
use agora_game_api::{fold_path_case, GamePath, RelPath};
use regex::Regex;
use std::path::{Path, PathBuf};
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

/// A pack location split into the caller's trusted root and the pack's relative part. The base may
/// itself be a junction or symlink: people redirect Documents and game folders, and the caller
/// chose that root. Every component below it is checked separately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLocation {
    pub base: PathBuf,
    /// The pack's `/`-separated part below `base`.
    pub relative: String,
}

/// Turns a pack's [`GamePath`] into a trusted base and a relative part. The game plugin owns the
/// roots; a pack never holds an absolute path.
pub trait SourceRoots {
    fn resolve(&self, location: &GamePath) -> Result<SourceLocation, String>;
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
    UnsafeName,
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
    ReparsePoint,
    InvalidRegex,
    UnreadableEntry,
    TooManyEntries,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceProblem {
    pub plugin_id: String,
    pub source_id: String,
    pub kind: SourceProblemKind,
    pub detail: String,
}

/// How much collection may look at in one session. A huge folder must not take unbounded time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CollectionLimits {
    pub max_entries: usize,
}

impl Default for CollectionLimits {
    fn default() -> Self {
        Self {
            max_entries: 100_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectionStop {
    Entries,
}

/// Everything collection found, good and bad.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceCollection {
    pub bound: Vec<FoundArtifact>,
    pub seen: Vec<SeenArtifact>,
    pub folders: Vec<FolderObservation>,
    pub problems: Vec<SourceProblem>,
    /// Present when a limit stopped collection, so the caller knows the set is incomplete.
    pub stopped_early: Option<CollectionStop>,
}

/// Whether a name is safe to treat as a file on this platform. This is the same rule everywhere:
/// top-level files, folder children and session folder names. It reuses
/// [`crate::content_store::validate_entry_path`], which implements Microsoft's naming rules
/// (`NUL`, `COM1`, trailing dots and spaces, forbidden characters):
/// <https://learn.microsoft.com/en-us/windows/win32/fileio/naming-a-file>.
pub fn is_safe_artifact_name(name: &str) -> bool {
    crate::content_store::validate_entry_path(name).is_ok()
}

/// Find, filter and bind every artifact the given sources name.
pub fn collect_sources(
    specs: &[SourceSpec<'_>],
    roots: &dyn SourceRoots,
    facts: &dyn ArtifactFacts,
    session: &CrashSession,
    limits: CollectionLimits,
) -> SourceCollection {
    let mut collection = SourceCollection::default();
    let mut entries_seen = 0usize;

    'sources: for spec in specs {
        if collection.stopped_early.is_some() {
            break;
        }
        let source = spec.source;
        for candidate in &source.bind {
            if let SourceBind::PidInName { regex } = candidate {
                if let Err(error) = Regex::new(regex) {
                    collection.problems.push(SourceProblem {
                        plugin_id: spec.plugin_id.to_string(),
                        source_id: source.id.clone(),
                        kind: SourceProblemKind::InvalidRegex,
                        detail: format!("`{regex}`: {error}"),
                    });
                }
            }
        }
        let location = match roots.resolve(&source.location) {
            Ok(location) => location,
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

        let relative = match RelPath::new(location.relative.clone()) {
            Ok(relative) => relative,
            Err(error) => {
                collection.problems.push(SourceProblem {
                    plugin_id: spec.plugin_id.to_string(),
                    source_id: source.id.clone(),
                    kind: SourceProblemKind::PathEscapes,
                    detail: error.to_string(),
                });
                continue;
            }
        };

        let folder = match folder_below_trusted_root(&location.base, relative.as_str()) {
            Ok(folder) => folder,
            Err((kind, detail)) => {
                collection.problems.push(SourceProblem {
                    plugin_id: spec.plugin_id.to_string(),
                    source_id: source.id.clone(),
                    kind,
                    detail,
                });
                continue;
            }
        };

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
            entries_seen += 1;
            if entries_seen > limits.max_entries {
                collection.stopped_early = Some(CollectionStop::Entries);
                collection.problems.push(SourceProblem {
                    plugin_id: spec.plugin_id.to_string(),
                    source_id: source.id.clone(),
                    kind: SourceProblemKind::TooManyEntries,
                    detail: "collection reached its entry limit; the evidence is incomplete"
                        .to_string(),
                });
                break 'sources;
            }
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

            if !is_safe_artifact_name(&name) {
                collection.seen.push(SeenArtifact {
                    plugin_id: spec.plugin_id.to_string(),
                    source_id: source.id.clone(),
                    name,
                    path,
                    reason: RejectedReason::UnsafeName,
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
                Err(reason) => {
                    collection.seen.push(SeenArtifact {
                        plugin_id: spec.plugin_id.to_string(),
                        source_id: source.id.clone(),
                        name,
                        path,
                        reason,
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

/// Join the pack's part one component at a time, checking every component below the trusted root.
/// The base itself is not checked: the caller chose it, and redirected folders are legitimate.
pub(crate) fn folder_below_trusted_root(
    base: &Path,
    relative: &str,
) -> Result<PathBuf, (SourceProblemKind, String)> {
    let mut current = base.to_path_buf();
    for part in relative.split('/').filter(|part| !part.is_empty()) {
        current.push(part);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if crate::content_store::is_reparse_point_or_symlink(&metadata) {
                    return Err((
                        SourceProblemKind::ReparsePoint,
                        format!(
                            "`{}` is a junction or symlink below the trusted root",
                            current.display()
                        ),
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err((
                    SourceProblemKind::UnreadableEntry,
                    format!("`{}`: {error}", current.display()),
                ))
            }
        }
    }
    Ok(current)
}

pub(crate) fn marker_present(path: &Path, matching: &SourceMatch, folder: bool) -> bool {
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

/// Evaluate every declared bind. A bind that finds positive contradictory evidence vetoes the
/// artifact whatever the others say: a log naming another process is that process's log. Absent
/// evidence is not a contradiction, so another bind may still tie it to the session.
fn bind(
    path: &Path,
    name: &str,
    source: &CrashSource,
    session: &CrashSession,
    facts: &dyn ArtifactFacts,
) -> Result<MatchedBy, RejectedReason> {
    if source.bind.is_empty() {
        return Err(RejectedReason::NoBindDeclared);
    }

    let mut best: Option<(u8, MatchedBy)> = None;
    let mut veto: Option<(u8, RejectedReason)> = None;
    let mut failures = Vec::new();

    for candidate in &source.bind {
        let strength = bind_strength(candidate);
        match evaluate_bind(candidate, path, name, session, facts) {
            Ok(Some(matched_by)) => {
                if best.is_none_or(|(current, _)| strength > current) {
                    best = Some((strength, matched_by));
                }
            }
            Ok(None) => failures.push(RejectedReason::NoBindMatched),
            Err(reason) => {
                if is_contradiction(&reason)
                    && veto.as_ref().is_none_or(|(current, _)| strength > *current)
                {
                    veto = Some((strength, reason.clone()));
                }
                failures.push(reason);
            }
        }
    }

    if let Some((_, reason)) = veto {
        return Err(reason);
    }
    match best {
        Some((_, matched_by)) => Ok(matched_by),
        None => Err(strongest_failure(&failures)),
    }
}

fn is_contradiction(reason: &RejectedReason) -> bool {
    matches!(
        reason,
        RejectedReason::PidMismatch { found: Some(_) } | RejectedReason::WorkingDirMismatch
    )
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
) -> Result<Option<MatchedBy>, RejectedReason> {
    match bind {
        SourceBind::PidInName { regex } => {
            let Ok(regex) = Regex::new(regex) else {
                // The invalid regex is reported separately; it cannot tie anything to a session.
                return Ok(None);
            };
            let Some(captures) = regex.captures(name) else {
                return Ok(None);
            };
            let found = captures
                .get(1)
                .and_then(|group| group.as_str().parse::<u32>().ok());
            match (found, session.pid) {
                (Some(found), Some(pid)) if found == pid => Ok(Some(MatchedBy::Pid)),
                (Some(found), Some(_)) => Err(RejectedReason::PidMismatch { found: Some(found) }),
                // No pid in the name, or Agora did not see the game's pid: no contradiction.
                _ => Ok(None),
            }
        }
        SourceBind::PidInFacts => {
            let found = facts.process_facts(path).and_then(|facts| facts.pid);
            match (found, session.pid) {
                (Some(found), Some(pid)) if found == pid => Ok(Some(MatchedBy::Pid)),
                (Some(found), Some(_)) => Err(RejectedReason::PidMismatch { found: Some(found) }),
                _ => Ok(None),
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
                Err(RejectedReason::WorkingDirMismatch)
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
                Err(RejectedReason::OutsideSessionWindow)
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
