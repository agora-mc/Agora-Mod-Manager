//! Copying collected evidence into Agora's own store, reading it back by id, and retaining only so
//! much of it.

use super::rfc3339;
use super::source::{glob_matches, is_safe_artifact_name, FoundArtifact, SourceCollection};
use agora_game_api::crash::{
    Artifact, ArtifactUnit, ChildPolicy, CopyPolicy, LineGap, ParserLimits, Privacy, Retained,
};
use agora_plugin_api::host::{ScriptReadError, ScriptReads};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The folder, under the data root, that holds crash evidence.
pub const EVIDENCE_STORE_DIR: &str = "crash-evidence";

/// A copy larger than this is reduced to its head and tail. Loggers can write hundreds of
/// megabytes; Agora must not keep that, and must not read it into memory to find out.
pub const DEFAULT_MAX_COPY_BYTES: u64 = 8 * 1024 * 1024;
pub const DEFAULT_HEAD_BYTES: u64 = 64 * 1024;
pub const DEFAULT_TAIL_BYTES: u64 = 256 * 1024;
/// A single session never stores more files or bytes than these, however many a folder names.
pub const DEFAULT_MAX_COPY_FILES: usize = 2_000;
pub const DEFAULT_MAX_COPY_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
pub const DEFAULT_MAX_COPY_ENTRIES: usize = 100_000;
pub const DEFAULT_MAX_COPY_DEPTH: usize = 16;

const READ_CHUNK: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyLimits {
    pub max_file_bytes: u64,
    pub head_bytes: u64,
    pub tail_bytes: u64,
    /// Files this session may store.
    pub max_files: usize,
    /// Bytes this session may store, across all its artifacts.
    pub max_total_bytes: u64,
    /// Directory entries a folder artifact walk may visit.
    pub max_entries: usize,
    /// Folder levels a folder artifact walk may descend.
    pub max_depth: usize,
}

impl Default for CopyLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: DEFAULT_MAX_COPY_BYTES,
            head_bytes: DEFAULT_HEAD_BYTES,
            tail_bytes: DEFAULT_TAIL_BYTES,
            max_files: DEFAULT_MAX_COPY_FILES,
            max_total_bytes: DEFAULT_MAX_COPY_TOTAL_BYTES,
            max_entries: DEFAULT_MAX_COPY_ENTRIES,
            max_depth: DEFAULT_MAX_COPY_DEPTH,
        }
    }
}

/// Why copying stopped before the end. A caller must treat this as "the evidence is incomplete",
/// never as "there was nothing else".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyStop {
    Files,
    TotalBytes,
    Entries,
    Depth,
}

#[derive(Debug, Clone, Default)]
struct CopyState {
    files: usize,
    bytes: u64,
    entries: usize,
    stopped: Option<CopyStop>,
}

impl CopyState {
    fn stop(&mut self, reason: CopyStop) {
        if self.stopped.is_none() {
            self.stopped = Some(reason);
        }
    }
}

/// How many sessions and bytes the store may keep. Kept as one value so a caller has one thing to
/// change, not two that can disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionLimits {
    pub max_sessions: usize,
    pub max_bytes: u64,
}

