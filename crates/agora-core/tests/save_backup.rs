//! Saving an instance's save folder before a crash test, and putting it back (§30.4, crash answer
//! screen). Uses the Skyrim SE definition the app ships and temp user-data roots, so no real save
//! folder is ever read or written.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use agora_core::ctx::CoreContext;
use agora_core::game_instance::{self, GameInstanceManifest, SavesChoice};
use agora_core::save_backup::{self, SaveBackupError, MAX_BACKUPS};
use agora_game_api::{BaseReference, GameDefinition, GameId, InstallId, RuntimeIdentity, StoreId};
use tempfile::TempDir;

static TEST_LOCK: Mutex<()> = Mutex::new(());

mod common;

use common::skyrim_definition as skyrim;

struct Harness {
    _lock: MutexGuard<'static, ()>,
    _data: TempDir,
    user: TempDir,
    ctx: CoreContext,
}

impl Harness {
    fn new() -> Self {
        let lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let data = TempDir::new().unwrap();
        let user = TempDir::new().unwrap();
        std::env::set_var("AGORA_TEST_USER_DATA_ROOT", user.path());
        let ctx = CoreContext::for_testing(data.path().join("app_data"));
        Self {
            _lock: lock,
            _data: data,
            user,
            ctx,
        }
    }

    fn shared_saves(&self) -> PathBuf {
        self.user
            .path()
            .join("documents")
            .join("My Games")
            .join("Skyrim Special Edition")
            .join("Saves")
    }

    fn instance_dir(&self, instance_id: &str) -> PathBuf {
        self.ctx.paths.instance_dir(instance_id).unwrap()
    }

