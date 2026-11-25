//! The folders a game's snapshots cover, declared by its package (MASTER_SPEC §32.7).
//!
//! Core's snapshot code holds no game's folder list: it reads the declaration a package makes and
//! captures and restores exactly what it names. A game that declares nothing gets no automatic
//! snapshot rather than a guessed one — the caller is told, and the instance is left alone.

use crate::RelPath;
use serde::{Deserialize, Serialize};

/// The folders a snapshot of this game's instance captures, in the package's own words.
///
/// A declaration is a promise about what a snapshot owns: capture walks these folders, and a
/// restore replaces these and nothing else. Anything the instance holds outside them — a file at
/// the instance root, another game's folder — is not part of a snapshot and is never touched by a
/// restore of one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotScope {
    /// The folders a full snapshot captures.
    pub tracked: Vec<RelPath>,
    /// The folders the mandatory pre-launch snapshot captures. Absent means the same as
    /// `tracked`; a game whose world data changes every session declares a smaller set here so a
    /// launch does not re-hash it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prelaunch: Option<Vec<RelPath>>,
}

/// A declared folder list that cannot be used, and why. The value is a sentence naming the
/// offending folder, for the person who wrote the package.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the snapshot folder list is refused: {0}")]
pub struct SnapshotScopeError(pub String);

impl SnapshotScope {
    /// A declaration from folder names, checked before it is used.
    pub fn declared(
        tracked: &[&str],
        prelaunch: Option<&[&str]>,
    ) -> Result<Self, SnapshotScopeError> {
        let scope = Self {
            tracked: parse_folders("tracked", tracked)?,
            prelaunch: match prelaunch {
                Some(folders) => Some(parse_folders("prelaunch", folders)?),
                None => None,
            },
        };
        scope.validate()?;
        Ok(scope)
    }

    /// Check a declaration read from a package: every folder stays inside the instance and is a
    /// folder, not the instance root itself.
    ///
    /// `RelPath` already refuses `..`, an absolute path, a drive letter and a NUL byte when the
    /// value is read, so this covers the rest: the root, an empty component and a trailing
    /// separator.
    pub fn validate(&self) -> Result<(), SnapshotScopeError> {
        // Each list is checked on its own: the pre-launch set is expected to repeat folders the
        // tracked set holds, and only a folder twice in one list is a mistake.
        let mut seen = std::collections::BTreeSet::new();
        for folder in &self.tracked {
            validate_declared_folder(folder)?;
            if !seen.insert(folder.as_str().to_lowercase()) {
                return Err(SnapshotScopeError(format!("'{folder}' is declared twice")));
            }
        }
        seen.clear();
        for folder in self.prelaunch.iter().flatten() {
            validate_declared_folder(folder)?;
            if !seen.insert(folder.as_str().to_lowercase()) {
                return Err(SnapshotScopeError(format!(
                    "'{folder}' is declared twice in its pre-launch list"
                )));
            }
        }
        if self.tracked.is_empty() {
            return Err(SnapshotScopeError(
                "it names no folder, so a snapshot would capture nothing".into(),
            ));
        }
        if self
            .prelaunch
            .as_ref()
            .is_some_and(|prelaunch| prelaunch.is_empty())
        {
            return Err(SnapshotScopeError(
                "its pre-launch list names no folder; leave it out to use the tracked list".into(),
            ));
        }
        Ok(())
    }

    /// The folders a full snapshot captures.
    pub fn tracked_entries(&self) -> Vec<&str> {
        self.tracked.iter().map(|folder| folder.as_str()).collect()
    }

    /// The folders the mandatory pre-launch snapshot captures.
    pub fn prelaunch_entries(&self) -> Vec<&str> {
        match &self.prelaunch {
            Some(prelaunch) => prelaunch.iter().map(|folder| folder.as_str()).collect(),
            None => self.tracked_entries(),
        }
    }
}

/// One declared folder, checked: inside the instance, and a folder rather than its root.
pub fn validate_declared_folder(folder: &RelPath) -> Result<(), SnapshotScopeError> {
    let path = folder.as_str();
    if path.is_empty() {
        return Err(SnapshotScopeError(
            "the instance root is not a folder a snapshot can own".into(),
        ));
    }
    if path.split('/').any(|part| part.is_empty() || part == ".") {
        return Err(SnapshotScopeError(format!(
            "'{path}' is not a plain relative folder: it has an empty or '.' part"
        )));
    }
    // B1a: `StrictRelPath` replaces this once it lands, adding the rules `RelPath` leaves to new
    // writers — a component ending in a dot or space, a Windows reserved name, a control character.
    Ok(())
}

fn parse_folders(list: &str, folders: &[&str]) -> Result<Vec<RelPath>, SnapshotScopeError> {
    folders
        .iter()
        .map(|folder| {
            RelPath::new(*folder).map_err(|error| {
                SnapshotScopeError(format!(
                    "its {list} list names '{}', which is refused: {}",
                    error.value, error.reason
                ))
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_declaration_reads_back_the_folders_it_names() {
        let scope = SnapshotScope::declared(&["mods", "config"], Some(&["mods"]))
            .expect("a plain declaration is usable");
        assert_eq!(scope.tracked_entries(), vec!["mods", "config"]);
        assert_eq!(scope.prelaunch_entries(), vec!["mods"]);
    }

    #[test]
    fn a_declaration_without_a_prelaunch_list_uses_the_tracked_one() {
        let scope = SnapshotScope::declared(&["mods"], None).expect("usable");
        assert_eq!(scope.prelaunch_entries(), vec!["mods"]);
    }

    #[test]
    fn a_folder_that_leaves_the_instance_is_refused() {
        for listed in [
            "..",
            "../elsewhere",
            "D:/elsewhere",
            "/etc",
            "mods/../../elsewhere",
        ] {
            let refused = SnapshotScope::declared(&[listed], None);
            assert!(
                refused.is_err(),
                "'{listed}' must not be accepted as a snapshot folder"
            );
        }
    }

    #[test]
    fn the_instance_root_is_not_a_folder_a_snapshot_owns() {
        let refused = SnapshotScope::declared(&[""], None).expect_err("the root is refused");
        assert!(refused.0.contains("instance root"));
    }

    #[test]
    fn a_malformed_folder_spelling_is_refused() {
        for listed in ["mods/", "mods//config", "./mods"] {
            assert!(
                SnapshotScope::declared(&[listed], None).is_err(),
                "'{listed}' is not a plain relative folder"
            );
        }
    }

    #[test]
    fn a_folder_listed_twice_is_refused() {
        let refused = SnapshotScope::declared(&["mods", "Mods"], None)
            .expect_err("a folder that appears twice is a mistake");
        assert!(refused.0.contains("twice"));
    }

    #[test]
    fn an_empty_declaration_is_refused() {
        assert!(SnapshotScope::declared(&[], None).is_err());
        assert!(SnapshotScope::declared(&["mods"], Some(&[])).is_err());
    }
}
