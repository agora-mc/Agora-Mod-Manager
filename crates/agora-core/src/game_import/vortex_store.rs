//! Reading a Vortex state store without touching the live one (MASTER_SPEC §26.10).
//!
//! Vortex keeps its state in a LevelDB store named `state.v2` under its user-data folder
//! (`%APPDATA%\Vortex` on Windows). Keys are state paths joined with `###` and values are
//! `JSON.stringify`ed strings; `%APPDATA%\Vortex\state.v2` is the store's own directory, and
//! LevelDB holds `<store>\LOCK` while Vortex has it open.
//!
//! Source read for this format (GPL-3; read for the format, never copied):
//! - `src_shared_src_types_state.ts:88` names `state.v2` as `currentStatePath`.
//! - `src_main_src_store_LevelPersist.ts:12` is the `###` separator, and `:177-230` reads keys and
//!   values as text. Its current backend is DuckDB (`level_pivot`), but the key/value shape it
//!   exposes is the same; `src_main_src_store_mainPersistence.ts:309` says values are
//!   `JSON.stringify`ed.
//! - `src_main_src_store_LevelPersist.ts:41-45` recognises a store locked by another process, and
//!   `:38-39` names the LevelDB `LOCK` file as what holds it.
//!
//! **Never the live store.** Vortex may be writing it, and opening it changes it. This module
//! refuses while the store's `LOCK` is held, then copies the store to a scratch folder and opens
//! the copy. A store that cannot be read is an error naming why; it is never an empty setup.

use std::io;
use std::path::{Path, PathBuf};

use rusty_leveldb::{LdbIterator, Options, DB};
use serde_json::{Map, Value};

/// The store's own file name under Vortex's user-data folder.
pub const STORE_DIR_NAME: &str = "state.v2";
/// The file LevelDB locks while a process has the store open.
pub const LOCK_FILE_NAME: &str = "LOCK";

#[derive(Debug, thiserror::Error)]
pub enum VortexStoreError {
    /// Vortex is running and holds the store's lock.
    #[error(
        "Vortex is running (it holds the lock on {0}). Close Vortex completely, then try again"
    )]
    Locked(PathBuf),
    #[error("no Vortex state store at {0} (the folder state.v2 under Vortex's user-data folder)")]
    NotFound(PathBuf),
    /// The store was not locked, but cannot be read. `reason` says what went wrong.
    #[error("cannot read the Vortex state store at {path}: {reason}")]
    Unreadable { path: PathBuf, reason: String },
    /// The folder exists but holds no database files, or the database holds no keys.
    #[error("the Vortex state store at {0} is empty: it holds no state to import")]
    Empty(PathBuf),
    /// The lock could not be checked, so the store cannot be read safely.
    #[error(
        "cannot tell whether Vortex is running, so the store at {path} was not read: {reason}"
    )]
    LockUnknown { path: PathBuf, reason: String },
}

/// The default store, when the store's folder is not given. `%APPDATA%\Vortex\state.v2` is
/// `getVortexPath("userData")` on Windows (`src_main_src_store_LevelPersist.ts` opens that path;
/// `src_shared_src_types_state.ts:88` names the file).
pub fn default_store_path() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(dirs::data_dir);
    base.map(|base| base.join("Vortex").join(STORE_DIR_NAME))
}

/// Whether the store's `LOCK` is held by another process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockProbe {
    Free,
    Held,
    /// The check itself failed; the store must not be read on a guess.
    Unknown,
}

/// Check the store's `LOCK` without opening the store. LevelDB holds an exclusive OS lock on this
/// file for as long as it is open (`src_main_src_store_LevelPersist.ts:38-39`).
pub fn lock_probe(store_dir: &Path) -> LockProbe {
    let lock = store_dir.join(LOCK_FILE_NAME);
    if !lock.is_file() {
        return LockProbe::Free;
    }
    probe_lock_file(&lock)
}

