//! Placement against the content layouts official packages actually ship, with file lists copied
//! from real archives, so a marker list edited in a package is checked against what players get.

use std::path::PathBuf;

use agora_game_api::{suggest_placement, ContentLayout, RelPath, Suggestion};

fn shipped_layout(package: &str, game: &str) -> ContentLayout {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/official")
        .join(package)
        .join("games/package.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{} could not be read: {e}", path.display()));
    let package: serde_json::Value = serde_json::from_str(&text).unwrap();
    let entry = package["games"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["id"] == game)
        .unwrap_or_else(|| panic!("{} declares no game {game}", path.display()));
    serde_json::from_value(entry["content_layout"].clone()).unwrap()
}

fn paths(list: &[&str]) -> Vec<RelPath> {
    list.iter().map(|p| RelPath::new(*p).unwrap()).collect()
}

/// PGPatcher 2.3.1's release zip (SHA-256 50e50cb6…, listed 2026-11-07): a `PGPatcher/` folder
/// and a loose ini at the top. The Skyrim package's `pgpatcher` tool expects
/// `Data/PGPatcher/PGPatcher.exe`, so the archive must land in `Data` without asking.
#[test]
fn pgpatcher_release_zip_goes_in_skyrims_data_folder() {
    let files = paths(&[
        "PG_YOU_CAN_DELETE_THIS_FILE.ini",
        "PGPatcher/PGPatcher.exe",
        "PGPatcher/pgtools.exe",
    ]);
    match suggest_placement(&files, &shipped_layout("skyrim-se", "skyrim-se")) {
        Suggestion::Place {
            source_path,
            mount_path,
            ..
        } => {
            assert_eq!(source_path.as_str(), "");
            assert_eq!(mount_path.as_str(), "Data");
        }
        other => panic!("expected PGPatcher to be placed in Data, got {other:?}"),
    }
}

/// Skyrim Together 1.8.4's release assets (https://github.com/tiltedphoques/TiltedEvolution/releases/tag/v1.8.4),
/// against the catalog entry's patterns, matched the way `catalog_install` picks an asset.
#[test]
fn skyrim_together_patterns_each_pick_one_build() {
    let assets = [
        "SkyrimTogether-1.8.4-windows-x64-debug-symbols.zip",
        "SkyrimTogether-1.8.4-windows-x64-sse1.6.1170-debug-symbols.zip",
        "SkyrimTogether-1.8.4-windows-x64-sse1.6.1170.zip",
        "SkyrimTogether-1.8.4-windows-x64.zip",
        "SkyrimTogetherServer-1.8.4-linux-amd64.tar.gz",
        "SkyrimTogetherServer-1.8.4-linux-arm64.tar.gz",
    ];
    let picks = |pattern: &str| -> Vec<&str> {
        let pattern = pattern.to_ascii_lowercase();
        assets
            .iter()
            .copied()
            .filter(|a| agora_game_api::glob_match(&pattern, &a.to_ascii_lowercase()))
            .collect()
    };
    assert_eq!(
        picks("SkyrimTogether-*-windows-x64.zip"),
        ["SkyrimTogether-1.8.4-windows-x64.zip"]
    );
    assert_eq!(
        picks("SkyrimTogether-*-windows-x64-sse1.6.1170.zip"),
        ["SkyrimTogether-1.8.4-windows-x64-sse1.6.1170.zip"]
    );
}
