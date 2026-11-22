//! Fail-closed path probes (MASTER_SPEC §32.13).
//!
//! Most late review findings were one class of bug: an error read as "nothing there". `Path::exists`
//! is the usual cause. It answers one `bool`, so a permission denial, a Windows sharing violation
//! and a path that runs through a regular file all look exactly like a missing path, and the caller
//! that sees `false` concludes nothing is there.
//!
//! [`probe`] keeps the three answers apart:
//!
//! - [`Probe::Present`] — something is at the path, and the kind says what it is;
//! - [`Probe::Absent`] — the operating system said, and only said, "not found";
//! - [`Probe::CantTell`] — anything else went wrong, so nothing is known.
//!
//! `Absent` is the only answer that may be read as "there is nothing there", and even that is not
//! permission to create one: see below.
//!
//! # A probe never authorizes a write
//!
//! A probe answers about the moment it ran; the path can be something else by the time a `write`,
//! `remove` or `rename` reaches it. So a probe result is not a grant:
//!
//! - a mutation resolves its destination and keeps that identity (MASTER_SPEC §32.3's
//!   `resolve_destination`), rather than probing a path and then writing the path;
//! - `Absent` does not mean "safe to create", and `Present` does not mean "safe to replace";
//! - `CantTell` never permits a delete or an overwrite, and the caller must report it instead.
//!
//! # What is never followed
//!
//! The probe reads the link itself, never its target ([`std::fs::symlink_metadata`]). A dangling
//! link is `Present` with [`PresentKind::Symlink`], not `Absent`: replacing it would replace the
//! link, and following it would act on a path nobody validated.
//!
//! # One platform difference, and why it is safe
//!
//! Unix reports a path that runs through a regular file as `ENOTDIR`, so it is `CantTell`. Windows
//! reports that case as `ERROR_PATH_NOT_FOUND`, the same answer it gives for a missing directory,
//! so it is `Absent` there. Both are honest answers to "is there something at this path?" — a
//! regular file where a directory was expected has nothing below it — and neither authorizes a
//! write, which is why the difference is recorded here instead of being papered over.

use std::fs::Metadata;
use std::io;
use std::path::Path;

/// What a path probe found.
#[derive(Debug)]
pub enum Probe {
    /// Something is at the path.
    Present(PresentKind),
    /// The path was not found. Only this answer means "there is nothing there", and it still does
    /// not authorize a write.
    Absent,
    /// The path could not be examined, so whether anything is there is unknown.
    CantTell(io::Error),
}

/// What is at a present path. A link is reported as a link, never as what it points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentKind {
    File,
    Directory,
    Symlink,
    /// A socket, named pipe, device or anything else the operating system reports.
    Other,
}

impl Probe {
    /// Whether the path was absent. This is the only answer that may be read as "nothing there",
    /// and it is still not permission to create, replace or delete anything at the path.
    pub fn is_absent(&self) -> bool {
        matches!(self, Probe::Absent)
    }

    /// What is at the path, when anything is.
    pub fn kind(&self) -> Option<PresentKind> {
        match self {
            Probe::Present(kind) => Some(*kind),
            Probe::Absent | Probe::CantTell(_) => None,
        }
    }
}

/// Examine `path` without following a link and without turning an error into "not there".
pub fn probe(path: impl AsRef<Path>) -> Probe {
    match std::fs::symlink_metadata(path.as_ref()) {
        Ok(metadata) => Probe::Present(kind_of(&metadata)),
        // The one answer that means the path was not found. Everything else is unknown, not
        // empty: a permission denial, a sharing violation, or a path through a regular file.
        Err(error) if error.kind() == io::ErrorKind::NotFound => Probe::Absent,
        Err(error) => Probe::CantTell(error),
    }
}