    fn make_instance(&self, instance_id: &str, saves: SavesChoice) {
        let dir = self.instance_dir(instance_id);
        std::fs::create_dir_all(&dir).unwrap();
        let game = GameId::new("skyrim-se").unwrap();
        let runtime = RuntimeIdentity {
            game: game.clone(),
            store: StoreId::steam(),
            version: "1.6.1170.0".into(),
            build: None,
        };
        let mut manifest = GameInstanceManifest::new(
            game,
            instance_id,
            instance_id,
            Some(runtime),
            BaseReference::Unpinned {
                install: InstallId::new("steam:489830").unwrap(),
                reason: "test".into(),
            },
        );
        manifest.saves = saves;
        std::fs::write(
            dir.join("instance_manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        assert_eq!(
            game_instance::get_manifest(&self.ctx, instance_id)
                .unwrap()
                .saves,
            saves
        );
    }

    fn write_save(&self, relative: &str, contents: &[u8]) {
        let path = self.shared_saves().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn read_save(&self, relative: &str) -> Vec<u8> {
        std::fs::read(self.shared_saves().join(relative)).unwrap()
    }

    fn backup(&self, instance_id: &str, def: &GameDefinition) -> save_backup::SaveBackup {
        save_backup::backup(&self.ctx, instance_id, def).unwrap()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        std::env::remove_var("AGORA_TEST_USER_DATA_ROOT");
    }
}

#[test]
fn a_backup_then_a_restore_gives_back_the_exact_files() {
    let h = Harness::new();
    let def = skyrim();
    h.make_instance("exact", SavesChoice::Shared);
    h.write_save("Run.ess", b"original save");
    h.write_save("Run.skse", b"original skse");
    h.write_save("Backup/Deep.ess", b"nested save");

    let backup = h.backup("exact", &def);
    assert_eq!(backup.file_count, 3);
    assert_eq!(
        backup.bytes,
        (b"original save".len() + b"original skse".len() + b"nested save".len()) as u64
    );

    h.write_save("Run.ess", b"changed later");
    h.write_save("Run.skse", b"changed later");
    h.write_save("Backup/Deep.ess", b"changed later");

    let restore = save_backup::restore(&h.ctx, "exact", &def, &backup.id).unwrap();
    assert_eq!(restore.restored, 3);
    assert_eq!(h.read_save("Run.ess"), b"original save");
    assert_eq!(h.read_save("Run.skse"), b"original skse");
    assert_eq!(h.read_save("Backup/Deep.ess"), b"nested save");
}

#[test]
fn restore_keeps_a_newer_save_and_the_current_copy_of_an_overwritten_one() {
    let h = Harness::new();
    let def = skyrim();
    h.make_instance("keep", SavesChoice::Shared);
    h.write_save("Overwritten.ess", b"backed up save");

    let backup = h.backup("keep", &def);

    h.write_save("Overwritten.ess", b"the save as it is now");
    h.write_save("MadeAfter.ess", b"a save the test did not have");

    let restore = save_backup::restore(&h.ctx, "keep", &def, &backup.id).unwrap();

    assert_eq!(h.read_save("Overwritten.ess"), b"backed up save");
    assert_eq!(
        h.read_save("Overwritten.ess.before-restore"),
        b"the save as it is now"
    );
    // A save made after the backup is not deleted or rewritten.
    assert_eq!(
        h.read_save("MadeAfter.ess"),
        b"a save the test did not have"
    );
    assert_eq!(restore.kept_current.len(), 1);
    assert!(restore.kept_current[0].ends_with("Overwritten.ess.before-restore"));
}

#[test]
fn restoring_the_same_backup_twice_keeps_both_current_copies() {
    let h = Harness::new();
    let def = skyrim();
    h.make_instance("twice", SavesChoice::Shared);
    h.write_save("Run.ess", b"first");

    let backup = h.backup("twice", &def);
    h.write_save("Run.ess", b"second");
    save_backup::restore(&h.ctx, "twice", &def, &backup.id).unwrap();

    h.write_save("Run.ess", b"third");
    save_backup::restore(&h.ctx, "twice", &def, &backup.id).unwrap();

    assert_eq!(h.read_save("Run.ess.before-restore"), b"second");
    assert_eq!(h.read_save("Run.ess.before-restore-2"), b"third");
    assert_eq!(h.read_save("Run.ess"), b"first");
}

#[test]
fn no_save_folder_means_no_backup() {
    let h = Harness::new();
    let def = skyrim();
    h.make_instance("no-folder", SavesChoice::Own);

    let error = save_backup::backup(&h.ctx, "no-folder", &def).unwrap_err();
    assert!(matches!(error, SaveBackupError::NoSaveFolder));
    assert!(save_backup::list(&h.ctx, "no-folder").unwrap().is_empty());
}

#[test]
fn an_empty_save_folder_is_reported_not_backed_up_as_healthy() {
    let h = Harness::new();
    let def = skyrim();
    h.make_instance("empty", SavesChoice::Shared);
    std::fs::create_dir_all(h.shared_saves()).unwrap();

    let error = save_backup::backup(&h.ctx, "empty", &def).unwrap_err();
    assert!(matches!(error, SaveBackupError::EmptySaveFolder));
    assert!(save_backup::list(&h.ctx, "empty").unwrap().is_empty());
}

#[test]
fn a_failed_copy_leaves_no_half_backup_counted_as_good() {
    let h = Harness::new();
    let def = skyrim();
    h.make_instance("broken-store", SavesChoice::Shared);
    h.write_save("Run.ess", b"a save");
    // A file where the backup store's folder belongs makes the staging copy impossible.
    std::fs::write(
        h.instance_dir("broken-store").join(".agora_save_backups"),
        b"not a folder",
    )
    .unwrap();

    let error = save_backup::backup(&h.ctx, "broken-store", &def).unwrap_err();
    assert!(matches!(error, SaveBackupError::Write { .. }));
    assert!(save_backup::list(&h.ctx, "broken-store")
        .unwrap()
        .is_empty());
}

#[test]
fn retention_drops_the_oldest_backup() {
    let h = Harness::new();
    let def = skyrim();
    h.make_instance("retained", SavesChoice::Shared);
    h.write_save("Run.ess", b"a save");

    let mut ids = Vec::new();
    for _ in 0..=MAX_BACKUPS {
        ids.push(h.backup("retained", &def).id);
    }

    let backups = save_backup::list(&h.ctx, "retained").unwrap();
    assert_eq!(backups.len(), MAX_BACKUPS);
    let kept: Vec<&str> = backups.iter().map(|backup| backup.id.as_str()).collect();
    assert!(
        !kept.contains(&ids[0].as_str()),
        "the oldest backup was kept"
    );
    for id in &ids[1..] {
        assert!(kept.contains(&id.as_str()), "backup {id} was dropped early");
    }
}

#[test]
fn a_restore_is_refused_while_the_game_is_running_and_changes_nothing() {
    let h = Harness::new();
    let def = skyrim();
    h.make_instance("running", SavesChoice::Shared);
    h.write_save("Run.ess", b"before the test");
    let backup = h.backup("running", &def);
    h.write_save("Run.ess", b"written while the game ran");

    let generation =
        agora_core::instance_runtime::publish_starting(&h.ctx.paths, "running").unwrap();
    let error = save_backup::restore(&h.ctx, "running", &def, &backup.id).unwrap_err();
    assert!(
        matches!(error, SaveBackupError::Other(_)),
        "a restore under a running game must be refused, got {error:?}"
    );
    assert_eq!(
        h.read_save("Run.ess"),
        b"written while the game ran",
        "a refused restore must not touch a save"
    );

    agora_core::instance_runtime::clear(&h.ctx.paths, "running", Some(&generation)).unwrap();
    let restore = save_backup::restore(&h.ctx, "running", &def, &backup.id).unwrap();
    assert_eq!(restore.restored, 1);
    assert_eq!(h.read_save("Run.ess"), b"before the test");
}

#[test]
fn list_is_empty_before_any_backup_and_unknown_backups_are_errors() {
    let h = Harness::new();
    let def = skyrim();
    h.make_instance("never-backed-up", SavesChoice::Shared);
    assert!(save_backup::list(&h.ctx, "never-backed-up")
        .unwrap()
        .is_empty());

    let error = save_backup::restore(&h.ctx, "never-backed-up", &def, "missing").unwrap_err();
    assert!(matches!(error, SaveBackupError::BackupNotFound(_)));
    let error = save_backup::restore(&h.ctx, "never-backed-up", &def, "../escape").unwrap_err();
    assert!(matches!(error, SaveBackupError::InvalidBackupId(_)));
}
