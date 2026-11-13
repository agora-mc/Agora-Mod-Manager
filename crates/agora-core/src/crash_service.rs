//! Core-owned crash enable/disable operations for mod files and other content.
//!
//! Owns: instance and filename validation, instance locking, atomic rename, manifest update and
//! no-clobber enforcement. Crash diagnosis itself lives in the crash engine; the desktop's
//! experiment flow and the CLI call these operations after an answer names a suspect.

use crate::ctx::Ctx;
use crate::error::{LauncherError, LauncherResult};
use crate::helpers::{rename_in_content_dir, set_enabled_in_all_arrays};
use crate::lock_manager::LockResource;
use crate::models::InstanceManifest;
use serde::Serialize;

#[derive(Clone)]
pub struct CrashService {
    ctx: Ctx,
}

impl CrashService {
    pub fn new(ctx: Ctx) -> Self {
        Self { ctx }
    }

    /// Rename `mods/<filename>` to `mods/<filename>.disabled` atomically.
    ///
    /// Validates instance ID and filename, acquires an instance lock, and
    /// updates the manifest to mark the mod as disabled. Never clobbers an
    /// existing `.disabled` file. Returns `Ok(())` if the mod is already
    /// disabled, and an error if the named file does not exist.
    pub fn disable_mod(&self, instance_id: &str, filename: &str) -> LauncherResult<()> {
        let sanitized = validate_instance_id(instance_id)?;
        validate_filename(filename)?;

        let _guard = self
            .ctx
            .lock_manager
            .acquire(LockResource::Instance(sanitized.clone()), "disable_mod")?;

        let mods_dir = self.ctx.paths.instance_dir(&sanitized)?.join("mods");

        let source = mods_dir.join(filename);
        let dest = mods_dir.join(format!("{}.disabled", filename));

        if dest.exists() {
            return Ok(());
        }

        if !source.exists() {
            return Err(LauncherError::Generic {
                code: "ERR_MOD_FILE_NOT_FOUND".into(),
                message: format!("Mod file '{filename}' not found in instance '{instance_id}'."),
            });
        }

        std::fs::rename(&source, &dest).map_err(|e| LauncherError::Generic {
            code: "ERR_MOD_DISABLE".into(),
            message: format!("Failed to rename mod file '{filename}': {e}"),
        })?;

        let manifest_path = self.ctx.paths.instance_manifest(&sanitized)?;
        update_manifest_disable(&manifest_path, filename)?;

        Ok(())
    }

    /// Reverse of `disable_mod`: rename `mods/<filename>.disabled` back to
    /// `mods/<filename>` atomically.
    ///
    /// Validates instance ID and filename, acquires an instance lock, and
    /// updates the manifest to mark the mod as enabled. Never clobbers an
    /// existing target file. Returns `Ok(())` if the mod is already enabled,
    /// and an error if the named file does not exist.
    pub fn enable_mod(&self, instance_id: &str, filename: &str) -> LauncherResult<()> {
        let sanitized = validate_instance_id(instance_id)?;
        validate_filename(filename)?;

        let _guard = self
            .ctx
            .lock_manager
            .acquire(LockResource::Instance(sanitized.clone()), "enable_mod")?;

        let mods_dir = self.ctx.paths.instance_dir(&sanitized)?.join("mods");

        let disabled_path = mods_dir.join(format!("{}.disabled", filename));
        let source = mods_dir.join(filename);

        if !disabled_path.exists() {
            if source.exists() {
                return Ok(());
            }
            return Err(LauncherError::Generic {
                code: "ERR_MOD_FILE_NOT_FOUND".into(),
                message: format!("Mod file '{filename}' not found in instance '{instance_id}'."),
            });
        }

        if source.exists() {
            return Ok(());
        }

        std::fs::rename(&disabled_path, &source).map_err(|e| LauncherError::Generic {
            code: "ERR_MOD_ENABLE".into(),
            message: format!("Failed to rename mod file '{filename}': {e}"),
        })?;

        let manifest_path = self.ctx.paths.instance_manifest(&sanitized)?;
        update_manifest_enable(&manifest_path, filename)?;

        Ok(())
    }

