//! Path rules and path comparison (MASTER_SPEC §32.3).
//!
//! One way to decide whether a path may be written, and one way to say whether two paths are the
//! same. Both live here because core and the game packages need them and neither may own the other.
//!
//! Two rules about strictness, because stored values predate the strict rule:
//!
//! * [`RelPath::new`] is **lenient**: the rules a stored value or a plugin-supplied definition is
//!   held to on the way in. It stops only what would escape the root.
//! * [`RelPath::strict`] is the rule for a **new write and for anything about to touch the disk**:
//!   no `..`, no absolute path or drive letter, no `.` component, no `:`, no NUL, no component that
//!   ends in a dot or a space, and no Windows reserved device name — on every platform, because a
//!   catalog shared between platforms must produce names every platform can create. Backslashes are
//!   read as `/` before any rule is applied.
//!
//! Deserialization stays lenient, so old data keeps loading. A stored value that fails the strict
//! rule is reported where it is used ([`join_under`] is that place), never silently dropped.

use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fmt;
use std::path::{Component, Path, PathBuf};

use crate::game::fold_path_case;

/// The Windows attribute bit that marks a reparse point: a symbolic link, a junction, a mount point,
/// or a reparse point that carries data instead (a cloud placeholder, a deduplicated file).
/// `FILE_ATTRIBUTE_REPARSE_POINT`.
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

/// A validated path relative to a game, instance or base root.
///
/// Ensures the path cannot escape its root: backslashes are normalized to `/`,
/// `..` components are rejected, drive letters and absolute paths are rejected,
/// and null bytes are rejected. Empty string `""` represents the root directory itself, for the
/// callers that read a stored definition; use [`RelPath::strict`] and [`InstanceRoot`] before
/// writing anything.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RelPath(String);

/// A relative path broke the rules of the constructor that judged it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid relative path {value:?}: {reason}")]
pub struct RelPathError {
    /// The value as the caller passed it, so a message names what was refused.
    pub value: String,
    /// Why it was refused, in words a person can act on.
    pub reason: String,
}

impl RelPathError {
    fn new(value: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            reason: reason.into(),
        }
    }

    /// Why the value was refused.
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

/// The escape rules: the ones a stored value is held to, and the ones both constructors share.
const ESCAPE_RULE: &str =
    "must be relative and stay inside its root: no '..', no drive letters, no absolute paths";

impl RelPath {
    /// Validate a stored or supplied path **leniently**: it must stay inside its root.
    ///
    /// This is the constructor deserialization uses, so a value written before the strict rule
    /// existed still loads. Check it with [`RelPath::strict`] before anything is written.
    pub fn new(value: impl Into<String>) -> Result<Self, RelPathError> {
        let value = value.into();
        let normalized = value.replace('\\', "/");
        if normalized.starts_with('/')
            || normalized.contains('\0')
            || normalized.contains(':')
            || normalized.split('/').any(|part| part == "..")
        {
            return Err(RelPathError::new(value, ESCAPE_RULE));
        }
        Ok(Self(normalized))
    }

    /// Validate a path for a **new write, or for anything about to touch the disk**.
    ///
    /// Stricter than [`RelPath::new`] by every rule that would produce a file a platform cannot
    /// create, or a different file than the one named. The empty path is not a file: it is the
    /// root, which is [`InstanceRoot`]. The result is a [`StrictRelPath`], which is what a writer
    /// takes, so "did the caller check strictness" is answered by the compiler.
    pub fn strict(value: impl Into<String>) -> Result<StrictRelPath, RelPathError> {
        let value = value.into();
        let normalized = value.replace('\\', "/");
        if normalized.is_empty() {
            return Err(RelPathError::new(
                value,
                "it is empty: the root is InstanceRoot, not a file under it",
            ));
        }
        if normalized.starts_with('/') {
            return Err(RelPathError::new(
                value,
                "it is absolute: a leading '/' or a UNC path leaves the root",
            ));
        }
        if normalized.contains(':') {
            return Err(RelPathError::new(
                value,
                "a ':' is a drive letter or an NTFS stream, not a file name",
            ));
        }
        for component in normalized.split('/') {
            if component.is_empty() {
                return Err(RelPathError::new(
                    value,
                    "it has an empty component: a leading, trailing or doubled '/'",
                ));
            }
            if component == "." {
                return Err(RelPathError::new(
                    value,
                    "it has a '.' component, which names the folder it is in",
                ));
            }
            if component == ".." {
                return Err(RelPathError::new(
                    value,
                    "it has a '..' component, which leaves the root",
                ));
            }
            if component.ends_with('.') || component.ends_with(' ') {
                // Windows drops a trailing dot or space, so the name would not be the one asked for.
                return Err(RelPathError::new(
                    value,
                    format!("component '{component}' ends in a dot or a space"),
                ));
            }
            if let Some(bad) = component
                .chars()
                .find(|c| matches!(c, '?' | '*' | '"' | '<' | '>' | '|') || c.is_control())
            {
                return Err(RelPathError::new(
                    value,
                    format!(
                        "component '{component}' contains {} which Windows cannot have in a file \
                         name",
                        if bad.is_control() {
                            format!("the control character U+{:04X}", u32::from(bad))
                        } else {
                            format!("'{bad}'")
                        }
                    ),
                ));
            }
            let stem = component.split('.').next().unwrap_or("");
            if is_windows_device_name(stem) {
                return Err(RelPathError::new(
                    value,
                    format!("component '{component}' uses Windows device name '{stem}'"),
                ));
            }
        }
        Ok(StrictRelPath(Self(normalized)))
    }

    /// The path with `/` separators, as it will be written.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this path names the root itself rather than a file under it.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Re-check a value this type already holds, for a caller about to use it on disk.
    ///
    /// The error carries this path, so a stored value that breaks the strict rule is reported by
    /// name rather than dropped.
    pub fn check_strict(&self) -> Result<StrictRelPath, RelPathError> {
        Self::strict(self.0.clone())
    }
}

/// A relative path that passed the **strict** rule: a path Agora may write, or touch on disk.
///
/// This exists so the question "did the caller check strictness" is answered by the compiler rather
/// than by a runtime test at the last moment. A writer takes a `StrictRelPath`; the only ways to get
/// one are [`RelPath::strict`], [`StrictRelPath::new`] and [`RelPath::check_strict`], all of which
/// apply the rule. A [`RelPath`] that came out of storage or a plugin must be converted, which is
/// where a value written before the rule existed is reported — never dropped.
///
/// The empty path is not one: it is the root, which is [`InstanceRoot`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct StrictRelPath(RelPath);

