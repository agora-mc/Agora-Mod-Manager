//! Where an installed file came from: one list for every writer and every reader.
//!
//! An instance's manifest records each file's source as text (`InstalledMod::source` in agora-core), and each
//! writer used to spell its own: `registry`, `modrinth_raw`, `modrinth-pack`, `imported_prism`,
//! `manual`, `manual_drag_drop`... with three readers each decoding them in its own way. This is the
//! one list (owner, 2026-11-28). Writers store [`ContentSource::stored`]; readers go through
//! [`ContentSource::parse`], which also reads every spelling written before it existed, so no
//! manifest needs rewriting.

/// Where an installed file came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentSource {
    /// A catalog entry, whatever its download type (GitHub release, Modrinth project, direct link,
    /// browser handoff); the type is recorded beside it, in `source_strategy`.
    Catalog,
    /// A Modrinth project outside the catalog.
    Modrinth,
    /// A content provider plugin (its id is in `provider`).
    Provider,
    /// A file a modpack brought (a `.mrpack` import or a pack update).
    Pack,
    /// A file an import from another launcher brought, named by that launcher (`prism`, ...).
    LauncherImport(String),
    /// A file Agora cannot tie to any source: added by hand, dragged in, or picked up as it was.
    Local,
    /// A spelling this list does not know, kept exactly as it was stored.
    Other(String),
}

impl ContentSource {
    /// Read a stored source, in any spelling Agora has written.
    pub fn parse(stored: &str) -> Self {
        let trimmed = stored.trim();
        let normalized = trimmed.to_ascii_lowercase().replace([' ', '-'], "_");
        match normalized.as_str() {
            "registry" | "curated" => ContentSource::Catalog,
            "modrinth_raw" | "modrinth" => ContentSource::Modrinth,
            "provider" => ContentSource::Provider,
            "modrinth_pack" => ContentSource::Pack,
            "local" | "manual" | "manual_drag_drop" => ContentSource::Local,
            other => match other.strip_prefix("imported_") {
                Some(launcher) if !launcher.is_empty() => {
                    ContentSource::LauncherImport(launcher.to_string())
                }
                _ => ContentSource::Other(trimmed.to_string()),
            },
        }
    }

    /// The text a manifest stores for it. These are the spellings already on disk, so a file
    /// written now reads the same to an older Agora.
    pub fn stored(&self) -> String {
        match self {
            ContentSource::Catalog => "registry".into(),
            ContentSource::Modrinth => "modrinth_raw".into(),
            ContentSource::Provider => "provider".into(),
            ContentSource::Pack => "modrinth-pack".into(),
            ContentSource::LauncherImport(launcher) => format!("imported_{launcher}"),
            ContentSource::Local => "local".into(),
            ContentSource::Other(stored) => stored.clone(),
        }
    }

    /// Whether the file came with a pack (a modpack or another launcher's instance), the reading
    /// that heals `pack_managed` on manifests written before that flag existed.
    pub fn came_with_a_pack(&self) -> bool {
        matches!(self, ContentSource::Pack | ContentSource::LauncherImport(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_spelling_agora_has_written_reads_as_its_source() {
        for (stored, source) in [
            ("registry", ContentSource::Catalog),
            ("curated", ContentSource::Catalog),
            ("modrinth_raw", ContentSource::Modrinth),
            ("modrinth", ContentSource::Modrinth),
            ("provider", ContentSource::Provider),
            ("modrinth-pack", ContentSource::Pack),
            ("Modrinth Pack", ContentSource::Pack),
            (
                "imported_prism",
                ContentSource::LauncherImport("prism".into()),
            ),
            ("local", ContentSource::Local),
            ("manual", ContentSource::Local),
            ("manual_drag_drop", ContentSource::Local),
        ] {
            assert_eq!(ContentSource::parse(stored), source, "{stored}");
        }
    }

    #[test]
    fn what_is_stored_reads_back_as_the_same_source() {
        for source in [
            ContentSource::Catalog,
            ContentSource::Modrinth,
            ContentSource::Provider,
            ContentSource::Pack,
            ContentSource::LauncherImport("curseforge".into()),
            ContentSource::Local,
            ContentSource::Other("github_release".into()),
        ] {
            assert_eq!(ContentSource::parse(&source.stored()), source);
        }
    }

    #[test]
    fn an_unknown_spelling_is_kept_not_guessed() {
        assert_eq!(
            ContentSource::parse(" Something New "),
            ContentSource::Other("Something New".into())
        );
        assert_eq!(
            ContentSource::parse("imported_"),
            ContentSource::Other("imported_".into())
        );
        assert_eq!(
            ContentSource::parse(""),
            ContentSource::Other(String::new())
        );
    }

    #[test]
    fn only_pack_and_launcher_files_came_with_a_pack() {
        assert!(ContentSource::parse("modrinth-pack").came_with_a_pack());
        assert!(ContentSource::parse("imported_prism").came_with_a_pack());
        for stored in ["registry", "modrinth_raw", "local", "provider", "other"] {
            assert!(!ContentSource::parse(stored).came_with_a_pack(), "{stored}");
        }
    }
}
