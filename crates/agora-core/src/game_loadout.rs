//! Loadout profiles for game instances (MASTER_SPEC §26.10).
//!
//! A Minecraft loadout records which content is on and restores that set with an
//! undo snapshot (`crate::loadout`). A game instance keeps more of its setup in
//! more places: content layers in the instance manifest, plugin lists and INI
//! copies under the instance folder, load-after rules in the database, and its
//! save choice in the manifest. This module records all of it in the same
//! `.agora_loadouts` store the Minecraft profiles use, so the CLI and the
//! desktop show one kind of loadout, not two.
//!
//! Core holds no game knowledge here: a game's plugin lists, INI files and save
//! rule all come from its [`GameDefinition`], and a game without them records
//! no such section.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use agora_game_api::{GameDefinition, LayerSource};
use serde::{Deserialize, Serialize};

use crate::ctx::Ctx;
use crate::game_instance::{self, SavesChoice};
use crate::loadout::{
    self, LoadoutApplyError, LoadoutChange, LoadoutChangeKind, LoadoutProfile, LoadoutUpdate,
};
use crate::lock_manager::LockResource;

/// One content layer's enabled state, by layer id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameLoadoutContent {
    pub item: String,
    pub enabled: bool,
}

/// One plugin list's file, as it was. `None` means the instance had no copy of
/// the list when the profile was saved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameLoadoutPluginList {
    pub list: String,
    /// The bytes of the instance's copy of the list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contents: Option<Vec<u8>>,
}

/// One INI file's instance copy. `None` means the instance used the game's
/// shared file when the profile was saved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameLoadoutIni {
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contents: Option<Vec<u8>>,
}

/// One list's load-after rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameLoadoutRules {
    pub list: String,
    pub rules: Vec<crate::game_load_order::LoadAfterRule>,
}

/// The part of a game instance's setup a loadout records.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameLoadout {
    #[serde(default)]
    pub content: Vec<GameLoadoutContent>,
    #[serde(default)]
    pub plugin_lists: Vec<GameLoadoutPluginList>,
    #[serde(default)]
    pub inis: Vec<GameLoadoutIni>,
    #[serde(default)]
    pub rules: Vec<GameLoadoutRules>,
    #[serde(default)]
    pub saves: SavesChoice,
}

/// The state an apply replaced, kept so the change can be undone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameUndo {
    pub profile: String,
    pub state: GameLoadout,
}

fn instance_dir(ctx: &Ctx, instance_id: &str) -> Result<PathBuf, String> {
    ctx.paths
        .instance_dir(instance_id)
        .map_err(|e| e.to_string())
}

/// The undo point lives in its own folder, because `list_profiles` reads every
/// `.json` file directly in the loadouts folder as a profile.
fn undo_path(instance_dir: &std::path::Path) -> PathBuf {
    loadout::loadouts_dir(instance_dir)
        .join("undo")
        .join("last.json")
}

fn read_optional(path: &std::path::Path) -> Result<Option<Vec<u8>>, String> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("Cannot read {}: {e}", path.display())),
    }
}

fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Cannot create {}: {e}", parent.display()))?;
    }
    crate::snapshot::atomic_write_pub(path, bytes)
}