impl StrictRelPath {
    /// Validate a path under the strict rule.
    pub fn new(value: impl Into<String>) -> Result<Self, RelPathError> {
        RelPath::strict(value)
    }

    /// The path with `/` separators, as it will be written.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// The same path as the lenient type, for an API that stores one.
    pub fn as_rel_path(&self) -> &RelPath {
        &self.0
    }

    /// The same path as the lenient type, by value.
    pub fn into_rel_path(self) -> RelPath {
        self.0
    }
}

impl From<StrictRelPath> for RelPath {
    fn from(path: StrictRelPath) -> Self {
        path.0
    }
}

impl From<StrictRelPath> for String {
    fn from(path: StrictRelPath) -> Self {
        path.0 .0
    }
}

impl TryFrom<String> for StrictRelPath {
    type Error = RelPathError;
    fn try_from(value: String) -> Result<Self, RelPathError> {
        Self::new(value)
    }
}

impl TryFrom<&str> for StrictRelPath {
    type Error = RelPathError;
    fn try_from(value: &str) -> Result<Self, RelPathError> {
        Self::new(value)
    }
}

impl fmt::Display for StrictRelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AsRef<str> for StrictRelPath {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::ops::Deref for StrictRelPath {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl TryFrom<String> for RelPath {
    type Error = RelPathError;
    fn try_from(value: String) -> Result<Self, RelPathError> {
        Self::new(value)
    }
}

impl TryFrom<&str> for RelPath {
    type Error = RelPathError;
    fn try_from(value: &str) -> Result<Self, RelPathError> {
        Self::new(value)
    }
}

impl From<RelPath> for String {
    fn from(path: RelPath) -> String {
        path.0
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for RelPath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::ops::Deref for RelPath {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl std::borrow::Borrow<str> for RelPath {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl PartialEq<str> for RelPath {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for RelPath {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<String> for RelPath {
    fn eq(&self, other: &String) -> bool {
        self.0 == *other
    }
}

impl PartialEq<RelPath> for str {
    fn eq(&self, other: &RelPath) -> bool {
        self == other.0.as_str()
    }
}

impl PartialEq<RelPath> for &str {
    fn eq(&self, other: &RelPath) -> bool {
        *self == other.0.as_str()
    }
}

impl PartialEq<RelPath> for String {
    fn eq(&self, other: &RelPath) -> bool {
        self == &other.0
    }
}

/// The root of an instance (or base): the folder itself, not a file under it.
///
/// A type of its own, and deliberately not convertible to [`RelPath`]: an operation that adds,
/// replaces or removes a file takes a [`RelPath`], so there is no value it could be given that
/// means "the instance root". The empty string is never a file path.
///
/// ```compile_fail
/// use agora_game_api::{InstanceRoot, RelPath};
/// // The root is not a file path, so it cannot be passed where one is needed.
/// let root: RelPath = InstanceRoot::new().into();
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct InstanceRoot;

impl InstanceRoot {
    pub fn new() -> Self {
        Self
    }

    /// The folder a root names, for the callers that read or list it rather than write into it.
    pub fn path_in(&self, root: &Path) -> PathBuf {
        root.to_path_buf()
    }
}

/// One file name: a path [`RelPath`] that has a single component.
///
/// Used where a writer names one file rather than a path — a download's file name, a chosen save
/// name. Same rules as [`RelPath::strict`]: a name every platform can create.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileName(String);

impl FileName {
    /// Validate one file name under the strict rules.
    pub fn new(value: impl Into<String>) -> Result<Self, RelPathError> {
        let value = value.into();
        let path = RelPath::strict(value.clone())?;
        if path.as_str().contains('/') {
            return Err(RelPathError::new(
                value,
                "is a path, not a file name: it contains a separator",
            ));
        }
        Ok(Self(path.into_rel_path().into()))
    }

    /// The name, as it will be written.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The one-component path this name is, for an API that takes a [`StrictRelPath`].
    pub fn as_strict(&self) -> StrictRelPath {
        StrictRelPath(RelPath(self.0.clone()))
    }

    /// The one-component path this name is, for an API that takes a [`RelPath`].
    pub fn as_rel_path(&self) -> RelPath {
        RelPath(self.0.clone())
    }
}

impl fmt::Display for FileName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for FileName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// A set of files breaks a rule that only shows when they are written together.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathSetError {
    /// No file at all: a caller that meant to write a set wrote nothing instead.
    #[error("no files")]
    EmptySet,
    /// Two paths that a case-insensitive disk stores as one file.
    #[error("'{path}' equals another file's path case-insensitively")]
    Collides { path: String },
    /// A path that one file uses, and another spelling of it uses as a folder.
    #[error("'{path}': folder '{prefix}' matches another file")]
    FolderIsAFile { path: String, prefix: String },
}

impl PathSetError {
    /// Why the set was refused, without the path, for an error type of another layer.
    pub fn reason(&self) -> String {
        match self {
            Self::EmptySet => "no files".to_string(),
            Self::Collides { .. } => "equals another file's path case-insensitively".to_string(),
            Self::FolderIsAFile { prefix, .. } => {
                format!("prefix folder '{prefix}' matches another file")
            }
        }
    }
}

/// Refuse a set of paths that a case-insensitive disk cannot hold separately.
///
/// Two files whose names differ only in case are one file on NTFS and one on the default APFS or
/// ext4-with-casefold setup too: writing the set would silently lose one of them. A path that is a
/// folder for one file and a file for another fails the same way.
///
/// Takes [`StrictRelPath`]: a set about to be written has been through the strict rule already, so
/// there is no way to hand this function a path a disk cannot hold.
pub fn validate_path_set(paths: &[StrictRelPath]) -> Result<(), PathSetError> {
    if paths.is_empty() {
        return Err(PathSetError::EmptySet);
    }

    let mut seen = std::collections::HashSet::with_capacity(paths.len());
    for path in paths {
        if !seen.insert(collision_key(path.as_str())) {
            return Err(PathSetError::Collides {
                path: path.to_string(),
            });
        }
    }

    for path in paths {
        let parts: Vec<&str> = path.as_str().split('/').collect();
        for i in 1..parts.len() {
            let prefix = parts[..i].join("/");
            if seen.contains(&collision_key(&prefix)) {
                return Err(PathSetError::FolderIsAFile {
                    path: path.to_string(),
                    prefix,
                });
            }
        }
    }

    Ok(())
}

/// The key a set of names is checked against: two names with one key may be one file on a
/// case-insensitive disk.
///
/// This folds harder than [`fold_path_case`]: the full Unicode case mapping first, so `ß` meets
/// `SS`, then the NTFS spelling of the result. A volume whose upcase table folds less than Unicode
/// does keeps the two files apart, and refusing them costs a refusal; the other error costs a file.
fn collision_key(name: &str) -> String {
    let full: String = name
        .chars()
        .flat_map(char::to_uppercase)
        .flat_map(char::to_lowercase)
        .collect();
    fold_path_case(&full)
}

/// What two paths are to each other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathRelation {
    /// One path, spelled differently.
    Same,
    /// Two things on disk.
    Different,
    /// At least one path could not be read, so the question has no answer. A caller that grants
    /// permission (signals a process, deletes a folder) treats this as "not the same" and says so.
    CantTell(String),
}

/// What two paths are to each other: one, two, or a question nobody could answer.
///
/// This is the question a caller asks when it has to find something it recorded earlier and the
/// thing may be gone: a base folder named in a manifest, an executable the OS reports for a
/// process. Comparing what exists resolves a symlinked or junctioned parent once, so one folder
/// does not look like two, and it stays exact where the disk is case-sensitive. Two paths that are
/// both genuinely absent can only be compared by spelling.
///
/// A path that exists but cannot be resolved — a link whose target is gone, a link loop, a name that
/// is not a folder on the way, a permission this process does not have — is `CantTell`, never
/// "the same": the two paths might be one.
pub fn compare_folder(left: &Path, right: &Path) -> PathRelation {
    match (read_target(left), read_target(right)) {
        (Ok(Some(left)), Ok(Some(right))) => {
            if same_target(&left, &right) {
                PathRelation::Same
            } else {
                PathRelation::Different
            }
        }
        (Ok(None), Ok(None)) => {
            if same_spelling(left, right) {
                PathRelation::Same
            } else {
                PathRelation::Different
            }
        }
        // One of them is there and the other is not: two things, never one.
        (Ok(_), Ok(_)) => PathRelation::Different,
        (Err(why), _) | (_, Err(why)) => PathRelation::CantTell(why),
    }
}

/// What a path is: its canonical target, or nothing at all when no name is there.
///
/// A name that exists but does not resolve is an error, not "absent": a dangling link, a link loop,
/// a name that is a file where the path continues, or a permission this process lacks. The question
/// "may this missing tail have been created" is the same one [`resolve_destination`] answers, so it
/// asks that.
fn read_target(path: &Path) -> Result<Option<PathBuf>, String> {
    match path.canonicalize() {
        Ok(real) => Ok(Some(real)),
        Err(_) => match resolve_destination(path) {
            // Nothing is there and nothing stops a write: the path is genuinely absent.
            Ok(_) => Ok(None),
            Err(error) => Err(error.to_string()),
        },
    }
}

/// A path could not be used safely on disk.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    /// The path breaks the strict rule, so writing it would create a different file than named.
    #[error("'{path}' is not a path Agora can write: {reason}")]
    UnsafePath { path: String, reason: String },
    /// Something on the way to the path is a link or a junction, so the write would land elsewhere.
    #[error("'{path}' goes through the link '{link}', which Agora will not write through")]
    LinkOnTheWay { path: String, link: String },
    /// What is on the way could not be read, so the question has no answer.
    #[error("cannot tell what '{path}' is: {error}")]
    Unreadable { path: String, error: String },
    /// Something on the way exists but is not a folder a write can go into.
    #[error("'{path}' cannot be written into: {reason}")]
    NotAFolder { path: String, reason: String },
    /// The path could not be resolved to a value that could then be checked and written.
    #[error("cannot resolve '{path}': {reason}")]
    Unresolvable { path: String, reason: String },
}

/// Join a relative path under a root, for a caller about to write there.
///
/// The relative path is a [`StrictRelPath`], so a value that predates the strict rule (a trailing
/// dot, a reserved name) cannot reach a write without being converted where it is reported. What
/// this adds to `root.join(rel)` at run time:
///
/// * the root is resolved **once**, and the result is a path under the resolved root. A folder the
///   user linked onto another drive — a junction, a symbolic link, a shared folder that Agora
///   made — is written through at its real name, and every check below is about the folder the
///   write will actually land in. A root that cannot be resolved is an error: a write must not
///   guess where it goes, and a missing root is reported as missing;
/// * the resolved root is a folder;
/// * below the root, nothing on the way is a link that redirects a name (a symbolic link, a
///   junction, a mount point). A reparse point that carries data instead of redirecting — a cloud
///   placeholder, a deduplicated file — is written through, because it is not a redirection;
/// * nothing on the way is a file where the path continues.
///
/// **This is a path check, not a handle, and the answer is only good while the caller holds the
/// instance's lock.** Between the check and the write another process — a syncing client, an
/// antivirus scanner, the user with a shell — can replace a folder on the way with a link, and the
/// write then lands outside the root. Closing that needs the write to happen through an opened
/// handle whose final path is compared, which is the staged-apply work, not this check.
///
/// A component that does not exist ends the walk: nothing below it can exist either.
///
/// ```compile_fail
/// # use std::path::Path;
/// use agora_game_api::{join_under, RelPath};
/// // A stored or supplied path has not passed the strict rule, so it cannot be written under.
/// let lenient = RelPath::new("mods/a.jar").unwrap();
/// let _ = join_under(Path::new("root"), &lenient);
/// ```
///
/// ```compile_fail
/// # use std::path::Path;
/// use agora_game_api::{join_under, RelPath};
/// let _ = join_under(Path::new("root"), &"mods/a.jar");
/// ```
pub fn join_under(root: &Path, rel: &StrictRelPath) -> Result<PathBuf, PathError> {
    // The root is the caller's own choice, and it may be a link: users junction an instance folder
    // onto another drive, and Agora's own shared folders are links. Resolving it once is what keeps
    // the answer honest — the path returned is under the folder the write lands in, and the checks
    // below are about that folder.
    let resolved_root = match root.canonicalize() {
        Ok(resolved) => resolved,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(PathError::NotAFolder {
                path: root.display().to_string(),
                reason: "it is missing".to_string(),
            })
        }
        Err(error) => {
            return Err(PathError::Unreadable {
                path: root.display().to_string(),
                error: error.to_string(),
            })
        }
    };
    let root_is_a_folder = match std::fs::metadata(&resolved_root) {
        Ok(metadata) => metadata.is_dir(),
        Err(error) => {
            return Err(PathError::Unreadable {
                path: root.display().to_string(),
                error: error.to_string(),
            })
        }
    };
    if !root_is_a_folder {
        return Err(PathError::NotAFolder {
            path: root.display().to_string(),
            reason: "it is not a folder".to_string(),
        });
    }

    let parts: Vec<&str> = rel.as_str().split('/').collect();
    let mut joined = resolved_root;
    let mut walked = String::new();
    let mut parent_is_a_folder = true;
    for (index, part) in parts.iter().enumerate() {
        let is_last = index + 1 == parts.len();
        joined.push(part);
        if !walked.is_empty() {
            walked.push('/');
        }
        walked.push_str(part);
        if !parent_is_a_folder {
            // Nothing below a component that is absent can exist, and a write will report it.
            continue;
        }
        match std::fs::symlink_metadata(&joined) {
            Ok(metadata) => {
                // A reparse point that carries data (a cloud placeholder, a deduplicated file) is
                // not a redirection: it is what the name means, so the walk goes on through it.
                if is_a_reparse_point(&metadata) && is_link(&joined)? {
                    return Err(PathError::LinkOnTheWay {
                        path: walked,
                        link: joined.display().to_string(),
                    });
                }
                if !is_last && !metadata.is_dir() {
                    return Err(PathError::NotAFolder {
                        path: walked,
                        reason: "it is not a folder, so nothing can be written under it"
                            .to_string(),
                    });
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                parent_is_a_folder = false;
            }
            Err(error) => {
                return Err(PathError::Unreadable {
                    path: walked,
                    error: error.to_string(),
                })
            }
        }
    }
    Ok(joined)
}

/// Whether `path` is a **link that redirects a name**: a symbolic link, a Windows junction or a
/// mount point.
///
/// A reparse point that carries data instead of redirecting — a cloud placeholder (OneDrive, a
/// synced folder) or a deduplicated file — names its own file: a write through it lands where the
/// caller put the name, so it is not a redirection and is not refused. Windows reports both kinds
/// with one attribute bit, so the reparse *tag* decides: only a name surrogate
/// (`IsReparseTagNameSurrogate`, the `0x2000_0000` bit of the tag) redirects a name.
///
/// Fails closed: a path that cannot be asked about is an error, never "not a link".
pub fn is_link(path: &Path) -> Result<bool, PathError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| PathError::Unreadable {
        path: path.display().to_string(),
        error: error.to_string(),
    })?;
    link_from_metadata(path, &metadata)
}

/// [`is_link`] for a caller that already has the entry own metadata. A sweep of a large
/// instance asks about tens of thousands of entries, so the metadata each one fetched is reused
/// rather than fetched twice.
fn link_from_metadata(path: &Path, metadata: &std::fs::Metadata) -> Result<bool, PathError> {
    if !is_a_reparse_point(metadata) {
        return Ok(false);
    }
    #[cfg(windows)]
    {
        let tag = reparse_tag(path).map_err(|error| PathError::Unreadable {
            path: path.display().to_string(),
            error: error.to_string(),
        })?;
        Ok(tag.redirects_a_name())
    }
    #[cfg(not(windows))]
    {
        Ok(metadata.file_type().is_symlink())
    }
}

/// [`is_link`] for a caller whose only choices are to skip the entry or to refuse it.
///
/// This is the shape every walk, loader and deployer needs, so the one rule has one home. A cloud
/// placeholder or a deduplicated file is an ordinary file everywhere; a symbolic link, a junction
/// and a mount point are refused everywhere.
///
/// A path that is genuinely absent redirects nothing — there is no name there to redirect, and the
/// write that follows reports its own error. Every other path that cannot be read counts as a
/// redirection: that is the answer a caller cannot follow. A dangling link is not absent, so it is
/// still refused.
pub fn redirects_a_name(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        // A path that cannot be asked about may be a link; following it is the failure. One stat
        // is enough: that metadata is what the answer is made of.
        Ok(metadata) => link_from_metadata(path, &metadata).unwrap_or(true),
        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
    }
}

/// Whether the reparse tag of a Windows reparse point wraps another name, which is what makes a
/// symbolic link, a junction and a mount point redirections. `IsReparseTagNameSurrogate` is the
/// documented test: the tag's `0x2000_0000` bit.
///
/// Exposed because the interesting cases cannot all be made on one machine — a real OneDrive
/// placeholder or a deduplicated file needs a cloud account or ReFS — so the classification is
/// tested on the tags Windows documents rather than only through a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReparseTag(u32);

impl ReparseTag {
    /// The tag Windows reported for a path, or `None` when the path is not a reparse point.
    pub fn of(tag: u32) -> Self {
        Self(tag)
    }

