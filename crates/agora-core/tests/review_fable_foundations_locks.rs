//! Foundations review (Fable): the cross-process lock can be held twice after a crash.
//!
//! Expected to FAIL until the finding is fixed; nothing here changes production code.

use agora_core::lock_manager::{LockManager, LockResource};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

/// `lock_manager.rs:389`: a waiter that read a stale lock removes "the lock file" by path, without
/// checking it is still the stale one. If another waiter already broke the stale lock and took it,
/// this removes the new holder's lock and then takes it too: two holders of an exclusive lock.
///
/// The scene is the one after a crash: a lock left by a dead process, and several tasks (the
/// app's startup work, a CLI call) reaching for it at once. It is a race, so the test repeats it.
#[test]
fn a_stale_lock_is_never_handed_to_two_waiters_at_once() {
    const ROUNDS: usize = 30;
    const WAITERS: usize = 6;

    let tmp = tempfile::TempDir::new().unwrap();
    let manager = LockManager::new(tmp.path().to_path_buf());
    let lock_path = tmp.path().join("content-store.lock");
    let inside = Arc::new(AtomicUsize::new(0));
    let overlaps = Arc::new(AtomicUsize::new(0));

    for round in 0..ROUNDS {
        // A lock whose owner is gone: no process has this PID.
        let stale = serde_json::json!({
            "pid": 4_000_000_000u32,
            "pid_start_time": 1,
            "hostname": "review",
            "acquired_at": "2026-01-01T00:00:00Z",
            "operation": "crashed",
            "nonce": round as u64,
            "program": "Agora app",
        });
        std::fs::write(&lock_path, stale.to_string()).unwrap();

        let barrier = Arc::new(Barrier::new(WAITERS));
        let handles: Vec<_> = (0..WAITERS)
            .map(|_| {
                let manager = manager.clone();
                let barrier = barrier.clone();
                let inside = inside.clone();
                let overlaps = overlaps.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    let guard = manager
                        .acquire(LockResource::ContentStore, "review")
                        .expect("the stale lock is broken and the lock taken");
                    if inside.fetch_add(1, Ordering::SeqCst) != 0 {
                        overlaps.fetch_add(1, Ordering::SeqCst);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(2));
                    inside.fetch_sub(1, Ordering::SeqCst);
                    drop(guard);
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        if overlaps.load(Ordering::SeqCst) > 0 {
            break;
        }
    }

    assert_eq!(
        overlaps.load(Ordering::SeqCst),
        0,
        "two waiters held the exclusive content-store lock at the same time after breaking a \
         stale lock"
    );
}
