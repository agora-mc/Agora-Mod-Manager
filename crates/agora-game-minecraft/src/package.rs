use agora_game_api::{
    DeploymentStrategy, DownloadAdd, DownloadRecognition, GameDefinition, GameId, GamePackage,
    PackageDefinition, VersionSource,
};
use std::sync::{Arc, OnceLock};

struct MinecraftPackage(PackageDefinition);

impl GamePackage for MinecraftPackage {
    fn definition(&self) -> &PackageDefinition {
        &self.0
    }

    fn recognize_download(
        &self,
        game: &GameId,
        file_name: &str,
        entries: &[String],
    ) -> Option<DownloadRecognition> {
        if !game.is_minecraft() {
            return None;
        }
        recognize_minecraft_download(file_name, entries)
    }
}

/// Recognise Minecraft content by what is inside the archive, never by its name alone.
/// `entries` are forward-slash paths in the archive; the file's extension says how to read them.
fn recognize_minecraft_download(
    file_name: &str,
    entries: &[String],
) -> Option<DownloadRecognition> {
    let name = file_name.to_ascii_lowercase();
    let entries = strip_single_wrapper(entries);
    let has = |wanted: &str| entries.iter().any(|entry| entry == wanted);

    if name.ends_with(".mrpack") && has("modrinth.index.json") {
        return Some(recognition(
            "modpack",
            "has modrinth.index.json",
            "modpacks",
        ));
    }

    if name.ends_with(".jar") {
        for (marker, label) in [
            ("fabric.mod.json", "has fabric.mod.json"),
            ("quilt.mod.json", "has quilt.mod.json"),
            (
                "meta-inf/neoforge.mods.toml",
                "has META-INF/neoforge.mods.toml",
            ),
            ("meta-inf/mods.toml", "has META-INF/mods.toml"),
            ("mcmod.info", "has mcmod.info"),
        ] {
            if has(marker) {
                return Some(recognition("mod", label, "mods"));
            }
        }
        return None;
    }

    if name.ends_with(".zip") {
        let has_pack_meta = has("pack.mcmeta");
        let has_assets = entries.iter().any(|entry| entry.starts_with("assets/"));
        let has_data = entries.iter().any(|entry| entry.starts_with("data/"));
        if has_pack_meta && has_assets {
            return Some(recognition(
                "resource pack",
                "has pack.mcmeta and assets/",
                "resourcepacks",
            ));
        }
        if has_pack_meta && has_data {
            return Some(recognition(
                "data pack",
                "has pack.mcmeta and data/",
                "datapacks",
            ));
        }
        if entries
            .iter()
            .any(|entry| entry == "shaders" || entry.starts_with("shaders/"))
        {
            return Some(recognition(
                "shader pack",
                "has a shaders/ folder at its root",
                "shaderpacks",
            ));
        }
    }

    None
}

fn recognition(kind: &str, reason: &str, folder: &str) -> DownloadRecognition {
    DownloadRecognition {
        kind: kind.to_string(),
        reason: reason.to_string(),
        add: DownloadAdd::File {
            folder: folder.to_string(),
        },
    }
}

/// Many archives put everything under one folder. Recognition looks at the content's own root,
/// so a single shared top-level folder is dropped. Nested paths keep their own shape.
fn strip_single_wrapper(entries: &[String]) -> Vec<String> {
    let normalized: Vec<String> = entries
        .iter()
        .map(|entry| entry.replace('\\', "/").to_ascii_lowercase())
        .collect();
    let Some(first) = normalized
        .first()
        .and_then(|entry| entry.split('/').next())
        .filter(|part| !part.is_empty())
        .map(str::to_string)
    else {
        return normalized;
    };
    // These folders are content roots, not a wrapper around it.
    if ["shaders", "assets", "data", "meta-inf"]
        .iter()
        .any(|root| first.eq_ignore_ascii_case(root))
    {
        return normalized;
    }
    let wrapped = normalized.iter().all(|entry| {
        entry
            .strip_prefix(&first)
            .is_some_and(|rest| rest.starts_with('/') && rest.len() > 1)
    });
    if wrapped {
        normalized
            .into_iter()
            .map(|entry| entry[first.len() + 1..].to_string())
            .collect()
    } else {
        normalized
    }
}

