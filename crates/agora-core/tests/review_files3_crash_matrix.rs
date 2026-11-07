//! Third-pass review of the hash-first user-file journal (d75f4914): a crash/error matrix over every step of swap-in and restore.
#![allow(unused_imports, dead_code)]
use std::path::PathBuf;
use std::sync::Mutex;

use agora_core::ctx::CoreContext;
use agora_core::game_user_files::{
    is_session_running, record_process, restore, status, swap_in, Journal, JournaledFile,
};
use agora_core::process_identity;
use agora_game_api::{
    DeploymentStrategy, GameDefinition, GameId, GamePath, RelPath, StoreId, StoreIdentifier,
    UserDataLocation, UserFileMapping, UserFileStrategy,
};
use tempfile::TempDir;

static TEST_LOCK: Mutex<()> = Mutex::new(());

struct TestHarness {
    _lock: std::sync::MutexGuard<'static, ()>,
    _data_dir: TempDir,
    user_data_dir: TempDir,
    ctx: CoreContext,
    running_from: PathBuf,
}

impl TestHarness {
    fn new() -> Self {
        let lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let data_dir = TempDir::new().unwrap();
        let user_data_dir = TempDir::new().unwrap();

        std::env::set_var("AGORA_TEST_USER_DATA_ROOT", user_data_dir.path());

        let ctx = CoreContext::for_testing(data_dir.path().join("app_data"));
        let _ = agora_core::db::init_local_state_db(&ctx.paths.local_state_db());

        let running_from = data_dir.path().join("running_from");
        std::fs::create_dir_all(&running_from).unwrap();

        Self {
            _lock: lock,
            _data_dir: data_dir,
            user_data_dir,
            ctx,
            running_from,
        }
    }

    fn local_dir(&self) -> PathBuf {
        self.user_data_dir.path().join("local")
    }

    fn documents_dir(&self) -> PathBuf {
        self.user_data_dir.path().join("documents")
    }

