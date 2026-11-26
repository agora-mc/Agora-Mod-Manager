//! Locked, context-aware entry points for snapshot creation and restore.
//!
//! [`crate::snapshot`] is a free-function module over an `instance_dir` with no
//! access to the lock manager, so it cannot serialize itself against anything.
//! Leaving that to the adapters meant the desktop command checked only its own
//! in-process launch state — a check it then released before starting the
//! blocking work — and the CLI checked nothing at all, so a concurrent
//! `agora snapshots restore` could run underneath an install.
//!
//! Core owns the invariant instead. Every adapter goes through here, and the
//! whole operation — recovery, validation, undo capture, replacement, and
//! retention — happens inside one critical section.

use crate::ctx::Ctx;
use crate::error::{LauncherError, LauncherResult};
#[cfg(test)]
use crate::lock_manager::LockResource;
use crate::snapshot::{self, RestoreOutcome, Snapshot};

pub struct SnapshotService {
    ctx: Ctx,
}

impl SnapshotService {
    pub fn new(ctx: Ctx) -> Self {
        Self { ctx }
    }

    /// The game `instance_id` belongs to, from the identity table every instance has a row in
    /// (MASTER_SPEC §32.7). An instance with no row is a Minecraft one, the same rule the rest of
    /// this service uses; a database that cannot be read is cannot-tell, never a guess.
    fn instance_game(&self, instance_id: &str) -> LauncherResult<agora_game_api::GameId> {
        crate::game_instance::game_of(&self.ctx, instance_id)
            .map(|game| game.unwrap_or_else(agora_game_api::GameId::minecraft))
            .map_err(|error| LauncherError::Generic {
                code: "ERR_LOCAL_STATE_FAILED".into(),
                message: format!("cannot tell which game '{instance_id}' belongs to: {error}"),
            })
    }

    /// The folders the package of `instance_id`'s game declared (MASTER_SPEC §32.7).
    ///
    /// Core holds no game's folder list: the instance's own game decides, so a generic game that
    /// declares folders is captured and restored by the same rule as Minecraft. An instance with no
    /// row in the identity table is a Minecraft one, the same rule the rest of this service uses. A
    /// game that declares nothing, or is not installed, is refused with `ERR_SNAPSHOT_SCOPE` rather
    /// than captured against an empty scope.
    fn declared_scope(&self, instance_id: &str) -> LauncherResult<Vec<String>> {
        let game = self.instance_game(instance_id)?;
        let declared = self
            .ctx
            .games
            .game(&game)
            .and_then(|definition| definition.snapshot.as_ref());
        let Some(scope) = declared else {
            return Err(LauncherError::Generic {
                code: "ERR_SNAPSHOT_SCOPE".into(),
                message: format!(
                    "no package declares the folders a snapshot of a '{}' instance covers, so \
                     nothing was captured or restored",
                    game.as_str()
                ),
            });
        };
        let entries: Vec<String> = scope
            .tracked_entries()
            .into_iter()
            .map(str::to_string)
            .collect();
        if entries.is_empty() {
            return Err(LauncherError::Generic {
                code: "ERR_SNAPSHOT_SCOPE".into(),
                message: format!(
                    "the snapshot scope declared for '{}' is empty, so nothing was captured or \
                     restored",
                    game.as_str()
                ),
            });
        }
        Ok(entries)
    }

    /// Whether `instance_id` is a generic game instance rather than a
    /// Minecraft one. A database that cannot be read is an error, never a
    /// guess: the two kinds take different recovery paths.
    fn is_game_instance(&self, instance_id: &str) -> LauncherResult<bool> {
        crate::game_instance::get(&self.ctx, instance_id)
            .map(|record| record.is_some())
            .map_err(|error| LauncherError::Generic {
                code: "ERR_LOCAL_STATE_FAILED".into(),
                message: format!("cannot tell whether '{instance_id}' is a game instance: {error}"),
            })
    }