impl Default for RetentionLimits {
    fn default() -> Self {
        Self {
            max_sessions: 40,
            max_bytes: 512 * 1024 * 1024,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("`{0}` is not a usable evidence session folder name")]
    BadSessionId(String),
    #[error("could not create the evidence store at `{path}`: {source}")]
    Create {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not write crash evidence to `{path}`: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not read the evidence store at `{path}`: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not resolve the evidence store at `{path}`: {source}")]
    Root {
        path: PathBuf,
        source: std::io::Error,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyProblemKind {
    Vanished,
    Unreadable,
    ReparseSkipped,
    UnsafeName,
    DepthExceeded,
}

/// A bound file that could not be copied. Recorded rather than dropped: the difference between
/// "there was no log" and "the log was locked" is what a person needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyProblem {
    pub source_id: String,
    pub name: String,
    pub kind: CopyProblemKind,
    pub detail: String,
}

/// One session's artifacts after copying, plus the id-to-file map parsers read through.
#[derive(Debug, Clone, Default)]
pub struct CopiedSession {
    pub session_id: String,
    pub artifacts: Vec<Artifact>,
    pub problems: Vec<CopyProblem>,
    /// Present when a file, byte, entry or depth limit stopped the copy. The evidence is then
    /// incomplete, and a caller must say so rather than call an absent log missing.
    pub stopped_early: Option<CopyStop>,
    /// Object id to the stored file. Never shown to a parser; only read operations use it.
    pub(crate) objects: BTreeMap<String, PathBuf>,
    /// Ids that deliberately have no bytes, with the reason a read is refused.
    pub(crate) unavailable: BTreeMap<String, String>,
    /// For a head-and-tail copy, where its two parts stop being adjacent.
    pub(crate) gaps: BTreeMap<String, LineGap>,
}

impl CopiedSession {
    /// A reader that reaches only this session's retained artifacts, by id.
    pub fn reads(&self, limits: ParserLimits) -> StoreReads {
        StoreReads {
            files: self.objects.clone(),
            unavailable: self.unavailable.clone(),
            gaps: self.gaps.clone(),
            limits,
            total: AtomicU64::new(0),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionSkip {
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RetentionReport {
    pub removed: Vec<String>,
    pub kept: Vec<String>,
    pub skipped: Vec<RetentionSkip>,
    pub bytes_before: u64,
    pub bytes_after: u64,
    /// Bytes held by sessions the user kept.
    pub kept_bytes: u64,
    /// True when the kept sessions alone are over a limit. Nothing is deleted for it.
    pub kept_over_limits: bool,
}

/// The evidence store. Everything it deletes lives under its own root; it never writes to, moves or
/// removes anything in a game's or a user's folder.
#[derive(Debug, Clone)]
pub struct EvidenceStore {
    root: PathBuf,
}

impl EvidenceStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn session_dir(&self, session_id: &str) -> PathBuf {
        self.root.join(session_id)
    }

    /// Copy everything bound to the session into its own folder, content addressed. A source file
    /// that is locked or gone is a problem on the result; a failure writing Agora's own store is an
    /// error, because then nothing was kept at all.
    pub fn copy(
        &self,
        session_id: &str,
        collection: &SourceCollection,
        limits: CopyLimits,
    ) -> Result<CopiedSession, StoreError> {
        if !is_safe_component(session_id) {
            return Err(StoreError::BadSessionId(session_id.to_string()));
        }
        let dir = self.session_dir(session_id);
        std::fs::create_dir_all(&dir).map_err(|source| StoreError::Create {
            path: dir.clone(),
            source,
        })?;

        let mut copied = CopiedSession {
            session_id: session_id.to_string(),
            ..CopiedSession::default()
        };
        let mut state = CopyState::default();

        for found in &collection.bound {
            if state.stopped.is_some() {
                break;
            }
            match found.unit {
                ArtifactUnit::File => {
                    if let Some(artifact) = copy_file(
                        &dir,
                        &found.path,
                        found,
                        found.copy,
                        found.privacy,
                        found.name.clone(),
                        found.size,
                        found.modified,
                        &limits,
                        &mut copied,
                        &mut state,
                    )? {
                        copied.artifacts.push(artifact);
                    }
                }
                ArtifactUnit::Folder => {
                    let artifact = copy_folder(&dir, found, &limits, &mut copied, &mut state)?;
                    copied.artifacts.push(artifact);
                }
            }
        }

        copied.stopped_early = state.stopped;
        copied
            .artifacts
            .sort_by(|left, right| left.name.cmp(&right.name));
        Ok(copied)
    }

    /// Delete whole sessions, oldest first, until the store is within both limits. Sessions whose
    /// answer the user chose to keep are never touched.
    pub fn retention(
        &self,
        keep_sessions: &[String],
        limits: RetentionLimits,
    ) -> Result<RetentionReport, StoreError> {
        let mut report = RetentionReport::default();
        let root = &self.root;
        let root_real = match root.canonicalize() {
            Ok(path) => path,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(report),
            Err(source) => {
                return Err(StoreError::Root {
                    path: root.clone(),
                    source,
                })
            }
        };

        let entries = std::fs::read_dir(root).map_err(|source| StoreError::Read {
            path: root.clone(),
            source,
        })?;

        let mut sessions: Vec<SessionOnDisk> = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(source) => {
                    return Err(StoreError::Read {
                        path: root.clone(),
                        source,
                    })
                }
            };
            let metadata = entry.metadata().map_err(|source| StoreError::Read {
                path: entry.path(),
                source,
            })?;
            if crate::content_store::is_reparse_point_or_symlink(&metadata) {
                report.skipped.push(RetentionSkip {
                    path: entry.path(),
                    reason: "a reparse point is never followed or removed".to_string(),
                });
                continue;
            }
            if !metadata.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            if keep_sessions.iter().any(|kept| kept == &name) {
                let (bytes, _) = measure_tree(&path, &mut report);
                report.kept_bytes = report.kept_bytes.saturating_add(bytes);
                report.kept.push(name);
                continue;
            }
            // A folder that resolves outside the store (via a junction planted at the session
            // level) is not the store's to delete.
            match path.canonicalize() {
                Ok(real) if real.starts_with(&root_real) => {}
                _ => {
                    report.skipped.push(RetentionSkip {
                        path,
                        reason: "it resolves outside the evidence store".to_string(),
                    });
                    continue;
                }
            }
            let (bytes, _) = measure_tree(&path, &mut report);
            sessions.push(SessionOnDisk {
                name,
                path,
                bytes,
                modified: metadata.modified().ok(),
            });
        }

        report.bytes_before =
            sessions.iter().map(|session| session.bytes).sum::<u64>() + report.kept_bytes;
        report.kept_over_limits =
            report.kept.len() > limits.max_sessions || report.kept_bytes > limits.max_bytes;

        let (deletions, mut skipped) = retention_decisions(&mut sessions, limits);
        report.skipped.append(&mut skipped);
        let mut freed_total = 0u64;
        for name in deletions {
            let Some(session) = sessions.iter().find(|session| session.name == name) else {
                continue;
            };
            let (freed, skipped) = remove_tree_inside(&root_real, &session.path);
            freed_total = freed_total.saturating_add(freed);
            if skipped.is_empty() {
                report.removed.push(name);
            } else {
                report.skipped.extend(skipped);
                report.skipped.push(RetentionSkip {
                    path: session.path.clone(),
                    reason: "some entries could not be removed and were left in place".to_string(),
                });
            }
        }

        report.bytes_after = report.bytes_before.saturating_sub(freed_total);
        Ok(report)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SessionOnDisk {
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    pub(crate) bytes: u64,
    pub(crate) modified: Option<std::time::SystemTime>,
}

/// Decide which sessions to delete, oldest first. A session whose age cannot be read is skipped and
/// reported, never guessed to be the oldest. Kept sessions never reach this list.
pub(crate) fn retention_decisions(
    sessions: &mut [SessionOnDisk],
    limits: RetentionLimits,
) -> (Vec<String>, Vec<RetentionSkip>) {
    let mut skipped = Vec::new();
    sessions.sort_by(|left, right| match (left.modified, right.modified) {
        (Some(left_time), Some(right_time)) => left_time
            .cmp(&right_time)
            .then_with(|| left.name.cmp(&right.name)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => left.name.cmp(&right.name),
    });

    let mut sessions_left = sessions.len();
    let mut bytes_left: u64 = sessions.iter().map(|session| session.bytes).sum();
    let mut deletions = Vec::new();
    for session in sessions.iter() {
        if sessions_left <= limits.max_sessions && bytes_left <= limits.max_bytes {
            break;
        }
        if session.modified.is_none() {
            skipped.push(RetentionSkip {
                path: session.path.clone(),
                reason: "its age could not be read, so it is not deleted".to_string(),
            });
            continue;
        }
        deletions.push(session.name.clone());
        sessions_left = sessions_left.saturating_sub(1);
        bytes_left = bytes_left.saturating_sub(session.bytes);
    }
    (deletions, skipped)
}

fn is_safe_component(name: &str) -> bool {
    name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains(':')
        && !name.contains('\0')
        && is_safe_artifact_name(name)
}

fn copy_folder(
    dir: &Path,
    found: &FoundArtifact,
    limits: &CopyLimits,
    copied: &mut CopiedSession,
    state: &mut CopyState,
) -> Result<Artifact, StoreError> {
    let mut children = Vec::new();
    let mut stack: Vec<(PathBuf, usize)> = vec![(found.path.clone(), 0)];

    while let Some((folder, depth)) = stack.pop() {
        if state.stopped.is_some() {
            break;
        }
        if depth > limits.max_depth {
            state.stop(CopyStop::Depth);
            copied.problems.push(CopyProblem {
                source_id: found.source_id.clone(),
                name: relative_name(&found.path, &folder),
                kind: CopyProblemKind::DepthExceeded,
                detail: "the folder is nested deeper than the copy limit".to_string(),
            });
            break;
        }

        let entries = match std::fs::read_dir(&folder) {
            Ok(entries) => entries,
            Err(error) => {
                copied.problems.push(CopyProblem {
                    source_id: found.source_id.clone(),
                    name: relative_name(&found.path, &folder),
                    kind: CopyProblemKind::Unreadable,
                    detail: error.to_string(),
                });
                continue;
            }
        };
        for entry in entries {
            if state.stopped.is_some() {
                break;
            }
            state.entries += 1;
            if state.entries > limits.max_entries {
                state.stop(CopyStop::Entries);
                break;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    copied.problems.push(CopyProblem {
                        source_id: found.source_id.clone(),
                        name: relative_name(&found.path, &folder),
                        kind: CopyProblemKind::Unreadable,
                        detail: error.to_string(),
                    });
                    continue;
                }
            };
            let path = entry.path();
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(error) => {
                    copied.problems.push(CopyProblem {
                        source_id: found.source_id.clone(),
                        name: relative_name(&found.path, &path),
                        kind: CopyProblemKind::Unreadable,
                        detail: error.to_string(),
                    });
                    continue;
                }
            };
            if crate::content_store::is_reparse_point_or_symlink(&metadata) {
                copied.problems.push(CopyProblem {
                    source_id: found.source_id.clone(),
                    name: relative_name(&found.path, &path),
                    kind: CopyProblemKind::ReparseSkipped,
                    detail: "a junction or symlink inside the folder was not followed".to_string(),
                });
                continue;
            }
            if metadata.is_dir() {
                stack.push((path, depth + 1));
                continue;
            }
            if !metadata.is_file() {
                continue;
            }
            let relative = relative_name(&found.path, &path);
            if !is_safe_artifact_name(&relative) {
                copied.problems.push(CopyProblem {
                    source_id: found.source_id.clone(),
                    name: relative,
                    kind: CopyProblemKind::UnsafeName,
                    detail: "the name is not safe on this platform; it was not copied".to_string(),
                });
                continue;
            }
            let (copy, privacy) = child_policy(&found.children, &relative)
                .map(|policy| (policy.copy, policy.privacy))
                .unwrap_or((found.copy, found.privacy));
            let Some(artifact) = copy_file(
                dir,
                &path,
                found,
                copy,
                privacy,
                relative,
                metadata.len(),
                metadata.modified().ok(),
                limits,
                copied,
                state,
            )?
            else {
                continue;
            };
            children.push(artifact);
        }
    }

    children.sort_by(|left, right| left.name.cmp(&right.name));
    let mut hasher = Sha256::new();
    for child in &children {
        hasher.update(child.name.as_bytes());
        hasher.update(b"\0");
        hasher.update(child.id.as_bytes());
        hasher.update(b"\0");
    }
    let id = hex::encode(hasher.finalize());
    let kept_any = children.iter().any(|child| {
        !matches!(
            child.retained,
            Retained::NotRetained { .. } | Retained::FactsOnly
        )
    });
    let retained = if kept_any {
        Retained::Whole
    } else if state.stopped.is_some() {
        Retained::NotRetained {
            why: "the folder copy stopped before its files were kept".to_string(),
        }
    } else {
        Retained::NotRetained {
            why: "none of the folder's files could be kept".to_string(),
        }
    };
    if kept_any {
        copied.unavailable.insert(
            id.clone(),
            "the folder's files are kept as children".to_string(),
        );
    }

    Ok(Artifact {
        id,
        name: found.name.clone(),
        size: children.iter().map(|child| child.size).sum(),
        modified_at: found.modified.map(rfc3339),
        source: Some(found.source_id.clone()),
        bound_by: Some(found.matched_by),
        privacy: found.privacy,
        retained,
        attributed_to: None,
        children,
    })
}

fn child_policy<'a>(policies: &'a [ChildPolicy], relative: &str) -> Option<&'a ChildPolicy> {
    let name = relative.rsplit('/').next().unwrap_or(relative);
    policies
        .iter()
        .find(|policy| glob_matches(&policy.glob, relative) || glob_matches(&policy.glob, name))
}

fn relative_name(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// How many bytes a copy would store, from its listed size and the policy. Used to refuse before
/// writing, so a folder of large files cannot exceed the session budget.
fn projected_stored_bytes(listed: u64, policy: CopyPolicy, limits: &CopyLimits) -> u64 {
    let head_tail = policy == CopyPolicy::HeadTail || listed > limits.max_file_bytes;
    if head_tail {
        let head = limits.head_bytes.min(listed);
        let tail = limits.tail_bytes.min(listed.saturating_sub(head));
        head.saturating_add(tail)
    } else {
        listed
    }
}

#[allow(clippy::too_many_arguments)]
fn copy_file(
    dir: &Path,
    source_path: &Path,
    found: &FoundArtifact,
    policy: CopyPolicy,
    privacy: Privacy,
    name: String,
    size_hint: u64,
    modified: Option<std::time::SystemTime>,
    limits: &CopyLimits,
    copied: &mut CopiedSession,
    state: &mut CopyState,
) -> Result<Option<Artifact>, StoreError> {
    let mut artifact = Artifact {
        id: String::new(),
        name,
        size: size_hint,
        modified_at: modified.map(rfc3339),
        source: Some(found.source_id.clone()),
        bound_by: Some(found.matched_by),
        privacy,
        retained: Retained::Whole,
        attributed_to: None,
        children: Vec::new(),
    };

    let unavailable = |artifact: &mut Artifact, copied: &mut CopiedSession, why: &str| {
        let id = synthetic_id(&found.source_id, &artifact.name);
        artifact.id = id.clone();
        artifact.size = 0;
        artifact.retained = Retained::NotRetained {
            why: why.to_string(),
        };
        copied.unavailable.insert(id, why.to_string());
    };

    match policy {
        CopyPolicy::Never => {
            unavailable(
                &mut artifact,
                copied,
                "this source is never copied; its parser reads it where it lies",
            );
            return Ok(Some(artifact));
        }
        CopyPolicy::FactsOnly => {
            let id = synthetic_id(&found.source_id, &artifact.name);
            artifact.id = id.clone();
            artifact.size = 0;
            artifact.retained = Retained::FactsOnly;
            copied
                .unavailable
                .insert(id, "only this artifact's facts were kept".to_string());
            return Ok(Some(artifact));
        }
        CopyPolicy::Whole | CopyPolicy::HeadTail => {}
    }

    if state.files >= limits.max_files {
        state.stop(CopyStop::Files);
        return Ok(None);
    }
    let projected = projected_stored_bytes(size_hint, policy, limits);
    if state.bytes.saturating_add(projected) > limits.max_total_bytes {
        state.stop(CopyStop::TotalBytes);
        return Ok(None);
    }
    let remaining = limits.max_total_bytes.saturating_sub(state.bytes);

    // Check again, now, in case the file became a junction or symlink since collection. A small
    // window remains between this check and `open`; that is acceptable because this code only
    // reads the source and never writes through it.
    match std::fs::symlink_metadata(source_path) {
        Ok(metadata) if crate::content_store::is_reparse_point_or_symlink(&metadata) => {
            copied.problems.push(CopyProblem {
                source_id: found.source_id.clone(),
                name: artifact.name.clone(),
                kind: CopyProblemKind::ReparseSkipped,
                detail: "the file became a junction or symlink before it was copied".to_string(),
            });
            unavailable(
                &mut artifact,
                copied,
                "the file became a junction or symlink before it could be copied",
            );
            return Ok(Some(artifact));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            copied.problems.push(CopyProblem {
                source_id: found.source_id.clone(),
                name: artifact.name.clone(),
                kind: CopyProblemKind::Vanished,
                detail: error.to_string(),
            });
            unavailable(
                &mut artifact,
                copied,
                "the file disappeared before it could be copied",
            );
            return Ok(Some(artifact));
        }
        _ => {}
    }

    let mut source = match File::open(source_path) {
        Ok(file) => file,
        Err(error) => {
            let (kind, why) = if error.kind() == std::io::ErrorKind::NotFound {
                (
                    CopyProblemKind::Vanished,
                    "the file disappeared before it could be copied".to_string(),
                )
            } else {
                (
                    CopyProblemKind::Unreadable,
                    format!("the file could not be read: {error}"),
                )
            };
            copied.problems.push(CopyProblem {
                source_id: found.source_id.clone(),
                name: artifact.name.clone(),
                kind,
                detail: error.to_string(),
            });
            unavailable(&mut artifact, copied, &why);
            return Ok(Some(artifact));
        }
    };

    let listed = source
        .metadata()
        .map_err(|source_error| StoreError::Read {
            path: source_path.to_path_buf(),
            source: source_error,
        })?
        .len();
    let store_cap = limits.max_file_bytes.min(remaining);
    let head_tail = policy == CopyPolicy::HeadTail || listed > limits.max_file_bytes;
    let (head_target, tail_target) = if head_tail {
        let head = limits.head_bytes.min(listed).min(store_cap);
        let tail = limits
            .tail_bytes
            .min(listed.saturating_sub(head))
            .min(store_cap.saturating_sub(head));
        (head, tail)
    } else {
        (listed.min(store_cap), 0)
    };

    let temp = dir.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|source| StoreError::Write {
            path: temp.clone(),
            source,
        })?;

    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; READ_CHUNK];
    let mut read_error = None;
    let mut actual_head = 0u64;
    let mut actual_tail = 0u64;
    let mut head_newlines = 0u64;

    // Head (or the whole file).
    let mut remaining_head = head_target;
    while remaining_head > 0 {
        let want = remaining_head.min(buffer.len() as u64) as usize;
        match source.read(&mut buffer[..want]) {
            Ok(0) => break,
            Ok(read) => {
                if let Err(source) = output.write_all(&buffer[..read]) {
                    let _ = std::fs::remove_file(&temp);
                    return Err(StoreError::Write {
                        path: temp.clone(),
                        source,
                    });
                }
                head_newlines +=
                    buffer[..read].iter().filter(|byte| **byte == b'\n').count() as u64;
                hasher.update(&buffer[..read]);
                actual_head += read as u64;
                remaining_head -= read as u64;
            }
            Err(error) => {
                read_error = Some(error);
                break;
            }
        }
    }

    // Tail, when there is one and the file did not shrink below it.
    if read_error.is_none() && tail_target > 0 {
        let tail_start = listed.saturating_sub(tail_target);
        if let Err(error) = source.seek(SeekFrom::Start(tail_start)) {
            read_error = Some(error);
        } else {
            let mut remaining_tail = tail_target;
            while remaining_tail > 0 {
                let want = remaining_tail.min(buffer.len() as u64) as usize;
                match source.read(&mut buffer[..want]) {
                    Ok(0) => break,
                    Ok(read) => {
                        if let Err(source) = output.write_all(&buffer[..read]) {
                            let _ = std::fs::remove_file(&temp);
                            return Err(StoreError::Write {
                                path: temp.clone(),
                                source,
                            });
                        }
                        hasher.update(&buffer[..read]);
                        actual_tail += read as u64;
                        remaining_tail -= read as u64;
                    }
                    Err(error) => {
                        read_error = Some(error);
                        break;
                    }
                }
            }
        }
    }

    if let Err(error) = output.flush() {
        let _ = std::fs::remove_file(&temp);
        return Err(StoreError::Write {
            path: temp.clone(),
            source: error,
        });
    }
    drop(output);

    if let Some(error) = read_error {
        let _ = std::fs::remove_file(&temp);
        copied.problems.push(CopyProblem {
            source_id: found.source_id.clone(),
            name: artifact.name.clone(),
            kind: CopyProblemKind::Unreadable,
            detail: error.to_string(),
        });
        unavailable(
            &mut artifact,
            copied,
            &format!("the file could not be read: {error}"),
        );
        return Ok(Some(artifact));
    }

    let stored = actual_head.saturating_add(actual_tail);
    let omitted = listed.saturating_sub(stored);
    let id = hex::encode(hasher.finalize());
    let object = dir.join(&id);
    if object.exists() {
        let _ = std::fs::remove_file(&temp);
    } else {
        std::fs::rename(&temp, &object).map_err(|source| StoreError::Write {
            path: object.clone(),
            source,
        })?;
    }

    artifact.id = id.clone();
    artifact.size = stored;
    artifact.retained = if omitted == 0 {
        Retained::Whole
    } else {
        Retained::HeadTail {
            head: actual_head,
            tail: actual_tail,
        }
    };
    if omitted > 0 && actual_tail > 0 {
        copied.gaps.insert(
            id.clone(),
            LineGap {
                at_line: head_newlines,
                omitted_bytes: omitted,
            },
        );
    }
    copied.objects.insert(id, object);
    state.files += 1;
    state.bytes = state.bytes.saturating_add(stored);
    Ok(Some(artifact))
}

fn synthetic_id(source_id: &str, name: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(source_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(name.as_bytes());
    format!("nr-{}", hex::encode(hasher.finalize()))
}

/// Every file under a session folder, with the total size and whether any link was found. Links
/// are not followed.
fn measure_tree(folder: &Path, report: &mut RetentionReport) -> (u64, bool) {
    let mut bytes = 0u64;
    let mut has_reparse = false;
    let mut stack = vec![folder.to_path_buf()];
    while let Some(folder) = stack.pop() {
        let entries = match std::fs::read_dir(&folder) {
            Ok(entries) => entries,
            Err(source) => {
                report.skipped.push(RetentionSkip {
                    path: folder,
                    reason: format!("could not be measured: {source}"),
                });
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(source) => {
                    report.skipped.push(RetentionSkip {
                        path: folder.clone(),
                        reason: format!("could not be measured: {source}"),
                    });
                    continue;
                }
            };
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(source) => {
                    report.skipped.push(RetentionSkip {
                        path: entry.path(),
                        reason: format!("could not be measured: {source}"),
                    });
                    continue;
                }
            };
            if crate::content_store::is_reparse_point_or_symlink(&metadata) {
                has_reparse = true;
                continue;
            }
            if metadata.is_dir() {
                stack.push(entry.path());
            } else if metadata.is_file() {
                bytes = bytes.saturating_add(metadata.len());
            }
        }
    }
    (bytes, has_reparse)
}

/// Remove ordinary files and empty folders under `path`, never a reparse point. Every resolved
/// path is checked against the store root first, so a link planted inside cannot delete outside.
fn remove_tree_inside(root_real: &Path, path: &Path) -> (u64, Vec<RetentionSkip>) {
    let mut freed = 0u64;
    let mut skipped = Vec::new();
    let mut stack = vec![path.to_path_buf()];
    let mut folders = Vec::new();

    while let Some(folder) = stack.pop() {
        let entries = match std::fs::read_dir(&folder) {
            Ok(entries) => entries,
            Err(source) => {
                skipped.push(RetentionSkip {
                    path: folder,
                    reason: format!("could not be read: {source}"),
                });
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(source) => {
                    skipped.push(RetentionSkip {
                        path: folder.clone(),
                        reason: format!("could not be read: {source}"),
                    });
                    continue;
                }
            };
            let path = entry.path();
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(source) => {
                    skipped.push(RetentionSkip {
                        path,
                        reason: format!("could not be read: {source}"),
                    });
                    continue;
                }
            };
            if crate::content_store::is_reparse_point_or_symlink(&metadata) {
                skipped.push(RetentionSkip {
                    path,
                    reason: "a reparse point is never followed or removed".to_string(),
                });
                continue;
            }
            let inside = path
                .canonicalize()
                .map(|real| real.starts_with(root_real))
                .unwrap_or(false);
            if !inside {
                skipped.push(RetentionSkip {
                    path,
                    reason: "it resolves outside the evidence store".to_string(),
                });
                continue;
            }
            if metadata.is_dir() {
                folders.push(path.clone());
                stack.push(path);
            } else if metadata.is_file() {
                match std::fs::remove_file(&path) {
                    Ok(()) => freed = freed.saturating_add(metadata.len()),
                    Err(source) => skipped.push(RetentionSkip {
                        path,
                        reason: format!("could not be removed: {source}"),
                    }),
                }
            }
        }
    }

    // Deepest first, so every folder is empty by the time it is removed.
    folders.sort_by_key(|folder| std::cmp::Reverse(folder.components().count()));
    for folder in folders {
        if let Err(source) = std::fs::remove_dir(&folder) {
            skipped.push(RetentionSkip {
                path: folder,
                reason: format!("could not be removed: {source}"),
            });
        }
    }
    if skipped.is_empty() {
        if let Err(source) = std::fs::remove_dir(path) {
            skipped.push(RetentionSkip {
                path: path.to_path_buf(),
                reason: format!("could not be removed: {source}"),
            });
        }
    }
    (freed, skipped)
}

/// Reads a copied artifact by id. Ids that were deliberately not kept say so; an id from another
/// session is out of scope.
pub struct StoreReads {
    files: BTreeMap<String, PathBuf>,
    unavailable: BTreeMap<String, String>,
    gaps: BTreeMap<String, LineGap>,
    limits: ParserLimits,
    total: AtomicU64,
}

impl StoreReads {
    fn refused(code: &str, message: impl Into<String>) -> ScriptReadError {
        ScriptReadError::new(code, message)
    }

