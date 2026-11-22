//! Test seams belong to the context they were installed on (MASTER_SPEC §32.9).
//!
//! A hook is installed on one `Ctx` and must reach exactly the operations run with that context —
//! on any thread, since an operation may hand a clone of the context to another thread — and
//! nothing else. A process-wide seam lets one test's hook catch another test's code when tests run
//! as threads in one process (plain `cargo test`); a thread-local one cannot reach code the
//! operation runs on another thread. Both bit us in Windows CI, which ran one process per test.

use std::path::Path;

use agora_core::ctx::Ctx;
use agora_core::game_tool_swap::{self as swap, RunHookPoint, VolumeSeam};

fn context(name: &str) -> (tempfile::TempDir, Ctx) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join(name);
    std::fs::create_dir_all(&root).unwrap();
    let ctx = Ctx::for_testing(root);
    (dir, ctx)
}

/// A free-space stand-in installed on one context answers for that context only: another context
/// still asks the volume itself.
#[test]
fn a_volume_seam_belongs_to_the_context_that_installed_it() {
    let (_a_dir, a) = context("a");
    let (_b_dir, b) = context("b");
    let a_folder = a.paths.root().to_path_buf();
    let b_folder = b.paths.root().to_path_buf();

    a.test_seams.set_volume_seam(Some(VolumeSeam {
        same_volume: Some(false),
        free_bytes: Some(7),
    }));

    assert_eq!(
        swap::available_bytes(&a.test_seams, &a_folder),
        Some(7),
        "the context that installed the seam sees its stand-in"
    );
    assert_ne!(
        swap::available_bytes(&b.test_seams, &b_folder),
        Some(7),
        "a seam installed on another context answered for this one"
    );
}

/// A journal write fault armed on a context fires wherever that context's code runs, including
/// another thread the operation was handed off to.
#[test]
fn a_journal_write_fault_armed_on_a_context_fires_on_another_thread() {
    let (_dir, ctx) = context("ctx");
    let fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let armed = fired.clone();
    ctx.test_seams
        .set_write_fault_hook(Some(std::sync::Arc::new(move |_path: &Path| {
            armed.store(true, std::sync::atomic::Ordering::SeqCst);
            true
        })));

    let carried = ctx.clone();
    let path = ctx.paths.tool_swap_journal_path("test-game", "steam");
    let observed = std::thread::spawn(move || swap::write_fault_fires(&carried.test_seams, &path))
        .join()
        .unwrap();

    assert!(
        fired.load(std::sync::atomic::Ordering::SeqCst),
        "the fault hook never fired on the thread the context was carried to"
    );
    assert!(
        observed,
        "the journal write must fail on the thread the context was carried to"
    );
}

/// A run hook armed on a context fires where that context's operation runs, on another thread too.
#[test]
fn a_run_hook_armed_on_a_context_fires_on_another_thread() {
    let (_dir, ctx) = context("ctx");
    let points = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = points.clone();
    ctx.test_seams.set_run_hook(Some(std::sync::Arc::new(
        move |_ctx: &Ctx, _journal: Option<&swap::SwapJournal>, point: RunHookPoint| {
            seen.lock().unwrap().push(point);
        },
    )));

    let carried = ctx.clone();
    std::thread::spawn(move || {
        swap::run_hook(&carried, None, RunHookPoint::BeforeSpawn);
    })
    .join()
    .unwrap();

    assert_eq!(
        points.lock().unwrap().as_slice(),
        [RunHookPoint::BeforeSpawn],
        "the run hook did not fire on the thread the context was carried to"
    );
}