    /// Capture a full snapshot of an instance.
    pub fn create(&self, instance_id: &str, label: Option<&str>) -> LauncherResult<Snapshot> {
        let instance_dir = self.ctx.paths.instance_dir(instance_id)?;
        let _guard = crate::instance_runtime::InstanceGuard::for_mutation(
            &self.ctx,
            instance_id,
            "snapshot",
        )?;

        // A game instance declares its own scope from its definition; capture
        // the same way a change there takes an automatic snapshot.
        if self.is_game_instance(instance_id)? {
            let definition = crate::game_snapshot::definition_for(&self.ctx, instance_id)
                .map_err(snapshot_error)?;
            let snapshot = crate::game_snapshot::create_locked(
                &self.ctx,
                instance_id,
                &definition,
                label,
                snapshot::SnapshotOrigin::User,
            )
            .map_err(snapshot_error)?;
            if let Err(error) = crate::lkg::run_retention(&instance_dir) {
                eprintln!("[snapshot] retention after capture failed: {error}");
            }
            return Ok(snapshot);
        }

        let declared = self.declared_scope(instance_id)?;
        let entries: Vec<&str> = declared.iter().map(String::as_str).collect();

        let snapshot = snapshot::create_snapshot_with_origin(
            &instance_dir,
            label,
            &entries,
            snapshot::SnapshotOrigin::User,
        )
        .map_err(snapshot_error)?;
        // Retention is housekeeping: a failure here must not present a
        // successful capture as a failure.
        if let Err(error) = crate::lkg::run_retention(&instance_dir) {
            eprintln!("[snapshot] retention after capture failed: {error}");
        }
        Ok(snapshot)
    }

    /// Restore an instance to a snapshot.
    ///
    /// Holds the instance lock across the whole operation, so the undo
    /// snapshot and the restore it protects cannot be separated by another
    /// mutation.
    pub fn restore(&self, instance_id: &str, snapshot_id: &str) -> LauncherResult<RestoreOutcome> {
        self.restore_forcing(instance_id, snapshot_id, false)
    }

    /// [`restore`](Self::restore) with the caller's explicit answer for a game
    /// that is running (or cannot be proven idle). Minecraft instances have no
    /// such override; the flag is ignored for them.
    pub fn restore_forcing(
        &self,
        instance_id: &str,
        snapshot_id: &str,
        force_while_running: bool,
    ) -> LauncherResult<RestoreOutcome> {
        let instance_dir = self.ctx.paths.instance_dir(instance_id)?;
        let is_game_instance = self.is_game_instance(instance_id)?;
        let _guard = if is_game_instance && force_while_running {
            crate::instance_runtime::InstanceGuard::for_mutation_while_running(
                &self.ctx,
                instance_id,
                "restore_snapshot",
            )?
        } else {
            crate::instance_runtime::InstanceGuard::for_mutation(
                &self.ctx,
                instance_id,
                "restore_snapshot",
            )?
        };

        if is_game_instance {
            let definition = crate::game_snapshot::definition_for(&self.ctx, instance_id)
                .map_err(snapshot_error)?;
            let outcome = crate::game_snapshot::restore_locked(
                &self.ctx,
                instance_id,
                &definition,
                snapshot_id,
                force_while_running,
            )
            .map_err(|error| LauncherError::Generic {
                code: error.code().into(),
                message: error.to_string(),
            })?;
            return Ok(outcome.outcome);
        }

        let declared = self.declared_scope(instance_id)?;
        let entries: Vec<&str> = declared.iter().map(String::as_str).collect();
        // A snapshot can carry another instance's manifest (an imported backup,
        // a clone's source). The restore replaces state, never identity.
        let identity =
            crate::helpers::read_manifest(&self.ctx.paths.instance_manifest(instance_id)?)
                .ok()
                .map(|manifest| (manifest.instance_id, manifest.name));

        // Validate the target before capturing anything: an invalid request
        // should not cost the user a snapshot.
        let scope = snapshot::restore_scope(&instance_dir, snapshot_id).map_err(snapshot_error)?;

        // The undo snapshot covers exactly what the restore will replace.
        // Matching the scope means undoing a configuration-only restore does
        // not copy gigabytes of world data it was never going to touch.
        let label = pre_restore_label(chrono::Utc::now());
        let scope_refs: Vec<&str> = scope.iter().map(String::as_str).collect();
        snapshot::create_snapshot_scoped(&instance_dir, Some(&label), &scope_refs).map_err(
            |error| LauncherError::Generic {
                code: "ERR_SNAPSHOT".into(),
                message: format!("Could not create the undo snapshot: {error}"),
            },
        )?;

        let outcome = snapshot::restore_snapshot(&instance_dir, snapshot_id, &entries)
            .map_err(snapshot_error)?;
        let mut outcome = outcome;
        // A snapshot owns what it captured. A folder this game no longer declares is restored
        // anyway, and the result says which, so the person restoring is not left guessing about
        // state that came back from outside the current scope (MASTER_SPEC §32.7). The question is
        // asked of the scope the snapshot recorded, compared by path components with the platform's
        // case rule at any depth: dropping `state/characters` while `state/config` stays is reported.
        let game = self.instance_game(instance_id)?;
        for folder in &scope {
            if !snapshot::scope_covers_root(&entries, folder) {
                outcome.notices.push(format!(
                    "folder '{folder}' is no longer tracked by {}; restored from the snapshot anyway",
                    game.as_str()
                ));
            }
        }
        self.restore_manifest_identity(instance_id, identity.as_ref())?;
        self.sync_row_from_restored_manifest(instance_id)?;

        if let Err(error) = crate::lkg::run_retention(&instance_dir) {
            eprintln!("[snapshot] retention after restore failed: {error}");
        }
        Ok(outcome)
    }

