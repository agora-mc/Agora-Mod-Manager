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
use std::borrow::Cow;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::game::fold_path_case;

/// The Windows attribute bit that marks a reparse point: a symbolic link, a junction or another
/// filesystem redirection. `FILE_ATTRIBUTE_REPARSE_POINT`.
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
    /// root, which is [`InstanceRoot`].
    pub fn strict(value: impl Into<String>) -> Result<Self, RelPathError> {
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
        Ok(Self(normalized))
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
    pub fn check_strict(&self) -> Result<(), RelPathError> {
        Self::strict(self.0.clone()).map(|_| ())
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
        Ok(Self(path.0))
    }

    /// The name, as it will be written.
    pub fn as_str(&self) -> &str {
        &self.0
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
pub fn validate_path_set(paths: &[RelPath]) -> Result<(), PathSetError> {
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
    /// The path could not be resolved to a value that could then be checked and written.
    #[error("cannot resolve '{path}': {reason}")]
    Unresolvable { path: String, reason: String },
}

/// Join a relative path under a root, for a caller about to write there.
///
/// Two things this checks and a plain `root.join(rel)` does not:
///
/// * the relative path is re-checked against the **strict** rule, so a stored value that predates it
///   (a trailing dot, a reserved name) is reported here rather than written as a different file;
/// * nothing on the way is a link or junction. A junction is not a symbolic link to
///   [`std::fs::symlink_metadata`]'s `file_type().is_symlink()` on Windows, so this asks the
///   attribute Windows sets for both.
///
/// A component that does not exist ends the walk: nothing below it can exist either.
pub fn join_under(root: &Path, rel: &RelPath) -> Result<PathBuf, PathError> {
    rel.check_strict().map_err(|error| PathError::UnsafePath {
        path: error.value.clone(),
        reason: error.reason.clone(),
    })?;
    let mut joined = root.to_path_buf();
    let mut walked = String::new();
    let mut exists = true;
    for part in rel.as_str().split('/') {
        joined.push(part);
        if !walked.is_empty() {
            walked.push('/');
        }
        walked.push_str(part);
        if !exists {
            // Nothing below a component that does not exist can exist either.
            continue;
        }
        match std::fs::symlink_metadata(&joined) {
            Ok(metadata) if is_link(&metadata) => {
                return Err(PathError::LinkOnTheWay {
                    path: walked,
                    link: joined.display().to_string(),
                })
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => exists = false,
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

/// Whether this metadata describes a link: a symbolic link, or a Windows junction or other reparse
/// point. Windows reports a junction as a directory whose attributes carry a reparse point, so
/// `file_type().is_symlink()` alone misses it.
pub fn is_link(metadata: &std::fs::Metadata) -> bool {
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

/// Resolve a path once, to the exact value to check and then write.
///
/// The longest part of the path that exists is canonicalized — so every symbolic link and junction
/// in it is followed now, once — and the rest is appended as it was written. A `..` is refused
/// rather than resolved, because the two platforms resolve it differently when a folder on the way
/// does not exist: Windows collapses it lexically, Unix fails. The same path would then name two
/// different targets, which is the one thing one path may not do.
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
                let mut resolved = real;
                resolved.extend(tail.iter().rev());
                return Ok(resolved);
            }
            Err(error) => {
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

/// Whether two paths are two spellings of one path: case ignored, `/` and `\` ignored, `.` ignored.
///
/// This is the question a journal asks about a path it recorded earlier, and the one a list of
/// roots asks before adding a root twice. It says nothing about where the two point: a link and its
/// target are different spellings of different targets, and callers that delete or overwrite must
/// still confirm the identity of what they act on.
///
/// A path that is not valid UTF-8 is compared through its lossy spelling, so two such paths that
/// differ only in the invalid bytes compare equal. That errs toward "the same", which only costs a
/// refusal or a skipped duplicate; [`same_target`] is the lossless one.
pub fn same_spelling(left: &Path, right: &Path) -> bool {
    spelling_parts(left) == spelling_parts(right)
}

fn spelling_parts(path: &Path) -> Vec<String> {
    let text = strip_verbatim(path).to_string_lossy().into_owned();
    text.split(['/', '\\'])
        .filter(|part| !part.is_empty() && *part != ".")
        .map(fold_path_case)
        .collect()
}

/// Whether two **canonical** paths are the same target: component by component, exactly, losslessly.
///
/// The Windows `\\?\` prefix that [`std::fs::canonicalize`] returns is not part of the target, so it
/// is dropped before comparing. Everything else must match exactly, byte for byte: on a
/// case-insensitive disk two spellings of one file resolve to one value, so a difference here means
/// two different things on disk.
pub fn same_target(left: &Path, right: &Path) -> bool {
    let (left, right) = (strip_verbatim(left), strip_verbatim(right));
    let mut left = left.components();
    let mut right = right.components();
    loop {
        match (left.next(), right.next()) {
            (None, None) => return true,
            (Some(left), Some(right)) if left.as_os_str() == right.as_os_str() => {}
            _ => return false,
        }
    }
}

/// A path without the Windows `\\?\` prefix, which names no folder of its own.
///
/// Stripped on every platform, so a path a Windows journal recorded compares the same everywhere.
/// The `\\?\UNC\server\share` form is not rebuilt into `\\server\share`: it keeps its own spelling,
/// which only costs a "not the same" answer on a UNC path that arrives both ways.
#[cfg(unix)]
fn strip_verbatim(path: &Path) -> Cow<'_, Path> {
    use std::os::unix::ffi::OsStrExt;
    const VERBATIM: &[u8] = br"\\?\";
    match path.as_os_str().as_bytes().strip_prefix(VERBATIM) {
        Some(rest) => Cow::Owned(PathBuf::from(std::ffi::OsStr::from_bytes(rest))),
        None => Cow::Borrowed(path),
    }
}

#[cfg(windows)]
fn strip_verbatim(path: &Path) -> Cow<'_, Path> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    // `\\?\` as UTF-16: backslash, backslash, question mark, backslash.
    const VERBATIM: &[u16] = &[0x5C, 0x5C, 0x3F, 0x5C];
    let units: Vec<u16> = path.as_os_str().encode_wide().collect();
    match units.strip_prefix(VERBATIM) {
        Some(rest) => Cow::Owned(PathBuf::from(std::ffi::OsString::from_wide(rest))),
        None => Cow::Borrowed(path),
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
        let paths = |names: &[&str]| -> Vec<RelPath> {
            names
                .iter()
                .map(|name| RelPath::new(*name).unwrap())
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
    fn one_spelling_ignores_case_and_separators_and_one_target_does_not() {
        // Two spellings of one path: case, separators and a `.` component do not matter.
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
