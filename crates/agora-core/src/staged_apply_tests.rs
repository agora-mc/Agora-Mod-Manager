//! Staged apply's own rules (MASTER_SPEC §32.4): one durable way to put files into an instance.
//!
//! Written before the code. The promise under test is "the old instance or the new one after
//! successful recovery, never a mix", with the commit point at the journal's `committed` record,
//! and "files the user changed are never deleted".

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use sha2::{Digest, Sha256};

use crate::ctx::Ctx;
use crate::instance_runtime::InstanceGuard;
use crate::lock_manager::{LockManager, LockResource};
use crate::staged_apply::{self, Boundary, Expect, Stage, StagedRecovery};
use agora_game_api::{RelPath, StrictRelPath};

const ID: &str = "staged";

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn rel(path: &str) -> StrictRelPath {
    RelPath::strict(path).unwrap()
}

fn fixture() -> (tempfile::TempDir, Ctx, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::for_testing(dir.path().join("app"));
    crate::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
    let root = ctx.paths.instance_dir(ID).unwrap();
    fs::create_dir_all(&root).unwrap();
    (dir, ctx, root)
}

fn put(root: &Path, path: &str, bytes: &[u8]) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

/// Every file under `root`, by its `/` path, with its bytes. Empty folders do not count.
fn tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut folders = vec![root.to_path_buf()];
    while let Some(folder) = folders.pop() {
        for entry in fs::read_dir(&folder).unwrap() {
            let path = entry.unwrap().path();
            let kind = fs::symlink_metadata(&path).unwrap();
            if kind.is_dir() {
                folders.push(path);
            } else {
                let name = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                files.insert(name, fs::read(&path).unwrap());
            }
        }
    }
    files
}

fn without(mut files: BTreeMap<String, Vec<u8>>, prefix: &str) -> BTreeMap<String, Vec<u8>> {
    files.retain(|name, _| !name.starts_with(prefix));
    files
}

fn files(pairs: &[(&str, &[u8])]) -> BTreeMap<String, Vec<u8>> {
    pairs
        .iter()
        .map(|(name, bytes)| (name.to_string(), bytes.to_vec()))
        .collect()
}

fn guard(ctx: &Ctx) -> InstanceGuard {
    InstanceGuard::for_mutation(ctx, ID, "staged apply test").unwrap()
}

fn no_stage_left(root: &Path) {
    let stage = staged_apply::stage_root(root);
    let left = if stage.exists() {
        tree(&stage)
    } else {
        BTreeMap::new()
    };
    assert!(
        left.is_empty(),
        "nothing is left in the stage: {:?}",
        left.keys()
    );
}

// ---------------------------------------------------------------------------
// 1. What a commit does
// ---------------------------------------------------------------------------

#[test]
fn a_commit_adds_replaces_removes_and_renames() {
    let (_dir, ctx, root) = fixture();
    put(&root, "mods/old.jar", b"old");
    put(&root, "config/gone.txt", b"gone");
    put(&root, "notes/a.txt", b"moving");
    let guard = guard(&ctx);

    let mut stage = Stage::begin(&guard, &root).unwrap();
    stage
        .write(&rel("mods/new.jar"), b"new", Expect::Absent)
        .unwrap();
    stage
        .write(&rel("mods/old.jar"), b"old-v2", Expect::Agora(sha(b"old")))
        .unwrap();
    stage
        .remove(&rel("config/gone.txt"), Expect::Agora(sha(b"gone")))
        .unwrap();
    stage
        .rename(&rel("notes/a.txt"), &rel("notes/b.txt"))
        .unwrap();
    let report = stage.commit().unwrap();

    assert!(report.kept_aside.is_empty(), "{report:?}");
    assert_eq!(
        tree(&root),
        files(&[
            ("mods/new.jar", b"new"),
            ("mods/old.jar", b"old-v2"),
            ("notes/b.txt", b"moving"),
        ])
    );
    no_stage_left(&root);
}