    /// Keep the live instance's id and name when the restored manifest came
    /// from another instance. Everything else in the manifest is state.
    fn restore_manifest_identity(
        &self,
        instance_id: &str,
        identity: Option<&(String, String)>,
    ) -> LauncherResult<()> {
        let path = self.ctx.paths.instance_manifest(instance_id)?;
        if !path.exists() {
            return Ok(());
        }
        let mut manifest = crate::helpers::read_manifest(&path)?;
        let (wanted_id, wanted_name) = match identity {
            Some((id, name)) => (id.as_str(), Some(name.as_str())),
            None => (instance_id, None),
        };
        let mut changed = false;
        if manifest.instance_id != wanted_id {
            manifest.instance_id = wanted_id.to_string();
            changed = true;
        }
        if let Some(name) = wanted_name {
            if manifest.name != name {
                manifest.name = name.to_string();
                changed = true;
            }
        }
        if changed {
            crate::helpers::atomic_write_manifest(&path, &manifest)?;
        }
        Ok(())
    }

    /// A restore replaces `instance_manifest.json` wholesale, but the
    /// `minecraft_instances` row caches the Minecraft version, loader and locked
    /// flag the UI shows. Re-sync it from the restored manifest, or restoring
    /// a migration snapshot leaves the old version on screen while the game
    /// launches the restored one. Every user-facing restore (Snapshots tab,
    /// Crash Doctor, backup import, undo) goes through [`Self::restore`].
    fn sync_row_from_restored_manifest(&self, instance_id: &str) -> LauncherResult<()> {
        let manifest_path = self.ctx.paths.instance_manifest(instance_id)?;
        // A snapshot taken before the instance had a manifest restores none.
        if !manifest_path.exists() {
            return Ok(());
        }
        let manifest = crate::helpers::read_manifest(&manifest_path)?;
        let conn = crate::db::local_state_connection(&self.ctx.paths.local_state_db())
            .map_err(|_| LauncherError::LocalStateFailed)?;
        crate::db::sync_instance_row_from_manifest(&conn, instance_id, &manifest).map_err(
            |error| LauncherError::Generic {
                code: "ERR_RESTORE_ROW_SYNC".into(),
                message: format!(
                    "The snapshot was restored, but the instance record could not be updated to match it: {error}"
                ),
            },
        )?;
        Ok(())
    }

