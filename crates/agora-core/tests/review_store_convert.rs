//! Review tests for the store-convert slice. Each `#[ignore]`d test proves one bug; run them with
//! `cargo test -p agora-core --test review_store_convert -- --ignored`.

use std::path::{Path, PathBuf};

use agora_core::content_store::convert::convert_instance;
use agora_core::content_store::place::same_file;
use agora_core::content_store::{add_file, PlacementMode};
use agora_core::ctx::Ctx;
use agora_core::lock_manager::LockResource;
use agora_core::models::{InstalledMod, InstanceManifest, CURRENT_MANIFEST_VERSION};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

fn test_ctx() -> (TempDir, Ctx) {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = Ctx::for_testing(tmp.path().to_path_buf());
    (tmp, ctx)
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn entry(filename: &str, sha256: &str, enabled: bool) -> InstalledMod {
    InstalledMod {
        provider: None,
        update_pinned: false,
        pack_managed: false,
        installed_as_dependency: false,
        filename: filename.to_string(),
        registry_id: None,
        modrinth_id: None,
        source: "manual_drag_drop".to_string(),
        source_url: None,
        version: None,
        sha256: sha256.to_string(),
        hash_verified: true,
        installed_at: "2026-01-01T00:00:00Z".to_string(),
        java_packages: Vec::new(),
        mod_jar_id: None,
        provided_mod_ids: Vec::new(),
        enabled,
        content_type: "mod".to_string(),
        depends_on: Vec::new(),
        optional_deps: Vec::new(),
        incompatible_deps: Vec::new(),
        content_item_id: None,
        object_sha256: None,
    }
}

fn manifest_with(entries: Vec<InstalledMod>) -> InstanceManifest {
    InstanceManifest {
        manifest_version: CURRENT_MANIFEST_VERSION,
        game_data: Default::default(),
        pack_origin: None,
        instance_id: "inst".to_string(),
        name: "Test".to_string(),
        created_from_pack: None,
        minecraft_version: "1.21".to_string(),
        loader: "fabric".to_string(),
        loader_version: "1".to_string(),
        is_locked: false,
        mods: entries,
        resourcepacks: Vec::new(),
        shaders: Vec::new(),
        datapacks: Vec::new(),
        worlds: Vec::new(),
        user_preferences: serde_json::Value::Object(Default::default()),
    }
}

fn manifest_path(ctx: &Ctx) -> PathBuf {
    ctx.paths.instance_manifest("inst").unwrap()
}

fn setup_instance(ctx: &Ctx, manifest: &InstanceManifest) -> PathBuf {
    let dir = ctx.paths.instance_dir("inst").unwrap();
    std::fs::create_dir_all(dir.join("mods")).unwrap();
    agora_core::helpers::atomic_write_manifest(&manifest_path(ctx), manifest).unwrap();
    dir
}

fn write_mod(dir: &Path, name: &str, bytes: &[u8]) {
    std::fs::write(dir.join("mods").join(name), bytes).unwrap();
}

fn store_bytes(ctx: &Ctx, name: &str, bytes: &[u8]) -> String {
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join(name);
    std::fs::write(&source, bytes).unwrap();
    let outcome = add_file(ctx, &source, Some(name), Some(&format!("mods/{name}"))).unwrap();
    outcome.item().files[0].sha256.clone()
}

// ---------------------------------------------------------------------------
// Hypothesis 2/5: the manifest is read BEFORE the instance lock is taken, then written back whole.
// ---------------------------------------------------------------------------

#[test]
#[ignore = "proves bug: manifest is read before the instance lock, so a change made while waiting for it is overwritten (convert.rs:125-136 reads, :280 locks, :299 writes)"]
fn a_change_made_while_the_conversion_waits_for_the_lock_is_not_overwritten() {
    let (_tmp, ctx) = test_ctx();
    let bytes = b"a mod that will be linked".repeat(8);
    let manifest = manifest_with(vec![entry("a.jar", &sha256_hex(&bytes), true)]);
    let dir = setup_instance(&ctx, &manifest);
    write_mod(&dir, "a.jar", &bytes);
    let late_bytes = b"installed by someone else".repeat(8);

    let (held_tx, held_rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            // Stand-in for an install: it owns the instance lock, finishes, and records its mod.
            let guard = ctx
                .lock_manager
                .acquire(LockResource::Instance("inst".to_string()), "install")
                .unwrap();
            held_tx.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(1500));
            write_mod(&dir, "late.jar", &late_bytes);
            let mut on_disk = agora_core::helpers::read_manifest(&manifest_path(&ctx)).unwrap();
            on_disk
                .mods
                .push(entry("late.jar", &sha256_hex(&late_bytes), true));
            agora_core::helpers::atomic_write_manifest(&manifest_path(&ctx), &on_disk).unwrap();
            drop(guard);
        });
        held_rx.recv().unwrap();
        // Reads the manifest now, then waits for the lock the "install" holds.
        convert_instance(&ctx, "inst", PlacementMode::Link).unwrap();
    });

    let after = agora_core::helpers::read_manifest(&manifest_path(&ctx)).unwrap();
    assert!(
        after.mods.iter().any(|m| m.filename == "late.jar"),
        "the mod installed while the conversion waited vanished from the manifest: {:?}",
        after.mods.iter().map(|m| &m.filename).collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// Hypothesis 1/6: a step that fails after earlier files were already linked.
// ---------------------------------------------------------------------------

#[test]
#[ignore = "proves bug: a failure part-way drops the manifest record (item id, object hash) of files already linked, and a re-run never records it (convert.rs:294 `?` before :299, and :380-382)"]
fn files_linked_before_a_failure_still_get_their_manifest_record_after_a_rerun() {
    let (_tmp, ctx) = test_ctx();
    let first = b"first mod bytes".repeat(16);
    let second = b"second mod bytes".repeat(16);
    let manifest = manifest_with(vec![
        entry("a.jar", &sha256_hex(&first), true),
        entry("b.jar", &sha256_hex(&second), true),
    ]);
    let dir = setup_instance(&ctx, &manifest);
    write_mod(&dir, "a.jar", &first);
    write_mod(&dir, "b.jar", &second);

    // b.jar's store object exists but is damaged (replaced with other bytes, unprotected).
    let hash_b = store_bytes(&ctx, "b.jar", &second);
    let object_b = ctx.paths.content_object_path(&hash_b);
    std::fs::remove_file(&object_b).unwrap();
    std::fs::write(&object_b, b"not the bytes the name says").unwrap();

    let failed = convert_instance(&ctx, "inst", PlacementMode::Link);
    assert!(failed.is_err(), "a damaged object stops the conversion");
    // Checked and expected fine: the file that failed is back, byte for byte.
    assert_eq!(std::fs::read(dir.join("mods/b.jar")).unwrap(), second);
    assert!(
        same_file(
            &dir.join("mods/a.jar"),
            &ctx.paths.content_object_path(&sha256_hex(&first))
        ),
        "a.jar was linked before the failure"
    );

    // The user repairs the store and runs the conversion again.
    std::fs::remove_file(&object_b).unwrap();
    convert_instance(&ctx, "inst", PlacementMode::Link).unwrap();

    let after = agora_core::helpers::read_manifest(&manifest_path(&ctx)).unwrap();
    let a = after.mods.iter().find(|m| m.filename == "a.jar").unwrap();
    assert!(
        a.content_item_id.is_some() && a.object_sha256.is_some(),
        "a.jar is a store link but its manifest entry never recorded it: {a:?}"
    );
}

// ---------------------------------------------------------------------------
// Hypothesis 3: a manifest written by a newer Agora.
// ---------------------------------------------------------------------------

#[test]
#[ignore = "proves bug: a newer-version (read-only) manifest is not refused up front, so files are replaced and then the manifest write fails (convert.rs:134 read_manifest accepts it, :299 write refuses)"]
fn a_read_only_manifest_is_refused_before_any_file_is_replaced() {
    let (_tmp, ctx) = test_ctx();
    let bytes = b"a mod in a newer instance".repeat(8);
    let manifest = manifest_with(vec![entry("a.jar", &sha256_hex(&bytes), true)]);
    let dir = setup_instance(&ctx, &manifest);
    write_mod(&dir, "a.jar", &bytes);
    // A newer Agora wrote this manifest.
    let path = manifest_path(&ctx);
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    value["manifest_version"] = serde_json::json!(CURRENT_MANIFEST_VERSION + 1);
    std::fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

    let result = convert_instance(&ctx, "inst", PlacementMode::Link);
    let object = ctx.paths.content_object_path(&sha256_hex(&bytes));
    let replaced = object.is_file() && same_file(&dir.join("mods/a.jar"), &object);
    assert!(
        !(result.is_err() && replaced),
        "the conversion failed ({:?}) yet a.jar was already replaced with a store link and the \
         manifest can no longer be updated",
        result.err().map(|e| e.to_string())
    );
    assert!(result.is_err(), "a read-only manifest must be refused");
}

// ---------------------------------------------------------------------------
// Hypothesis 1: interruption in the middle of placing a COPY (a link cannot be partial).
// ---------------------------------------------------------------------------

#[test]
#[ignore = "proves bug: a half-written target left by a crashed copy is kept and the intact journaled original is shoved into moved-aside, so a re-run leaves a truncated mod (convert.rs:572-603 recover_journal; place.rs:copy_new writes in place, not via temp+rename)"]
fn a_half_copied_target_does_not_replace_the_journaled_original_on_rerun() {
    let (_tmp, ctx) = test_ctx();
    let original = b"the complete original jar".repeat(64);
    let manifest = manifest_with(vec![entry("a.jar", &sha256_hex(&original), true)]);
    let dir = setup_instance(&ctx, &manifest);
    store_bytes(&ctx, "a.jar", &original);
    // Interrupted state: original moved into the journal, copy only partly written at the target.
    let journal = dir.join(".agora/content-convert/mods");
    std::fs::create_dir_all(&journal).unwrap();
    std::fs::write(journal.join("a.jar"), &original).unwrap();
    write_mod(&dir, "a.jar", &original[..original.len() / 3]);

    let _ = convert_instance(&ctx, "inst", PlacementMode::Link);

    let now = std::fs::read(dir.join("mods/a.jar")).unwrap();
    assert_eq!(
        sha256_hex(&now),
        sha256_hex(&original),
        "mods/a.jar is {} of {} bytes after the re-run; the whole original sits in {:?}",
        now.len(),
        original.len(),
        dir.join(".agora/moved-aside").exists()
    );
}

// ---------------------------------------------------------------------------
// Hypothesis 4 (Windows): converting links to copies from a folder without FILE_DELETE_CHILD.
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn restrict_folder_to_modify(dir: &Path) {
    let user = std::env::var("USERNAME").expect("USERNAME");
    let out = std::process::Command::new("icacls")
        .arg(dir)
        .arg("/inheritance:r")
        .arg("/grant:r")
        .arg(format!("{user}:(OI)(CI)M"))
        .output()
        .expect("run icacls");
    assert!(out.status.success(), "icacls failed");
}

#[cfg(windows)]
#[test]
#[ignore = "proves bug: the journal folder is created without FILE_DELETE_CHILD, so the link moved into it cannot be deleted; the error is swallowed (convert.rs:514 `let _ = remove_file`) and every later run fails at :591 `remove_file(..)?`"]
fn converting_to_separate_cleans_up_its_journal_on_a_default_acl_folder() {
    let (_tmp, ctx) = test_ctx();
    let bytes = b"a shared mod jar".repeat(32);
    let manifest = manifest_with(vec![entry("a.jar", &sha256_hex(&bytes), true)]);
    let dir = setup_instance(&ctx, &manifest);
    // The instance folder grants only Modify, like an instance on a drive with the default ACL.
    restrict_folder_to_modify(&dir);
    write_mod(&dir, "a.jar", &bytes);

    convert_instance(&ctx, "inst", PlacementMode::Link).unwrap();
    let object = ctx.paths.content_object_path(&sha256_hex(&bytes));
    assert!(same_file(&dir.join("mods/a.jar"), &object));

    let first = convert_instance(&ctx, "inst", PlacementMode::Copy);
    let leftover = dir.join(".agora/content-convert/mods/a.jar");
    let leftover_exists = leftover.exists();
    let second = convert_instance(&ctx, "inst", PlacementMode::Copy);
    assert!(
        first.is_ok() && !leftover_exists && second.is_ok(),
        "first: {:?}; journal link left behind: {leftover_exists}; second run: {:?}",
        first.err().map(|e| e.to_string()),
        second.err().map(|e| e.to_string())
    );
}

// ---------------------------------------------------------------------------
// Hypothesis 2: a file edited between the hash check and the replacement.
// ---------------------------------------------------------------------------

#[test]
#[ignore = "proves bug (racy): bytes the user wrote after the hash check are moved to the journal and then deleted unverified (convert.rs:367 hash, :394 replace_with, :514 remove_file of the journaled copy)"]
fn an_edit_between_the_hash_check_and_the_replacement_is_not_lost() {
    // Many bytes make the add_file step (store the object) long; the editing thread waits for the
    // object to appear (hash and store are already done) and then appends to the file.
    let mut lost = 0;
    let mut landed = 0;
    for round in 0..15 {
        let (_tmp, ctx) = test_ctx();
        let mut bytes = vec![0u8; 24 * 1024 * 1024];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i.wrapping_mul(31).wrapping_add(round)) as u8;
        }
        let hash = sha256_hex(&bytes);
        let manifest = manifest_with(vec![entry("big.jar", &hash, true)]);
        let dir = setup_instance(&ctx, &manifest);
        write_mod(&dir, "big.jar", &bytes);
        let object = ctx.paths.content_object_path(&hash);
        let target = dir.join("mods/big.jar");
        let edited = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let started = std::time::Instant::now();
                while started.elapsed().as_secs() < 20 {
                    if object.is_file() {
                        use std::io::Write;
                        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&target) {
                            if f.write_all(b"USER EDIT").is_ok() {
                                edited.store(true, std::sync::atomic::Ordering::SeqCst);
                            }
                        }
                        return;
                    }
                    std::hint::spin_loop();
                }
            });
            let _ = convert_instance(&ctx, "inst", PlacementMode::Link);
        });
        if !edited.load(std::sync::atomic::Ordering::SeqCst) {
            continue;
        }
        landed += 1;
        // The edit either stayed in the file, or the file was left alone, or a copy was kept.
        let mut survivors = vec![target.clone()];
        for sub in [".agora/content-convert", ".agora/moved-aside"] {
            let root = dir.join(sub);
            if root.exists() {
                survivors.extend(walk(&root));
            }
        }
        let kept = survivors.iter().any(|p| {
            std::fs::read(p)
                .map(|b| b.ends_with(b"USER EDIT"))
                .unwrap_or(false)
        });
        if !kept {
            lost += 1;
        }
    }
    assert!(landed > 0, "the race never landed; the test proved nothing");
    assert_eq!(
        lost, 0,
        "the user's edit was lost in {lost} of {landed} landed races"
    );
}

fn walk(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(root).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}