/// Record the setup of `instance_id` as it is now.
pub fn capture(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
) -> Result<GameLoadout, String> {
    let manifest = game_instance::get_manifest(ctx, instance_id).map_err(|e| e.to_string())?;
    let dir = instance_dir(ctx, instance_id)?;

    let mut content = Vec::new();
    for layer in manifest.layers.layers() {
        if matches!(
            layer.source,
            LayerSource::Content { .. } | LayerSource::InstanceContent { .. }
        ) {
            content.push(GameLoadoutContent {
                item: layer.id.as_str().to_string(),
                enabled: layer.enabled,
            });
        }
    }

    let mut plugin_lists = Vec::new();
    for rule in &definition.load_orders {
        let contents = read_optional(&dir.join(rule.file.as_str()))?;
        plugin_lists.push(GameLoadoutPluginList {
            list: rule.id.clone(),
            contents,
        });
    }

    // Only the instance's own copies are part of a profile. The game's shared file is where a
    // copy is seeded from, not something an apply restores, so capturing never needs the user's
    // Documents folder and a machine without one still gets a complete profile.
    let store = crate::game_ini::store_of(ctx, instance_id).map_err(|e| e.to_string())?;
    let mut inis = Vec::new();
    for mapping in definition
        .user_files
        .iter()
        .filter(|m| m.applies_to_store(&store))
    {
        let instance_path = mapping.instance_path.as_str().to_string();
        let contents = read_optional(&crate::game_ini::join_rel(&dir, &instance_path))?;
        inis.push(GameLoadoutIni {
            file: instance_path,
            contents,
        });
    }

    let mut rules = Vec::new();
    for rule in &definition.load_orders {
        let list = crate::game_load_order::load_after_rules(ctx, instance_id, &rule.id)
            .map_err(|e| e.to_string())?;
        rules.push(GameLoadoutRules {
            list: rule.id.clone(),
            rules: list,
        });
    }

    Ok(GameLoadout {
        content,
        plugin_lists,
        inis,
        rules,
        saves: manifest.saves,
    })
}

/// Create a loadout profile from the setup of a game instance.
pub fn create_game_profile(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    name: &str,
) -> Result<LoadoutProfile, String> {
    if name.trim().is_empty() {
        return Err("A loadout profile needs a name".into());
    }
    let state = capture(ctx, instance_id, definition)?;
    let enabled_mods = state
        .content
        .iter()
        .filter(|item| item.enabled)
        .map(|item| item.item.clone())
        .collect();
    let profile = LoadoutProfile {
        name: name.to_string(),
        enabled_mods,
        created_at: chrono::Utc::now().to_rfc3339(),
        covers: None,
        items: None,
        game: Some(state),
    };
    loadout::write_profile(&instance_dir(ctx, instance_id)?, &profile)?;
    Ok(profile)
}

/// Write a profile with `state` under `name`. Used by an import, which builds
/// each profile's state from the setup it read rather than from the instance as
/// it stands. An existing profile keeps its `created_at`.
pub fn save_game_profile(
    ctx: &Ctx,
    instance_id: &str,
    name: &str,
    state: GameLoadout,
) -> Result<LoadoutProfile, String> {
    if name.trim().is_empty() {
        return Err("A loadout profile needs a name".into());
    }
    let dir = instance_dir(ctx, instance_id)?;
    let created_at = loadout::read_profile(&dir, name)
        .map(|existing| existing.created_at)
        .unwrap_or_else(|_| chrono::Utc::now().to_rfc3339());
    let enabled_mods = state
        .content
        .iter()
        .filter(|item| item.enabled)
        .map(|item| item.item.clone())
        .collect();
    let profile = LoadoutProfile {
        name: name.to_string(),
        enabled_mods,
        created_at,
        covers: None,
        items: None,
        game: Some(state),
    };
    loadout::write_profile(&dir, &profile)?;
    Ok(profile)
}

/// The changes that [`update_game_profile`] would make, in the order the
/// sections are recorded. Only the content items are compared one by one; each
/// other section is reported as a whole when its bytes or rules differ.
pub fn plan_game_profile_update(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    profile_name: &str,
) -> Result<LoadoutUpdate, String> {
    let dir = instance_dir(ctx, instance_id)?;
    let old = loadout::read_profile(&dir, profile_name)?;
    let old_state = old
        .game
        .as_ref()
        .ok_or_else(|| format!("'{profile_name}' is not a game-instance loadout"))?;
    let current = capture(ctx, instance_id, definition)?;
    Ok(LoadoutUpdate {
        name: old.name,
        changes: diff(old_state, &current),
    })
}

