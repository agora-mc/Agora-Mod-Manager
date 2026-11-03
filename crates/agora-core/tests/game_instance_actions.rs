//! Rename, clone, stop, play time and Recently deleted for generic game instances.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agora_core::ctx::CoreContext;
use agora_core::game_instance::trash::{self, PurgeSummary};
use agora_core::game_instance::{
    self, CloneOptions, GameInstanceManifest, InstanceError, SavesChoice,
};
use agora_core::process_identity::{self, ProcessIdentity};
use agora_core::settings::SettingsService;
use agora_game_api::{
    BaseMode, BaseReference, GameDefinition, GameId, Layer, LayerId, LayerSource, RelPath,
    RuntimeIdentity, StoreId,
};
use tempfile::TempDir;

fn runtime() -> RuntimeIdentity {
    RuntimeIdentity {
        game: GameId::new("skyrim-se").expect("a game id"),
        store: StoreId::new("steam").expect("a store id"),
        version: "1.6.1170.0".into(),
        build: None,
    }
}

fn manifest(id: &str, name: &str) -> GameInstanceManifest {
    let runtime = runtime();
    let mut manifest = GameInstanceManifest::new(
        runtime.game.clone(),
        id,
        name,
        Some(runtime.clone()),
        BaseReference::Pinned {
            id: "base-1".into(),
            runtime,
            mode: BaseMode::Linked,
        },
    );
    manifest.layers = agora_game_api::LayerStack::new(vec![Layer {
        id: LayerId::new("content-1").expect("a layer id"),
        enabled: true,
        mount_path: RelPath::new("Data").expect("a mount path"),
        source_path: RelPath::default(),
        source: LayerSource::Content {
            content: "stored-content-1".into(),
        },
        whiteouts: Vec::new(),
        own_copy: false,
    }])
    .expect("a valid layer stack");
    manifest
}

fn definition() -> GameDefinition {
    serde_json::from_value(serde_json::json!({
        "id": "skyrim-se",
        "name": "Skyrim Special Edition",
        "stores": [],
        "version_sources": [],
        "deployment": "virtual_file_system",
        "content_rules": [],
        "native_code_patterns": [],
        "framework_ids": [],
        "tool_ids": [],
        "launch": {
            "executable": { "root": "runtime", "path": "" },
            "arguments": [],
            "environment": {},
            "working_directory": { "root": "runtime", "path": "" }
        },
        "log_paths": [],
        "crash_paths": [],
        "user_files": [],
        "save_paths": [],
        "linked_archive_patterns": []
    }))
    .expect("a game definition the core reads")
}

struct Fixture {
    _tmp: TempDir,
    root: PathBuf,
    ctx: CoreContext,
}

impl Fixture {
    fn new() -> Self {
        let tmp = TempDir::new().expect("a temp dir");
        let root = tmp.path().join("app_data");
        let ctx = CoreContext::for_testing(root.clone());
        agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).expect("a state db");
        Self {
            _tmp: tmp,
            root,
            ctx,
        }
    }

    fn reopen(&self) -> CoreContext {
        CoreContext::for_testing(self.root.clone())
    }
}

/// Write an instance row and folder without building a base. Good enough for everything that
/// judges the instance's own state.
fn write_instance(ctx: &CoreContext, manifest: &GameInstanceManifest) -> PathBuf {
    let dir = ctx
        .paths
        .instance_dir(&manifest.instance_id)
        .expect("an instance dir");
    std::fs::create_dir_all(&dir).expect("the instance folder");
    std::fs::write(
        dir.join("instance_manifest.json"),
        serde_json::to_vec_pretty(manifest).expect("a manifest"),
    )
    .expect("the manifest file");
    let conn =
        agora_core::db::local_state_connection(&ctx.paths.local_state_db()).expect("a connection");
    conn.execute(
        "INSERT INTO game_instances (instance_id, game, name, base_json, created_at, last_launched_at)
         VALUES (?1, ?2, ?3, ?4, datetime('now'), NULL)",
        rusqlite::params![
            &manifest.instance_id,
            manifest.game.as_str(),
            &manifest.name,
            serde_json::to_string(&manifest.base).expect("a base record")
        ],
    )
    .expect("the instance row");
    dir
}

fn write_journal(
    ctx: &CoreContext,
    instance_id: &str,
    started_at: chrono::DateTime<chrono::Utc>,
    processes: Vec<ProcessIdentity>,
) -> PathBuf {
    let path = ctx.paths.user_files_journal_path("skyrim-se", "steam");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("the session folder");
    }
    let journal = serde_json::json!({
        "game": "skyrim-se",
        "store": "steam",
        "instance_id": instance_id,
        "created_at": started_at.to_rfc3339(),
        "running_from": ctx.paths.instances_root().join(instance_id),
        "processes": processes,
        "files": []
    });
    std::fs::write(&path, serde_json::to_vec(&journal).expect("a journal")).expect("the journal");
    path
}