#[test]
fn a_commit_with_nothing_staged_changes_nothing_and_leaves_nothing() {
    let (_dir, ctx, root) = fixture();
    put(&root, "mods/keep.jar", b"keep");
    let guard = guard(&ctx);

    let report = Stage::begin(&guard, &root).unwrap().commit().unwrap();

    assert!(report.kept_aside.is_empty());
    assert_eq!(tree(&root), files(&[("mods/keep.jar", b"keep")]));
    no_stage_left(&root);
}

#[test]
fn a_dropped_stage_changes_nothing_and_leaves_nothing() {
    let (_dir, ctx, root) = fixture();
    put(&root, "mods/keep.jar", b"keep");
    let guard = guard(&ctx);
    {
        let mut stage = Stage::begin(&guard, &root).unwrap();
        stage
            .write(&rel("mods/keep.jar"), b"never", Expect::Agora(sha(b"keep")))
            .unwrap();
    }
    assert_eq!(tree(&root), files(&[("mods/keep.jar", b"keep")]));
    no_stage_left(&root);
}

#[test]
fn adding_over_an_existing_file_is_refused_before_anything_changes() {
    let (_dir, ctx, root) = fixture();
    put(&root, "mods/taken.jar", b"theirs");
    put(&root, "mods/other.jar", b"other");
    let guard = guard(&ctx);

    let mut stage = Stage::begin(&guard, &root).unwrap();
    stage
        .remove(&rel("mods/other.jar"), Expect::Agora(sha(b"other")))
        .unwrap();
    stage
        .write(&rel("mods/taken.jar"), b"mine", Expect::Absent)
        .unwrap();
    assert!(stage.commit().is_err());

    assert_eq!(
        tree(&root),
        files(&[("mods/other.jar", b"other"), ("mods/taken.jar", b"theirs")]),
        "a refused commit changes nothing, including the steps staged before it"
    );
    no_stage_left(&root);
}

// ---------------------------------------------------------------------------
// 2. Files the user changed are never deleted
// ---------------------------------------------------------------------------

#[test]
fn a_replaced_file_the_user_changed_is_kept_aside_and_reported() {
    let (_dir, ctx, root) = fixture();
    put(&root, "mods/mine.jar", b"user edited");
    let guard = guard(&ctx);

    let mut stage = Stage::begin(&guard, &root).unwrap();
    stage
        .write(
            &rel("mods/mine.jar"),
            b"v2",
            Expect::Agora(sha(b"what Agora wrote")),
        )
        .unwrap();
    let report = stage.commit().unwrap();

    assert_eq!(fs::read(root.join("mods/mine.jar")).unwrap(), b"v2");
    assert_eq!(report.kept_aside.len(), 1, "{report:?}");
    let kept = &report.kept_aside[0];
    assert_eq!(kept.path.as_str(), "mods/mine.jar");
    assert_eq!(fs::read(&kept.kept_at).unwrap(), b"user edited");
    assert!(
        kept.kept_at.starts_with(&root),
        "kept inside the instance, where the person can find it"
    );
    no_stage_left(&root);
}

#[test]
fn a_removed_file_the_user_changed_is_kept_aside() {
    let (_dir, ctx, root) = fixture();
    put(&root, "config/mine.txt", b"user edited");
    let guard = guard(&ctx);

    let mut stage = Stage::begin(&guard, &root).unwrap();
    stage
        .remove(
            &rel("config/mine.txt"),
            Expect::Agora(sha(b"what Agora wrote")),
        )
        .unwrap();
    let report = stage.commit().unwrap();

    assert!(!root.join("config/mine.txt").exists());
    assert_eq!(report.kept_aside.len(), 1, "{report:?}");
    assert_eq!(
        fs::read(&report.kept_aside[0].kept_at).unwrap(),
        b"user edited"
    );
}

/// With no record of what Agora wrote, the file is the user's.
#[test]
fn a_file_agora_has_no_record_of_is_the_users() {
    let (_dir, ctx, root) = fixture();
    put(&root, "mods/unknown.jar", b"whose?");
    let guard = guard(&ctx);

    let mut stage = Stage::begin(&guard, &root).unwrap();
    stage
        .write(&rel("mods/unknown.jar"), b"v2", Expect::Unknown)
        .unwrap();
    let report = stage.commit().unwrap();

    assert_eq!(report.kept_aside.len(), 1, "{report:?}");
    assert_eq!(fs::read(&report.kept_aside[0].kept_at).unwrap(), b"whose?");
}