fn kind_of(metadata: &Metadata) -> PresentKind {
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        PresentKind::Symlink
    } else if file_type.is_dir() {
        PresentKind::Directory
    } else if file_type.is_file() {
        PresentKind::File
    } else {
        PresentKind::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A directory of this test's own, removed when it goes out of scope. `agora-game-api` depends
    /// only on the contract's own libraries, so the tests build the smallest thing that gives a
    /// unique path per test process and test.
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp() -> TempRoot {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "agora-game-api-probe-test-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create a temp directory");
        TempRoot(path)
    }

    /// A test can only tell one kind from another when one exists; a platform that cannot create
    /// a link at all says so instead of passing silently.
    fn make_symlink(target: &Path, link: &Path) -> io::Result<()> {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link)
        }
        #[cfg(windows)]
        {
            // Windows has separate calls for a link to a directory and a link to a file, and the
            // account may not be allowed to create either.
            match std::fs::metadata(target) {
                Ok(metadata) if metadata.is_dir() => {
                    std::os::windows::fs::symlink_dir(target, link)
                }
                _ => std::os::windows::fs::symlink_file(target, link),
            }
        }
    }

    fn symlink_or_skip(target: &Path, link: &Path) -> bool {
        match make_symlink(target, link) {
            Ok(()) => true,
            Err(error) => {
                println!(
                    "skipping the symlink probe test: this account cannot create links ({error})"
                );
                false
            }
        }
    }

    #[test]
    fn a_present_file_is_present() {
        let dir = temp();
        let file = dir.path().join("manifest.json");
        std::fs::write(&file, b"{}").unwrap();
        assert!(matches!(probe(&file), Probe::Present(PresentKind::File)));
    }

    #[test]
    fn a_directory_is_present() {
        let dir = temp();
        assert!(matches!(
            probe(dir.path()),
            Probe::Present(PresentKind::Directory)
        ));
    }

    #[test]
    fn a_missing_path_is_absent() {
        let dir = temp();
        assert!(probe(dir.path().join("nothing-here.json")).is_absent());
    }

    /// A link is the thing at the path. Following it would report the target, and a dangling link
    /// would read as absent — which would let a caller overwrite the link.
    #[test]
    fn a_symlink_is_present_as_a_link_and_never_as_its_target() {
        let dir = temp();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let link = dir.path().join("link");
        if !symlink_or_skip(&target, &link) {
            return;
        }
        assert!(matches!(probe(&link), Probe::Present(PresentKind::Symlink)));

        let dangling = dir.path().join("dangling");
        if !symlink_or_skip(&dir.path().join("gone"), &dangling) {
            return;
        }
        assert!(
            matches!(probe(&dangling), Probe::Present(PresentKind::Symlink)),
            "a dangling link is still a link, not an absent path"
        );
    }

    /// Reaching a path through a regular file is an error the operating system reports, and it is
    /// not "not found". Windows reports this case as `ERROR_PATH_NOT_FOUND`, the same answer it
    /// gives for a missing directory, and both mean nothing is at the path there, so the case is
    /// Unix's.
    #[cfg(unix)]
    #[test]
    fn a_path_through_a_regular_file_is_cant_tell() {
        let dir = temp();
        let file = dir.path().join("plain");
        std::fs::write(&file, b"x").unwrap();
        match probe(file.join("child")) {
            Probe::CantTell(_) => {}
            other => panic!("expected can't-tell, got {other:?}"),
        }
    }

    /// A name the operating system refuses is not a missing path: nothing was found out.
    #[cfg(windows)]
    #[test]
    fn a_name_windows_refuses_is_cant_tell() {
        let dir = temp();
        match probe(dir.path().join("bad<name")) {
            Probe::CantTell(_) => {}
            other => panic!("expected can't-tell, got {other:?}"),
        }
    }

    /// An unreadable path is the case the class of bug got wrong. A non-root user cannot traverse a
    /// directory with no permissions; root can, so the test says it skipped instead of passing.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_path_is_cant_tell() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        let file = locked.join("inside.json");
        std::fs::write(&file, b"{}").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

        let answer = probe(&file);
        // Restore before asserting, so a failure does not leave an unremovable directory behind.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        match answer {
            Probe::CantTell(_) => {}
            Probe::Present(_) => println!(
                "skipping the unreadable-path probe test: this account can read a 0o000 directory \
                 (running as root)"
            ),
            Probe::Absent => panic!("an unreadable path was read as absent"),
        }
    }

    /// A device is present, and is neither a file nor a directory.
    #[cfg(unix)]
    #[test]
    fn a_device_is_present_but_not_a_file() {
        assert!(matches!(
            probe("/dev/null"),
            Probe::Present(PresentKind::Other)
        ));
    }
}
