//! Cross-process filesystem locking for Agora.
//!
//! `LockManager` provides RAII lock guards backed by the operating system's own
//! advisory locks on an open file handle: `LockFileEx` on Windows and `flock`
//! on Unix (through [`fs2`]). An advisory lock belongs to the open handle, so
//! the kernel releases it when the process dies. There is no stale lock to
//! break and no PID probe to race: a crashed holder cannot hand the same lock
//! to two waiters.
//!
//! The JSON written into the lock file is only a "who holds it" record for
//! messages. An empty or unreadable file is ignored, never treated as an
//! occupied lock. The file itself is never removed: unlinking a lock file
//! while another process may be about to open it would let two processes lock
//! two different inodes with the same name.
//!
//! **Not re-entrant.** A `LockManager` remembers the paths it holds, so a
//! second take from the same manager waits like any other contender instead
//! of creating a second holder. The OS lock is the exclusion; the remembered
//! path only improves the timeout message, which says when *this* process
//! already holds the lock and for which operation. Callers that need a lock
//! without waiting use the `*_locked` function in the owning module rather
//! than taking it again.
//!
//! **Waiting**: acquisition tries the lock, then backs off (10 ms doubling to
//! 200 ms) until the timeout or cancellation. It never sleeps a fixed second.
//! Async callers use [`LockManager::acquire_async`] /
//! [`LockManager::acquire_with_timeout_async`], which back off with
//! `tokio::time::sleep` instead of blocking a runtime thread.
//!
//! **Cancellation**: `acquire_with_timeout` accepts an optional
//! [`CancellationToken`]; the wait loop checks it between attempts.