#[test]
fn an_unchanged_agora_file_is_replaced_without_a_copy_kept() {
    let (_dir, ctx, root) = fixture();
    put(&root, "mods/agora.jar", b"v1");
    let guard = guard(&ctx);

    let mut stage = Stage::begin(&guard, &root).unwrap();
    stage
        .write(&rel("mods/agora.jar"), b"v2", Expect::Agora(sha(b"v1")))
        .unwrap();
    let report = stage.commit().unwrap();

    assert!(report.kept_aside.is_empty());
    assert_eq!(tree(&root), files(&[("mods/agora.jar", b"v2")]));
}

// ---------------------------------------------------------------------------
// 3. A crash at each persistence boundary
// ---------------------------------------------------------------------------

/// The instance before the apply below.
fn old_instance(root: &Path) {
    put(root, "mods/agora.jar", b"agora-v1");
    put(root, "mods/mine.jar", b"user edited");
    put(root, "config/old.txt", b"old");
    put(root, "notes/a.txt", b"rename me");
}

/// The apply: one add, one replace of an unchanged file, one replace of a file the user changed,
/// one remove, one rename. Seven moves: 1 + 2 + 2 + 1 + 1.
fn stage_the_apply<'g>(guard: &'g InstanceGuard, root: &Path) -> Stage<'g> {
    let mut stage = Stage::begin(guard, root).unwrap();
    stage
        .write(&rel("mods/new.jar"), b"new", Expect::Absent)
        .unwrap();
    stage
        .write(
            &rel("mods/agora.jar"),
            b"agora-v2",
            Expect::Agora(sha(b"agora-v1")),
        )
        .unwrap();
    stage
        .write(
            &rel("mods/mine.jar"),
            b"mine-v2",
            Expect::Agora(sha(b"mine-v1")),
        )
        .unwrap();
    stage
        .remove(&rel("config/old.txt"), Expect::Agora(sha(b"old")))
        .unwrap();
    stage
        .rename(&rel("notes/a.txt"), &rel("notes/b.txt"))
        .unwrap();
    stage
}

fn old_tree() -> BTreeMap<String, Vec<u8>> {
    files(&[
        ("config/old.txt", b"old"),
        ("mods/agora.jar", b"agora-v1"),
        ("mods/mine.jar", b"user edited"),
        ("notes/a.txt", b"rename me"),
    ])
}

fn new_tree() -> BTreeMap<String, Vec<u8>> {
    files(&[
        ("mods/agora.jar", b"agora-v2"),
        ("mods/mine.jar", b"mine-v2"),
        ("mods/new.jar", b"new"),
        ("notes/b.txt", b"rename me"),
    ])
}

/// How many files anywhere under `root` hold the user's edit.
fn copies_of_the_users_edit(root: &Path) -> usize {
    tree(root)
        .values()
        .filter(|bytes| bytes.as_slice() == b"user edited")
        .count()
}

const ASIDE: &str = ".agora/moved-aside/";

fn boundaries() -> Vec<(Boundary, bool)> {
    let mut list = vec![(Boundary::AfterPlan, false)];
    list.extend((1..=7).map(|n| (Boundary::AfterMove(n), false)));
    list.push((Boundary::AfterCommit, true));
    list.extend((1..=2).map(|n| (Boundary::AfterCleanup(n), true)));
    list
}