/// Save `profile_name` again so it records the game instance's current setup.
/// The profile's `created_at` stays as it was.
pub fn update_game_profile(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    profile_name: &str,
) -> Result<LoadoutUpdate, String> {
    let dir = instance_dir(ctx, instance_id)?;
    let old = loadout::read_profile(&dir, profile_name)?;
    let old_state = old
        .game
        .as_ref()
        .ok_or_else(|| format!("'{profile_name}' is not a game-instance loadout"))?;
    let current = capture(ctx, instance_id, definition)?;
    let changes = diff(old_state, &current);
    let enabled_mods = current
        .content
        .iter()
        .filter(|item| item.enabled)
        .map(|item| item.item.clone())
        .collect();
    let profile = LoadoutProfile {
        name: old.name.clone(),
        enabled_mods,
        created_at: old.created_at,
        covers: None,
        items: None,
        game: Some(current),
    };
    loadout::write_profile(&dir, &profile)?;
    Ok(LoadoutUpdate {
        name: old.name,
        changes,
    })
}

fn item_change(filename: &str, content_type: &str, kind: LoadoutChangeKind) -> LoadoutChange {
    LoadoutChange {
        filename: filename.to_string(),
        content_type: content_type.to_string(),
        kind,
    }
}

fn diff(old: &GameLoadout, new: &GameLoadout) -> Vec<LoadoutChange> {
    let mut changes = Vec::new();

    let old_content: HashMap<&str, bool> = old
        .content
        .iter()
        .map(|item| (item.item.as_str(), item.enabled))
        .collect();
    for item in &new.content {
        match old_content.get(item.item.as_str()) {
            None => changes.push(item_change(
                &item.item,
                "content",
                LoadoutChangeKind::Added {
                    enabled: item.enabled,
                },
            )),
            Some(was) if *was == item.enabled => {}
            Some(true) => changes.push(item_change(
                &item.item,
                "content",
                LoadoutChangeKind::TurnedOff,
            )),
            Some(false) => changes.push(item_change(
                &item.item,
                "content",
                LoadoutChangeKind::TurnedOn,
            )),
        }
    }
    let new_content: HashMap<&str, bool> = new
        .content
        .iter()
        .map(|item| (item.item.as_str(), item.enabled))
        .collect();
    for item in &old.content {
        if !new_content.contains_key(item.item.as_str()) {
            changes.push(item_change(
                &item.item,
                "content",
                LoadoutChangeKind::Removed {
                    enabled: item.enabled,
                },
            ));
        }
    }

    for old_list in &old.plugin_lists {
        let new_list = new.plugin_lists.iter().find(|l| l.list == old_list.list);
        if new_list.map(|l| &l.contents) != Some(&old_list.contents) {
            changes.push(item_change(
                &old_list.list,
                "plugin_list",
                LoadoutChangeKind::Added { enabled: true },
            ));
        }
    }
    for new_list in &new.plugin_lists {
        if !old.plugin_lists.iter().any(|l| l.list == new_list.list) {
            changes.push(item_change(
                &new_list.list,
                "plugin_list",
                LoadoutChangeKind::Added { enabled: true },
            ));
        }
    }

    for old_ini in &old.inis {
        let new_ini = new.inis.iter().find(|i| i.file == old_ini.file);
        if new_ini.map(|i| &i.contents) != Some(&old_ini.contents) {
            changes.push(item_change(
                &old_ini.file,
                "ini",
                LoadoutChangeKind::Added { enabled: true },
            ));
        }
    }
    for new_ini in &new.inis {
        if !old.inis.iter().any(|i| i.file == new_ini.file) {
            changes.push(item_change(
                &new_ini.file,
                "ini",
                LoadoutChangeKind::Added { enabled: true },
            ));
        }
    }

    for old_rules in &old.rules {
        let new_rules = new.rules.iter().find(|r| r.list == old_rules.list);
        if new_rules.map(|r| &r.rules) != Some(&old_rules.rules) {
            changes.push(item_change(
                &old_rules.list,
                "rule",
                LoadoutChangeKind::Added { enabled: true },
            ));
        }
    }

    if old.saves != new.saves {
        changes.push(item_change(
            "saves",
            "save_choice",
            LoadoutChangeKind::Added { enabled: true },
        ));
    }

    changes
}