    /// The raw tag.
    pub fn value(self) -> u32 {
        self.0
    }

    /// Whether this tag redirects a name: a symbolic link, a junction, a mount point.
    pub fn redirects_a_name(self) -> bool {
        // IsReparseTagNameSurrogate: a name surrogate stores the name it stands for.
        self.0 & 0x2000_0000 != 0
    }
}

/// Whether the metadata of a path says it is some kind of reparse point (Windows) or a symbolic link
/// (Unix). Only the first half of the question: on Windows it does not say whether the tag redirects
/// a name, which is what [`is_link`] answers.
fn is_a_reparse_point(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        (metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

/// The reparse tag Windows reports for `path`, read from `FindFirstFileW`.
///
/// `dwReserved0` holds the tag for a reparse point; it is the one documented place the tag is
/// available without opening a handle on the reparse point itself.
#[cfg(windows)]
fn reparse_tag(path: &Path) -> std::io::Result<ReparseTag> {
    use std::os::windows::ffi::OsStrExt;

    // `FindFirstFileW` refuses a path that ends in a separator, except a bare drive ("C:\"), which
    // is what a caller passing a root hands it. Everything else loses its trailing separator.
    let mut name = path.as_os_str().to_os_string();
    {
        let text = name.to_string_lossy();
        let trimmed = text.trim_end_matches(['\\', '/']);
        if !trimmed.is_empty() && trimmed.len() != text.len() && !trimmed.ends_with(':') {
            name = std::ffi::OsString::from(trimmed);
        }
    }
    let wide: Vec<u16> = name.encode_wide().chain(std::iter::once(0)).collect();

    let mut data: Win32FindDataW = unsafe { std::mem::zeroed() };
    let found = unsafe { FindFirstFileW(wide.as_ptr(), &mut data) };
    if found as isize == -1 {
        return Err(std::io::Error::last_os_error());
    }
    unsafe { FindClose(found) };
    Ok(ReparseTag(data.reserved0))
}

/// `WIN32_FIND_DATAW` as far as its layout can be relied on: the attribute word first and
/// `dwReserved0` — the reparse tag — at its documented offset.
#[cfg(windows)]
#[repr(C)]
struct Win32FindDataW {
    file_attributes: u32,
    creation_time: [u32; 2],
    last_access_time: [u32; 2],
    last_write_time: [u32; 2],
    file_size_high: u32,
    file_size_low: u32,
    /// The reparse tag, when `file_attributes` carries `FILE_ATTRIBUTE_REPARSE_POINT`.
    reserved0: u32,
    reserved1: u32,
    file_name: [u16; 260],
    alternate_file_name: [u16; 14],
}

#[cfg(windows)]
extern "system" {
    fn FindFirstFileW(name: *const u16, data: *mut Win32FindDataW) -> *mut std::ffi::c_void;
    fn FindClose(handle: *mut std::ffi::c_void) -> i32;
}

/// Resolve a path once, to the exact value to check and then write.
///
/// The longest part of the path that exists is canonicalized — so every symbolic link and junction
/// in it is followed now, once — and the rest is appended as it was written. A `..` is refused
/// rather than resolved, because the two platforms resolve it differently when a folder on the way
/// does not exist: Windows collapses it lexically, Unix fails. The same path would then name two
/// different targets, which is the one thing one path may not do.
///
/// It fails closed. A part of the path that is only *missing* is an answer: nothing is there, and
/// the caller may create it. Anything else that stops the resolution is not — a link whose target is
/// gone, a link loop, a name that is a file where the path continues, a permission this process does
/// not have — because each of those might be a real destination Agora cannot see.
pub fn resolve_destination(path: &Path) -> Result<PathBuf, PathError> {
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(PathError::Unresolvable {
            path: path.display().to_string(),
            reason: "a '..' is resolved differently by each platform when a folder on the way is \
                     missing, so it is refused instead"
                .to_string(),
        });
    }
    let mut existing = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        match existing.canonicalize() {
            Ok(real) => {
                if tail.is_empty() {
                    return Ok(real);
                }
                // The part that exists has to be a folder for a missing tail to hang off it: a file
                // here means the tail names nothing that could ever be written.
                match std::fs::symlink_metadata(&real) {
                    Ok(metadata) if metadata.is_dir() => {}
                    Ok(_) => {
                        return Err(PathError::Unresolvable {
                            path: path.display().to_string(),
                            reason: format!(
                                "'{}' is not a folder, so nothing can be written under it",
                                real.display()
                            ),
                        })
                    }
                    Err(error) => {
                        return Err(PathError::Unresolvable {
                            path: path.display().to_string(),
                            reason: format!("cannot tell what '{}' is: {error}", real.display()),
                        })
                    }
                }
                let mut resolved = real;
                resolved.extend(tail.iter().rev());
                return Ok(resolved);
            }
            Err(error) => {
                // A name is there but the path it leads to is not: a dangling link, a link loop, a
                // permission. Only an absent name may be walked past.
                match std::fs::symlink_metadata(&existing) {
                    Err(missing) if missing.kind() == std::io::ErrorKind::NotFound => {}
                    Ok(_) => {
                        return Err(PathError::Unresolvable {
                            path: path.display().to_string(),
                            reason: format!(
                                "'{}' exists but does not resolve: {error}",
                                existing.display()
                            ),
                        })
                    }
                    Err(other) => {
                        return Err(PathError::Unresolvable {
                            path: path.display().to_string(),
                            reason: format!(
                                "cannot tell what '{}' is: {other}",
                                existing.display()
                            ),
                        })
                    }
                }
                let Some(name) = existing.file_name() else {
                    return Err(PathError::Unresolvable {
                        path: path.display().to_string(),
                        reason: error.to_string(),
                    });
                };
                tail.push(name.to_os_string());
                if !existing.pop() {
                    return Err(PathError::Unresolvable {
                        path: path.display().to_string(),
                        reason: error.to_string(),
                    });
                }
            }
        }
    }
}

/// Whether two paths are two spellings of one path.
///
/// The root and the prefix are part of the spelling and are compared as their own part, so a
/// relative path is never the absolute path that ends the same way, and a share is not a folder
/// chain. The verbatim forms of one path are one path (`\\?\C:\x` is `C:\x`, `\\?\UNC\server\share\x`
/// is `\\server\share\x`). A `.` component names the folder it sits in and is ignored; everything
/// else is compared component by component, as native name bytes, never through a lossy string, so
/// two names that differ only in invalid UTF-8 are two names.
///
/// Case is ignored only where this platform's disk ignores it — Windows and the default macOS
/// volume — because on Linux two names that differ in case are two files.
///
/// This is the question a journal asks about a path it recorded earlier, and the one a list of roots
/// asks before adding a root twice. It says nothing about where the two point: a link and its target
/// are different spellings of different targets, and callers that delete or overwrite must confirm
/// the identity of what they act on — see [`compare_folder`].
pub fn same_spelling(left: &Path, right: &Path) -> bool {
    let mut left = comparable_components(left).into_iter();
    let mut right = comparable_components(right).into_iter();
    loop {
        match (left.next(), right.next()) {
            (None, None) => return true,
            (Some(left), Some(right)) if spelling_eq(left, right) => {}
            _ => return false,
        }
    }
}

/// Whether two **canonical** paths are the same target: component by component, exactly, losslessly.
///
/// The Windows `\\?\` prefix that [`std::fs::canonicalize`] returns is not part of the target, so it
/// is dropped before comparing. Everything else must match exactly, byte for byte: on a
/// case-insensitive disk two spellings of one file resolve to one value, so a difference here means
/// two different things on disk.
pub fn same_target(left: &Path, right: &Path) -> bool {
    let mut left = comparable_components(left).into_iter();
    let mut right = comparable_components(right).into_iter();
    loop {
        match (left.next(), right.next()) {
            (None, None) => return true,
            (Some(left), Some(right)) => match (left, right) {
                (Component::Prefix(left), Component::Prefix(right)) => {
                    if !prefix_kind_eq(left.kind(), right.kind()).0 {
                        return false;
                    }
                }
                (Component::RootDir, Component::RootDir) => {}
                (Component::ParentDir, Component::ParentDir) => {}
                (Component::Normal(left), Component::Normal(right)) => {
                    if !name_bytes_eq(left, right) {
                        return false;
                    }
                }
                _ => return false,
            },
            _ => return false,
        }
    }
}

/// Whether this platform's filesystem compares names by case: Windows (NTFS) and the default macOS
/// volume do, Linux and the BSDs do not.
const FOLDS_CASE: bool = cfg!(windows) || cfg!(target_os = "macos");

/// The components a comparison walks.
///
/// A `.` component is not a name, so it is not part. A UNC prefix names its own root: Rust
/// emits an implicit `RootDir` after the ordinary `\\server\share` form and none after
/// `\\?\UNC\server\share`, so the root that follows a UNC prefix is folded into the prefix and
/// the two spellings of one share are one share. A drive-relative `C:x` keeps its missing root,
/// because there the root decides which file the name means.
fn comparable_components(path: &Path) -> Vec<Component<'_>> {
    let mut parts: Vec<Component<'_>> = Vec::new();
    let mut after_share = false;
    for component in path.components() {
        if matches!(component, Component::CurDir) {
            continue;
        }
        if after_share && matches!(component, Component::RootDir) {
            after_share = false;
            continue;
        }
        after_share = matches!(
            component,
            Component::Prefix(prefix)
                if matches!(
                    prefix.kind(),
                    std::path::Prefix::UNC(..) | std::path::Prefix::VerbatimUNC(..)
                )
        );
        parts.push(component);
    }
    parts
}

/// Whether two components are one component under the spelling rule.
fn spelling_eq(left: Component<'_>, right: Component<'_>) -> bool {
    match (left, right) {
        (Component::Prefix(left), Component::Prefix(right)) => {
            prefix_kind_eq(left.kind(), right.kind()).0
        }
        (Component::RootDir, Component::RootDir) => true,
        (Component::ParentDir, Component::ParentDir) => true,
        (Component::Normal(left), Component::Normal(right)) => name_eq(left, right),
        _ => false,
    }
}

/// Whether one name is the other under the spelling rule: exactly, or folded where the disk folds
/// case. A name that is not valid UTF-8 can only be compared exactly.
fn name_eq(left: &OsStr, right: &OsStr) -> bool {
    if name_bytes_eq(left, right) {
        return true;
    }
    if !FOLDS_CASE {
        return false;
    }
    match (left.to_str(), right.to_str()) {
        (Some(left), Some(right)) => fold_path_case(left) == fold_path_case(right),
        _ => false,
    }
}

/// Whether one name is the other, byte for byte (wide character for wide character on Windows). No
/// lossy conversion: a name that is not valid UTF-8 never meets another one this way unless it is
/// the same name.
fn name_bytes_eq(left: &OsStr, right: &OsStr) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        left.as_bytes() == right.as_bytes()
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        left.encode_wide().eq(right.encode_wide())
    }
    #[cfg(not(any(unix, windows)))]
    {
        left == right
    }
}