use crate::app_paths;
use crate::error::{LauncherError, LauncherResult};
use crate::event_sink::CancellationToken;
use agora_game_api::{GameId, StoreId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// OS advisory locking on an open handle.
///
/// The handle owns the lock, so the kernel frees it when the process dies.
/// Windows' byte-range locks are mandatory: a waiter reading the owner record
/// would be refused if we locked the whole file, so the lock is placed far
/// past the JSON. Unix `flock` is advisory and locks the whole file.
#[cfg(windows)]
mod platform {
    use std::fs::File;
    use std::io;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{ERROR_LOCK_VIOLATION, HANDLE};
    use windows_sys::Win32::Storage::FileSystem::{
        LockFileEx, UnlockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;

    /// Far beyond any owner record we write (a few hundred bytes).
    const LOCK_OFFSET: u64 = 1 << 40;

    fn overlapped() -> OVERLAPPED {
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.Anonymous.Anonymous.Offset = LOCK_OFFSET as u32;
        overlapped.Anonymous.Anonymous.OffsetHigh = (LOCK_OFFSET >> 32) as u32;
        overlapped
    }

    /// Try to take the exclusive lock without waiting. `Ok(false)` means
    /// another handle holds it.
    pub fn try_lock(file: &File) -> io::Result<bool> {
        let mut overlapped = overlapped();
        let locked = unsafe {
            LockFileEx(
                file.as_raw_handle() as HANDLE,
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                0,
                1,
                0,
                &mut overlapped,
            )
        };
        if locked != 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        // 33 = ERROR_LOCK_VIOLATION, 32 = ERROR_SHARING_VIOLATION. Rust does
        // not map these to `WouldBlock`, so classify them as contention here.
        match error.raw_os_error() {
            Some(code) if code == ERROR_LOCK_VIOLATION as i32 => Ok(false),
            Some(32) => Ok(false),
            _ => Err(error),
        }
    }

    pub fn unlock(file: &File) -> io::Result<()> {
        let mut overlapped = overlapped();
        let unlocked =
            unsafe { UnlockFileEx(file.as_raw_handle() as HANDLE, 0, 1, 0, &mut overlapped) };
        if unlocked != 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

#[cfg(unix)]
mod platform {
    use fs2::FileExt;
    use std::fs::File;
    use std::io;

    pub fn try_lock(file: &File) -> io::Result<bool> {
        match file.try_lock_exclusive() {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(false),
            Err(error) => Err(error),
        }
    }

    pub fn unlock(file: &File) -> io::Result<()> {
        FileExt::unlock(file)
    }
}

/// Default timeout for acquiring a lock (30 seconds).
const DEFAULT_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);

/// First wait between attempts when a lock is contested.
const INITIAL_RETRY_DELAY: Duration = Duration::from_millis(10);

/// Longest wait between attempts. A waiter still notices cancellation and the
/// deadline within this interval.
const MAX_RETRY_DELAY: Duration = Duration::from_millis(200);

// ---------------------------------------------------------------------------
// Lock metadata
// ---------------------------------------------------------------------------

/// Metadata written into every lock file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockMetadata {
    /// PID of the process holding the lock.
    pub pid: u32,
    /// Process start time (seconds since epoch), for informational use.
    pub pid_start_time: u64,
    /// Hostname (best-effort).
    pub hostname: String,
    /// ISO-8601 timestamp when the lock was acquired.
    pub acquired_at: String,
    /// Human-readable description of the operation holding the lock.
    pub operation: String,
    /// Random ownership nonce for diagnostics.
    pub nonce: u128,
    /// Which Agora program holds the lock ("Agora app", "Agora CLI"), so a
    /// waiting program can say who it is waiting for. Empty in locks written
    /// before this field existed.
    #[serde(default)]
    pub program: String,
}

static PROGRAM_NAME: OnceLock<String> = OnceLock::new();

/// Name this process in the locks it takes, e.g. "Agora CLI".
///
/// The desktop app and the CLI share one data root, so a lock is often held by
/// the *other* program. Each adapter calls this once at startup; the first call
/// wins.
pub fn set_program_name(name: &str) {
    let _ = PROGRAM_NAME.set(name.to_string());
}

pub(crate) fn program_name() -> String {
    PROGRAM_NAME.get().cloned().unwrap_or_default()
}

/// Who holds a lock, in words a person can act on.
fn describe_holder(meta: &LockMetadata) -> String {
    if meta.pid == std::process::id() {
        return "another task in this program".into();
    }
    if meta.program.is_empty() {
        format!("another Agora program (PID {})", meta.pid)
    } else {
        format!("the {} (PID {})", meta.program, meta.pid)
    }
}

// ---------------------------------------------------------------------------
// Held locks in this process
// ---------------------------------------------------------------------------

/// The open handle that owns the OS lock. Dropping it releases the lock.
#[derive(Debug)]
struct HeldLock {
    file: File,
}

impl Drop for HeldLock {
    fn drop(&mut self) {
        let _ = platform::unlock(&self.file);
    }
}

/// One lock this `LockManager` holds.
#[derive(Debug)]
struct HeldState {
    operation: String,
    /// Keeps the locked handle alive while the map entry exists. Dropping it
    /// (when the entry is removed) releases the OS lock.
    _held: HeldLock,
}

/// The locks one manager holds, by lock-file path.
type HeldLocks = Arc<Mutex<HashMap<PathBuf, HeldState>>>;

fn lock_table(held: &HeldLocks) -> std::sync::MutexGuard<'_, HashMap<PathBuf, HeldState>> {
    held.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// LockError (structured, public)
// ---------------------------------------------------------------------------

/// Structured error from a lock acquisition attempt.
#[derive(Debug)]
pub enum LockError {
    /// The lock is held by another owner. The metadata is the holder's when the
    /// lock file could be read; `None` when it is empty, unreadable or being
    /// written.
    Contested(Option<LockMetadata>),
    /// I/O error interacting with the lock file.
    Io(std::io::Error),
}

impl LockError {
    /// Return the lock metadata if available.
    pub fn metadata(&self) -> Option<&LockMetadata> {
        match self {
            LockError::Contested(meta) => meta.as_ref(),
            LockError::Io(_) => None,
        }
    }

    fn into_launcher_error(self, resource: &str) -> LauncherError {
        match self {
            LockError::Contested(meta) => LauncherError::Generic {
                code: "ERR_LOCK_CONTESTED".into(),
                message: match meta {
                    Some(meta) => format!(
                        "Resource '{resource}' is in use by {} on {} (since {}, operation: {})",
                        describe_holder(&meta),
                        meta.hostname,
                        meta.acquired_at,
                        meta.operation,
                    ),
                    None => format!(
                        "Resource '{resource}' is in use by another Agora program; \
                         the lock file has no readable owner record."
                    ),
                },
            },
            LockError::Io(e) => LauncherError::Generic {
                code: "ERR_LOCK_IO".into(),
                message: format!("Lock I/O error for '{resource}': {e}"),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// LockGuard
// ---------------------------------------------------------------------------

/// A filesystem-based lock guard.
///
/// Dropping the guard releases the OS lock and removes the manager's record of
/// it. The lock file stays on disk with the last holder's metadata.
#[derive(Debug)]
pub struct LockGuard {
    lock_path: PathBuf,
    key: PathBuf,
    held: HeldLocks,
    /// For an instance lock: the change to record on release, so another
    /// program showing this instance knows to reload it.
    change: Option<PendingChange>,
}

#[derive(Debug)]
struct PendingChange {
    locks_root: PathBuf,
    instance_id: String,
    operation: String,
}

impl LockGuard {
    /// The path to the lock file on disk.
    pub fn path(&self) -> &Path {
        &self.lock_path
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        // Record the change while the OS lock is still held, so whoever takes
        // it next already sees what this holder changed.
        if let Some(change) = &self.change {
            crate::change_feed::record_instance_change(
                &change.locks_root,
                &change.instance_id,
                &change.operation,
            );
        }
        // Removing the entry drops the handle, which releases the OS lock.
        lock_table(&self.held).remove(&self.key);
    }
}

// ---------------------------------------------------------------------------
// LockResource
// ---------------------------------------------------------------------------

/// Named locks that the manager knows about.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LockResource {
    /// Exclusive access to registry update/download.
    RegistryUpdate,
    /// Exclusive access to loader installation.
    LoaderInstall,
    /// Per-major-version Java runtime provisioning (e.g., `JavaMajor(21)`).
    JavaMajor(u32),
    /// Exclusive access to launch materialization.
    Materialization,
    /// Per-instance lock for install/remove/update operations.
    Instance(String),
    /// Exclusive access to the content store for adding or removing items.
    ContentStore,
    /// Exclusive access while scanning live and deleted instances or moving between their roots.
    /// Take it after Instance and ContentStore, in that order when both are needed, and acquire
    /// no other resource while holding it.
    InstanceLocations,
    /// Exclusive access for per-user game files journaled swap.
    GameUserFiles(GameId, StoreId),
    /// Exclusive access while a plugin's files are swapped (install, update, rollback), so a
    /// starting process's crash recovery cannot undo a live install.
    PluginInstall,
    /// Exclusive access to building a base by id (the rename into place through its record).
    Base(String),
    /// Exclusive access to opening, checking and (if needed) restoring the local state database.
    /// Two processes starting at once must not both quarantine or replace the same file.
    LocalStateDatabase,
}

impl LockResource {
    /// Return the filesystem-safe lock name, validating all inputs.
    ///
    /// Returns `Err` if:
    /// - An instance ID contains path separators, traversal, or is empty.
    /// - A Java major version is 0.
    pub fn lock_name(&self) -> LauncherResult<String> {
        match self {
            LockResource::RegistryUpdate => Ok("registry-update".into()),
            LockResource::LoaderInstall => Ok("loader-install".into()),
            LockResource::JavaMajor(major) => {
                if *major == 0 {
                    return Err(LauncherError::Generic {
                        code: "ERR_INVALID_LOCK".into(),
                        message: "Java major version must be >= 1 for lock acquisition.".into(),
                    });
                }
                Ok(format!("java-major-{major}"))
            }
            LockResource::Materialization => Ok("materialization".into()),
            LockResource::Instance(id) => {
                app_paths::validate_path_component(id)?;
                Ok(format!("instance-{id}"))
            }
            LockResource::ContentStore => Ok("content-store".into()),
            LockResource::InstanceLocations => Ok("instance-locations".into()),
            LockResource::GameUserFiles(game, store) => {
                app_paths::validate_path_component(game.as_str())?;
                app_paths::validate_path_component(store.as_str())?;
                Ok(format!(
                    "game-user-files-{}-{}",
                    game.as_str(),
                    store.as_str()
                ))
            }
            LockResource::PluginInstall => Ok("plugin-install".into()),
            LockResource::LocalStateDatabase => Ok("local-state-db".into()),
            LockResource::Base(id) => {
                app_paths::validate_path_component(id)?;
                Ok(format!("base-{id}"))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// LockManager
// ---------------------------------------------------------------------------

/// Cross-process lock manager backed by OS advisory locks.
///
/// All locks live under the configured `locks_root` directory.
#[derive(Debug, Clone)]
pub struct LockManager {
    locks_root: PathBuf,
    /// Locks this manager holds, keyed by normalised lock-file path. Clones of
    /// one manager share it; a separately constructed manager has its own, and
    /// the OS lock still serialises the two.
    held: HeldLocks,
}

impl LockManager {
    /// Create a new lock manager that stores lock files under `locks_root`.
    pub fn new(locks_root: PathBuf) -> Self {
        Self {
            locks_root,
            held: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The root directory for lock files.
    pub fn locks_root(&self) -> &Path {
        &self.locks_root
    }

    /// The operation this manager holds `key` for, if any.
    fn held_operation(&self, key: &Path) -> Option<String> {
        lock_table(&self.held)
            .get(key)
            .map(|state| state.operation.clone())
    }

    /// The operation of the live holder of `resource`, when one holds it. A free
    /// lock is not a holder. Used to fail fast when the holder is a long tool
    /// run instead of waiting out the acquire timeout.
    pub fn current_operation(&self, resource: &LockResource) -> Option<String> {
        let name = resource.lock_name().ok()?;
        let lock_path = self.locks_root.join(format!("{name}.lock"));
        let key = lock_key(&lock_path);
        if let Some(operation) = self.held_operation(&key) {
            return Some(operation);
        }
        if lock_is_held(&lock_path) {
            return read_lock_metadata(&lock_path)
                .ok()
                .map(|meta| meta.operation);
        }
        None
    }

    /// Record a change to an instance for other Agora programs, for an edit that takes no
    /// instance lock (releasing one records it already). Best-effort, as the change feed is.
    pub fn record_instance_change(&self, instance_id: &str, operation: &str) {
        crate::change_feed::record_instance_change(&self.locks_root, instance_id, operation);
    }

    // ------------------------------------------------------------------
    // Public acquire helpers
    // ------------------------------------------------------------------

    /// Takes the locations lock without waiting. Moves and destructive scans can be retried
    /// when the current holder finishes.
    pub fn try_instance_locations(&self, operation: &str) -> LauncherResult<LockGuard> {
        self.acquire_with_timeout(
            LockResource::InstanceLocations,
            operation,
            Duration::ZERO,
            None,
        )
    }

    fn prepare(&self, resource: &LockResource) -> LauncherResult<(String, PathBuf, PathBuf)> {
        let lock_name = resource.lock_name()?;
        let lock_path = self.locks_root.join(format!("{lock_name}.lock"));
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| LauncherError::Generic {
                code: "ERR_LOCK_IO".into(),
                message: format!("Could not create the lock directory: {e}"),
            })?;
        }
        let key = lock_key(&lock_path);
        Ok((lock_name, lock_path, key))
    }

    fn with_change(
        &self,
        mut guard: LockGuard,
        resource: &LockResource,
        operation: &str,
    ) -> LockGuard {
        if let LockResource::Instance(instance_id) = resource {
            guard.change = Some(PendingChange {
                locks_root: self.locks_root.clone(),
                instance_id: instance_id.clone(),
                operation: operation.to_string(),
            });
        }
        guard
    }

    fn cancel_error(lock_name: &str) -> LauncherError {
        LauncherError::Generic {
            code: "ERR_LOCK_CANCELLED".into(),
            message: format!("Lock acquisition cancelled for '{lock_name}'"),
        }
    }

    /// Attempt to acquire a lock with the default timeout (30s) and no
    /// cancellation token. Blocks the calling thread; use
    /// [`Self::acquire_async`] from an async task.
    pub fn acquire(&self, resource: LockResource, operation: &str) -> LauncherResult<LockGuard> {
        self.acquire_with_timeout(resource, operation, DEFAULT_ACQUIRE_TIMEOUT, None)
    }

    /// Attempt to acquire a lock with a custom timeout, blocking the calling
    /// thread between attempts.
    pub fn acquire_with_timeout(
        &self,
        resource: LockResource,
        operation: &str,
        timeout: Duration,
        cancel: Option<&CancellationToken>,
    ) -> LauncherResult<LockGuard> {
        let (lock_name, lock_path, key) = self.prepare(&resource)?;
        let deadline = Instant::now() + timeout;
        let mut delay = INITIAL_RETRY_DELAY;

        loop {
            if let Some(tok) = cancel {
                if tok.is_cancelled() {
                    return Err(Self::cancel_error(&lock_name));
                }
            }

            match self.try_acquire(&key, &lock_path, operation) {
                Ok(guard) => return Ok(self.with_change(guard, &resource, operation)),
                Err(LockError::Contested(meta)) => {
                    let now = Instant::now();
                    if now >= deadline {
                        let local = self.held_operation(&key);
                        return Err(timeout_error(&lock_name, timeout, meta, local));
                    }
                    let remaining = deadline.saturating_duration_since(now);
                    std::thread::sleep(delay.min(remaining));
                    delay = (delay * 2).min(MAX_RETRY_DELAY);
                }
                Err(LockError::Io(e)) => {
                    return Err(LockError::Io(e).into_launcher_error(&lock_name));
                }
            }
        }
    }

    /// Async counterpart of [`Self::acquire`]. The wait uses
    /// `tokio::time::sleep`, so it does not block a runtime worker.
    pub async fn acquire_async(
        &self,
        resource: LockResource,
        operation: &str,
    ) -> LauncherResult<LockGuard> {
        self.acquire_with_timeout_async(resource, operation, DEFAULT_ACQUIRE_TIMEOUT, None)
            .await
    }

    /// Async counterpart of [`Self::acquire_with_timeout`].
    pub async fn acquire_with_timeout_async(
        &self,
        resource: LockResource,
        operation: &str,
        timeout: Duration,
        cancel: Option<&CancellationToken>,
    ) -> LauncherResult<LockGuard> {
        let (lock_name, lock_path, key) = self.prepare(&resource)?;
        let deadline = Instant::now() + timeout;
        let mut delay = INITIAL_RETRY_DELAY;

        loop {
            if let Some(tok) = cancel {
                if tok.is_cancelled() {
                    return Err(Self::cancel_error(&lock_name));
                }
            }

            match self.try_acquire(&key, &lock_path, operation) {
                Ok(guard) => return Ok(self.with_change(guard, &resource, operation)),
                Err(LockError::Contested(meta)) => {
                    let now = Instant::now();
                    if now >= deadline {
                        let local = self.held_operation(&key);
                        return Err(timeout_error(&lock_name, timeout, meta, local));
                    }
                    let remaining = deadline.saturating_duration_since(now);
                    tokio::time::sleep(delay.min(remaining)).await;
                    delay = (delay * 2).min(MAX_RETRY_DELAY);
                }
                Err(LockError::Io(e)) => {
                    return Err(LockError::Io(e).into_launcher_error(&lock_name));
                }
            }
        }
    }
}

fn timeout_error(
    lock_name: &str,
    timeout: Duration,
    meta: Option<LockMetadata>,
    local_operation: Option<String>,
) -> LauncherError {
    let holder = match local_operation {
        // Another task in this program holds it. If the caller is nested, this
        // is a self-deadlock; the operation names the code to wait for.
        Some(operation) => format!(
            "this Agora process already holds it for: {operation}. The lock is not re-entrant; \
             wait for that operation to finish"
        ),
        None => match meta {
            Some(meta) => format!(
                "it is in use by {} since {} for: {}",
                describe_holder(&meta),
                meta.acquired_at,
                meta.operation,
            ),
            None => "it is in use by another Agora program whose lock record cannot be read".into(),
        },
    };
    LauncherError::Generic {
        code: "ERR_LOCK_TIMEOUT".into(),
        message: format!(
            "Could not acquire lock '{lock_name}' within {}s: {holder}. \
             Try again once it finishes.",
            timeout.as_secs(),
        ),
    }
}

// ---------------------------------------------------------------------------
// Internal: try_acquire
// ---------------------------------------------------------------------------

/// Attempt to acquire the lock at `lock_path` once. Does not retry.
///
/// Opens (creating if needed) the lock file and asks the operating system for
/// an exclusive advisory lock on the handle. The kernel queues contenders and
/// releases the lock if the holder dies, so no owner liveness probe is needed.
///
/// The error carries the holder's metadata so a waiting caller can name it. It
/// is returned on every retry, so its size does not matter.
impl LockManager {
    #[allow(clippy::result_large_err)]
    fn try_acquire(
        &self,
        key: &Path,
        lock_path: &Path,
        operation: &str,
    ) -> Result<LockGuard, LockError> {
        // This manager already holds it. Do not open a second handle (on some
        // platforms that could succeed); report contention so the caller waits
        // and the timeout names the operation it is waiting for.
        if self.held_operation(key).is_some() {
            return Err(LockError::Contested(read_lock_metadata(lock_path).ok()));
        }

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .map_err(LockError::Io)?;

        match platform::try_lock(&file) {
            Ok(true) => {
                let metadata = LockMetadata {
                    pid: std::process::id(),
                    pid_start_time: get_own_start_time(),
                    hostname: hostname(),
                    acquired_at: chrono::Utc::now().to_rfc3339(),
                    operation: operation.to_string(),
                    nonce: uuid::Uuid::new_v4().as_u128(),
                    program: program_name(),
                };
                write_lock_metadata(&mut file, &metadata).map_err(LockError::Io)?;
                lock_table(&self.held).insert(
                    key.to_path_buf(),
                    HeldState {
                        operation: operation.to_string(),
                        _held: HeldLock { file },
                    },
                );
                Ok(LockGuard {
                    lock_path: lock_path.to_path_buf(),
                    key: key.to_path_buf(),
                    held: Arc::clone(&self.held),
                    change: None,
                })
            }
            Ok(false) => {
                // The file may be empty, half-written or left by a dead process.
                // It is only a message here; the OS lock is the authority.
                Err(LockError::Contested(read_lock_metadata(lock_path).ok()))
            }
            Err(e) => Err(LockError::Io(e)),
        }
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Normalise a lock path so the held-lock table uses one key per file. The
/// file may not exist yet; canonicalise its parent, which does exist by the
/// time this is called.
fn lock_key(lock_path: &Path) -> PathBuf {
    if let (Some(parent), Some(name)) = (lock_path.parent(), lock_path.file_name()) {
        if let Ok(parent) = std::fs::canonicalize(parent) {
            return parent.join(name);
        }
    }
    lock_path.to_path_buf()
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "unknown".into())
}

fn write_lock_metadata(file: &mut File, metadata: &LockMetadata) -> std::io::Result<()> {
    let json = serde_json::to_vec(metadata).map_err(std::io::Error::other)?;
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&json)?;
    file.sync_all()
}

fn read_lock_metadata(lock_path: &Path) -> Result<LockMetadata, std::io::Error> {
    let mut file = File::open(lock_path)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    // Reject empty files (partial writes, crash residue).
    if contents.trim().is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "lock file is empty",
        ));
    }
    serde_json::from_str(&contents)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Whether another handle currently holds the OS lock. Used only for the
/// `current_operation` hint; a free lock is unheld and immediately released.
fn lock_is_held(lock_path: &Path) -> bool {
    let file = match OpenOptions::new().read(true).write(true).open(lock_path) {
        Ok(file) => file,
        Err(_) => return false,
    };
    match platform::try_lock(&file) {
        Ok(true) => {
            let _ = platform::unlock(&file);
            false
        }
        Ok(false) => true,
        // Could not tell; report the lock as held rather than as free.
        Err(_) => true,
    }
}

/// Get our own process start time (seconds since epoch) for the lock record.
fn get_own_start_time() -> u64 {
    let mut system = sysinfo::System::new();
    let own_pid = sysinfo::Pid::from_u32(std::process::id());
    system.refresh_process(own_pid);
    system.process(own_pid).map(|p| p.start_time()).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    static DIR_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn test_manager() -> (LockManager, PathBuf) {
        let n = DIR_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("agora-lock-test-{}-{}", std::process::id(), n));
        let _ = std::fs::create_dir_all(&dir);
        (LockManager::new(dir.clone()), dir)
    }

    fn cleanup(dir: &Path) {
        if dir.exists() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn test_acquire_and_release() {
        let (mgr, dir) = test_manager();
        let lock = mgr.acquire(LockResource::RegistryUpdate, "test");
        assert!(lock.is_ok(), "should acquire: {:?}", lock.err());
        drop(lock);
        cleanup(&dir);
    }

    #[test]
    fn test_same_process_contention() {
        // Another thread still waits even though this process holds the lock.
        let (mgr, dir) = test_manager();
        let lock1 = mgr.acquire(LockResource::RegistryUpdate, "first").unwrap();
        let contender = mgr.clone();
        let result = std::thread::spawn(move || {
            contender.acquire_with_timeout(
                LockResource::RegistryUpdate,
                "second",
                Duration::from_millis(100),
                None,
            )
        })
        .join()
        .unwrap();
        assert!(result.is_err(), "another thread should time out");
        assert_eq!(result.unwrap_err().code(), "ERR_LOCK_TIMEOUT");
        drop(lock1);
        cleanup(&dir);
    }

    #[test]
    fn a_second_take_waits_and_names_the_operation_it_is_waiting_for() {
        let (mgr, dir) = test_manager();
        let outer = mgr.acquire(LockResource::ContentStore, "outer").unwrap();
        let error = mgr
            .acquire_with_timeout(
                LockResource::ContentStore,
                "inner",
                Duration::from_millis(50),
                None,
            )
            .expect_err("a nested take is not re-entrant");
        assert_eq!(error.code(), "ERR_LOCK_TIMEOUT");
        let message = error.to_string();
        assert!(
            message.contains("this Agora process already holds it for: outer"),
            "{message}"
        );

        drop(outer);
        mgr.acquire_with_timeout(
            LockResource::ContentStore,
            "after",
            Duration::from_secs(1),
            None,
        )
        .expect("the lock is free after the guard drops");
        cleanup(&dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn two_tasks_on_one_thread_do_not_share_a_lock() {
        let (mgr, dir) = test_manager();
        let (held_tx, held_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let first_manager = mgr.clone();
        let first = tokio::spawn(async move {
            let _guard = first_manager
                .acquire_async(LockResource::ContentStore, "first-task")
                .await
                .expect("the first task takes the lock");
            let _ = held_tx.send(());
            let _ = release_rx.await;
        });
        held_rx.await.expect("the first task acquired the lock");

        // Same OS thread, different task: this must not re-enter.
        let error = mgr
            .acquire_with_timeout_async(
                LockResource::ContentStore,
                "second-task",
                Duration::from_millis(50),
                None,
            )
            .await
            .expect_err("a second task must not get the lock");
        assert_eq!(error.code(), "ERR_LOCK_TIMEOUT");
        assert!(
            error
                .to_string()
                .contains("this Agora process already holds it for: first-task"),
            "{error}"
        );

        let _ = release_tx.send(());
        first.await.expect("the first task ends cleanly");
        cleanup(&dir);
    }

    #[test]
    fn test_serial_acquire_after_release() {
        let (mgr, dir) = test_manager();
        let lock1 = mgr.acquire(LockResource::Materialization, "first").unwrap();
        drop(lock1);
        let lock2 = mgr.acquire(LockResource::Materialization, "second");
        assert!(lock2.is_ok(), "should re-acquire after release");
        drop(lock2);
        cleanup(&dir);
    }

    #[test]
    fn a_dropped_guard_leaves_the_lock_file_and_the_lock_free() {
        let (mgr, dir) = test_manager();
        let lock_path;
        {
            let lock = mgr
                .acquire(LockResource::LoaderInstall, "drop-test")
                .unwrap();
            lock_path = lock.path().to_path_buf();
            assert!(lock_path.exists(), "lock file should exist");
        }
        assert!(
            lock_path.exists(),
            "the lock file is the last holder record and is kept"
        );
        // Another process/manager can take it at once.
        let other = LockManager::new(dir.clone());
        other
            .acquire_with_timeout(
                LockResource::LoaderInstall,
                "next",
                Duration::from_secs(1),
                None,
            )
            .expect("a dropped guard releases the OS lock");
        cleanup(&dir);
    }

    #[test]
    fn an_empty_or_garbage_lock_file_never_blocks() {
        let (mgr, dir) = test_manager();
        let lock_path = dir.join("content-store.lock");
        for contents in [
            &b""[..],
            &b"not-json"[..],
            &br#"{"pid":4000000000,"pid_start_time":1"#[..],
        ] {
            std::fs::write(&lock_path, contents).unwrap();
            let guard = mgr
                .acquire_with_timeout(
                    LockResource::ContentStore,
                    "after-garbage",
                    Duration::from_millis(200),
                    None,
                )
                .unwrap_or_else(|e| {
                    panic!(
                        "an unreadable lock record must not block: {e:?} (contents: {:?})",
                        String::from_utf8_lossy(contents)
                    )
                });
            drop(guard);
        }
        cleanup(&dir);
    }

    #[test]
    fn a_timeout_names_the_holder() {
        let (mgr, dir) = test_manager();
        let holder = mgr
            .acquire(LockResource::ContentStore, "disable_mod")
            .unwrap();
        let contender = mgr.clone();
        let error = std::thread::spawn(move || {
            contender
                .acquire_with_timeout(
                    LockResource::ContentStore,
                    "waiting",
                    Duration::from_millis(50),
                    None,
                )
                .unwrap_err()
        })
        .join()
        .unwrap();
        let message = error.to_string();
        assert!(
            message.contains("this Agora process already holds it for: disable_mod"),
            "{message}"
        );
        assert!(
            !message.contains("another task in this program"),
            "{message}"
        );
        drop(holder);
        cleanup(&dir);
    }

    #[test]
    fn current_operation_reports_only_a_held_lock() {
        let (mgr, dir) = test_manager();
        assert_eq!(mgr.current_operation(&LockResource::ContentStore), None);
        let holder = mgr
            .acquire(LockResource::ContentStore, "content-remove")
            .unwrap();
        assert_eq!(
            mgr.current_operation(&LockResource::ContentStore)
                .as_deref(),
            Some("content-remove")
        );
        drop(holder);
        assert_eq!(mgr.current_operation(&LockResource::ContentStore), None);
        cleanup(&dir);
    }

    #[test]
    fn test_different_lock_names_do_not_conflict() {
        let (mgr, dir) = test_manager();
        let lock1 = mgr.acquire(LockResource::RegistryUpdate, "reg");
        let lock2 = mgr.acquire(LockResource::LoaderInstall, "loader");
        assert!(lock1.is_ok());
        assert!(lock2.is_ok(), "different locks should not conflict");
        drop(lock2);
        drop(lock1);
        cleanup(&dir);
    }

    #[test]
    fn test_java_major_locks_are_independent() {
        let (mgr, dir) = test_manager();
        let lock17 = mgr.acquire(LockResource::JavaMajor(17), "java17");
        let lock21 = mgr.acquire(LockResource::JavaMajor(21), "java21");
        assert!(lock17.is_ok());
        assert!(
            lock21.is_ok(),
            "different Java major locks should be independent"
        );
        drop(lock21);
        drop(lock17);
        cleanup(&dir);
    }

    #[test]
    fn test_instance_lock_contention() {
        let (mgr, dir) = test_manager();
        let lock = mgr
            .acquire(LockResource::Instance("my-instance".into()), "install")
            .unwrap();
        let contender = mgr.clone();
        let result = std::thread::spawn(move || {
            contender.acquire_with_timeout(
                LockResource::Instance("my-instance".into()),
                "second",
                Duration::from_millis(100),
                None,
            )
        })
        .join()
        .unwrap();
        assert!(result.is_err(), "same instance lock should be contested");
        drop(lock);
        cleanup(&dir);
    }

    #[test]
    fn test_cancellation() {
        let (mgr, dir) = test_manager();
        let cancel = CancellationToken::new();

        // Acquire a lock to create contention from another thread.
        let _lock1 = mgr.acquire(LockResource::RegistryUpdate, "holder").unwrap();
        cancel.cancel();
        let contender = mgr.clone();
        let token = cancel.clone();
        let result = std::thread::spawn(move || {
            contender.acquire_with_timeout(
                LockResource::RegistryUpdate,
                "cancelled",
                Duration::from_secs(30),
                Some(&token),
            )
        })
        .join()
        .unwrap();
        assert!(result.is_err(), "cancelled acquisition should fail");
        assert_eq!(result.unwrap_err().code(), "ERR_LOCK_CANCELLED");

        drop(_lock1);
        cleanup(&dir);
    }

    #[test]
    fn test_lock_metadata_serialization() {
        use std::str::FromStr;
        let meta = LockMetadata {
            pid: 12345,
            pid_start_time: 1000000,
            hostname: "test-host".into(),
            acquired_at: "2026-01-01T00:00:00Z".into(),
            operation: "test".into(),
            nonce: uuid::Uuid::from_str("550e8400-e29b-41d4-a716-446655440000")
                .unwrap()
                .as_u128(),
            program: "Agora CLI".into(),
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: LockMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pid, 12345);
        assert_eq!(back.pid_start_time, 1000000);
        assert_eq!(back.hostname, "test-host");
        assert_eq!(back.program, "Agora CLI");
    }

    #[test]
    fn lock_files_from_older_versions_still_read() {
        // A lock written before `program` existed, e.g. by an older CLI.
        let json = r#"{"pid":7,"pid_start_time":0,"hostname":"h","acquired_at":"2026-01-01T00:00:00Z","operation":"install","nonce":1}"#;
        let meta: LockMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(meta.program, "");
        assert_eq!(describe_holder(&meta), "another Agora program (PID 7)");
    }

    #[test]
    fn a_waiting_program_names_the_holder() {
        let mut meta: LockMetadata = serde_json::from_str(
            r#"{"pid":7,"pid_start_time":0,"hostname":"h","acquired_at":"t","operation":"disable_mod","nonce":1,"program":"Agora CLI"}"#,
        )
        .unwrap();
        assert_eq!(describe_holder(&meta), "the Agora CLI (PID 7)");
        meta.pid = std::process::id();
        assert_eq!(describe_holder(&meta), "another task in this program");
    }

    #[test]
    fn test_lock_name_rejects_empty_instance_id() {
        let r = LockResource::Instance("".into());
        assert!(r.lock_name().is_err());
    }

    #[test]
    fn test_lock_name_rejects_traversal_instance_id() {
        let r = LockResource::Instance("../evil".into());
        assert!(r.lock_name().is_err());
    }

    #[test]
    fn test_lock_name_rejects_separator_instance_id() {
        let r = LockResource::Instance("a/b".into());
        assert!(r.lock_name().is_err());
    }

    #[test]
    fn test_lock_name_rejects_zero_java_major() {
        let r = LockResource::JavaMajor(0);
        assert!(r.lock_name().is_err());
    }

    #[test]
    fn test_lock_name_accepts_valid_java_major() {
        let r = LockResource::JavaMajor(17);
        assert_eq!(r.lock_name().unwrap(), "java-major-17");
    }

    #[test]
    fn test_lock_name_accepts_valid_instance() {
        let r = LockResource::Instance("my-instance".into());
        assert_eq!(r.lock_name().unwrap(), "instance-my-instance");
    }

    #[test]
    fn test_lock_name_accepts_content_store() {
        let r = LockResource::ContentStore;
        assert_eq!(r.lock_name().unwrap(), "content-store");
    }

    #[test]
    fn test_lock_name_accepts_game_user_files() {
        let r = LockResource::GameUserFiles(
            GameId::new("skyrim-se").unwrap(),
            StoreId::new("steam").unwrap(),
        );
        assert_eq!(r.lock_name().unwrap(), "game-user-files-skyrim-se-steam");
    }

    #[test]
    fn test_unique_nonce() {
        // Acquire two locks in different directories — their nonces should differ.
        let (mgr1, dir1) = test_manager();
        let (mgr2, dir2) = test_manager();
        let lock1 = mgr1.acquire(LockResource::RegistryUpdate, "test1").unwrap();
        let lock2 = mgr2.acquire(LockResource::RegistryUpdate, "test2").unwrap();
        let first = read_lock_metadata(lock1.path()).unwrap();
        let second = read_lock_metadata(lock2.path()).unwrap();
        assert_ne!(first.nonce, second.nonce, "nonces should be unique");
        drop(lock2);
        drop(lock1);
        cleanup(&dir2);
        cleanup(&dir1);
    }

    // -----------------------------------------------------------------------
    // Cross-process contention tests: independent LockManager instances
    // test that the filesystem-based locking works across logical "processes."
    // -----------------------------------------------------------------------

    #[test]
    fn test_independent_managers_contend_on_registry() {
        let dir = std::env::temp_dir().join(format!("agora-lock-cross-reg-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);

        let mgr1 = LockManager::new(dir.clone());
        let mgr2 = LockManager::new(dir.clone());

        let _lock1 = mgr1
            .acquire(LockResource::RegistryUpdate, "process-a")
            .expect("mgr1 should acquire RegistryUpdate");
        let result = std::thread::spawn({
            let mgr2 = mgr2.clone();
            move || {
                mgr2.acquire_with_timeout(
                    LockResource::RegistryUpdate,
                    "process-b",
                    Duration::from_millis(100),
                    None,
                )
            }
        })
        .join()
        .unwrap();
        assert!(
            result.is_err(),
            "independent managers must contend on RegistryUpdate"
        );
        assert_eq!(result.unwrap_err().code(), "ERR_LOCK_TIMEOUT");

        drop(_lock1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_independent_managers_contend_on_materialization() {
        let dir = std::env::temp_dir().join(format!("agora-lock-cross-mat-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);

        let mgr1 = LockManager::new(dir.clone());
        let mgr2 = LockManager::new(dir.clone());

        let _lock1 = mgr1
            .acquire(LockResource::Materialization, "process-a")
            .expect("mgr1 should acquire Materialization");
        let result = std::thread::spawn({
            let mgr2 = mgr2.clone();
            move || {
                mgr2.acquire_with_timeout(
                    LockResource::Materialization,
                    "process-b",
                    Duration::from_millis(100),
                    None,
                )
            }
        })
        .join()
        .unwrap();
        assert!(
            result.is_err(),
            "independent managers must contend on Materialization"
        );
        assert_eq!(result.unwrap_err().code(), "ERR_LOCK_TIMEOUT");

        drop(_lock1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_independent_managers_different_resources_no_conflict() {
        let dir =
            std::env::temp_dir().join(format!("agora-lock-cross-diff-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);

        let mgr1 = LockManager::new(dir.clone());
        let mgr2 = LockManager::new(dir.clone());

        let _lock_reg = mgr1
            .acquire(LockResource::RegistryUpdate, "process-a-reg")
            .expect("mgr1 should acquire RegistryUpdate");
        let _lock_mat = mgr2
            .acquire(LockResource::Materialization, "process-b-mat")
            .expect("mgr2 should acquire Materialization (different resource)");

        // Both held simultaneously without conflict.
        drop(_lock_reg);
        drop(_lock_mat);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_lock_released_on_drop_after_scope() {
        let (mgr, dir) = test_manager();

        // Scope simulating a service operation that acquires the lock then
        // exits early (simulating a failure mid-transaction).
        {
            let _guard = mgr
                .acquire(LockResource::RegistryUpdate, "test-op")
                .unwrap();
            // Guard drops here as scope exits — the OS lock is released.
        }

        // Re-acquire succeeds after the simulated failure.
        let reacquire = mgr.acquire_with_timeout(
            LockResource::RegistryUpdate,
            "retry",
            Duration::from_millis(100),
            None,
        );
        assert!(
            reacquire.is_ok(),
            "should reacquire after failure-scope release"
        );

        cleanup(&dir);
    }
}
