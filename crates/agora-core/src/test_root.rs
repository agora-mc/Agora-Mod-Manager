//! A folder that a unit test creates under the system temp directory, removed when the test ends.
//!
//! Tests that only use `std::env::temp_dir().join(..)` leave their folders behind unless they
//! remove them by hand, and a test that fails part way never gets to. Holding one of these keeps the
//! folder for the test's life, and its drop removes the whole tree.

use std::ops::Deref;
use std::path::{Path, PathBuf};

use crate::content_store::protect;

/// A folder removed (with everything in it) when this value drops.
#[derive(Debug)]
pub(crate) struct RemoveOnDrop(PathBuf);

impl RemoveOnDrop {
    /// Take ownership of `path`. The folder is not created here.
    pub(crate) fn new(path: PathBuf) -> Self {
        Self(path)
    }
}

impl Deref for RemoveOnDrop {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for RemoveOnDrop {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        unprotect_objects(&self.0);
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Clear the content store's protection from every object under `root`, at any
/// depth, before a recursive delete. Uses the same production `protect::unprotect`
/// the content store calls when it removes an object. Links are not followed: a
/// link back into the tree would make the walk loop.
fn unprotect_objects(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if !kind.is_dir() || kind.is_symlink() {
            continue;
        }
        let path = entry.path();
        let is_objects = path.file_name().is_some_and(|name| name == "objects")
            && path
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == "content");
        if is_objects {
            let Ok(shards) = std::fs::read_dir(&path) else {
                continue;
            };
            for shard in shards.flatten() {
                let Ok(objects) = std::fs::read_dir(shard.path()) else {
                    continue;
                };
                for object in objects.flatten() {
                    let _ = protect::unprotect(&object.path());
                }
            }
        } else {
            unprotect_objects(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A content object is protected (a deny-delete ACE on Windows, read-only on
    /// Unix). Dropping the root must still remove the whole tree, or every
    /// content-store unit test leaves a folder behind in the system temp.
    #[test]
    fn dropping_a_root_with_a_protected_object_leaves_no_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.keep();
        let object = root.join("app_data/content/objects/aa/object");
        std::fs::create_dir_all(object.parent().unwrap()).unwrap();
        std::fs::write(&object, b"stored bytes").unwrap();
        protect::protect(&object).unwrap();
        assert_eq!(
            protect::protection(&object).unwrap(),
            protect::Protection::Protected
        );

        let path = root.clone();
        drop(RemoveOnDrop::new(root));
        assert!(
            !path.exists(),
            "a protected object kept the temp root behind"
        );
    }
}