    /// The roots a restore of this snapshot would cover.
    pub fn scope(&self, instance_id: &str, snapshot_id: &str) -> LauncherResult<Vec<String>> {
        let instance_dir = self.ctx.paths.instance_dir(instance_id)?;
        snapshot::restore_scope(&instance_dir, snapshot_id).map_err(snapshot_error)
    }

    /// Return an unlocked instance to the snapshot taken when it was last locked.
    ///
    /// Goes through [`restore`](Self::restore), so the undo snapshot and the
    /// instance lock rules are the same as any other restore. An instance with
    /// no lock snapshot (locked before they existed) gets a plain refusal, not
    /// a guess at an older state.
    pub fn revert_to_lock(&self, instance_id: &str) -> LauncherResult<RestoreOutcome> {
        let instance_dir = self.ctx.paths.instance_dir(instance_id)?;
        let conn = crate::db::local_state_connection(&self.ctx.paths.local_state_db())
            .map_err(|_| LauncherError::LocalStateFailed)?;
        let row = crate::db::get_instance(&conn, instance_id)
            .map_err(|error| LauncherError::Generic {
                code: "ERR_LOCAL_STATE_FAILED".into(),
                message: error.to_string(),
            })?
            .ok_or_else(|| LauncherError::Generic {
                code: "ERR_INSTANCE_NOT_FOUND".into(),
                message: format!("Instance '{instance_id}' not found"),
            })?;
        if row.is_locked {
            return Err(LauncherError::Generic {
                code: "ERR_INSTANCE_LOCKED".into(),
                message: "Unlock the instance before reverting it.".into(),
            });
        }
        let mut dated = Vec::new();
        for lock in snapshot::lock_snapshots(&instance_dir).map_err(snapshot_error)? {
            let time = chrono::DateTime::parse_from_rfc3339(&lock.created_at).map_err(|error| {
                snapshot_error(format!(
                    "lock snapshot {} has an unreadable time, so Agora will not guess which to revert to: {error}",
                    lock.id
                ))
            })?;
            dated.push((time, lock));
        }
        let newest = dated
            .into_iter()
            .max_by_key(|(time, _)| *time)
            .map(|(_, lock)| lock)
            .ok_or_else(|| LauncherError::Generic {
                code: "ERR_NO_LOCK_SNAPSHOT".into(),
                message: "This instance has no lock snapshot to revert to. Lock it once to take one; Revert then returns to that point.".into(),
            })?;
        self.restore(instance_id, &newest.id)
    }
}

fn snapshot_error(message: String) -> LauncherError {
    LauncherError::Generic {
        code: "ERR_SNAPSHOT".into(),
        message,
    }
}