/// Whether two prefixes name the same root: a drive letter, a UNC server and share, or a device
/// namespace. Returns whether they are the same, and whether the comparison had to fold case (which
/// only a case-insensitive disk allows).
fn prefix_kind_eq(left: std::path::Prefix<'_>, right: std::path::Prefix<'_>) -> (bool, bool) {
    use std::path::Prefix;
    // The drive letter is compared without its case on every platform: `C:` and `c:` are one drive.
    match (left, right) {
        (Prefix::Disk(left), Prefix::Disk(right))
        | (Prefix::Disk(left), Prefix::VerbatimDisk(right))
        | (Prefix::VerbatimDisk(left), Prefix::Disk(right))
        | (Prefix::VerbatimDisk(left), Prefix::VerbatimDisk(right)) => {
            (left.eq_ignore_ascii_case(&right), true)
        }
        (Prefix::UNC(left_server, left_share), Prefix::UNC(right_server, right_share))
        | (Prefix::UNC(left_server, left_share), Prefix::VerbatimUNC(right_server, right_share))
        | (Prefix::VerbatimUNC(left_server, left_share), Prefix::UNC(right_server, right_share))
        | (
            Prefix::VerbatimUNC(left_server, left_share),
            Prefix::VerbatimUNC(right_server, right_share),
        ) => (
            name_eq(left_server, right_server) && name_eq(left_share, right_share),
            FOLDS_CASE,
        ),
        (Prefix::Verbatim(left), Prefix::Verbatim(right))
        | (Prefix::DeviceNS(left), Prefix::DeviceNS(right)) => (name_eq(left, right), FOLDS_CASE),
        _ => (false, false),
    }
}