#[test]
fn a_crash_at_each_boundary_recovers_to_the_old_instance_or_the_new_one() {
    for (boundary, committed) in boundaries() {
        let (_dir, ctx, root) = fixture();
        old_instance(&root);
        {
            let guard = guard(&ctx);
            let mut stage = stage_the_apply(&guard, &root);
            stage.crash_at(boundary);
            assert!(
                stage.commit().is_err(),
                "{boundary:?}: the seam must stop the commit (the apply has 7 moves and 2 backups)"
            );
        }

        let outcomes = staged_apply::recover(&ctx.lock_manager, ID, &root);
        assert!(
            outcomes.iter().all(|outcome| matches!(
                outcome,
                StagedRecovery::RolledBack { .. } | StagedRecovery::Finished { .. }
            )),
            "{boundary:?}: {outcomes:?}"
        );
        assert_eq!(outcomes.len(), 1, "{boundary:?}: {outcomes:?}");

        if committed {
            assert_eq!(
                without(tree(&root), ASIDE),
                new_tree(),
                "{boundary:?}: after the commit point recovery only finishes"
            );
            assert!(
                matches!(outcomes[0], StagedRecovery::Finished { .. }),
                "{boundary:?}"
            );
        } else {
            assert_eq!(
                tree(&root),
                old_tree(),
                "{boundary:?}: before the commit point recovery puts the old instance back"
            );
            assert!(
                matches!(outcomes[0], StagedRecovery::RolledBack { .. }),
                "{boundary:?}"
            );
        }
        assert_eq!(
            copies_of_the_users_edit(&root),
            1,
            "{boundary:?}: the user's edit is never lost and never duplicated"
        );
        no_stage_left(&root);
        assert!(
            staged_apply::recover(&ctx.lock_manager, ID, &root).is_empty(),
            "{boundary:?}: a second recovery finds nothing to do"
        );
    }
}

#[test]
fn a_recovery_interrupted_by_another_crash_resumes() {
    for undo in 1..=6 {
        let (_dir, ctx, root) = fixture();
        old_instance(&root);
        {
            let guard = guard(&ctx);
            let mut stage = stage_the_apply(&guard, &root);
            stage.crash_at(Boundary::AfterMove(7));
            assert!(stage.commit().is_err());
        }
        // The first recovery is itself cut off after `undo` of its moves.
        let first = staged_apply::recover_crashing_after(&ctx.lock_manager, ID, &root, undo);
        assert!(
            first
                .iter()
                .all(|outcome| !matches!(outcome, StagedRecovery::RolledBack { .. })),
            "undo {undo}: the cut-off recovery did not finish: {first:?}"
        );

        let second = staged_apply::recover(&ctx.lock_manager, ID, &root);
        assert!(
            matches!(second.as_slice(), [StagedRecovery::RolledBack { .. }]),
            "undo {undo}: {second:?}"
        );
        assert_eq!(tree(&root), old_tree(), "undo {undo}");
        no_stage_left(&root);
    }
}

#[test]
fn starting_a_new_apply_first_settles_an_unresolved_one() {
    let (_dir, ctx, root) = fixture();
    old_instance(&root);
    {
        let guard = guard(&ctx);
        let mut stage = stage_the_apply(&guard, &root);
        stage.crash_at(Boundary::AfterMove(3));
        assert!(stage.commit().is_err());
    }

    let guard = guard(&ctx);
    let mut stage = Stage::begin(&guard, &root).unwrap();
    assert_eq!(
        without(tree(&root), ".agora_stage/"),
        old_tree(),
        "begin rolled the cut-off apply back first (the new stage's own journal aside)"
    );
    stage
        .write(&rel("mods/after.jar"), b"after", Expect::Absent)
        .unwrap();
    stage.commit().unwrap();
    let mut expected = old_tree();
    expected.insert("mods/after.jar".into(), b"after".to_vec());
    assert_eq!(tree(&root), expected);
    no_stage_left(&root);
}

// ---------------------------------------------------------------------------
// 4. Who may recover, and what it may touch
// ---------------------------------------------------------------------------

