//! Per-plugin blob storage (MASTER_SPEC §30.13).
//!
//! A plugin's bytes live as files in its own folder under `plugins/data/`, so
//! a masterlist does not have to travel through the script on every run. The
//! script names a key, never a path: core validates the key, enforces one
//! total quota per plugin, and installs a write with a temp file and a rename,
//! so a failed write never damages the copy already there.

use std::path::{Path, PathBuf};

use agora_plugin_api::host::{
    validate_blob_key, ScriptReadError, StageBudget, StageLimits, StoredBlobs,
};
use agora_plugin_api::manifest::PluginId;

use crate::ctx::Ctx;
use crate::error::{LauncherError, LauncherResult};

/// Bytes one plugin may keep in its blob store unless this changes.
pub const DEFAULT_BLOB_QUOTA_BYTES: u64 = 256 * 1024 * 1024;

/// The folder this plugin's blobs live in.
pub fn plugin_dir(ctx: &Ctx, plugin_id: &PluginId) -> PathBuf {
    ctx.paths
        .plugins_root()
        .join("data")
        .join(plugin_id.as_str())
}

fn blob_path(ctx: &Ctx, plugin_id: &PluginId, key: &str) -> PathBuf {
    plugin_dir(ctx, plugin_id).join(key)
}

fn storage_key_error(message: String) -> LauncherError {
    LauncherError::Generic {
        code: "ERR_PLUGIN_STORAGE_KEY".into(),
        message,
    }
}

fn storage_quota_error(message: String) -> LauncherError {
    LauncherError::Generic {
        code: "ERR_PLUGIN_STORAGE_QUOTA".into(),
        message,
    }
}

fn io_error(path: &Path, source: std::io::Error) -> LauncherError {
    LauncherError::Generic {
        code: "ERR_PLUGIN_STORAGE_IO".into(),
        message: format!("could not use {}: {source}", path.display()),
    }
}

fn validate_key(key: &str) -> LauncherResult<()> {
    validate_blob_key(key).map_err(storage_key_error)
}

/// Bytes already stored for a plugin, ignoring a temp file left by a failed
/// write.
fn used_bytes(dir: &Path) -> LauncherResult<u64> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(source) => return Err(io_error(dir, source)),
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(".tmp-") {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_file() {
            total = total.saturating_add(metadata.len());
        }
    }
    Ok(total)
}