/// Reserved DOS device names, per Microsoft's "Naming Files, Paths, and Namespaces"
/// (<https://learn.microsoft.com/en-us/windows/win32/fileio/naming-a-file>): `CON`, `PRN`, `AUX`,
/// `NUL`, `COM1`-`COM9`, `LPT1`-`LPT9`, and the superscript forms `COM¹`/`COM²`/`COM³` and
/// `LPT¹`/`LPT²`/`LPT³`. `COM0` and `LPT0` are refused too: the same page shows `COM0` as a device
/// symlink, and refusing a name Windows may route to a device is the safe direction.
///
/// Windows ignores trailing spaces in a name, including before the extension (`aux .txt` is
/// `AUX`), so they are trimmed before matching. The caller passes the part before the first dot.
pub fn is_windows_device_name(stem: &str) -> bool {
    let s = stem.trim_end_matches([' ', '.']).to_ascii_uppercase();
    matches!(
        s.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM0"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "COM¹"
            | "COM²"
            | "COM³"
            | "LPT0"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
            | "LPT¹"
            | "LPT²"
            | "LPT³"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lenient_rule_rejects_only_what_escapes_the_root() {
        for bad in [
            "/abs",
            "\\abs",
            "../x",
            "a/../b",
            "a\\..\\b",
            "C:/x",
            "D:\\x",
            "a\0b",
            "//server/s",
        ] {
            assert!(RelPath::new(bad).is_err(), "{bad:?} must be rejected");
        }

        assert_eq!(RelPath::new("").unwrap().as_str(), "");
        assert_eq!(
            RelPath::new("Data\\SKSE\\Plugins").unwrap().as_str(),
            "Data/SKSE/Plugins"
        );
        assert_eq!(RelPath::new("a..b/c").unwrap().as_str(), "a..b/c");
    }

    #[test]
    fn the_lenient_rule_keeps_the_values_stored_before_the_strict_rule() {
        // Every one of these was written by an Agora that had no strict rule. Loading them is what
        // "stored data keeps working" means; the strict rule judges them when they are used.
        for stored in ["mods/x.", "mods/x ", "mods/CON.jar", "Data/./x"] {
            let path = RelPath::new(stored).expect("a stored value still loads");
            assert!(
                path.check_strict().is_err(),
                "{stored:?} must not pass the strict rule"
            );
        }
    }

    #[test]
    fn a_stored_value_is_reported_by_name_rather_than_dropped() {
        // The value arrives through deserialization, which is lenient on purpose.
        let stored: RelPath = serde_json::from_str("\"mods/BuildCraft.\"").expect("it loads");
        let error = stored
            .check_strict()
            .expect_err("the strict rule refuses a trailing dot");
        assert_eq!(error.value, "mods/BuildCraft.");
        assert!(
            error.reason.contains("ends in"),
            "the reason names the rule: {}",
            error.reason
        );
    }

    #[test]
    fn the_strict_rule_refuses_on_every_platform() {
        for bad in [
            // Escapes and absolutes.
            "..",
            "../x",
            "a/../b",
            "/abs",
            "\\abs",
            "//server/share/x",
            "\\\\server\\share\\x",
            "C:/x",
            "C:x",
            "D:\\x",
            // The stream and drive separator.
            "a:b",
            "mods/x:stream",
            // Components Windows cannot create, or creates as something else.
            ".",
            "./x",
            "a/./b",
            "a//b",
            "a/",
            "mods/x.",
            "mods/x ",
            "mods/nul\0byte",
            "mods/tab\there",
            "mods/what?.txt",
            "mods/a*b.txt",
            "mods/quote\"d.txt",
            "mods/less<than.txt",
            "mods/greater>than.txt",
            "mods/pipe|x.txt",
            // Reserved device names, with and without an extension and in any case.
            "CON",
            "con",
            "mods/AUX",
            "mods/nul.jar",
            "mods/com1.txt",
            "mods/LPT9",
            "mods/COM¹.txt",
            "mods/aux .txt",
        ] {
            assert!(
                RelPath::strict(bad).is_err(),
                "{bad:?} must be refused by the strict rule"
            );
        }
    }

    #[test]
    fn the_strict_rule_reads_backslashes_as_separators_and_refuses_an_empty_path() {
        assert_eq!(
            RelPath::strict("Data\\SKSE\\Plugins").unwrap().as_str(),
            "Data/SKSE/Plugins"
        );
        assert!(
            RelPath::strict("").is_err(),
            "the empty path is the root, not a file ([`InstanceRoot`])"
        );
        assert!(
            RelPath::strict("mods\\..\\x").is_err(),
            "'..' is refused after the separators are read"
        );
    }

    #[test]
    fn the_strict_rule_accepts_names_every_platform_can_create() {
        for good in [
            "mods/a.jar",
            "Data/SKSE/Plugins/SKSE64.dll",
            "a..b/c",
            "mods/BuildCraft-1.2.3 (4).jar",
            "Data/café.txt",
            "saves/Ünïcode/world",
            "mods/COM10.txt",
            "mods/auxiliary.txt",
            "mods/conifer",
        ] {
            assert!(
                RelPath::strict(good).is_ok(),
                "{good:?} is a name every platform can create"
            );
        }
    }

    #[test]
    fn a_file_name_is_one_component() {
        assert_eq!(FileName::new("a.jar").unwrap().as_str(), "a.jar");
        assert_eq!(
            FileName::new("a.jar").unwrap().as_rel_path().as_str(),
            "a.jar"
        );
        for bad in [
            "mods/a.jar",
            "mods\\a.jar",
            "",
            ".",
            "..",
            "CON",
            "x.",
            "a\0b",
        ] {
            assert!(FileName::new(bad).is_err(), "{bad:?} is not one file name");
        }
    }

    #[test]
    fn a_path_set_that_a_case_insensitive_disk_cannot_hold_is_refused() {
        let paths = |names: &[&str]| -> Vec<StrictRelPath> {
            names
                .iter()
                .map(|name| RelPath::strict(*name).unwrap())
                .collect()
        };

        assert_eq!(
            validate_path_set(&paths(&["mods/A.txt", "mods/a.txt"])),
            Err(PathSetError::Collides {
                path: "mods/a.txt".to_string()
            })
        );
        assert_eq!(
            validate_path_set(&paths(&["mods", "mods/a.txt"])),
            Err(PathSetError::FolderIsAFile {
                path: "mods/a.txt".to_string(),
                prefix: "mods".to_string()
            })
        );
        assert_eq!(validate_path_set(&[]), Err(PathSetError::EmptySet));
        validate_path_set(&paths(&["mods/a.txt", "mods/A.jar", "config/x.toml"]))
            .expect("names that differ by more than case are distinct");
    }

    #[test]
    fn an_empty_path_is_the_root_rather_than_a_rel_path() {
        let root = InstanceRoot::new();
        assert_eq!(
            root.path_in(Path::new("/instances/one")),
            PathBuf::from("/instances/one")
        );
        // The compile-time half of this is the `compile_fail` doc test on `InstanceRoot`.
        assert!(RelPath::strict("").is_err());
    }

    #[test]
    fn one_spelling_ignores_a_dot_component_and_one_target_does_not() {
        // What holds on every platform: a `.` component names the folder it sits in, a trailing
        // separator adds nothing, and a different name is different.
        assert!(same_spelling(
            Path::new("Data/./Mods/A.jar"),
            Path::new("Data/Mods/A.jar")
        ));
        assert!(same_spelling(
            Path::new("Data/Mods/A.jar/"),
            Path::new("Data/Mods/A.jar")
        ));
        assert!(!same_spelling(
            Path::new("Data/Mods/A.jar"),
            Path::new("Data/Mods/B.jar")
        ));

        // One target: exactly, component by component, on every platform. A trailing separator and
        // a `.` component are not names, so they do not make it a different path; a different name
        // does.
        let left = Path::new("Data/Mods/A.jar");
        assert!(same_target(left, Path::new("Data/Mods/A.jar/")));
        assert!(same_target(left, Path::new("Data/./Mods/A.jar")));
        assert!(!same_target(left, Path::new("Data/Mods/B.jar")));
    }

    #[cfg(windows)]
    #[test]
    fn one_spelling_ignores_case_and_separators_and_one_target_does_not() {
        // Two spellings of one path: case, separators and a `.` component do not matter. Windows
        // only: a backslash separates names there and not on Unix, and its disk folds case.
        assert!(same_spelling(
            Path::new("Data\\Mods\\A.jar"),
            Path::new("data/mods/a.jar")
        ));
        assert!(same_spelling(
            Path::new("Data/./Mods/A.jar"),
            Path::new("Data/Mods/A.jar")
        ));
        assert!(same_spelling(
            Path::new("Data/Mods/A.jar/"),
            Path::new("Data/Mods/A.jar")
        ));
        assert!(!same_spelling(
            Path::new("Data/Mods/A.jar"),
            Path::new("Data/Mods/B.jar")
        ));

        // One target: exactly, component by component.
        let left = Path::new("Data/Mods/A.jar");
        assert!(same_target(left, Path::new("Data/Mods/A.jar")));
        assert!(
            !same_target(left, Path::new("data/mods/a.jar")),
            "exact comparison keeps a case difference, which on a case-sensitive disk is two files"
        );
    }

    #[test]
    fn two_paths_that_are_both_gone_are_compared_by_spelling() {
        // The spelling rule decides: Windows and macOS fold these two names into one, Linux does
        // not. Either way it is an answer, not a shrug, because both paths are genuinely absent.
        let folded = cfg!(windows) || cfg!(target_os = "macos");
        assert_eq!(
            compare_folder(Path::new("/gone/One/mods"), Path::new("/gone/one/MODS")),
            if folded {
                PathRelation::Same
            } else {
                PathRelation::Different
            }
        );
        assert_eq!(
            compare_folder(Path::new("/gone/One/mods"), Path::new("/gone/One/mods2")),
            PathRelation::Different
        );
    }

    #[test]
    fn a_path_that_exists_is_not_the_same_as_one_that_does_not() {
        assert_eq!(
            compare_folder(Path::new("."), Path::new("./a-folder-that-is-not-there")),
            PathRelation::Different
        );
    }

    #[test]
    fn a_spelling_keeps_its_root() {
        // A relative path is never the absolute path that ends the same way: the root is part of
        // the spelling, not something a comparison may drop.
        assert!(!same_spelling(
            Path::new("/home/user/mods"),
            Path::new("home/user/mods")
        ));
        assert!(!same_spelling(Path::new("mods"), Path::new("/mods")));
    }

    #[cfg(windows)]
    #[test]
    fn a_bare_share_root_names_the_same_folder_as_its_verbatim_form() {
        // The share itself, with no folder below it. Rust gives the ordinary form an implicit root
        // and the verbatim form none, so a comparison that walks components sees an extra one on
        // one side; the prefix and its root are one part, and it is the same part either way.
        for (plain, verbatim) in [
            (r"\\server\share", r"\\?\UNC\server\share"),
            (r"\\server\share\", r"\\?\UNC\server\share\"),
            (r"\\server\share", r"\\?\UNC\server\share\"),
        ] {
            assert!(
                same_spelling(Path::new(plain), Path::new(verbatim)),
                "{plain} and {verbatim} are one share"
            );
            assert!(
                same_target(Path::new(plain), Path::new(verbatim)),
                "{plain} and {verbatim} are one target"
            );
        }
        // The share is still not a folder in it, and two shares are two shares.
        assert!(!same_spelling(
            Path::new(r"\\server\share"),
            Path::new(r"\\server\share\mods")
        ));
        assert!(!same_target(
            Path::new(r"\\server\share"),
            Path::new(r"\\server\share\mods")
        ));
        assert!(!same_spelling(
            Path::new(r"\\server\share"),
            Path::new(r"\\server\other")
        ));
    }

    #[cfg(windows)]
    #[test]
    fn a_share_is_a_prefix_rather_than_a_folder_chain() {
        assert!(!same_spelling(
            Path::new(r"\\server\share\x"),
            Path::new("server/share/x")
        ));
        assert!(same_spelling(
            Path::new(r"\\server\share\x"),
            Path::new(r"\\SERVER\Share\X")
        ));
        // The verbatim and UNC spellings of one share are one share.
        assert!(same_spelling(
            Path::new(r"\\?\UNC\server\share\x"),
            Path::new(r"\\server\share\x")
        ));
        assert!(same_spelling(
            Path::new(r"\\?\C:\instances\one"),
            Path::new(r"c:/instances/one")
        ));
        // A drive-relative path is not the rooted one.
        assert!(!same_spelling(Path::new(r"C:x"), Path::new(r"C:\x")));
        assert!(!same_spelling(Path::new(r"C:\x"), Path::new(r"D:\x")));
    }

    #[test]
    fn case_is_folded_only_where_the_disk_folds_it() {
        // Windows and the default macOS volume compare names case-insensitively; Linux and the BSDs
        // do not, so two names that differ in case are two files there.
        let folded = cfg!(windows) || cfg!(target_os = "macos");
        assert_eq!(
            same_spelling(
                Path::new("/games/Mods/A.jar"),
                Path::new("/games/mods/a.jar")
            ),
            folded
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_name_that_is_not_utf8_is_not_collapsed_into_another() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        // Two names that differ only in a byte that is not UTF-8: a lossy spelling would make them
        // one name, which is the bug this comparison must not have.
        let one = Path::new(OsStr::from_bytes(b"/games/mod\xff"));
        let other = Path::new(OsStr::from_bytes(b"/games/mod\xfe"));
        assert!(!same_spelling(one, other));
        assert!(same_spelling(one, one));
    }

    #[cfg(windows)]
    #[test]
    fn the_verbatim_prefix_names_no_folder_of_its_own() {
        // What `std::fs::canonicalize` returns on Windows, and what a caller configured: one target.
        assert!(same_target(
            Path::new(r"\\?\C:\instances\one\mods\a.jar"),
            Path::new(r"C:\instances\one\mods\a.jar")
        ));
        assert!(same_spelling(
            Path::new(r"\\?\C:\instances\one\mods"),
            Path::new(r"c:/instances/one/mods")
        ));
    }
}