/// Whether the instance's game is running, so an apply must be refused.
fn game_is_running(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
) -> Result<bool, String> {
    let manifest = game_instance::get_manifest(ctx, instance_id).map_err(|e| e.to_string())?;
    let Some(store) = game_instance::session_store(&manifest.base, &manifest) else {
        return Ok(false);
    };
    let status =
        crate::game_user_files::status(ctx, &definition.id, &store).map_err(|e| e.to_string())?;
    Ok(status.is_some_and(|status| status.running && status.instance == instance_id))
}

/// Apply a game-instance profile: record the undo point, then restore every
/// section the profile holds. Refused while the game runs. `unlisted` is not
/// used for content: a game profile records every content layer, and a layer it
/// does not name is switched off, as a Minecraft profile switches off an
/// unlisted mod.
pub fn apply_game_profile_with_undo(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    profile_name: &str,
) -> Result<(), LoadoutApplyError> {
    let dir = instance_dir(ctx, instance_id).map_err(LoadoutApplyError::Profile)?;
    let profile = loadout::read_profile(&dir, profile_name).map_err(LoadoutApplyError::Profile)?;
    let state = profile.game.ok_or_else(|| {
        LoadoutApplyError::Profile(format!("'{profile_name}' is not a game-instance loadout"))
    })?;

    let _lock = ctx
        .lock_manager
        .acquire(
            LockResource::Instance(instance_id.to_string()),
            "apply_game_loadout",
        )
        .map_err(|e| LoadoutApplyError::Profile(e.to_string()))?;

    if game_is_running(ctx, instance_id, definition).map_err(LoadoutApplyError::Profile)? {
        return Err(LoadoutApplyError::Profile(format!(
            "'{instance_id}' is running; close the game before applying a loadout"
        )));
    }

    let undo = capture(ctx, instance_id, definition).map_err(LoadoutApplyError::UndoPoint)?;
    let undo = GameUndo {
        profile: profile.name.clone(),
        state: undo,
    };
    let bytes = serde_json::to_vec_pretty(&undo)
        .map_err(|e| LoadoutApplyError::UndoPoint(e.to_string()))?;
    write_atomic(&undo_path(&dir), &bytes).map_err(LoadoutApplyError::UndoPoint)?;

    if let Err(error) = apply_state(ctx, instance_id, definition, &state) {
        let rollback =
            apply_state(ctx, instance_id, definition, &undo.state).map_err(|e| e.to_string());
        return Err(LoadoutApplyError::Apply { error, rollback });
    }
    Ok(())
}

/// Put back the state the last apply replaced, then forget it. An instance
/// with no undo point is an error, not a silent success.
pub fn undo_game_apply(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
) -> Result<String, String> {
    let dir = instance_dir(ctx, instance_id)?;
    let _lock = ctx
        .lock_manager
        .acquire(
            LockResource::Instance(instance_id.to_string()),
            "undo_game_loadout",
        )
        .map_err(|e| e.to_string())?;
    if game_is_running(ctx, instance_id, definition)? {
        return Err(format!(
            "'{instance_id}' is running; close the game before undoing a loadout"
        ));
    }
    let bytes = fs::read(undo_path(&dir))
        .map_err(|e| format!("No loadout apply to undo for '{instance_id}': {e}"))?;
    let undo: GameUndo =
        serde_json::from_slice(&bytes).map_err(|e| format!("Invalid undo point: {e}"))?;
    apply_state(ctx, instance_id, definition, &undo.state)?;
    fs::remove_file(undo_path(&dir)).map_err(|e| format!("Cannot clear the undo point: {e}"))?;
    Ok(undo.profile)
}