fn spawn_sleeper() -> std::process::Child {
    if cfg!(windows) {
        std::process::Command::new("cmd.exe")
            .args(["/c", "ping", "-n", "120", "127.0.0.1>nul"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("a sleeper process")
    } else {
        std::process::Command::new("sleep")
            .arg("120")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("a sleeper process")
    }
}

fn live_identity(child: &std::process::Child) -> ProcessIdentity {
    process_identity::capture(child.id()).expect("the child's identity")
}

/// The first process `parent` started, with the start time that tells it from a reused pid.
fn first_child_of(parent: u32) -> Option<(u32, u64)> {
    let mut system = sysinfo::System::new();
    system.refresh_processes();
    system
        .processes()
        .iter()
        .find(|(_, process)| process.parent() == Some(sysinfo::Pid::from_u32(parent)))
        .map(|(pid, process)| (pid.as_u32(), process.start_time()))
}

fn is_alive(pid: u32, start_time: u64) -> bool {
    let mut system = sysinfo::System::new();
    system.refresh_process(sysinfo::Pid::from_u32(pid));
    system
        .process(sysinfo::Pid::from_u32(pid))
        .is_some_and(|process| process.start_time() == start_time)
}

fn fake_trash() -> (game_instance::TrashFn, Arc<Mutex<Vec<PathBuf>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    let trash: game_instance::TrashFn = Arc::new(move |path: &Path| {
        recorded
            .lock()
            .expect("the trash log")
            .push(path.to_path_buf());
        std::fs::remove_dir_all(path).map_err(|error| error.to_string())
    });
    (trash, seen)
}

#[test]
fn rename_keeps_the_id_and_survives_a_restart() {
    let f = Fixture::new();
    let source = manifest("grounded-1", "Grounded");
    write_instance(&f.ctx, &source);

    game_instance::rename(&f.ctx, "grounded-1", "  Grounded Again  ").expect("the rename");

    let reopened = f.reopen();
    let record = game_instance::get(&reopened, "grounded-1")
        .expect("a read")
        .expect("the instance");
    assert_eq!(record.name, "Grounded Again");
    assert_eq!(record.instance_id, "grounded-1", "the id does not change");
    let manifest = game_instance::get_manifest(&reopened, "grounded-1").expect("the manifest");
    assert_eq!(manifest.name, "Grounded Again");
    assert_eq!(manifest.instance_id, "grounded-1");

    assert!(
        game_instance::rename(&f.ctx, "grounded-1", "   ").is_err(),
        "a blank name is refused"
    );
}

#[test]
fn clone_copies_layers_order_inis_and_generated_files_but_never_saves() {
    let f = Fixture::new();
    let source = manifest("grounded-1", "Grounded");
    let dir = write_instance(&f.ctx, &source);
    std::fs::write(
        dir.join("plugin_list_state.json"),
        br#"{"order":["a.esm","b.esp"]}"#,
    )
    .expect("the plugin state");
    std::fs::create_dir_all(dir.join("user")).expect("the user folder");
    std::fs::write(
        dir.join("user").join("Skyrim.ini"),
        b"[General]\nLanguage=EN\n",
    )
    .expect("the INI");
    std::fs::create_dir_all(dir.join("generated").join("nemesis").join("gen-1"))
        .expect("the generated folder");
    std::fs::write(
        dir.join("generated")
            .join("nemesis")
            .join("gen-1")
            .join("out.esp"),
        b"generated",
    )
    .expect("the generated file");
    std::fs::create_dir_all(dir.join("logs")).expect("the logs folder");
    std::fs::write(dir.join("logs").join("session.log"), b"old log").expect("the log");

    let cloned = game_instance::clone_instance(
        &f.ctx,
        "grounded-1",
        "Grounded Copy",
        &definition(),
        CloneOptions::default(),
    )
    .expect("the clone");

    assert_ne!(cloned.instance_id, "grounded-1");
    assert_eq!(cloned.name, "Grounded Copy");
    let cloned_dir = f
        .ctx
        .paths
        .instance_dir(&cloned.instance_id)
        .expect("a dir");
    let cloned_manifest =
        game_instance::get_manifest(&f.ctx, &cloned.instance_id).expect("the clone manifest");
    assert_eq!(cloned_manifest.layers, source.layers, "layers are the same");
    assert_eq!(cloned_manifest.instance_id, cloned.instance_id);
    assert_eq!(cloned_manifest.name, "Grounded Copy");
    assert_eq!(
        cloned_manifest.saves,
        SavesChoice::Shared,
        "the default clone copies no saves and leaves the choice where it was"
    );
    assert_eq!(
        std::fs::read(cloned_dir.join("plugin_list_state.json")).expect("the plugin state"),
        br#"{"order":["a.esm","b.esp"]}"#,
        "plugin order is copied"
    );
    assert_eq!(
        std::fs::read(cloned_dir.join("user").join("Skyrim.ini")).expect("the INI"),
        b"[General]\nLanguage=EN\n",
        "INIs are copied"
    );
    assert!(
        cloned_dir
            .join("generated")
            .join("nemesis")
            .join("gen-1")
            .join("out.esp")
            .is_file(),
        "tool output is copied"
    );
    assert!(
        !cloned_dir.join("logs").exists(),
        "per-launch logs are not the instance's content"
    );
    assert!(
        game_instance::get(&f.ctx, &cloned.instance_id)
            .expect("a read")
            .is_some(),
        "the clone is in the list"
    );
}

#[test]
fn clone_refuses_while_the_source_is_running() {
    let f = Fixture::new();
    let source = manifest("grounded-1", "Grounded");
    write_instance(&f.ctx, &source);
    write_journal(
        &f.ctx,
        "grounded-1",
        chrono::Utc::now(),
        vec![process_identity::capture_self().expect("this process")],
    );

    let error = game_instance::clone_instance(
        &f.ctx,
        "grounded-1",
        "Grounded Copy",
        &definition(),
        CloneOptions::default(),
    )
    .expect_err("a running source is refused");
    assert!(matches!(error, InstanceError::Running(id) if id == "grounded-1"));
    assert_eq!(
        game_instance::list(&f.ctx).expect("the list").len(),
        1,
        "nothing was left behind"
    );
}

#[test]
fn a_failed_clone_leaves_nothing_behind_and_succeeds_once_the_cause_is_gone() {
    let f = Fixture::new();
    let source = manifest("grounded-1", "Grounded");
    let dir = write_instance(&f.ctx, &source);

    // A link inside the instance is not copied: the clone fails rather than silently dropping it.
    let target = f.root.join("outside");
    std::fs::create_dir_all(&target).expect("the link target");
    let link = dir.join("linked");
    #[cfg(windows)]
    let created = std::process::Command::new("cmd.exe")
        .args(["/c", "mklink", "/J"])
        .arg(&link)
        .arg(&target)
        .output()
        .map(|_| link.exists())
        .unwrap_or(false);
    #[cfg(not(windows))]
    let created = std::os::unix::fs::symlink(&target, &link).is_ok();
    assert!(created, "the test could not create a link");

    let error = game_instance::clone_instance(
        &f.ctx,
        "grounded-1",
        "Grounded Copy",
        &definition(),
        CloneOptions::default(),
    )
    .expect_err("a link fails the clone");
    assert!(error.to_string().contains("link"), "{error}");
    assert_eq!(
        game_instance::list(&f.ctx).expect("the list").len(),
        1,
        "the failed clone added no instance"
    );
    let leftovers: Vec<_> = std::fs::read_dir(f.ctx.paths.instances_root())
        .expect("the instances root")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name != "grounded-1")
        .collect();
    assert!(leftovers.is_empty(), "the failed clone left {leftovers:?}");
    assert!(
        std::fs::read_dir(f.ctx.paths.staging_root())
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(true),
        "the staging folder was left behind"
    );

    // Negative control: with the cause removed, the same clone succeeds.
    #[cfg(windows)]
    let _ = std::fs::remove_dir(&link);
    #[cfg(not(windows))]
    let _ = std::fs::remove_file(&link);
    game_instance::clone_instance(
        &f.ctx,
        "grounded-1",
        "Grounded Copy",
        &definition(),
        CloneOptions::default(),
    )
    .expect("the clone works once the link is gone");
}

#[test]
fn stop_ends_agoras_process_tree_and_not_an_unrelated_process() {
    let f = Fixture::new();
    let source = manifest("grounded-1", "Grounded");
    write_instance(&f.ctx, &source);

    let mut game = spawn_sleeper();
    // Give the shell time to start the process it runs before the tree is walked.
    std::thread::sleep(std::time::Duration::from_millis(250));
    let identity = live_identity(&game);
    let child = first_child_of(game.id());
    let mut innocent = spawn_sleeper();
    let innocent_identity = live_identity(&innocent);

    let started = chrono::Utc::now() - chrono::Duration::minutes(90);
    let journal = write_journal(&f.ctx, "grounded-1", started, vec![identity.clone()]);

    let stopped = game_instance::stop(&f.ctx, "grounded-1", &definition()).expect("the stop");
    assert_eq!(stopped.instance_id, "grounded-1");
    assert!(
        stopped.processes_ended >= 1,
        "no process was ended: {stopped:?}"
    );
    assert!(
        stopped.play_time_ms >= 90 * 60 * 1000,
        "the session was not added to the play time: {stopped:?}"
    );
    assert!(
        process_identity::verify(&identity).is_err(),
        "the stopped game is still alive"
    );
    if let Some((pid, start_time)) = child {
        assert!(
            !is_alive(pid, start_time),
            "a child the game started was left running"
        );
    } else if cfg!(windows) {
        panic!("the test could not see the process the game started");
    }
    assert!(
        !journal.exists(),
        "the normal session end did not run (the journal is still there)"
    );
    assert!(
        process_identity::verify(&innocent_identity).is_ok(),
        "a process Agora did not start was killed"
    );

    let _ = innocent.kill();
    let _ = innocent.wait();
    let _ = game.wait();
}

#[test]
fn stop_refuses_when_nothing_is_running() {
    let f = Fixture::new();
    let source = manifest("grounded-1", "Grounded");
    write_instance(&f.ctx, &source);

    let error =
        game_instance::stop(&f.ctx, "grounded-1", &definition()).expect_err("nothing is running");
    assert!(matches!(error, InstanceError::NothingRunning(id) if id == "grounded-1"));
}

#[test]
fn play_time_accumulates_across_sessions() {
    let f = Fixture::new();
    let source = manifest("grounded-1", "Grounded");
    write_instance(&f.ctx, &source);

    let now = chrono::Utc::now();
    let first = write_journal(
        &f.ctx,
        "grounded-1",
        now - chrono::Duration::minutes(90),
        Vec::new(),
    );
    let ended = game_instance::end_session(&f.ctx, &definition(), &StoreId::steam())
        .expect("the first session ends")
        .expect("a session was there");
    assert_eq!(ended.instance_id, "grounded-1");
    assert!(!first.exists());

    write_journal(
        &f.ctx,
        "grounded-1",
        now - chrono::Duration::minutes(30),
        Vec::new(),
    );
    game_instance::end_session(&f.ctx, &definition(), &StoreId::steam())
        .expect("the second session ends")
        .expect("a session was there");

    let stats = game_instance::play_stats(&f.ctx, "grounded-1").expect("the play time");
    assert!(
        stats.total_play_ms >= 120 * 60 * 1000,
        "sessions did not accumulate: {stats:?}"
    );
    assert!(stats.last_played_at.is_some());
}

#[test]
fn delete_goes_to_recently_deleted_and_restore_puts_it_back() {
    let f = Fixture::new();
    let source = manifest("grounded-1", "Grounded");
    write_instance(&f.ctx, &source);
    let conn = agora_core::db::local_state_connection(&f.ctx.paths.local_state_db())
        .expect("a connection");
    conn.execute(
        "UPDATE game_instances SET total_play_ms = 12345 WHERE instance_id = 'grounded-1'",
        [],
    )
    .expect("the play time");

    let outcome = game_instance::delete(&f.ctx, "grounded-1").expect("the delete");
    let entry_id = outcome.trash_entry_id.clone().expect("a trash entry");
    assert!(!f
        .ctx
        .paths
        .instance_dir("grounded-1")
        .expect("a dir")
        .exists());
    assert!(
        game_instance::get(&f.ctx, "grounded-1")
            .expect("a read")
            .is_none(),
        "the row is gone from the list"
    );
    assert!(
        f.ctx
            .paths
            .recently_deleted_games_root()
            .join(&entry_id)
            .join("instance")
            .join("instance_manifest.json")
            .is_file(),
        "the files went to Agora's trash"
    );

    let restored = trash::restore(&f.ctx, &entry_id, None).expect("the restore");
    assert_eq!(restored.instance_id, "grounded-1");
    assert!(!restored.renamed);
    let record = game_instance::get(&f.ctx, "grounded-1")
        .expect("a read")
        .expect("the restored instance");
    assert_eq!(record.name, "Grounded");
    let stats = game_instance::play_stats(&f.ctx, "grounded-1").expect("the play time");
    assert_eq!(stats.total_play_ms, 12345, "play time survives the trash");
}

#[test]
fn restoring_a_taken_id_is_refused_and_a_new_name_works() {
    let f = Fixture::new();
    let source = manifest("grounded-1", "Grounded");
    write_instance(&f.ctx, &source);
    let outcome = game_instance::delete(&f.ctx, "grounded-1").expect("the delete");
    let entry_id = outcome.trash_entry_id.expect("a trash entry");

    // The id is used again by a different instance.
    write_instance(&f.ctx, &manifest("grounded-1", "Someone Else"));

    let error = trash::restore(&f.ctx, &entry_id, None).expect_err("the id is taken");
    assert!(matches!(error, InstanceError::RestoreIdTaken(id) if id == "grounded-1"));

    let restored = trash::restore(&f.ctx, &entry_id, Some("Grounded Restored"))
        .expect("restoring under a new name");
    assert!(restored.renamed);
    assert_ne!(restored.instance_id, "grounded-1");
    assert_eq!(restored.original_instance_id, "grounded-1");
    let restored_record = game_instance::get(&f.ctx, &restored.instance_id)
        .expect("a read")
        .expect("the restored instance");
    assert_eq!(restored_record.name, "Grounded Restored");
    let live = game_instance::get(&f.ctx, "grounded-1")
        .expect("a read")
        .expect("the live instance");
    assert_eq!(
        live.name, "Someone Else",
        "the live instance was not touched"
    );
}

#[test]
fn purge_removes_only_the_entry_it_was_given() {
    let f = Fixture::new();
    write_instance(&f.ctx, &manifest("one-1", "One"));
    write_instance(&f.ctx, &manifest("two-2", "Two"));
    let first = game_instance::delete(&f.ctx, "one-1")
        .expect("the first delete")
        .trash_entry_id
        .expect("an entry");
    let second = game_instance::delete(&f.ctx, "two-2")
        .expect("the second delete")
        .trash_entry_id
        .expect("an entry");

    let (trash, seen) = fake_trash();
    trash::purge(&f.ctx, &first, &trash).expect("the purge");
    assert_eq!(seen.lock().expect("the log").len(), 1);
    assert!(!f
        .ctx
        .paths
        .recently_deleted_games_root()
        .join(&first)
        .exists());
    assert!(
        f.ctx
            .paths
            .recently_deleted_games_root()
            .join(&second)
            .exists(),
        "the other entry was removed too"
    );

    // Purging everything takes the other one and leaves nothing.
    let summary = trash::purge_all(&f.ctx, &trash);
    assert_eq!(
        summary,
        PurgeSummary {
            purged: 1,
            failed: 0,
            kept: 0
        }
    );
    assert!(trash::list(&f.ctx, "skyrim-se").is_empty());
}

#[test]
fn cleanup_respects_the_retention_setting() {
    let f = Fixture::new();
    write_instance(&f.ctx, &manifest("short-1", "Short window"));
    game_instance::delete(&f.ctx, "short-1")
        .expect("the delete")
        .trash_entry_id
        .expect("an entry");

    let settings = SettingsService::new(f.ctx.clone());
    settings
        .set(trash::RETENTION_SETTING, &serde_json::json!(1))
        .expect("the setting");
    let (trash, _seen) = fake_trash();
    let later = chrono::Utc::now() + chrono::Duration::days(2);
    let summary = trash::cleanup(&f.ctx, later, &trash);
    assert_eq!(summary.purged, 1, "an expired entry is purged");

    write_instance(&f.ctx, &manifest("long-2", "Long window"));
    game_instance::delete(&f.ctx, "long-2")
        .expect("the delete")
        .trash_entry_id
        .expect("an entry");
    settings
        .set(trash::RETENTION_SETTING, &serde_json::json!(30))
        .expect("the setting");
    let summary = trash::cleanup(&f.ctx, later, &trash);
    assert_eq!(summary.purged, 0, "a 30-day window keeps a 2-day-old entry");
    assert_eq!(summary.kept, 1);
}

#[test]
fn a_clone_of_own_saves_starts_from_a_fresh_folder() {
    let f = Fixture::new();
    let mut source = manifest("grounded-1", "Grounded");
    source.saves = SavesChoice::Own;
    write_instance(&f.ctx, &source);

    // This game has no save rule, so an Own-saves clone cannot be pointed at a folder. It must
    // fail, and leave nothing behind, rather than copy the source's setting by accident.
    let error = game_instance::clone_instance(
        &f.ctx,
        "grounded-1",
        "Grounded Copy",
        &definition(),
        CloneOptions::default(),
    )
    .expect_err("a game with no save rule cannot own saves");
    assert!(error.to_string().contains("save"), "{error}");
    assert_eq!(
        game_instance::list(&f.ctx).expect("the list").len(),
        1,
        "the failed clone added no instance"
    );
}