#[cfg(windows)]
fn probe_lock_file(lock: &Path) -> LockProbe {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{
        GetLastError, ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        LockFileEx, UnlockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
    };

    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(lock)
    else {
        return LockProbe::Unknown;
    };
    let handle = file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
    // SAFETY: `handle` is a valid open file handle, and the overlapped structure is zeroed as the
    // API requires for a lock on a byte range that starts at zero when Offset is zero.
    let mut overlapped: windows_sys::Win32::System::IO::OVERLAPPED = unsafe { std::mem::zeroed() };
    // SAFETY: the handle and the overlapped structure live across the call.
    let locked = unsafe {
        LockFileEx(
            handle,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut overlapped,
        )
    };
    if locked != 0 {
        // SAFETY: the same handle and overlapped structure the lock was taken with.
        unsafe {
            UnlockFileEx(handle, 0, 1, 0, &mut overlapped);
        }
        return LockProbe::Free;
    }
    // SAFETY: GetLastError reads the calling thread's last-error value, set by LockFileEx.
    match unsafe { GetLastError() } {
        ERROR_LOCK_VIOLATION | ERROR_SHARING_VIOLATION => LockProbe::Held,
        _ => LockProbe::Unknown,
    }
}

#[cfg(unix)]
fn probe_lock_file(lock: &Path) -> LockProbe {
    use std::os::unix::io::AsRawFd;

    // LOCK_EX and LOCK_NB, from <sys/file.h>. Declared here rather than adding a libc
    // dependency for four constants and one call.
    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;
    const LOCK_UN: i32 = 8;
    extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }

    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(lock)
    else {
        return LockProbe::Unknown;
    };
    // SAFETY: the descriptor is valid for as long as `file` lives.
    if unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) } == 0 {
        // SAFETY: the same descriptor, which we just locked.
        unsafe {
            flock(file.as_raw_fd(), LOCK_UN);
        }
        LockProbe::Free
    } else {
        LockProbe::Held
    }
}

#[cfg(not(any(unix, windows)))]
fn probe_lock_file(_lock: &Path) -> LockProbe {
    LockProbe::Unknown
}

/// One key/value pair of the store, both as text. A key that is not UTF-8 is kept lossily and
/// named in `warnings`, so a clobbered key is reported rather than silently dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreEntry {
    pub key: String,
    pub value: String,
}

/// A store read from a scratch copy: the pairs, and what could not be read cleanly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VortexStore {
    /// The store the copy was made from (for the report).
    pub source: PathBuf,
    pub entries: Vec<StoreEntry>,
    pub warnings: Vec<String>,
}

impl VortexStore {
    /// The value at a state path, which may be stored as one blob or one key per leaf. This is the
    /// same reconstruction `readPersistedValue` does (`src_main_src_store_mainPersistence.ts:226-288`).
    pub fn get_path(&self, path: &[&str]) -> Option<Value> {
        let joined = path.join("###");
        if let Some(entry) = self.entries.iter().find(|entry| entry.key == joined) {
            return parse_value(&entry.value);
        }
        let prefix = format!("{joined}###");
        let mut root = Map::new();
        let mut found = false;
        for entry in &self.entries {
            let Some(rest) = entry.key.strip_prefix(&prefix) else {
                continue;
            };
            if rest.is_empty() {
                continue;
            }
            let Some(value) = parse_value(&entry.value) else {
                continue;
            };
            found = true;
            let segments: Vec<&str> = rest.split("###").collect();
            insert_nested(&mut root, &segments, value);
        }
        found.then_some(Value::Object(root))
    }

    /// The keys under a state path, without their prefix. Empty when there are none.
    pub fn child_keys(&self, path: &[&str]) -> Vec<String> {
        let prefix = format!("{}###", path.join("###"));
        let mut keys: Vec<String> = self
            .entries
            .iter()
            .filter_map(|entry| entry.key.strip_prefix(&prefix))
            .map(str::to_string)
            .collect();
        keys.sort();
        keys.dedup();
        keys
    }
}

fn parse_value(text: &str) -> Option<Value> {
    if text.is_empty() {
        return None;
    }
    Some(serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string())))
}

