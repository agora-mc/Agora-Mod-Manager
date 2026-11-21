//! Locks across real processes: the kernel owns the lock, so a killed holder
//! never leaves a stale lock for a waiter to race over.

use agora_core::lock_manager::{LockManager, LockResource};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

fn helper() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_agora-tool-test-helper"))
}

/// A helper process holding the content-store lock, killed and reaped however
/// the test ends, so nothing is left running.
struct Holder {
    child: Child,
}

impl Holder {
    fn spawn(root: &Path) -> Self {
        let mut child = Command::new(helper())
            .arg("--hold-lock")
            .arg(root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start the lock-holding helper");
        let mut line = String::new();
        BufReader::new(child.stdout.take().expect("piped stdout"))
            .read_line(&mut line)
            .expect("read the helper's readiness line");
        assert_eq!(line.trim(), "locked", "helper did not take the lock");
        Self { child }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn kill_and_wait(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn a_lock_held_by_a_killed_process_is_free_at_once() {
    let tmp = tempfile::tempdir().unwrap();
    let manager = LockManager::new(tmp.path().to_path_buf());
    let mut holder = Holder::spawn(tmp.path());

    // While the helper is alive, the lock is contended and the timeout names
    // the holder rather than returning a bare code.
    let error = manager
        .acquire_with_timeout(
            LockResource::ContentStore,
            "waiting",
            Duration::from_millis(150),
            None,
        )
        .expect_err("the helper holds the lock");
    assert_eq!(error.code(), "ERR_LOCK_TIMEOUT");
    let message = error.to_string();
    assert!(message.contains("in use by"), "{message}");
    assert!(message.contains("Agora test helper"), "{message}");
    assert!(
        message.contains(&format!("PID {}", holder.pid())),
        "{message}"
    );

    // Kill it. The kernel releases the lock with the process; the next taker
    // does not wait out a timeout or break a stale record.
    holder.kill_and_wait();
    let guard = manager
        .acquire_with_timeout(
            LockResource::ContentStore,
            "after-kill",
            Duration::from_secs(5),
            None,
        )
        .expect("a killed holder's lock is free");
    drop(guard);
}

#[test]
fn two_processes_contend_and_the_survivor_takes_the_lock() {
    let tmp = tempfile::tempdir().unwrap();
    let manager = LockManager::new(tmp.path().to_path_buf());
    let mut holder = Holder::spawn(tmp.path());

    let blocked = manager.acquire_with_timeout(
        LockResource::ContentStore,
        "second-process",
        Duration::from_millis(100),
        None,
    );
    assert!(blocked.is_err(), "two processes cannot hold one lock");

    holder.kill_and_wait();
    manager
        .acquire_with_timeout(
            LockResource::ContentStore,
            "second-process-retry",
            Duration::from_secs(5),
            None,
        )
        .expect("the survivor takes the lock once the other process is gone");
}