/// Label for the undo snapshot a restore takes. The stamp is local time because
/// the Snapshots list shows each snapshot's time in local time, and the label
/// is the name a person reads next to it.
fn pre_restore_label(at: chrono::DateTime<chrono::Utc>) -> String {
    format!(
        "pre-restore-{}",
        at.with_timezone(&chrono::Local).format("%Y%m%d-%H%M%S")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_root::RemoveOnDrop;
    use std::fs;

    /// The Minecraft folders the fixtures model.
    const TRACKED: &[&str] = &[
        "mods",
        "config",
        "resourcepacks",
        "shaderpacks",
        "datapacks",
        "saves",
        "options.txt",
        "instance_manifest.json",
    ];
    const PRELAUNCH: &[&str] = &[
        "mods",
        "config",
        "resourcepacks",
        "shaderpacks",
        "datapacks",
        "options.txt",
        "instance_manifest.json",
    ];

    #[test]
    fn pre_restore_label_stamps_the_local_clock_the_list_shows() {
        use chrono::TimeZone;
        let at = chrono::Utc
            .with_ymd_and_hms(2026, 10, 24, 5, 11, 23)
            .unwrap();
        let local = at.with_timezone(&chrono::Local);

        assert_eq!(
            pre_restore_label(at),
            format!("pre-restore-{}", local.format("%Y%m%d-%H%M%S"))
        );
    }

    /// A registry whose Minecraft package declares the scope these fixtures model, so the service
    /// reads the declaration the way a real package supplies it.
    fn registry() -> std::sync::Arc<crate::game_registry::GameRegistry> {
        use crate::game_registry::{DeclarativePackage, GameRegistry, PackageSource};
        use agora_game_api::{
            DeploymentStrategy, GameDefinition, GameId, PackageDefinition, SnapshotScope,
        };

        let game = GameDefinition {
            snapshot: Some(
                SnapshotScope::declared(TRACKED, Some(PRELAUNCH))
                    .expect("the fixture's Minecraft scope is usable"),
            ),
            id: GameId::minecraft(),
            name: "Minecraft: Java Edition".into(),
            extends: None,
            stores: Vec::new(),
            version_sources: Vec::new(),
            deployment: DeploymentStrategy::Redirect,
            content_rules: Vec::new(),
            content_layout: None,
            framework_ids: Vec::new(),
            tool_ids: Vec::new(),
            launch: None,
            user_files: Vec::new(),
            save_paths: Vec::new(),
            linked_archive_patterns: Vec::new(),
            declared_writes: Vec::new(),
            copy_patterns: Vec::new(),
            excluded_paths: Vec::new(),
            load_orders: Vec::new(),
            launch_alternatives: Vec::new(),
            runtime_files: Vec::new(),
            save_location: Vec::new(),
            ini_checks: Vec::new(),
            mo2_game_name: None,
            import_ids: Default::default(),
            session: Default::default(),
        };
        let package = PackageDefinition {
            id: "test.minecraft".into(),
            version: semver::Version::new(0, 1, 0),
            api_range: semver::VersionReq::parse(">=0.1, <0.2").unwrap(),
            parents: Vec::new(),
            games: vec![game],
            frameworks: Vec::new(),
            tools: Vec::new(),
            game_templates: Default::default(),
            behaviours: None,
        };
        let mut builder = GameRegistry::builder();
        builder
            .add(
                PackageSource::Compiled {
                    crate_name: "test".into(),
                },
                std::sync::Arc::new(DeclarativePackage(package)),
            )
            .unwrap();
        std::sync::Arc::new(builder.build())
    }

    /// The guard that removes the data root, then the service, its context and the instance id. The
    /// guard comes first so that it drops last, after the database handles the others hold.
    fn service() -> (RemoveOnDrop, SnapshotService, Ctx, String) {
        let root = std::env::temp_dir().join(format!(
            "agora-snapshot-service-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let guard = RemoveOnDrop::new(root.clone());
        let ctx = Ctx::for_testing(root).with_games(registry());
        crate::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
        let instance_id = "demo".to_string();
        let dir = ctx.paths.instance_dir(&instance_id).unwrap();
        fs::create_dir_all(dir.join("mods")).unwrap();
        fs::write(dir.join("mods").join("a.jar"), b"original").unwrap();
        fs::create_dir_all(dir.join("saves").join("world")).unwrap();
        fs::write(dir.join("saves").join("world").join("level.dat"), b"world").unwrap();
        (guard, SnapshotService::new(ctx.clone()), ctx, instance_id)
    }

    /// The instance's own game decides the scope (MASTER_SPEC §32.7): a Minecraft instance and an
    /// instance of a game whose package declares nothing are not treated the same, and the refusal
    /// names the game it could not read a declaration from.
    #[test]
    fn a_game_that_declares_no_snapshot_folder_is_refused_by_its_own_name() {
        let (_root, service, ctx, id) = service();
        let conn = crate::db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
        conn.execute(
            "INSERT INTO instances (instance_id, game, name) VALUES (?1, 'scope-game', 'demo')",
            [&id],
        )
        .unwrap();

        let refused = service
            .create(&id, None)
            .expect_err("no declaration, no snapshot");
        assert!(refused.to_string().contains("scope-game"), "{refused}");
    }

    /// A snapshot owns what it captured, so a folder the game no longer declares is still
    /// restored — and the result says so, naming the folder and the game, instead of doing it
    /// quietly (MASTER_SPEC §32.7).
    #[test]
    fn a_restore_reports_a_folder_the_game_no_longer_tracks() {
        let (_root, service, ctx, id) = service();
        let dir = ctx.paths.instance_dir(&id).unwrap();
        fs::create_dir_all(dir.join("characters")).unwrap();
        fs::write(dir.join("characters").join("hero.sav"), b"level 1").unwrap();
        // Captured while the folder was part of the scope; the package no longer declares it.
        let taken =
            snapshot::create_snapshot_scoped(&dir, Some("older scope"), &["mods", "characters"])
                .unwrap();
        fs::write(dir.join("characters").join("hero.sav"), b"level 9").unwrap();

        let outcome = service.restore(&id, &taken.id).unwrap();

        assert_eq!(
            fs::read(dir.join("characters").join("hero.sav")).unwrap(),
            b"level 1",
            "the snapshot owns what it captured"
        );
        let noticed = outcome
            .notices
            .iter()
            .find(|notice| notice.contains("characters"))
            .expect("the restore reports the folder it no longer tracks");
        assert!(
            noticed.contains("no longer tracked") && noticed.contains("minecraft"),
            "{noticed}"
        );
        assert!(
            noticed.contains("restored from the snapshot anyway"),
            "{noticed}"
        );
    }

    /// A retired folder is restored by the same wholesale mechanism as a tracked one, so a file
    /// created since the capture goes with the folder it sits in — and the undo point, sized to the
    /// recorded scope, puts it back (MASTER_SPEC §32.7).
    #[test]
    fn the_undo_of_a_restore_that_replaced_a_retired_folder_brings_back_its_new_files() {
        let (_root, service, ctx, id) = service();
        let dir = ctx.paths.instance_dir(&id).unwrap();
        fs::create_dir_all(dir.join("characters")).unwrap();
        fs::write(dir.join("characters").join("hero.sav"), b"level 1").unwrap();
        // Captured while the folder was part of the scope; the package no longer declares it.
        let taken =
            snapshot::create_snapshot_scoped(&dir, Some("older scope"), &["mods", "characters"])
                .unwrap();
        fs::write(dir.join("characters").join("hero.sav"), b"level 9").unwrap();
        fs::write(dir.join("characters").join("new.sav"), b"created since").unwrap();

        service.restore(&id, &taken.id).unwrap();

        assert_eq!(
            fs::read(dir.join("characters").join("hero.sav")).unwrap(),
            b"level 1"
        );
        assert!(
            !dir.join("characters").join("new.sav").exists(),
            "the folder comes back the way the snapshot captured it"
        );

        let undo = snapshot::list_snapshots(&dir)
            .unwrap()
            .into_iter()
            .find(|snapshot| {
                snapshot
                    .label
                    .as_deref()
                    .is_some_and(|label| label.starts_with("pre-restore-"))
            })
            .expect("undo snapshot was not created");
        service.restore(&id, &undo.id).unwrap();

        assert_eq!(
            fs::read(dir.join("characters").join("new.sav")).unwrap(),
            b"created since",
            "the undo covers the recorded scope, so the file the restore replaced is back"
        );
        assert_eq!(
            fs::read(dir.join("characters").join("hero.sav")).unwrap(),
            b"level 9"
        );
    }

    #[test]
    fn restore_takes_an_undo_snapshot_scoped_to_what_it_replaces() {
        let (_root, service, ctx, id) = service();
        let dir = ctx.paths.instance_dir(&id).unwrap();
        let scoped = snapshot::create_snapshot_scoped(&dir, Some("pre-launch"), PRELAUNCH).unwrap();

        fs::write(dir.join("mods").join("a.jar"), b"broken").unwrap();
        let outcome = service.restore(&id, &scoped.id).unwrap();

        assert_eq!(
            fs::read(dir.join("mods").join("a.jar")).unwrap(),
            b"original"
        );
        assert!(outcome.preserved_roots.iter().any(|r| r == "saves"));

        // The undo snapshot exists and covers the same roots, not the worlds.
        let undo = snapshot::list_snapshots(&dir)
            .unwrap()
            .into_iter()
            .find(|s| {
                s.label
                    .as_deref()
                    .is_some_and(|l| l.starts_with("pre-restore-"))
            })
            .expect("undo snapshot was not created");
        let undo_scope = service.scope(&id, &undo.id).unwrap();
        assert!(!undo_scope.iter().any(|r| r == "saves"));
    }

    /// Regression: restoring a 26.2 migration snapshot on a 26.1.2 instance
    /// restored the manifest (the game launched 26.2) but left the DB row —
    /// which the editor header and library card read — on 26.1.2.
    #[test]
    fn restore_resyncs_the_instance_row_from_the_restored_manifest() {
        let (_root, service, ctx, id) = service();
        let dir = ctx.paths.instance_dir(&id).unwrap();
        let manifest_path = ctx.paths.instance_manifest(&id).unwrap();
        let write_manifest = |version: &str, loader_version: &str, locked: bool| {
            let manifest = crate::models::InstanceManifest {
                manifest_version: crate::models::CURRENT_MANIFEST_VERSION,
                pack_origin: None,
                instance_id: id.clone(),
                name: "Demo".into(),
                created_from_pack: None,
                minecraft_version: version.into(),
                loader: "fabric".into(),
                loader_version: loader_version.into(),
                is_locked: locked,
                mods: vec![],
                resourcepacks: vec![],
                shaders: vec![],
                datapacks: vec![],
                worlds: vec![],
                user_preferences: serde_json::json!({}),
                game_data: Default::default(),
            };
            fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        };
        let row = crate::models::InstanceRow {
            instance_id: id.clone(),
            name: "Demo".into(),
            minecraft_version: "26.1.2".into(),
            loader: "fabric".into(),
            loader_version: "0.19.5".into(),
            is_modpack: false,
            is_locked: false,
            last_launched_at: None,
            jvm_memory_mb: 4096,
            jvm_memory_mode: "auto".into(),
            jvm_gc: "auto".into(),
            jvm_custom_args: String::new(),
            jvm_always_pre_touch: true,
            created_at: chrono::Utc::now().to_rfc3339(),
            java_path: None,
            java_incompatible_override: false,
            icon_path: None,
            launch_mode_override: "auto".into(),
            import_source: None,
            total_play_ms: 0,
        };
        let conn = crate::db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
        crate::db::upsert_instance(&conn, "minecraft", &row).unwrap();

        // Snapshot the 26.2 state, then move the instance back to 26.1.2.
        write_manifest("26.2", "0.20.0", true);
        let snap = snapshot::create_snapshot(&dir, Some("migration-26.2"), TRACKED).unwrap();
        write_manifest("26.1.2", "0.19.5", false);

        service.restore(&id, &snap.id).unwrap();

        let restored = crate::db::get_instance(&conn, &id).unwrap().unwrap();
        assert_eq!(restored.minecraft_version, "26.2");
        assert_eq!(restored.loader_version, "0.20.0");
        assert!(restored.is_locked);
        // Columns that are not manifest-derived are left alone.
        assert_eq!(restored.name, "Demo");
        assert_eq!(restored.jvm_memory_mb, 4096);
    }

    /// A snapshot can come from another instance (an imported backup). Its
    /// state restores, but the live instance keeps its own id and name.
    #[test]
    fn restore_keeps_the_live_instance_identity() {
        let (_root, service, ctx, id) = service();
        let dir = ctx.paths.instance_dir(&id).unwrap();
        let manifest_path = ctx.paths.instance_manifest(&id).unwrap();
        let write_manifest = |manifest_id: &str, name: &str, version: &str| {
            let manifest = crate::models::InstanceManifest {
                manifest_version: crate::models::CURRENT_MANIFEST_VERSION,
                pack_origin: None,
                instance_id: manifest_id.to_string(),
                name: name.to_string(),
                created_from_pack: None,
                minecraft_version: version.into(),
                loader: "fabric".into(),
                loader_version: "0.20.0".into(),
                is_locked: false,
                mods: vec![],
                resourcepacks: vec![],
                shaders: vec![],
                datapacks: vec![],
                worlds: vec![],
                user_preferences: serde_json::json!({}),
                game_data: Default::default(),
            };
            fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        };
        let row = crate::models::InstanceRow {
            instance_id: id.clone(),
            name: "Demo".into(),
            minecraft_version: "26.1.2".into(),
            loader: "fabric".into(),
            loader_version: "0.20.0".into(),
            is_modpack: false,
            is_locked: false,
            last_launched_at: None,
            jvm_memory_mb: 4096,
            jvm_memory_mode: "auto".into(),
            jvm_gc: "auto".into(),
            jvm_custom_args: String::new(),
            jvm_always_pre_touch: true,
            created_at: chrono::Utc::now().to_rfc3339(),
            java_path: None,
            java_incompatible_override: false,
            icon_path: None,
            launch_mode_override: "auto".into(),
            import_source: None,
            total_play_ms: 0,
        };
        let conn = crate::db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
        crate::db::upsert_instance(&conn, "minecraft", &row).unwrap();

        // Snapshot an instance from elsewhere, then restore it here.
        write_manifest("another-instance", "Another", "26.2");
        let snap = snapshot::create_snapshot(&dir, Some("imported"), TRACKED).unwrap();
        write_manifest(&id, "Demo", "26.1.2");

        service.restore(&id, &snap.id).unwrap();

        let restored = crate::helpers::read_manifest(&manifest_path).unwrap();
        assert_eq!(restored.instance_id, id);
        assert_eq!(restored.name, "Demo");
        assert_eq!(restored.minecraft_version, "26.2");
        let record = crate::db::get_instance(&conn, &id).unwrap().unwrap();
        assert_eq!(record.name, "Demo");
    }

    /// A restore must not proceed while a game may be writing to the instance.
    #[test]
    fn restore_refuses_while_a_launch_is_unaccounted_for() {
        let (_root, service, ctx, id) = service();
        let dir = ctx.paths.instance_dir(&id).unwrap();
        let snap = snapshot::create_snapshot(&dir, None, TRACKED).unwrap();
        fs::write(dir.join("mods").join("a.jar"), b"changed").unwrap();

        crate::instance_runtime::publish_starting(&ctx.paths, &id).unwrap();
        let error = service.restore(&id, &snap.id).unwrap_err();
        assert!(format!("{error:?}").contains("ERR_INSTANCE_LAUNCH_INDETERMINATE"));
        // Nothing was touched, including no undo snapshot.
        assert_eq!(
            fs::read(dir.join("mods").join("a.jar")).unwrap(),
            b"changed"
        );
        assert_eq!(snapshot::list_snapshots(&dir).unwrap().len(), 1);
    }

    /// An unknown snapshot must be rejected before an undo snapshot is taken.
    #[test]
    fn an_invalid_target_does_not_cost_a_snapshot() {
        let (_root, service, ctx, id) = service();
        let dir = ctx.paths.instance_dir(&id).unwrap();
        assert!(service.restore(&id, "does-not-exist").is_err());
        assert!(snapshot::list_snapshots(&dir).unwrap().is_empty());
    }

    /// Restore must not run while another process holds the instance lock.
    #[test]
    fn restore_is_serialized_against_other_instance_mutations() {
        let (_root, service, ctx, id) = service();
        let dir = ctx.paths.instance_dir(&id).unwrap();
        let snap = snapshot::create_snapshot(&dir, None, TRACKED).unwrap();

        let held = ctx
            .lock_manager
            .acquire(LockResource::Instance(id.clone()), "install")
            .unwrap();
        // Another task, not the holder: a holder may re-enter its own lock,
        // but a different thread still waits.
        let contender = ctx.lock_manager.clone();
        let contender_id = id.clone();
        let error = std::thread::spawn(move || {
            contender
                .acquire_with_timeout(
                    LockResource::Instance(contender_id),
                    "restore_snapshot",
                    std::time::Duration::from_millis(200),
                    None,
                )
                .unwrap_err()
        })
        .join()
        .unwrap();
        assert!(format!("{error:?}").contains("Lock") || format!("{error:?}").contains("lock"));
        drop(held);

        // With the lock free it proceeds.
        service.restore(&id, &snap.id).unwrap();
    }
}