fn apply_state(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    state: &GameLoadout,
) -> Result<(), String> {
    for ini in &state.inis {
        if crate::save_backup::safe_relative(&ini.file).is_none() {
            return Err(format!(
                "the loadout's INI '{}' is not a file inside the instance; nothing was applied",
                ini.file
            ));
        }
    }
    let mut manifest = game_instance::get_manifest(ctx, instance_id).map_err(|e| e.to_string())?;

    let wanted: HashMap<&str, bool> = state
        .content
        .iter()
        .map(|item| (item.item.as_str(), item.enabled))
        .collect();
    let mut layers = manifest.layers.layers().to_vec();
    for layer in &mut layers {
        if matches!(
            layer.source,
            LayerSource::Content { .. } | LayerSource::InstanceContent { .. }
        ) {
            layer.enabled = wanted.get(layer.id.as_str()).copied().unwrap_or(false);
        }
    }
    manifest.layers = agora_game_api::LayerStack::new(layers).map_err(|e| e.to_string())?;
    manifest.saves = state.saves;
    crate::game_deploy::write_instance_manifest_atomic(ctx, instance_id, &manifest)
        .map_err(|e| e.to_string())?;

    let dir = instance_dir(ctx, instance_id)?;

    for list in &state.plugin_lists {
        let rule = definition
            .load_orders
            .iter()
            .find(|rule| rule.id == list.list)
            .ok_or_else(|| format!("the game has no load order named '{}'", list.list))?;
        let path = dir.join(rule.file.as_str());
        match &list.contents {
            Some(bytes) => write_atomic(&path, bytes)?,
            None => remove_if_present(&path)?,
        }
    }

    for ini in &state.inis {
        let path = dir.join(&ini.file);
        match &ini.contents {
            Some(bytes) => write_atomic(&path, bytes)?,
            None => remove_if_present(&path)?,
        }
    }

    for rules in &state.rules {
        crate::game_load_order::replace_load_after_rules(
            ctx,
            instance_id,
            &rules.list,
            &rules.rules,
        )
        .map_err(|e| e.to_string())?;
    }

    Ok(())
}