    /// Disable an artifact (any content type) by renaming the file to
    /// `<filename>.disabled` and setting `enabled: false` in the manifest.
    ///
    /// Searches all content subdirectories (mods, resourcepacks, shaderpacks,
    /// datapacks, saves). Returns an error if the file is not found.
    pub fn disable_artifact(&self, instance_id: &str, filename: &str) -> LauncherResult<()> {
        let sanitized = crate::paths::sanitize_id(instance_id);
        let _guard = self.ctx.lock_manager.acquire(
            LockResource::Instance(sanitized.clone()),
            "disable_artifact",
        )?;
        let dir = self.ctx.paths.instance_dir(&sanitized)?;

        let renamed = rename_in_content_dir(&dir, filename, false).is_some();
        if !renamed {
            return Err(LauncherError::Generic {
                code: "ERR_MOD_FILE_NOT_FOUND".to_string(),
                message: format!("File '{filename}' not found in any content directory."),
            });
        }

        let manifest_path = self.ctx.paths.instance_manifest(&sanitized)?;
        let result = if manifest_path.exists() {
            (|| -> LauncherResult<()> {
                let mut manifest = crate::helpers::read_manifest(&manifest_path)?;
                set_enabled_in_all_arrays(&mut manifest, filename, false);
                crate::helpers::atomic_write_manifest(&manifest_path, &manifest)
            })()
        } else {
            Ok(())
        };
        if let Err(error) = result {
            let _ = rename_in_content_dir(&dir, filename, true);
            return Err(error);
        }

        let _ = crate::snapshot::mark_instance_mutated(&dir);

        Ok(())
    }

    /// Re-enable a disabled artifact by renaming `<filename>.disabled` back to
    /// `<filename>` and setting `enabled: true` in the manifest.
    ///
    /// Searches all content subdirectories. Returns `Ok(())` if the file is
    /// already enabled or not found.
    pub fn enable_artifact(&self, instance_id: &str, filename: &str) -> LauncherResult<()> {
        let sanitized = crate::paths::sanitize_id(instance_id);
        let _guard = self
            .ctx
            .lock_manager
            .acquire(LockResource::Instance(sanitized.clone()), "enable_artifact")?;
        let dir = self.ctx.paths.instance_dir(&sanitized)?;

        let renamed = rename_in_content_dir(&dir, filename, true).is_some();

        let manifest_path = self.ctx.paths.instance_manifest(&sanitized)?;
        let result = if manifest_path.exists() {
            (|| -> LauncherResult<()> {
                let mut manifest = crate::helpers::read_manifest(&manifest_path)?;
                set_enabled_in_all_arrays(&mut manifest, filename, true);
                crate::helpers::atomic_write_manifest(&manifest_path, &manifest)
            })()
        } else {
            Ok(())
        };
        if let Err(error) = result {
            if renamed {
                let _ = rename_in_content_dir(&dir, filename, false);
            }
            return Err(error);
        }

        let _ = crate::snapshot::mark_instance_mutated(&dir);

        Ok(())
    }
}

// -----------------------------------------------------------------------
// SuspectScore — ranked suspect mod result
// -----------------------------------------------------------------------

/// A ranked suspect mod with its score and per-signal breakdown.
#[derive(Debug, Clone, Serialize)]
pub struct SuspectScore {
    pub mod_id: String,
    pub filename: String,
    pub total_score: f64,
    pub breakdown: serde_json::Value,
    #[serde(default)]
    pub is_dependent_of: Option<String>,
    /// Curators are reviewing this catalog entry. Shown as context only: it
    /// says nothing about whether the mod caused the crash.
    #[serde(default)]
    pub under_review: bool,
}

// ---------------------------------------------------------------------------
// SuggestedAction — recommended next action for the triage UI
// ---------------------------------------------------------------------------

/// The recommended next action for the triage UI.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind")]
pub enum SuggestedAction {
    GuidedDisable { next_suspect: SuspectScore },
    NoSuspects,
}

// ---------------------------------------------------------------------------
// InvestigationResult — complete result of a crash investigation step
// ---------------------------------------------------------------------------