    fn instance_dir(&self, id: &str) -> PathBuf {
        let dir = self.ctx.paths.instances_root().join(id);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}

impl Drop for TestHarness {
    fn drop(&mut self) {
        std::env::remove_var("AGORA_TEST_USER_DATA_ROOT");
    }
}

fn test_skyrim_definition() -> GameDefinition {
    GameDefinition {
        mo2_game_name: None,
        session: Default::default(),
        id: GameId::new("skyrim-se").unwrap(),
        extends: None,
        name: "The Elder Scrolls V: Skyrim Special Edition".to_string(),
        stores: vec![
            StoreIdentifier {
                store: StoreId::steam(),
                product: "489830".into(),
            },
            StoreIdentifier {
                store: StoreId::gog(),
                product: "1711230643".into(),
            },
        ],
        version_sources: vec![],
        deployment: DeploymentStrategy::VirtualFileSystem,
        content_rules: vec![],
        framework_ids: vec![],
        tool_ids: vec![],
        launch: None,
        log_paths: vec![],
        crash_paths: vec![],
        user_files: vec![
            UserFileMapping {
                source: GamePath::UserData {
                    location: UserDataLocation::LocalAppData,
                    path: RelPath::new("Skyrim Special Edition/Plugins.txt").unwrap(),
                },
                instance_path: RelPath::new("user/Plugins.txt").unwrap(),
                strategy: UserFileStrategy::JournaledSwap,
                stores: vec![StoreId::steam()],
            },
            UserFileMapping {
                source: GamePath::UserData {
                    location: UserDataLocation::Documents,
                    path: RelPath::new("My Games/Skyrim Special Edition/Skyrim.ini").unwrap(),
                },
                instance_path: RelPath::new("user/Skyrim.ini").unwrap(),
                strategy: UserFileStrategy::JournaledSwap,
                stores: vec![StoreId::steam()],
            },
            UserFileMapping {
                source: GamePath::UserData {
                    location: UserDataLocation::LocalAppData,
                    path: RelPath::new("Skyrim Special Edition GOG/Plugins.txt").unwrap(),
                },
                instance_path: RelPath::new("user/Plugins.txt").unwrap(),
                strategy: UserFileStrategy::JournaledSwap,
                stores: vec![StoreId::gog()],
            },
            UserFileMapping {
                source: GamePath::UserData {
                    location: UserDataLocation::Documents,
                    path: RelPath::new("My Games/Skyrim Special Edition GOG/Skyrim.ini").unwrap(),
                },
                instance_path: RelPath::new("user/Skyrim.ini").unwrap(),
                strategy: UserFileStrategy::JournaledSwap,
                stores: vec![StoreId::gog()],
            },
        ],
        save_paths: vec![],
        linked_archive_patterns: vec![],
        declared_writes: vec![],
        excluded_paths: vec![],
        load_orders: Vec::new(),
        runtime_files: Vec::new(),
        save_location: Vec::new(),
        launch_alternatives: Vec::new(),
        content_layout: None,
        copy_patterns: Vec::new(),
    }
}

fn journal_path_of(h: &TestHarness, def: &GameDefinition) -> PathBuf {
    h.ctx
        .paths
        .user_files_journal_path(def.id.as_str(), StoreId::steam().as_str())
}

fn backup_dir_of(h: &TestHarness, def: &GameDefinition) -> PathBuf {
    h.ctx
        .paths
        .user_files_backup_dir(def.id.as_str(), StoreId::steam().as_str())
}

fn plugins_real(h: &TestHarness) -> PathBuf {
    h.local_dir()
        .join("Skyrim Special Edition")
        .join("Plugins.txt")
}

fn ini_real(h: &TestHarness) -> PathBuf {
    h.documents_dir()
        .join("My Games")
        .join("Skyrim Special Edition")
        .join("Skyrim.ini")
}

fn put(path: &PathBuf, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// Put the disk in the state a stop leaves after the backup copy of entry 0 and before the journal
// ---------------------------------------------------------------------------
// Hand-built on-disk states
// ---------------------------------------------------------------------------
use agora_core::game_user_files::FileState;
use sha2::{Digest, Sha256};

fn sha(b: &[u8]) -> String {
    format!("{:x}", Sha256::digest(b))
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Stage {
    // swap-in: where a stop leaves one entry
    Initial,    // only the initial journal record exists
    Hashed,     // journal has original hash, state backing_up, no copy yet
    TornTmp,    // + a half-written <i>.tmp
    BackupDone, // + verified backup renamed to <i>, journal still backing_up
    BackedUp,   // journal backed_up
    SwOrig,     // journal swapped, real file not yet written
    SwWritten,  // real file holds the instance's bytes, session not started
    SwGame,     // the game has changed the file (a normal finished session)
    // restore: where a stop leaves one entry that was in SwGame
    RInstTorn,  // instance copy half-written from the real file
    RInstSaved, // instance copy saved, real file still the game's
    RRealOrig,  // real file back to original, restored flag not yet written
    RDone,      // restored flag written, backup gone
    // torn writes of the real file (bugs)
    SwTorn,
    RRealTorn,
}
use Stage::*;

fn orig(i: usize) -> String {
    format!("original-{i}\nsecond line of the player's file\nthird line\n")
}
fn inst(i: usize) -> String {
    format!("instance-{i}\nthe instance's saved settings, longer than the torn prefix\n")
}
fn game(i: usize) -> String {
    format!("game-changed-{i}\nwhat the session wrote, all of it\nlast line\n")
}

fn real_of(h: &TestHarness, i: usize) -> PathBuf {
    if i == 0 {
        plugins_real(h)
    } else {
        ini_real(h)
    }
}
fn inst_of(h: &TestHarness, id: &str, i: usize) -> PathBuf {
    let n = if i == 0 { "Plugins.txt" } else { "Skyrim.ini" };
    h.instance_dir(id).join("user").join(n)
}
fn def_with(n: usize) -> GameDefinition {
    let mut d = test_skyrim_definition();
    d.user_files.truncate(n);
    d
}

fn build(h: &TestHarness, def: &GameDefinition, id: &str, stages: &[Stage]) {
    let bd = backup_dir_of(h, def);
    std::fs::create_dir_all(&bd).unwrap();
    let mut files = Vec::new();
    for (i, st) in stages.iter().enumerate() {
        let (o, x, g) = (orig(i), inst(i), game(i));
        let real = real_of(h, i);
        let ic = inst_of(h, id, i);
        let bp = bd.join(format!("{i}"));
        put(&real, &o);
        put(&ic, &x);
        let mut f = JournaledFile {
            real_path: real.clone(),
            instance_path: def.user_files[i].instance_path.clone(),
            instance_copy_path: ic.clone(),
            existed: false,
            backup_path: None,
            original_sha256: None,
            written_sha256: String::new(),
            swapped: false,
            state: Some(FileState::BackingUp),
            restored: false,
        };
        if *st != Initial {
            f.existed = true;
            f.original_sha256 = Some(sha(o.as_bytes()));
        }
        if *st == TornTmp {
            std::fs::write(bd.join(format!("{i}.tmp")), &o[..8]).unwrap();
        }
        if !matches!(st, Initial | Hashed | TornTmp) {
            std::fs::write(&bp, &o).unwrap();
        }
        if !matches!(st, Initial | Hashed | TornTmp | BackupDone) {
            f.backup_path = Some(bp.clone());
            f.state = Some(FileState::BackedUp);
        }
        if matches!(
            st,
            SwOrig
                | SwWritten
                | SwGame
                | RInstTorn
                | RInstSaved
                | RRealOrig
                | RDone
                | SwTorn
                | RRealTorn
        ) {
            f.state = Some(FileState::Swapped);
            f.swapped = true;
            f.written_sha256 = sha(x.as_bytes());
        }
        match st {
            SwWritten => std::fs::write(&real, &x).unwrap(),
            SwGame => std::fs::write(&real, &g).unwrap(),
            SwTorn => std::fs::write(&real, &x[..6]).unwrap(),
            RInstTorn => {
                std::fs::write(&real, &g).unwrap();
                std::fs::write(&ic, &g[..6]).unwrap();
            }
            RInstSaved => {
                std::fs::write(&real, &g).unwrap();
                std::fs::write(&ic, &g).unwrap();
            }
            RRealOrig => std::fs::write(&ic, &g).unwrap(),
            RDone => {
                std::fs::write(&ic, &g).unwrap();
                std::fs::remove_file(&bp).unwrap();
                f.restored = true;
            }
            RRealTorn => {
                std::fs::write(&real, &o[..6]).unwrap();
                std::fs::write(&ic, &g).unwrap();
            }
            _ => {}
        }
        files.push(f);
    }
    let journal = Journal {
        game: def.id.clone(),
        store: StoreId::steam(),
        instance_id: id.to_string(),
        created_at: "2026-01-01T00:00:00Z".into(),
        running_from: h.running_from.clone(),
        processes: vec![],
        starting_launcher: None,
        files,
    };
    let jp = journal_path_of(h, def);
    std::fs::create_dir_all(jp.parent().unwrap()).unwrap();
    std::fs::write(&jp, serde_json::to_string_pretty(&journal).unwrap()).unwrap();
}

fn expected_inst(st: Stage, i: usize) -> String {
    match st {
        SwGame | RInstTorn | RInstSaved | RRealOrig | RDone | RRealTorn => game(i),
        _ => inst(i),
    }
}

/// Recover from the hand-built state and report every violated invariant.
fn recover_and_check(stages: &[Stage]) -> Vec<String> {
    let h = TestHarness::new();
    let def = def_with(stages.len());
    let id = "m";
    build(&h, &def, id, stages);
    let mut bad = Vec::new();
    match restore(&h.ctx, &def, &StoreId::steam()) {
        Ok(_) => {}
        Err(e) => bad.push(format!("restore failed: {e}")),
    }
    for (i, st) in stages.iter().enumerate() {
        let real = std::fs::read_to_string(real_of(&h, i)).unwrap_or_default();
        if real != orig(i) {
            bad.push(format!(
                "entry {i} ({st:?}): real file is {real:?}, not the original"
            ));
        }
        let ic = std::fs::read_to_string(inst_of(&h, id, i)).unwrap_or_default();
        if ic != expected_inst(*st, i) {
            bad.push(format!(
                "entry {i} ({st:?}): instance copy is {ic:?}, expected {:?}",
                expected_inst(*st, i)
            ));
        }
    }
    if journal_path_of(&h, &def).exists() {
        bad.push("journal still present".into());
    }
    // A second recovery is a clean no-op.
    if !matches!(
        restore(&h.ctx, &def, &StoreId::steam()),
        Err(agora_core::game_user_files::UserFilesError::NoSession { .. })
    ) {
        bad.push("second restore was not NoSession".into());
    }
    bad
}

fn swap_side() -> Vec<Stage> {
    vec![
        Initial, Hashed, TornTmp, BackupDone, BackedUp, SwOrig, SwWritten, SwGame,
    ]
}

#[test]
fn matrix_one_entry_every_stop_point_recovers_to_original_and_keeps_session_data() {
    let mut all = Vec::new();
    for st in swap_side()
        .into_iter()
        .chain([RInstTorn, RInstSaved, RRealOrig, RDone])
    {
        for b in recover_and_check(&[st]) {
            all.push(format!("[{st:?}] {b}"));
        }
    }
    assert!(all.is_empty(), "{all:#?}");
}

#[test]
fn matrix_two_entries_every_stop_point_recovers_to_original_and_keeps_session_data() {
    let mut pairs: Vec<[Stage; 2]> = Vec::new();
    // swap-in: entry 0 mid-way, entry 1 untouched
    for s in swap_side().into_iter().filter(|s| *s != SwGame) {
        pairs.push([s, Initial]);
    }
    // swap-in: entry 0 done, entry 1 mid-way
    for s in swap_side().into_iter().filter(|s| *s != SwGame) {
        pairs.push([SwWritten, s]);
    }
    // restore: entry 0 mid-way, entry 1 not started; then entry 1 mid-way after entry 0 is done
    for s in [SwGame, RInstTorn, RInstSaved, RRealOrig, RDone] {
        pairs.push([s, SwGame]);
    }
    for s in [RInstTorn, RInstSaved, RRealOrig, RDone] {
        pairs.push([RDone, s]);
    }
    let mut all = Vec::new();
    for p in pairs {
        for b in recover_and_check(&p) {
            all.push(format!("{p:?}: {b}"));
        }
    }
    assert!(all.is_empty(), "{all:#?}");
}

#[test]
fn bug_torn_real_file_after_swap_in_write_overwrites_instance_saved_copy() {
    let bad = recover_and_check(&[SwTorn]);
    assert!(bad.is_empty(), "{bad:#?}");
}

#[test]
fn bug_torn_real_file_during_restore_overwrites_the_sessions_saved_copy() {
    let bad = recover_and_check(&[RRealTorn]);
    assert!(bad.is_empty(), "{bad:#?}");
}

#[test]
fn bug_torn_new_real_file_after_swap_in_write_overwrites_instance_saved_copy() {
    let h = TestHarness::new();
    let def = def_with(1);
    let real = real_of(&h, 0);
    let ic = inst_of(&h, "n", 0);
    let x = inst(0);
    put(&ic, &x);
    // journal: existed=false, swapped, written = hash(x); the write of x to real was cut short.
    let journal = Journal {
        game: def.id.clone(),
        store: StoreId::steam(),
        instance_id: "n".into(),
        created_at: "2026-01-01T00:00:00Z".into(),
        running_from: h.running_from.clone(),
        processes: vec![],
        starting_launcher: None,
        files: vec![JournaledFile {
            real_path: real.clone(),
            instance_path: def.user_files[0].instance_path.clone(),
            instance_copy_path: ic.clone(),
            existed: false,
            backup_path: None,
            original_sha256: None,
            written_sha256: sha(x.as_bytes()),
            swapped: true,
            state: Some(FileState::Swapped),
            restored: false,
        }],
    };
    let jp = journal_path_of(&h, &def);
    std::fs::create_dir_all(jp.parent().unwrap()).unwrap();
    std::fs::write(&jp, serde_json::to_string(&journal).unwrap()).unwrap();
    put(&real, &x[..6]);
    restore(&h.ctx, &def, &StoreId::steam()).unwrap();
    assert!(!real.exists());
    assert_eq!(
        std::fs::read_to_string(&ic).unwrap(),
        x,
        "instance copy replaced by a torn file"
    );
}

// ---------------------------------------------------------------------------
// Journals from the pre-hash-first format (655b29eb and earlier): no `state`, no `restored`
// ---------------------------------------------------------------------------

fn old_journal(h: &TestHarness, def: &GameDefinition, id: &str, swapped: bool) {
    let (o, x) = (orig(0), inst(0));
    let mut f = serde_json::json!({
        "real_path": plugins_real(h),
        "instance_path": "user/Plugins.txt",
        "instance_copy_path": inst_of(h, id, 0),
        "existed": swapped,
        "written_sha256": if swapped { sha(x.as_bytes()) } else { String::new() },
        "swapped": swapped,
    });
    if swapped {
        f["backup_path"] = serde_json::json!(backup_dir_of(h, def).join("0"));
        f["original_sha256"] = serde_json::json!(sha(o.as_bytes()));
    }
    let j = serde_json::json!({
        "game": def.id, "store": "steam", "instance_id": id,
        "created_at": "2026-01-01T00:00:00Z", "running_from": h.running_from,
        "processes": [], "files": [f],
    });
    let jp = journal_path_of(h, def);
    std::fs::create_dir_all(jp.parent().unwrap()).unwrap();
    std::fs::write(&jp, serde_json::to_string(&j).unwrap()).unwrap();
}

/// The player's original must survive somewhere, the instance copy must not be lost, and when
/// recovery refuses, the error must name where the copies are.
fn check_old(label: &str, real_now: &str, backup: Option<&str>, swapped: bool) {
    let h = TestHarness::new();
    let def = def_with(1);
    let id = "o";
    put(&inst_of(&h, id, 0), &inst(0));
    put(&real_of(&h, 0), real_now);
    std::fs::create_dir_all(backup_dir_of(&h, &def)).unwrap();
    if let Some(b) = backup {
        std::fs::write(backup_dir_of(&h, &def).join("0"), b).unwrap();
    }
    old_journal(&h, &def, id, swapped);
    let res = restore(&h.ctx, &def, &StoreId::steam());
    let real = std::fs::read_to_string(real_of(&h, 0)).unwrap_or_default();
    let bk = std::fs::read_to_string(backup_dir_of(&h, &def).join("0")).ok();
    assert!(
        real == orig(0) || bk.as_deref() == Some(orig(0).as_str()),
        "{label}: the original is nowhere (real={real:?}, backup={bk:?}, res={res:?})"
    );
    let ic = std::fs::read_to_string(inst_of(&h, id, 0)).unwrap();
    if !swapped {
        assert_eq!(ic, inst(0), "{label}: instance copy changed");
    }
    if let Err(e) = &res {
        let msg = e.to_string();
        assert!(
            msg.contains("Plugins.txt")
                && msg.contains(&format!("{}", backup_dir_of(&h, &def).display())),
            "{label}: error does not name the real file and the backup: {msg}"
        );
    }
}

#[test]
fn old_format_every_stop_point_keeps_the_original_and_names_paths_when_refusing() {
    let o = orig(0);
    let x = inst(0);
    // swapped=false: stop before the journal learned of the backup
    check_old("no backup yet", &o, None, false);
    check_old("complete backup, real untouched", &o, Some(&o), false);
    check_old("torn backup, real untouched", &o, Some(&o[..10]), false);
    check_old(
        "complete backup, real overwritten (stop between write and journal)",
        &x,
        Some(&o),
        false,
    );
    check_old(
        "complete backup, real torn mid-write",
        &x[..6],
        Some(&o),
        false,
    );
    // swapped=true: normal finished swap with the game's changes
    check_old("old swapped, game changed", &game(0), Some(&o), true);
}