fn remove_if_present(path: &std::path::Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("Cannot remove {}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game_instance::GameInstanceManifest;
    use agora_game_api::{
        BaseReference, GameId, Layer, LayerId, LoadOrderDefinition, LoadOrderFormat,
        LoadOrderItems, LoadOrderRules, RelPath, RuntimeIdentity, StoreId,
    };

    fn definition() -> GameDefinition {
        GameDefinition {
            mo2_game_name: None,
            import_ids: Default::default(),
            session: Default::default(),
            id: GameId::new("test-game").unwrap(),
            extends: None,
            name: "Test Game".into(),
            stores: vec![],
            version_sources: vec![],
            deployment: agora_game_api::DeploymentStrategy::Redirect,
            content_rules: vec![],
            content_layout: None,
            framework_ids: vec![],
            tool_ids: vec![],
            launch: None,
            user_files: vec![agora_game_api::UserFileMapping::new(
                agora_game_api::GamePath::UserData {
                    location: agora_game_api::UserDataLocation::Documents,
                    path: RelPath::new("Test Game/Settings.ini").unwrap(),
                },
                RelPath::new("user/Settings.ini").unwrap(),
                agora_game_api::UserFileStrategy::JournaledSwap,
            )
            .with_stores(vec![StoreId::steam()])],
            save_paths: vec![],
            linked_archive_patterns: vec![],
            declared_writes: vec![],
            excluded_paths: vec![],
            load_orders: vec![LoadOrderDefinition {
                id: "plugins".into(),
                name: "Plugins".into(),
                file: RelPath::new("user/Plugins.txt").unwrap(),
                destination: Default::default(),
                items: LoadOrderItems {
                    folder: RelPath::new("Data").unwrap(),
                    patterns: vec!["*.esp".into()],
                    folders: false,
                },
                format: LoadOrderFormat::Lines {
                    active_prefix: "*".into(),
                    header: vec![],
                },
                rules: LoadOrderRules {
                    kind: "none".into(),
                    entry: None,
                    implicit: Vec::new(),
                    implicit_list_file: None,
                    sorter: None,
                    overlaps: None,
                },
                intro: None,
                item_heading: None,
            }],
            runtime_files: Vec::new(),
            save_location: Vec::new(),
            ini_checks: Vec::new(),
            launch_alternatives: Vec::new(),
            copy_patterns: Vec::new(),
        }
    }

    fn manifest() -> GameInstanceManifest {
        let runtime = RuntimeIdentity {
            game: GameId::new("test-game").unwrap(),
            store: StoreId::steam(),
            version: "1.0.0".into(),
            build: None,
        };
        let mut manifest = GameInstanceManifest::new(
            runtime.game.clone(),
            "test-instance",
            "Test",
            Some(runtime),
            BaseReference::Unpinned {
                install: agora_game_api::InstallId::new("steam:1").unwrap(),
                reason: "test".into(),
            },
        );
        manifest.layers = agora_game_api::LayerStack::new(vec![
            Layer {
                id: LayerId::new("content:a").unwrap(),
                enabled: true,
                mount_path: RelPath::new("Data").unwrap(),
                source_path: RelPath::default(),
                source: LayerSource::Content {
                    content: "a".into(),
                },
                whiteouts: Vec::new(),
                own_copy: false,
            },
            Layer {
                id: LayerId::new("content:b").unwrap(),
                enabled: true,
                mount_path: RelPath::new("Data").unwrap(),
                source_path: RelPath::default(),
                source: LayerSource::Content {
                    content: "b".into(),
                },
                whiteouts: Vec::new(),
                own_copy: false,
            },
        ])
        .unwrap();
        manifest
    }

    struct Setup {
        _tmp: tempfile::TempDir,
        ctx: Ctx,
        dir: PathBuf,
        def: GameDefinition,
    }

    fn setup() -> Setup {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = Ctx::for_testing(tmp.path().join("data"));
        crate::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
        let dir = ctx.paths.instance_dir("test-instance").unwrap();
        fs::create_dir_all(dir.join("user")).unwrap();
        let manifest = manifest();
        crate::game_deploy::write_instance_manifest_atomic(&ctx, "test-instance", &manifest)
            .unwrap();
        fs::write(dir.join("user/Plugins.txt"), b"*A.esp\n*B.esp\n").unwrap();
        fs::write(dir.join("user/Settings.ini"), b"[General]\nValue=1\n").unwrap();
        let def = definition();
        crate::game_load_order::replace_load_after_rules(
            &ctx,
            "test-instance",
            "plugins",
            &[crate::game_load_order::LoadAfterRule {
                item: "B.esp".into(),
                after: "A.esp".into(),
            }],
        )
        .unwrap();
        Setup {
            _tmp: tmp,
            ctx,
            dir,
            def,
        }
    }

    #[test]
    fn create_apply_round_trip_and_undo() {
        let s = setup();
        create_game_profile(&s.ctx, "test-instance", &s.def, "base").unwrap();

        // Change everything the profile covers.
        let mut manifest = game_instance::get_manifest(&s.ctx, "test-instance").unwrap();
        let mut layers = manifest.layers.layers().to_vec();
        layers[0].enabled = false;
        layers[1].enabled = true;
        manifest.layers = agora_game_api::LayerStack::new(layers).unwrap();
        manifest.saves = SavesChoice::Own;
        crate::game_deploy::write_instance_manifest_atomic(&s.ctx, "test-instance", &manifest)
            .unwrap();
        fs::write(s.dir.join("user/Plugins.txt"), b"*C.esp\n").unwrap();
        fs::write(s.dir.join("user/Settings.ini"), b"[General]\nValue=2\n").unwrap();
        crate::game_load_order::replace_load_after_rules(&s.ctx, "test-instance", "plugins", &[])
            .unwrap();

        apply_game_profile_with_undo(&s.ctx, "test-instance", &s.def, "base").unwrap();

        let restored = game_instance::get_manifest(&s.ctx, "test-instance").unwrap();
        assert!(restored.layers.layers()[0].enabled);
        assert!(restored.layers.layers()[1].enabled);
        assert_eq!(restored.saves, SavesChoice::Shared);
        assert_eq!(
            fs::read(s.dir.join("user/Plugins.txt")).unwrap(),
            b"*A.esp\n*B.esp\n"
        );
        assert_eq!(
            fs::read(s.dir.join("user/Settings.ini")).unwrap(),
            b"[General]\nValue=1\n"
        );
        assert_eq!(
            crate::game_load_order::load_after_rules(&s.ctx, "test-instance", "plugins")
                .unwrap()
                .len(),
            1
        );

        // The undo point brings back the state before the apply.
        undo_game_apply(&s.ctx, "test-instance", &s.def).unwrap();
        let after_undo = game_instance::get_manifest(&s.ctx, "test-instance").unwrap();
        assert!(!after_undo.layers.layers()[0].enabled);
        assert_eq!(after_undo.saves, SavesChoice::Own);
        assert_eq!(
            fs::read(s.dir.join("user/Plugins.txt")).unwrap(),
            b"*C.esp\n"
        );
        assert_eq!(
            fs::read(s.dir.join("user/Settings.ini")).unwrap(),
            b"[General]\nValue=2\n"
        );
        assert!(
            crate::game_load_order::load_after_rules(&s.ctx, "test-instance", "plugins")
                .unwrap()
                .is_empty()
        );
        assert!(
            undo_game_apply(&s.ctx, "test-instance", &s.def).is_err(),
            "the undo point is used once"
        );
    }

    #[test]
    fn apply_is_refused_while_the_game_runs() {
        let s = setup();
        create_game_profile(&s.ctx, "test-instance", &s.def, "base").unwrap();
        // A session journal whose starting launcher is this still-live process
        // reads as running, the way a launch between the swap and the spawn does.
        let identity = crate::process_identity::capture_self().expect("self identity");
        let journal = serde_json::json!({
            "game": "test-game",
            "store": "steam",
            "instance_id": "test-instance",
            "created_at": "2026-01-01T00:00:00Z",
            "running_from": s.dir,
            "processes": [],
            "starting_launcher": identity,
            "files": [],
        });
        let path = s.ctx.paths.user_files_journal_path("test-game", "steam");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, serde_json::to_vec(&journal).unwrap()).unwrap();

        let error = apply_game_profile_with_undo(&s.ctx, "test-instance", &s.def, "base")
            .expect_err("a running game refuses the apply");
        assert!(error.to_string().contains("running"), "{error}");
        assert!(
            !undo_path(&s.dir).exists(),
            "nothing is written when the apply is refused"
        );
    }

    #[test]
    fn update_records_the_current_setup() {
        let s = setup();
        create_game_profile(&s.ctx, "test-instance", &s.def, "base").unwrap();

        let mut manifest = game_instance::get_manifest(&s.ctx, "test-instance").unwrap();
        let mut layers = manifest.layers.layers().to_vec();
        layers[1].enabled = false;
        manifest.layers = agora_game_api::LayerStack::new(layers).unwrap();
        crate::game_deploy::write_instance_manifest_atomic(&s.ctx, "test-instance", &manifest)
            .unwrap();
        fs::write(s.dir.join("user/Plugins.txt"), b"*A.esp\n").unwrap();

        let planned = plan_game_profile_update(&s.ctx, "test-instance", &s.def, "base").unwrap();
        assert!(!planned.changes.is_empty());

        let done = update_game_profile(&s.ctx, "test-instance", &s.def, "base").unwrap();
        assert_eq!(done.name, "base");
        let profiles = loadout::list_profiles(&s.dir).unwrap();
        let game = profiles[0].game.as_ref().unwrap();
        assert!(
            !game
                .content
                .iter()
                .find(|c| c.item == "content:b")
                .unwrap()
                .enabled
        );
        assert_eq!(
            game.plugin_lists[0].contents.as_deref(),
            Some(&b"*A.esp\n"[..])
        );

        // A second update with nothing changed reports nothing.
        assert!(update_game_profile(&s.ctx, "test-instance", &s.def, "base")
            .unwrap()
            .changes
            .is_empty());
    }
}