/// Another Agora holds the instance lock: its apply is live, and recovery must not undo it.
#[test]
fn recovery_leaves_a_live_apply_alone() {
    let (_dir, ctx, root) = fixture();
    old_instance(&root);
    {
        let guard = guard(&ctx);
        let mut stage = stage_the_apply(&guard, &root);
        stage.crash_at(Boundary::AfterMove(4));
        assert!(stage.commit().is_err());
    }
    let midway = tree(&root);

    let locks_root = ctx.paths.locks_root();
    let (held_tx, held_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        let manager = LockManager::new(locks_root);
        let _guard = manager
            .acquire(LockResource::Instance(ID.into()), "another Agora's apply")
            .unwrap();
        held_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    held_rx.recv().unwrap();

    let outcomes = staged_apply::recover(&ctx.lock_manager, ID, &root);
    release_tx.send(()).unwrap();
    holder.join().unwrap();

    assert!(
        matches!(outcomes.as_slice(), [StagedRecovery::Busy(_)]),
        "{outcomes:?}"
    );
    assert_eq!(tree(&root), midway, "nothing was touched");
}

/// The instance folder was replaced by another one with the same name after the crash: the
/// journal's files are not provably this folder's, so nothing is touched.
#[test]
fn a_replaced_instance_folder_needs_a_hand() {
    let (dir, ctx, root) = fixture();
    old_instance(&root);
    {
        let guard = guard(&ctx);
        let mut stage = stage_the_apply(&guard, &root);
        stage.crash_at(Boundary::AfterMove(4));
        assert!(stage.commit().is_err());
    }
    let moved = dir.path().join("moved-away");
    fs::rename(&root, &moved).unwrap();
    copy_tree(&moved, &root);
    let before = tree(&root);

    let outcomes = staged_apply::recover(&ctx.lock_manager, ID, &root);

    assert!(
        matches!(outcomes.as_slice(), [StagedRecovery::NeedsHand(_)]),
        "{outcomes:?}"
    );
    assert_eq!(tree(&root), before, "nothing was touched");
}

#[test]
fn a_link_inside_the_instance_is_refused_before_anything_changes() {
    let (dir, ctx, root) = fixture();
    put(&root, "config/keep.txt", b"keep");
    let outside = dir.path().join("outside-mods");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("theirs.jar"), b"outside").unwrap();
    assert!(
        make_folder_link(&outside, &root.join("mods")),
        "this process cannot create a folder link, so the check must not be skipped silently"
    );
    let guard = guard(&ctx);

    let mut stage = Stage::begin(&guard, &root).unwrap();
    stage
        .remove(&rel("config/keep.txt"), Expect::Agora(sha(b"keep")))
        .unwrap();
    // Either the staging call or the commit may refuse; neither may write through the link.
    let refused = stage
        .write(&rel("mods/through.jar"), b"x", Expect::Absent)
        .is_err()
        || stage.commit().is_err();

    assert!(refused, "a write through a link must be refused");
    assert_eq!(
        tree(&outside),
        files(&[("theirs.jar", b"outside")]),
        "nothing was written outside the instance"
    );
    assert_eq!(fs::read(root.join("config/keep.txt")).unwrap(), b"keep");
}

/// A stage left by a process that crashed right after `begin` (journal with nothing in it).
#[test]
fn a_stage_left_right_after_begin_is_cleared() {
    let (_dir, ctx, root) = fixture();
    put(&root, "mods/keep.jar", b"keep");
    {
        let guard = guard(&ctx);
        let mut stage = Stage::begin(&guard, &root).unwrap();
        stage
            .write(&rel("mods/staged.jar"), b"staged", Expect::Absent)
            .unwrap();
        stage.crash_at(Boundary::BeforePlan);
        assert!(stage.commit().is_err());
    }

    let outcomes = staged_apply::recover(&ctx.lock_manager, ID, &root);

    assert!(
        matches!(outcomes.as_slice(), [StagedRecovery::RolledBack { .. }]),
        "{outcomes:?}"
    );
    assert_eq!(tree(&root), files(&[("mods/keep.jar", b"keep")]));
    no_stage_left(&root);
}

#[test]
fn recovery_with_no_stage_finds_nothing() {
    let (_dir, ctx, root) = fixture();
    put(&root, "mods/keep.jar", b"keep");
    assert!(staged_apply::recover(&ctx.lock_manager, ID, &root).is_empty());
    assert!(!staged_apply::stage_root(&root).exists());
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn make_folder_link(target: &Path, link: &Path) -> bool {
    #[cfg(windows)]
    {
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false);
        made && fs::symlink_metadata(link)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
}