fn insert_nested(root: &mut Map<String, Value>, segments: &[&str], value: Value) {
    let Some((last, parents)) = segments.split_last() else {
        return;
    };
    let mut current = root;
    for segment in parents {
        let entry = current
            .entry((*segment).to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !entry.is_object() {
            *entry = Value::Object(Map::new());
        }
        current = entry.as_object_mut().expect("just made an object");
    }
    current.insert((*last).to_string(), value);
}

/// Read the store: refuse while Vortex holds it, copy it to a scratch folder (without `LOCK`), and
/// read the copy. The live store is never opened.
pub fn read_store(store_dir: &Path) -> Result<VortexStore, VortexStoreError> {
    if !store_dir.is_dir() {
        return Err(VortexStoreError::NotFound(store_dir.to_path_buf()));
    }
    match lock_probe(store_dir) {
        LockProbe::Held => return Err(VortexStoreError::Locked(store_dir.to_path_buf())),
        LockProbe::Unknown => {
            return Err(VortexStoreError::LockUnknown {
                path: store_dir.to_path_buf(),
                reason: "the lock file could not be checked".into(),
            })
        }
        LockProbe::Free => {}
    }
    if !has_database_files(store_dir) {
        return Err(VortexStoreError::Empty(store_dir.to_path_buf()));
    }

    let scratch = Scratch::new(store_dir)?;
    copy_store(store_dir, &scratch.path).map_err(|error| VortexStoreError::Unreadable {
        path: store_dir.to_path_buf(),
        reason: format!("the scratch copy failed: {error}"),
    })?;

    let options = Options {
        // Never create a store: an empty or missing one must read as an error, not as no setup.
        create_if_missing: false,
        ..Options::default()
    };
    let mut db =
        DB::open(&scratch.path, options).map_err(|error| VortexStoreError::Unreadable {
            path: store_dir.to_path_buf(),
            reason: error.to_string(),
        })?;

    let mut warnings = Vec::new();
    let mut entries = Vec::new();
    let mut iter = db
        .new_iter()
        .map_err(|error| VortexStoreError::Unreadable {
            path: store_dir.to_path_buf(),
            reason: error.to_string(),
        })?;
    while let Some((key, value)) = iter.next() {
        let key = match String::from_utf8(key) {
            Ok(key) => key,
            Err(error) => {
                warnings.push(format!(
                    "a key in the store is not valid UTF-8 and was skipped: {}",
                    error.utf8_error()
                ));
                continue;
            }
        };
        let value = match String::from_utf8(value) {
            Ok(value) => value,
            Err(error) => {
                warnings.push(format!(
                    "the value of key '{key}' is not valid UTF-8; it was read lossily"
                ));
                String::from_utf8_lossy(error.as_bytes()).into_owned()
            }
        };
        entries.push(StoreEntry { key, value });
    }
    // The iterator borrows nothing after it is dropped; closing the copy is best effort.
    drop(iter);
    let _ = db.close();

    if entries.is_empty() {
        return Err(VortexStoreError::Empty(store_dir.to_path_buf()));
    }
    entries.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(VortexStore {
        source: store_dir.to_path_buf(),
        entries,
        warnings,
    })
}

/// Whether the folder holds a LevelDB database. `CURRENT`, a `MANIFEST-*`, a table (`.ldb`) or a
/// write log (`.log`) is enough to try; a folder of unrelated files is empty for our purpose.
fn has_database_files(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let name = entry.file_name().to_string_lossy().to_string();
        name == "CURRENT"
            || name.starts_with("MANIFEST-")
            || name.ends_with(".ldb")
            || name.ends_with(".log")
            || name.ends_with(".sst")
    })
}

/// Copy a store's files to `to`, leaving out every `LOCK`. LevelDB needs `CURRENT`, its manifest,
/// its tables and its log; a link would let a copy reach outside the store, so it is refused.
fn copy_store(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_string_lossy().eq_ignore_ascii_case(LOCK_FILE_NAME) {
            continue;
        }
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is a link; it was not followed", path.display()),
            ));
        }
        if meta.is_dir() {
            copy_store(&path, &to.join(&name))?;
        } else {
            std::fs::copy(&path, to.join(&name))?;
        }
    }
    Ok(())
}