    fn file(&self, artifact: &str) -> Result<&PathBuf, ScriptReadError> {
        if let Some(path) = self.files.get(artifact) {
            return Ok(path);
        }
        if let Some(why) = self.unavailable.get(artifact) {
            return Err(Self::refused("not_retained", why.clone()));
        }
        Err(Self::refused(
            "out_of_scope",
            format!("`{artifact}` is not part of this evidence"),
        ))
    }

    fn spend(&self, wanted: u64) -> Result<(), ScriptReadError> {
        let mut current = self.total.load(Ordering::SeqCst);
        loop {
            if current.saturating_add(wanted) > self.limits.max_total_read_bytes {
                return Err(Self::refused(
                    "budget_spent",
                    "this parse has read its whole byte budget",
                ));
            }
            match self.total.compare_exchange(
                current,
                current + wanted,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return Ok(()),
                Err(actual) => current = actual,
            }
        }
    }
}

impl ScriptReads for StoreReads {
    fn read_bytes(
        &self,
        artifact: &str,
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>, ScriptReadError> {
        let path = self.file(artifact)?;
        if len > self.limits.max_read_bytes {
            return Err(Self::refused(
                "too_large",
                format!(
                    "asked for {len} bytes; one read may return {}",
                    self.limits.max_read_bytes
                ),
            ));
        }
        let mut file = File::open(path).map_err(|error| {
            Self::refused("unreadable", format!("could not open the copy: {error}"))
        })?;
        if offset > 0 {
            file.seek(SeekFrom::Start(offset))
                .map_err(|error| Self::refused("unreadable", error.to_string()))?;
        }
        let mut buffer = vec![0u8; len as usize];
        let mut filled = 0usize;
        while filled < buffer.len() {
            match file.read(&mut buffer[filled..]) {
                Ok(0) => break,
                Ok(read) => filled += read,
                Err(error) => return Err(Self::refused("unreadable", error.to_string())),
            }
        }
        buffer.truncate(filled);
        self.spend(buffer.len() as u64)?;
        Ok(buffer)
    }

    fn read_lines(
        &self,
        artifact: &str,
        from_line: u64,
        max_lines: u32,
    ) -> Result<serde_json::Value, ScriptReadError> {
        let path = self.file(artifact)?;
        let file = File::open(path).map_err(|error| {
            Self::refused("unreadable", format!("could not open the copy: {error}"))
        })?;
        let mut reader = BufReader::new(file);
        let max_lines = max_lines.min(self.limits.max_lines);
        let max_len = self.limits.max_line_len as usize;
        let mut lines: Vec<String> = Vec::with_capacity((max_lines as usize).min(1024));
        let mut cut = false;
        let mut index = 0u64;
        let mut more = false;
        let mut charged = 0u64;
        let mut buffer = Vec::new();

        loop {
            buffer.clear();
            let read = reader.read_until(b'\n', &mut buffer).map_err(|error| {
                Self::refused("unreadable", format!("could not read the copy: {error}"))
            })?;
            if read == 0 {
                break;
            }
            if index < from_line {
                index += 1;
                continue;
            }
            if lines.len() >= max_lines as usize {
                more = true;
                break;
            }
            let text = String::from_utf8_lossy(&buffer);
            let line = text.strip_suffix('\n').unwrap_or(&text);
            let line = line.strip_suffix('\r').unwrap_or(line);
            let (line, was_cut) = match line.char_indices().nth(max_len) {
                Some((byte, _)) => (&line[..byte], true),
                None => (line, false),
            };
            cut |= was_cut;
            charged = charged.saturating_add(line.len() as u64);
            lines.push(line.to_string());
            index += 1;
        }
        self.spend(charged)?;

        serde_json::to_value(agora_game_api::crash::LinesReply {
            lines,
            next_line: more.then_some(index),
            cut,
            gap: self.gaps.get(artifact).copied(),
        })
        .map_err(|error| Self::refused("unreadable", format!("could not encode lines: {error}")))
    }
}