/// Install one blob. The write goes to a temp file in the same folder and is
/// renamed over the old copy, so a failure before the rename leaves the old
/// one exactly as it was.
fn install(
    dir: &Path,
    key: &str,
    bytes: &[u8],
    before_rename: impl FnOnce(&Path) -> LauncherResult<()>,
) -> LauncherResult<()> {
    let temp = dir.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let result = (|| {
        std::fs::write(&temp, bytes).map_err(|source| io_error(&temp, source))?;
        before_rename(&temp)?;
        std::fs::rename(&temp, dir.join(key)).map_err(|source| io_error(&temp, source))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// Store `bytes` under `key`, replacing any old copy.
pub fn put(ctx: &Ctx, plugin_id: &PluginId, key: &str, bytes: &[u8]) -> LauncherResult<u64> {
    validate_key(key)?;
    let dir = plugin_dir(ctx, plugin_id);
    std::fs::create_dir_all(&dir).map_err(|source| io_error(&dir, source))?;
    let existing = std::fs::metadata(dir.join(key))
        .ok()
        .filter(|metadata| metadata.is_file())
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let used = used_bytes(&dir)?;
    let after = used
        .saturating_sub(existing)
        .saturating_add(bytes.len() as u64);
    if after > DEFAULT_BLOB_QUOTA_BYTES {
        return Err(storage_quota_error(format!(
            "this plugin may store {DEFAULT_BLOB_QUOTA_BYTES} bytes in its blob store; this write would make it {after}"
        )));
    }
    install(&dir, key, bytes, |_| Ok(()))?;
    Ok(bytes.len() as u64)
}

/// Read one stored blob. A key that was never saved is `None`, not an error.
pub fn get(ctx: &Ctx, plugin_id: &PluginId, key: &str) -> LauncherResult<Option<Vec<u8>>> {
    validate_key(key)?;
    match std::fs::read(blob_path(ctx, plugin_id, key)) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(io_error(&blob_path(ctx, plugin_id, key), source)),
    }
}

/// Remove one stored blob. Returns whether it existed.
pub fn delete(ctx: &Ctx, plugin_id: &PluginId, key: &str) -> LauncherResult<bool> {
    validate_key(key)?;
    match std::fs::remove_file(blob_path(ctx, plugin_id, key)) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(io_error(&blob_path(ctx, plugin_id, key), source)),
    }
}

/// The stored blobs of one plugin, as a wasm mount sees them.
pub struct PluginBlobs {
    pub ctx: Ctx,
    pub plugin_id: PluginId,
}

impl StoredBlobs for PluginBlobs {
    fn plugin(&self) -> Option<&str> {
        Some(self.plugin_id.as_str())
    }

    fn stage(
        &self,
        key: &str,
        destination: &Path,
        limits: &StageLimits,
        budget: &StageBudget,
    ) -> Result<u64, ScriptReadError> {
        validate_blob_key(key)
            .map_err(|message| ScriptReadError::new("invalid_arguments", message))?;
        let source = blob_path(&self.ctx, &self.plugin_id, key);
        let metadata = std::fs::metadata(&source).map_err(|_| {
            ScriptReadError::new(
                "not_found",
                format!("there is no stored blob named `{key}`"),
            )
        })?;
        if !metadata.is_file() {
            return Err(ScriptReadError::new(
                "not_found",
                format!("there is no stored blob named `{key}`"),
            ));
        }
        budget.spend(limits, metadata.len())?;
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                ScriptReadError::new(
                    "staging_failed",
                    format!("could not stage `{key}`: {error}"),
                )
            })?;
        }
        if std::fs::hard_link(&source, destination).is_err() {
            std::fs::copy(&source, destination).map_err(|error| {
                ScriptReadError::new(
                    "staging_failed",
                    format!("could not stage `{key}`: {error}"),
                )
            })?;
        }
        Ok(metadata.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::CoreContext;

    fn context() -> (tempfile::TempDir, Ctx) {
        let dir = tempfile::tempdir().unwrap();
        let ctx = CoreContext::for_testing(dir.path().join("app_data"));
        (dir, ctx)
    }

    fn plugin(name: &str) -> PluginId {
        PluginId::parse(name).unwrap()
    }

    #[test]
    fn a_hostile_key_is_refused() {
        let (_dir, ctx) = context();
        let id = plugin("acme.test");
        for key in [
            "../escape",
            "a/b",
            "a\\b",
            "nul\u{0}byte",
            ".",
            "..",
            "CON",
            "con.txt",
            "LPT9.log",
            "a:b",
            "trailing.",
            "trailing ",
            "",
        ] {
            let error = put(&ctx, &id, key, b"x").unwrap_err();
            assert!(
                matches!(&error, LauncherError::Generic { code, .. } if code == "ERR_PLUGIN_STORAGE_KEY"),
                "key {key:?} gave {error}"
            );
        }
        // A plain name is fine.
        assert_eq!(put(&ctx, &id, "masterlist.yaml", b"x").unwrap(), 1);
    }

    #[test]
    fn a_write_over_the_quota_is_refused_and_keeps_the_old_blob() {
        let (_dir, ctx) = context();
        let id = plugin("acme.test");
        put(&ctx, &id, "keep.bin", b"old").unwrap();
        // One byte over the default quota, minus the old blob's three bytes.
        let too_much = vec![0u8; DEFAULT_BLOB_QUOTA_BYTES as usize + 1];
        let error = put(&ctx, &id, "keep.bin", &too_much).unwrap_err();
        assert!(
            matches!(&error, LauncherError::Generic { code, .. } if code == "ERR_PLUGIN_STORAGE_QUOTA"),
            "{error}"
        );
        assert_eq!(
            get(&ctx, &id, "keep.bin").unwrap().as_deref(),
            Some(&b"old"[..])
        );
    }

    #[test]
    fn a_write_that_dies_before_the_rename_leaves_the_old_blob() {
        let (_dir, ctx) = context();
        let id = plugin("acme.test");
        put(&ctx, &id, "keep.bin", b"old").unwrap();
        let dir = plugin_dir(&ctx, &id);
        let error = install(&dir, "keep.bin", b"new", |_temp| {
            Err(LauncherError::Generic {
                code: "ERR_TEST_FAILURE".into(),
                message: "the write died before the rename".into(),
            })
        })
        .unwrap_err();
        assert!(
            matches!(&error, LauncherError::Generic { code, .. } if code == "ERR_TEST_FAILURE")
        );
        assert_eq!(
            get(&ctx, &id, "keep.bin").unwrap().as_deref(),
            Some(&b"old"[..])
        );
        // The temp file was cleaned up too.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn one_plugin_cannot_read_another_plugins_blobs() {
        let (_dir, ctx) = context();
        let a = plugin("acme.one");
        let b = plugin("acme.two");
        put(&ctx, &a, "secret.bin", b"only mine").unwrap();
        assert_eq!(get(&ctx, &b, "secret.bin").unwrap(), None);
        assert_eq!(
            get(&ctx, &a, "secret.bin").unwrap().as_deref(),
            Some(&b"only mine"[..])
        );
    }
}