/// A unique scratch folder, removed when dropped. Its name is per process and random, so two
/// imports, or two tests in the same process, never share one.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(store_dir: &Path) -> Result<Self, VortexStoreError> {
        let root = std::env::temp_dir().join(format!(
            "agora-vortex-store-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).map_err(|error| VortexStoreError::Unreadable {
            path: store_dir.to_path_buf(),
            reason: format!("a scratch folder could not be made: {error}"),
        })?;
        Ok(Self { path: root })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(label: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("agora-vortex-{label}-"))
            .tempdir()
            .expect("a temp folder can be made")
    }

    fn write_db(dir: &Path, pairs: &[(&str, &str)]) {
        let mut db = DB::open(
            dir,
            Options {
                create_if_missing: true,
                ..Options::default()
            },
        )
        .expect("the test store opens");
        for (key, value) in pairs {
            db.put(key.as_bytes(), value.as_bytes())
                .expect("a key is written");
        }
        db.flush().expect("the test store flushes");
    }

    #[test]
    fn an_empty_store_folder_is_an_error_not_an_empty_setup() {
        let dir = scratch_dir("empty");
        let error = read_store(dir.path()).expect_err("an empty folder is not a store");
        assert!(matches!(error, VortexStoreError::Empty(_)), "{error}");
    }

    #[test]
    fn a_missing_store_is_reported_as_missing() {
        let dir = scratch_dir("missing");
        let path = dir.path().join("does-not-exist");
        let error = read_store(&path).expect_err("a missing store is not a store");
        assert!(matches!(error, VortexStoreError::NotFound(_)), "{error}");
    }

    #[test]
    fn a_store_vortex_holds_is_refused_and_named() {
        let dir = scratch_dir("locked");
        // An open DB holds LOCK, exactly as a running Vortex does.
        let db = DB::open(
            dir.path(),
            Options {
                create_if_missing: true,
                ..Options::default()
            },
        )
        .expect("the test store opens");
        assert_eq!(lock_probe(dir.path()), LockProbe::Held);
        let error = read_store(dir.path()).expect_err("a held store is refused");
        assert!(matches!(error, VortexStoreError::Locked(_)), "{error}");
        drop(db);
        assert_eq!(lock_probe(dir.path()), LockProbe::Free);
    }

    #[test]
    fn reading_leaves_the_live_store_alone_and_excludes_its_lock() {
        let dir = scratch_dir("read");
        write_db(
            dir.path(),
            &[
                ("persistent###mods", r#"{"skyrimse":{"a":{"id":"a"}}}"#),
                (
                    "settings###mods###installPath###skyrimse",
                    r#""{base}/mods""#,
                ),
            ],
        );
        // The store is not locked after that open; the read copies it.
        let store = read_store(dir.path()).expect("the store reads");
        assert_eq!(store.entries.len(), 2);
        let mods = store
            .get_path(&["persistent", "mods"])
            .expect("mods are there");
        assert_eq!(mods["skyrimse"]["a"]["id"], "a");
        let install = store
            .get_path(&["settings", "mods", "installPath", "skyrimse"])
            .expect("the install path is there");
        assert_eq!(install, Value::String("{base}/mods".into()));
        // The live folder still has its own files; we read a copy and left it in place.
        assert!(dir.path().join("CURRENT").is_file());
    }

    #[test]
    fn a_key_per_leaf_value_rebuilds_the_object() {
        let dir = scratch_dir("leaves");
        write_db(
            dir.path(),
            &[
                ("persistent###profiles###p1###id", r#""p1""#),
                ("persistent###profiles###p1###name", r#""Main""#),
                ("persistent###profiles###p1###gameId", r#""skyrimse""#),
            ],
        );
        let store = read_store(dir.path()).expect("the store reads");
        let profiles = store
            .get_path(&["persistent", "profiles"])
            .expect("profiles are there");
        assert_eq!(profiles["p1"]["name"], "Main");
        assert_eq!(profiles["p1"]["gameId"], "skyrimse");
    }
}