/// The complete result of a crash investigation step.
#[derive(Debug, Clone, Serialize)]
pub struct InvestigationResult {
    pub suspects: Vec<SuspectScore>,
    pub suggested_action: SuggestedAction,
    pub ruled_out: Vec<String>,
}

// ---------------------------------------------------------------------------
// CrashInvestigation — unified read-only investigation result
// ---------------------------------------------------------------------------

/// What the interactive crash board reads: the evidence Agora collected and the engine answer's
/// ranked suspects, in the shape the experiment flow uses.
#[derive(Debug, Clone, Serialize)]
pub struct CrashInvestigation {
    pub evidence: crate::crash_evidence::CollectedEvidence,
    pub suspects: Vec<SuspectScore>,
    pub failure_category: crate::crash_evidence::FailureCategory,
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Validate and normalize an instance ID.
fn validate_instance_id(instance_id: &str) -> LauncherResult<String> {
    let sanitized = crate::paths::sanitize_id(instance_id);
    crate::app_paths::validate_path_component(&sanitized).map_err(|_| LauncherError::Generic {
        code: "ERR_INVALID_INSTANCE".into(),
        message: "Invalid instance ID.".into(),
    })?;
    if sanitized.is_empty() {
        return Err(LauncherError::Generic {
            code: "ERR_INVALID_INSTANCE".into(),
            message: "Instance ID must not be empty.".into(),
        });
    }
    Ok(sanitized)
}

/// Validate a mod filename:
/// - Must not be empty.
/// - Must not contain path separators.
/// - Must not be `.` or `..` or composed only of dots/dashes.
/// - Must not be absolute.
fn validate_filename(filename: &str) -> LauncherResult<()> {
    if filename.is_empty() {
        return Err(LauncherError::Generic {
            code: "ERR_INVALID_FILENAME".into(),
            message: "Filename must not be empty.".into(),
        });
    }
    if filename.contains('/') || filename.contains('\\') {
        return Err(LauncherError::Generic {
            code: "ERR_INVALID_FILENAME".into(),
            message: "Filename must not contain path separators.".into(),
        });
    }
    let trimmed = filename.trim_matches(|c: char| c == '.' || c == ' ' || c == '-');
    if trimmed.is_empty() {
        return Err(LauncherError::Generic {
            code: "ERR_INVALID_FILENAME".into(),
            message: "Filename must not be composed only of dots, dashes, or spaces.".into(),
        });
    }
    if cfg!(windows) {
        if filename.len() >= 2
            && filename.as_bytes()[0].is_ascii_alphabetic()
            && filename.as_bytes()[1] == b':'
        {
            return Err(LauncherError::Generic {
                code: "ERR_INVALID_FILENAME".into(),
                message: "Filename must not start with a drive letter.".into(),
            });
        }
        if filename.starts_with('\\') || filename.starts_with("//") {
            return Err(LauncherError::Generic {
                code: "ERR_INVALID_FILENAME".into(),
                message: "Filename must not be a UNC path.".into(),
            });
        }
    } else {
        if filename.starts_with('/') {
            return Err(LauncherError::Generic {
                code: "ERR_INVALID_FILENAME".into(),
                message: "Filename must not be an absolute path.".into(),
            });
        }
    }
    Ok(())
}

/// Update the instance manifest to mark a mod as disabled.
///
/// Appends ` [disabled]` to the version string, matching how the content list marks a disabled
/// item.
fn update_manifest_disable(manifest_path: &std::path::Path, filename: &str) -> LauncherResult<()> {
    if !manifest_path.exists() {
        return Ok(());
    }
    let mut manifest = crate::helpers::read_manifest(manifest_path)?;

    for entry in all_mod_entries_mut(&mut manifest) {
        if entry.filename == filename {
            entry.enabled = false;
            let v = entry.version.clone().unwrap_or_default();
            if !v.contains("[disabled]") {
                entry.version = if v.is_empty() {
                    Some("[disabled]".to_string())
                } else {
                    Some(format!("{v} [disabled]"))
                };
            }
            break;
        }
    }

    atomic_write_json(manifest_path, &manifest)
}

/// Update the instance manifest to mark a mod as enabled.
///
/// Strips ` [disabled]` from the version string.
fn update_manifest_enable(manifest_path: &std::path::Path, filename: &str) -> LauncherResult<()> {
    if !manifest_path.exists() {
        return Ok(());
    }
    let mut manifest = crate::helpers::read_manifest(manifest_path)?;

    for entry in all_mod_entries_mut(&mut manifest) {
        if entry.filename == filename {
            entry.enabled = true;
            if let Some(ref v) = entry.version {
                if let Some(stripped) = v.strip_suffix(" [disabled]") {
                    if stripped.is_empty() {
                        entry.version = None;
                    } else {
                        entry.version = Some(stripped.to_string());
                    }
                } else if v == "[disabled]" {
                    entry.version = None;
                }
            }
            break;
        }
    }

    atomic_write_json(manifest_path, &manifest)
}

/// Iterate over all mod entries across content types.
fn all_mod_entries_mut(
    manifest: &mut InstanceManifest,
) -> impl Iterator<Item = &mut crate::models::InstalledMod> {
    manifest
        .mods
        .iter_mut()
        .chain(manifest.resourcepacks.iter_mut())
        .chain(manifest.shaders.iter_mut())
        .chain(manifest.datapacks.iter_mut())
        .chain(manifest.worlds.iter_mut())
}

/// Atomically write JSON: write to .tmp, then rename.
fn atomic_write_json(path: &std::path::Path, value: &impl serde::Serialize) -> LauncherResult<()> {
    let tmp_path = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(value).map_err(|e| LauncherError::Generic {
        code: "ERR_MANIFEST_SERIALIZE".into(),
        message: format!("Could not serialize manifest: {e}"),
    })?;
    std::fs::write(&tmp_path, &text).map_err(|e| LauncherError::Generic {
        code: "ERR_MANIFEST_WRITE".into(),
        message: format!("Could not write manifest: {e}"),
    })?;
    std::fs::rename(&tmp_path, path).map_err(|e| LauncherError::Generic {
        code: "ERR_MANIFEST_WRITE".into(),
        message: format!("Could not finalize manifest: {e}"),
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::CoreContext;
    use std::path::PathBuf;

    /// Per-test unique seed for temp dir names.
    static TEST_COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    /// Create a temp directory and CoreContext unique to one test invocation.
    /// Initializes the local state database so connection-based methods work.
    fn setup_ctx() -> (CoreContext, PathBuf) {
        let seq = TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = std::env::temp_dir().join(format!(
            "agora-crash-svc-test-{}-{}",
            std::process::id(),
            seq
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let ctx = CoreContext::for_testing(tmp.clone());
        // Initialize local_state.db so crash_service connection() works
        crate::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
        (ctx, tmp)
    }

    fn create_instance(ctx: &CoreContext, instance_id: &str, mod_filenames: &[&str]) {
        let instance_dir = ctx.paths.instance_dir(instance_id).unwrap();
        std::fs::create_dir_all(instance_dir.join("mods")).unwrap();

        for fname in mod_filenames {
            std::fs::write(instance_dir.join("mods").join(fname), b"fake mod content").unwrap();
        }

        let manifest = crate::models::InstanceManifest {
            manifest_version: crate::models::CURRENT_MANIFEST_VERSION,
            game_data: Default::default(),
            pack_origin: None,
            instance_id: instance_id.to_string(),
            name: instance_id.to_string(),
            created_from_pack: None,
            minecraft_version: "1.20".into(),
            loader: "fabric".into(),
            loader_version: "0.15.0".into(),
            is_locked: false,
            mods: mod_filenames
                .iter()
                .map(|fname| crate::models::InstalledMod {
                    provider: None,
                    update_pinned: false,
                    pack_managed: false,
                    installed_as_dependency: false,
                    filename: fname.to_string(),
                    registry_id: None,
                    modrinth_id: None,
                    source: "local".into(),
                    source_url: None,
                    version: Some("1.0.0".into()),
                    sha256: "abc".into(),
                    hash_verified: true,
                    installed_at: "2024-01-01T00:00:00Z".into(),
                    java_packages: vec![],
                    mod_jar_id: None,
                    provided_mod_ids: vec![],
                    enabled: true,
                    content_type: "mod".into(),
                    depends_on: vec![],
                    optional_deps: vec![],
                    incompatible_deps: vec![],

                    content_item_id: None,
                    object_sha256: None,
                })
                .collect(),
            resourcepacks: vec![],
            shaders: vec![],
            datapacks: vec![],
            worlds: vec![],
            user_preferences: serde_json::Value::Null,
        };
        let manifest_path = ctx.paths.instance_manifest(instance_id).unwrap();
        std::fs::write(
            &manifest_path,
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
    }

    fn assert_mod_exists(ctx: &CoreContext, instance_id: &str, filename: &str) {
        let path = ctx
            .paths
            .instance_dir(instance_id)
            .unwrap()
            .join("mods")
            .join(filename);
        assert!(path.exists(), "Expected {filename} to exist at {:?}", path);
    }

    fn assert_mod_not_exists(ctx: &CoreContext, instance_id: &str, filename: &str) {
        let path = ctx
            .paths
            .instance_dir(instance_id)
            .unwrap()
            .join("mods")
            .join(filename);
        assert!(
            !path.exists(),
            "Expected {filename} to not exist at {:?}",
            path
        );
    }

    fn assert_disabled_exists(ctx: &CoreContext, instance_id: &str, filename: &str) {
        let path = ctx
            .paths
            .instance_dir(instance_id)
            .unwrap()
            .join("mods")
            .join(format!("{filename}.disabled"));
        assert!(
            path.exists(),
            "Expected {filename}.disabled to exist at {:?}",
            path
        );
    }

    fn read_manifest(ctx: &CoreContext, instance_id: &str) -> crate::models::InstanceManifest {
        let path = ctx.paths.instance_manifest(instance_id).unwrap();
        let text = std::fs::read_to_string(path).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    // ------------------------------------------------------------------
    // disable_mod
    // ------------------------------------------------------------------

    #[test]
    fn test_disable_mod_renames_file_and_updates_manifest() {
        let (ctx, tmp) = setup_ctx();
        let svc = CrashService::new(ctx.clone());
        create_instance(&ctx, "test-instance", &["testmod.jar"]);

        svc.disable_mod("test-instance", "testmod.jar").unwrap();

        assert_mod_not_exists(&ctx, "test-instance", "testmod.jar");
        assert_disabled_exists(&ctx, "test-instance", "testmod.jar");

        let manifest = read_manifest(&ctx, "test-instance");
        let entry = manifest
            .mods
            .iter()
            .find(|m| m.filename == "testmod.jar")
            .unwrap();
        assert!(!entry.enabled);
        assert!(entry.version.as_deref().unwrap().contains("[disabled]"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_disable_mod_already_disabled_noop() {
        let (ctx, tmp) = setup_ctx();
        let svc = CrashService::new(ctx.clone());
        create_instance(&ctx, "test-instance", &["testmod.jar"]);

        svc.disable_mod("test-instance", "testmod.jar").unwrap();
        assert_disabled_exists(&ctx, "test-instance", "testmod.jar");

        svc.disable_mod("test-instance", "testmod.jar").unwrap();
        assert_disabled_exists(&ctx, "test-instance", "testmod.jar");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_disable_mod_missing_source_errors() {
        let (ctx, tmp) = setup_ctx();
        let svc = CrashService::new(ctx.clone());
        create_instance(&ctx, "test-instance", &["other.jar"]);

        let err = svc
            .disable_mod("test-instance", "nonexistent.jar")
            .expect_err("disable should fail for a missing file");
        match err {
            LauncherError::Generic { code, message } => {
                assert_eq!(code, "ERR_MOD_FILE_NOT_FOUND");
                assert!(message.contains("nonexistent.jar"));
                assert!(message.contains("test-instance"));
            }
            other => panic!("expected ERR_MOD_FILE_NOT_FOUND, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ------------------------------------------------------------------
    // enable_mod
    // ------------------------------------------------------------------

    #[test]
    fn test_enable_mod_renames_back_and_updates_manifest() {
        let (ctx, tmp) = setup_ctx();
        let svc = CrashService::new(ctx.clone());
        create_instance(&ctx, "test-instance", &["testmod.jar"]);

        svc.disable_mod("test-instance", "testmod.jar").unwrap();
        assert_disabled_exists(&ctx, "test-instance", "testmod.jar");

        svc.enable_mod("test-instance", "testmod.jar").unwrap();

        assert_mod_exists(&ctx, "test-instance", "testmod.jar");
        assert_mod_not_exists(&ctx, "test-instance", "testmod.jar.disabled");

        let manifest = read_manifest(&ctx, "test-instance");
        let entry = manifest
            .mods
            .iter()
            .find(|m| m.filename == "testmod.jar")
            .unwrap();
        assert!(entry.enabled);
        assert!(!entry
            .version
            .as_deref()
            .unwrap_or("")
            .contains("[disabled]"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_enable_mod_already_enabled_noop() {
        let (ctx, tmp) = setup_ctx();
        let svc = CrashService::new(ctx.clone());
        create_instance(&ctx, "test-instance", &["testmod.jar"]);

        svc.enable_mod("test-instance", "testmod.jar").unwrap();
        assert_mod_exists(&ctx, "test-instance", "testmod.jar");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_enable_mod_missing_file_errors() {
        let (ctx, tmp) = setup_ctx();
        let svc = CrashService::new(ctx.clone());
        create_instance(&ctx, "test-instance", &["other.jar"]);

        let err = svc
            .enable_mod("test-instance", "nonexistent.jar")
            .expect_err("enable should fail for a missing file");
        match err {
            LauncherError::Generic { code, message } => {
                assert_eq!(code, "ERR_MOD_FILE_NOT_FOUND");
                assert!(message.contains("nonexistent.jar"));
                assert!(message.contains("test-instance"));
            }
            other => panic!("expected ERR_MOD_FILE_NOT_FOUND, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_disable_artifact_rolls_back_rename_when_manifest_update_fails() {
        let (ctx, tmp) = setup_ctx();
        let svc = CrashService::new(ctx.clone());
        create_instance(&ctx, "test-instance", &["testmod.jar"]);
        std::fs::write(
            ctx.paths.instance_manifest("test-instance").unwrap(),
            b"not valid json",
        )
        .unwrap();

        assert!(svc
            .disable_artifact("test-instance", "testmod.jar")
            .is_err());
        assert_mod_exists(&ctx, "test-instance", "testmod.jar");
        assert_mod_not_exists(&ctx, "test-instance", "testmod.jar.disabled");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ------------------------------------------------------------------
    // Collision no-clobber
    // ------------------------------------------------------------------

    #[test]
    fn test_disable_mod_no_clobber_existing_disabled() {
        let (ctx, tmp) = setup_ctx();
        let svc = CrashService::new(ctx.clone());
        create_instance(&ctx, "test-instance", &["testmod.jar"]);

        let disabled_path = ctx
            .paths
            .instance_dir("test-instance")
            .unwrap()
            .join("mods")
            .join("testmod.jar.disabled");
        std::fs::write(&disabled_path, b"pre-existing disabled content").unwrap();

        svc.disable_mod("test-instance", "testmod.jar").unwrap();

        let content = std::fs::read_to_string(&disabled_path).unwrap();
        assert_eq!(content, "pre-existing disabled content");

        assert_mod_exists(&ctx, "test-instance", "testmod.jar");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_enable_mod_no_clobber_existing_target() {
        let (ctx, tmp) = setup_ctx();
        let svc = CrashService::new(ctx.clone());
        create_instance(&ctx, "test-instance", &["testmod.jar"]);

        svc.disable_mod("test-instance", "testmod.jar").unwrap();
        assert_disabled_exists(&ctx, "test-instance", "testmod.jar");

        let target_path = ctx
            .paths
            .instance_dir("test-instance")
            .unwrap()
            .join("mods")
            .join("testmod.jar");
        std::fs::write(&target_path, b"replacement content").unwrap();

        svc.enable_mod("test-instance", "testmod.jar").unwrap();

        assert_disabled_exists(&ctx, "test-instance", "testmod.jar");

        let content = std::fs::read_to_string(&target_path).unwrap();
        assert_eq!(content, "replacement content");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ------------------------------------------------------------------
    // Traversal rejection
    // ------------------------------------------------------------------

    #[test]
    fn test_disable_mod_rejects_traversal_filename() {
        let (ctx, tmp) = setup_ctx();
        let svc = CrashService::new(ctx.clone());
        create_instance(&ctx, "safe-instance", &["legit.jar"]);

        let result = svc.disable_mod("safe-instance", "../evil.jar");
        assert!(result.is_err());
        assert!(result.unwrap_err().code().contains("ERR_INVALID_FILENAME"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_disable_mod_rejects_absolute_filename() {
        let (ctx, tmp) = setup_ctx();
        let svc = CrashService::new(ctx.clone());
        create_instance(&ctx, "safe-instance", &["legit.jar"]);

        let result = svc.disable_mod("safe-instance", "/etc/passwd");
        assert!(result.is_err());
        assert!(result.unwrap_err().code().contains("ERR_INVALID_FILENAME"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_disable_mod_rejects_empty_filename() {
        let (ctx, tmp) = setup_ctx();
        let svc = CrashService::new(ctx.clone());
        create_instance(&ctx, "safe-instance", &["legit.jar"]);

        let result = svc.disable_mod("safe-instance", "");
        assert!(result.is_err());
        assert!(result.unwrap_err().code().contains("ERR_INVALID_FILENAME"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_disable_mod_rejects_separator_in_filename() {
        let (ctx, tmp) = setup_ctx();
        let svc = CrashService::new(ctx.clone());
        create_instance(&ctx, "safe-instance", &["legit.jar"]);

        let result = svc.disable_mod("safe-instance", "foo/bar.jar");
        assert!(result.is_err());
        assert!(result.unwrap_err().code().contains("ERR_INVALID_FILENAME"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_enable_mod_rejects_traversal_filename() {
        let (ctx, tmp) = setup_ctx();
        let svc = CrashService::new(ctx.clone());
        create_instance(&ctx, "safe-instance", &["legit.jar"]);

        let result = svc.enable_mod("safe-instance", "../../malicious.jar");
        assert!(result.is_err());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_enable_mod_rejects_empty_filename() {
        let (ctx, tmp) = setup_ctx();
        let svc = CrashService::new(ctx.clone());
        create_instance(&ctx, "safe-instance", &["legit.jar"]);

        let result = svc.enable_mod("safe-instance", "");
        assert!(result.is_err());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ------------------------------------------------------------------
    // validate_filename unit tests (no ctx needed)
    // ------------------------------------------------------------------

    #[test]
    fn test_validate_filename_accepts_normal() {
        assert!(validate_filename("mod.jar").is_ok());
        assert!(validate_filename("my-mod_1.0.jar").is_ok());
        assert!(validate_filename("a").is_ok());
    }

    #[test]
    fn test_validate_filename_rejects_empty() {
        assert!(validate_filename("").is_err());
    }

    #[test]
    fn test_validate_filename_rejects_separator() {
        assert!(validate_filename("a/b").is_err());
        assert!(validate_filename("a\\b").is_err());
    }

    #[test]
    fn test_validate_filename_rejects_dot_only() {
        assert!(validate_filename(".").is_err());
        assert!(validate_filename("..").is_err());
        assert!(validate_filename("...").is_err());
    }

    #[test]
    fn test_validate_filename_rejects_absolute_unix() {
        if !cfg!(windows) {
            assert!(validate_filename("/etc/passwd").is_err());
        }
    }

    #[test]
    fn test_validate_filename_rejects_windows_drive_letter() {
        if cfg!(windows) {
            assert!(validate_filename("C:foo.jar").is_err());
            assert!(validate_filename("D:\\bar.jar").is_err());
        }
    }
}