/// The compiled game package for Minecraft: Java Edition.
pub fn game_package() -> Arc<dyn GamePackage> {
    static PACKAGE: OnceLock<Arc<dyn GamePackage>> = OnceLock::new();
    PACKAGE
        .get_or_init(|| {
            let def = PackageDefinition {
                id: "agora.minecraft".to_string(),
                version: semver::Version::new(0, 1, 0),
                api_range: semver::VersionReq::parse(">=0.1, <0.2").unwrap(),
                parents: Vec::new(),
                game_templates: std::collections::BTreeMap::new(),
                behaviours: None,
                games: vec![GameDefinition {
                    mo2_game_name: None,
                    session: Default::default(),
                    id: GameId::minecraft(),
                    extends: None,
                    name: "Minecraft: Java Edition".to_string(),
                    stores: Vec::new(),
                    version_sources: vec![VersionSource::PackageBehaviour],
                    deployment: DeploymentStrategy::Redirect,
                    content_rules: Vec::new(),
                    framework_ids: Vec::new(),
                    tool_ids: Vec::new(),
                    launch: None,
                    log_paths: Vec::new(),
                    crash_paths: Vec::new(),
                    user_files: Vec::new(),
                    save_paths: Vec::new(),
                    linked_archive_patterns: Vec::new(),
                    declared_writes: Vec::new(),
                    excluded_paths: Vec::new(),
                    load_orders: Vec::new(),
                    runtime_files: Vec::new(),
                    save_location: Vec::new(),
                    launch_alternatives: Vec::new(),
                    content_layout: None,
                    copy_patterns: Vec::new(),
                }],
                frameworks: Vec::new(),
                tools: Vec::new(),
            };
            Arc::new(MinecraftPackage(def))
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn recognises_each_minecraft_content_kind() {
        let package = game_package();
        let game = GameId::minecraft();

        let fabric = package
            .recognize_download(
                &game,
                "anything.jar",
                &entries(&["fabric.mod.json", "com/example/Mod.class"]),
            )
            .expect("a Fabric jar is a mod");
        assert_eq!(fabric.kind, "mod");
        assert_eq!(fabric.reason, "has fabric.mod.json");
        assert_eq!(
            fabric.add,
            DownloadAdd::File {
                folder: "mods".into()
            }
        );

        let resource = package
            .recognize_download(
                &game,
                "pack.zip",
                &entries(&["pack.mcmeta", "assets/minecraft/lang/en_us.json"]),
            )
            .expect("a pack.mcmeta zip with assets is a resource pack");
        assert_eq!(resource.kind, "resource pack");

        let data = package
            .recognize_download(
                &game,
                "pack.zip",
                &entries(&["pack.mcmeta", "data/example/loot_table/test.json"]),
            )
            .expect("a pack.mcmeta zip with data is a data pack");
        assert_eq!(data.kind, "data pack");

        let shader = package
            .recognize_download(
                &game,
                "shader.zip",
                &entries(&["shaders/final.fsh", "shaders/final.vsh"]),
            )
            .expect("a zip with shaders/ is a shader pack");
        assert_eq!(shader.kind, "shader pack");

        let pack = package
            .recognize_download(
                &game,
                "pack.mrpack",
                &entries(&["modrinth.index.json", "overrides/config/x.toml"]),
            )
            .expect("an mrpack with modrinth.index.json is a modpack");
        assert_eq!(pack.kind, "modpack");
    }

    #[test]
    fn recognises_a_wrapped_archive_by_its_inner_root() {
        let package = game_package();
        let found = package
            .recognize_download(
                &GameId::minecraft(),
                "wrapped.zip",
                &entries(&["MyPack/pack.mcmeta", "MyPack/assets/x/y.png"]),
            )
            .expect("the single wrapper folder is not part of the content's root");
        assert_eq!(found.kind, "resource pack");
    }

    #[test]
    fn ignores_an_unrelated_zip_and_does_not_read_the_name() {
        let package = game_package();
        assert!(package
            .recognize_download(
                &GameId::minecraft(),
                "photos.zip",
                &entries(&["holiday/beach.jpg", "holiday/menu.txt"]),
            )
            .is_none());
        // A name that looks like a mod is not enough.
        assert!(package
            .recognize_download(
                &GameId::minecraft(),
                "Definitely-A-Mod.jar",
                &entries(&["readme.txt"]),
            )
            .is_none());
    }

    // Only a `shaders/` folder at the archive root is a shader pack; a game project's
    // `src/shaders/` is not.
    #[test]
    fn a_zip_with_a_deep_shaders_folder_is_not_a_shader_pack() {
        let package = game_package();
        let found = package.recognize_download(
            &GameId::minecraft(),
            "my-game-project.zip",
            &entries(&[
                "proj/src/shaders/a.glsl",
                "proj/src/main.c",
                "proj/README.md",
            ]),
        );
        assert!(found.is_none(), "recognised as {:?}", found.map(|f| f.kind));
    }

    #[test]
    fn a_different_game_is_not_minecraft_content() {
        let package = game_package();
        let other = GameId::new("skyrim-se").unwrap();
        assert!(package
            .recognize_download(&other, "mod.jar", &entries(&["fabric.mod.json"]))
            .is_none());
    }

    #[test]
    fn minecraft_package_conforms_to_contract() {
        let pkg = game_package();
        let def = pkg.definition();
        assert_eq!(def.id, "agora.minecraft");
        assert_eq!(def.games.len(), 1);
        let game = &def.games[0];
        assert_eq!(game.id, GameId::minecraft());
        assert_eq!(game.name, "Minecraft: Java Edition");
        assert!(game.stores.is_empty());
        assert_eq!(game.version_sources, vec![VersionSource::PackageBehaviour]);
        assert_eq!(game.deployment, DeploymentStrategy::Redirect);
        assert!(game.launch.is_none());
    }
}
