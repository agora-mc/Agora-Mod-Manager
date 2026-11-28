//! Core-owned InstallService — resolves and executes install plans.
//!
//! Owns: intent validation, instance/manifest loading, registry revision,
//! removal reverse-dependency planning, source-specific artifact resolution
//! (curated, Modrinth, manual), and InstallPipeline delegation.

use crate::install_pipeline::{
    CancellationToken, InstallIntent, InstallOutcome, InstallPipeline, PreparedPlan,
    ProgressReporter, ResolvedInstallPlan, ResolvedOperation, ReverseDepInfo,
};
use crate::resolver::Resolver;
use ac::{
    artifact_hash::{verify_download, ConfirmableHash, HashOrigin, Sha1, Sha256, Sha512},
    ctx::Ctx,
    dependency_ops::AliasMap,
    error::{LauncherError, LauncherResult},
    models::{InstalledMod, InstanceManifest},
};
use std::path::PathBuf;

use agora_core as ac;

// ---------------------------------------------------------------------------
// InstanceLoadResult
// ---------------------------------------------------------------------------

/// Result of loading instance state from the filesystem.
pub struct InstanceLoadResult {
    pub instance_dir: PathBuf,
    pub manifest: InstanceManifest,
    pub registry_revision: String,
}

// ---------------------------------------------------------------------------
// InstallRequest (MASTER_SPEC §32.5)
// ---------------------------------------------------------------------------

/// Where a single-file install comes from. The manifest's `source` string and
/// the content store's provenance are derived from the variant, never passed
/// beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallSource {
    /// A catalog entry, resolved through its download sources.
    Catalog {
        registry_id: String,
        /// The entry's Modrinth project, when it has one.
        modrinth_id: Option<String>,
    },
    /// A Modrinth project version installed directly, outside the catalog.
    Modrinth { project_id: String },
}

impl InstallSource {
    /// The manifest's `source` value for a file installed from here.
    pub fn stored_source(&self) -> &'static str {
        match self {
            InstallSource::Catalog { .. } => "registry",
            InstallSource::Modrinth { .. } => "modrinth_raw",
        }
    }

    fn registry_id(&self) -> Option<&str> {
        match self {
            InstallSource::Catalog { registry_id, .. } => Some(registry_id),
            InstallSource::Modrinth { .. } => None,
        }
    }

    fn modrinth_id(&self) -> Option<&str> {
        match self {
            InstallSource::Catalog { modrinth_id, .. } => modrinth_id.as_deref(),
            InstallSource::Modrinth { project_id } => Some(project_id),
        }
    }

    /// The content store's provider and its id for the item.
    fn provenance(&self) -> (&'static str, &str) {
        match self {
            InstallSource::Catalog { registry_id, .. } => ("catalog", registry_id),
            InstallSource::Modrinth { project_id } => ("modrinth", project_id),
        }
    }
}

/// The file a single-file install writes, and where it is downloaded from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallTarget {
    /// One portable file name; refused before anything is written otherwise.
    pub filename: String,
    /// `mod`, `resourcepack`, `shader`, ...; empty means `mod`.
    pub content_type: String,
    /// The release the file belongs to, when the source names one.
    pub version: Option<String>,
    pub download_url: String,
}

/// Every hash a single-file install is checked against, other than the one
/// Agora remembers from an earlier install of the same release file (that is
/// read from the instance, [`HashOrigin::PreviousInstall`]).
///
/// Each one is a parsed digest: a field that is present was supplied and is
/// well formed, and "none" is `None`, never an empty string. So a blank or
/// malformed hash is refused where it is read, before any download.
///
/// The five are separate on purpose: they differ in who vouches for the file
/// and in what a mismatch does, so none of them merges into another.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExpectedHashes {
    /// Published by the source (Modrinth's file hashes, GitHub's asset digest,
    /// the manifest hash of a `direct_hash` or `technic_pack` entry, which names
    /// its one file). Every one present must match, and nothing overrides a
    /// mismatch. A file with none of the three is installed as not verified.
    pub published_sha1: Option<Sha1>,
    pub published_sha256: Option<Sha256>,
    pub published_sha512: Option<Sha512>,
    /// The SHA-256 a shared pack file lists for this file
    /// ([`HashOrigin::PackPin`]). A mismatch asks the user first; a match is
    /// never counted as the source having verified the file.
    pub pack_sha256: Option<Sha256>,
    /// The catalog curator's pin for this release file
    /// ([`HashOrigin::CuratorPin`]). A mismatch asks the user first.
    pub curator_sha256: Option<Sha256>,
}

impl ExpectedHashes {
    /// The hashes a source published for `file`, parsed (`None` for one it did
    /// not publish).
    pub fn published(
        file: &str,
        sha1: Option<&str>,
        sha256: Option<&str>,
        sha512: Option<&str>,
    ) -> LauncherResult<Self> {
        Ok(Self {
            published_sha1: Sha1::parse_optional(file, "published SHA-1", sha1)?,
            published_sha256: Sha256::parse_optional(file, "published SHA-256", sha256)?,
            published_sha512: Sha512::parse_optional(file, "published SHA-512", sha512)?,
            pack_sha256: None,
            curator_sha256: None,
        })
    }

    /// Add the pack file's hash for `file`, if it lists one.
    pub fn with_pack_hash(mut self, file: &str, sha256: Option<&str>) -> LauncherResult<Self> {
        self.pack_sha256 = Sha256::parse_optional(file, "pack hash", sha256)?;
        Ok(self)
    }

    /// Add the curator's pin for `file`, if the catalog has one.
    pub fn with_curator_pin(mut self, file: &str, sha256: Option<&str>) -> LauncherResult<Self> {
        self.curator_sha256 = Sha256::parse_optional(file, "curator pin", sha256)?;
        Ok(self)
    }
}

/// One single-file install: what to write, from where, checked against what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallRequest {
    pub instance_id: String,
    pub target: InstallTarget,
    pub source: InstallSource,
    pub hashes: ExpectedHashes,
    /// The user's answer to `ERR_HASH_CONFIRMATION_REQUIRED`: install a file
    /// that differs from a pack hash, a curator pin or an earlier install's
    /// hash. Never lets a mismatch against a published hash through.
    pub accept_hash_confirmation: bool,
}

impl InstallRequest {
    /// A catalog entry's release file, with the hashes its source and its
    /// curator assert for it. Fails, before anything is downloaded, when one of
    /// them is blank or malformed.
    pub fn for_catalog_candidate(
        instance_id: &str,
        item: &ac::registry::RegistryItem,
        candidate: &ac::models::ModVersionCandidate,
    ) -> LauncherResult<Self> {
        let file = candidate.filename.as_str();
        let published_sha256 =
            crate::agora_pack_import::expected_sha256_for_candidate(item, candidate)?;
        let curator_pin = crate::resolver::curator_pins_for(item, candidate)
            .into_iter()
            .next()
            .map(|pin| pin.sha256);
        let hashes = ExpectedHashes::published(
            file,
            candidate.sha1.as_deref(),
            published_sha256.as_deref(),
            candidate.sha512.as_deref(),
        )?
        .with_curator_pin(file, curator_pin.as_deref())?;
        Ok(Self {
            instance_id: instance_id.to_string(),
            target: InstallTarget {
                filename: candidate.filename.clone(),
                content_type: item.content_type.clone(),
                version: Some(candidate.version.clone()),
                download_url: candidate.download_url.clone(),
            },
            source: InstallSource::Catalog {
                registry_id: item.id.clone(),
                modrinth_id: item.modrinth_id.clone(),
            },
            hashes,
            accept_hash_confirmation: false,
        })
    }
}

// ---------------------------------------------------------------------------
// InstallService
// ---------------------------------------------------------------------------

/// Core-owned install service.
///
/// Create via [`InstallService::new`] with a [`Ctx`].
#[derive(Clone)]
pub struct InstallService {
    ctx: Ctx,
    providers: Option<ac::providers::ProviderRegistry>,
    /// The user's explicit answer to a running game: change the instance
    /// anyway. The instance lock is still taken; see [`InstanceGuard`].
    allow_while_running: bool,
}

impl InstallService {
    pub fn new(ctx: Ctx) -> Self {
        Self {
            ctx,
            providers: None,
            allow_while_running: false,
        }
    }

    /// Allow changes while the instance's game is running. Only for a caller
    /// that has explicitly confirmed it (the CLI's `--while-running` flag or a
    /// desktop confirmation); the instance lock is still taken.
    pub fn while_running(mut self) -> Self {
        self.allow_while_running = true;
        self
    }

    fn mutation_guard(
        &self,
        instance_id: &str,
        operation: &str,
    ) -> LauncherResult<ac::instance_runtime::InstanceGuard> {
        if self.allow_while_running {
            ac::instance_runtime::InstanceGuard::for_mutation_while_running(
                &self.ctx,
                instance_id,
                operation,
            )
        } else {
            ac::instance_runtime::InstanceGuard::for_mutation(&self.ctx, instance_id, operation)
        }
    }

    async fn mutation_guard_async(
        &self,
        instance_id: &str,
        operation: &str,
    ) -> LauncherResult<ac::instance_runtime::InstanceGuard> {
        if self.allow_while_running {
            ac::instance_runtime::InstanceGuard::for_mutation_while_running_async(
                &self.ctx,
                instance_id,
                operation,
            )
            .await
        } else {
            ac::instance_runtime::InstanceGuard::for_mutation_async(
                &self.ctx,
                instance_id,
                operation,
            )
            .await
        }
    }

    /// Resolve `SourceType::Provider` items against these providers. Without
    /// a registry, a provider item fails to resolve with a clear error rather
    /// than falling through to another source.
    pub fn with_providers(mut self, providers: ac::providers::ProviderRegistry) -> Self {
        self.providers = Some(providers);
        self
    }

    /// Validate instance ID and load its manifest + registry revision.
    pub fn load_instance(&self, instance_id: &str) -> LauncherResult<InstanceLoadResult> {
        let sanitized = ac::paths::sanitize_id(instance_id);
        ac::app_paths::validate_path_component(&sanitized).map_err(|_| LauncherError::Generic {
            code: "ERR_INVALID_INSTANCE".into(),
            message: "Invalid instance ID.".into(),
        })?;
        let instance_dir = self.ctx.paths.instance_dir(&sanitized)?;
        let manifest_path = self.ctx.paths.instance_manifest(&sanitized)?;
        if !instance_dir.exists() || !manifest_path.exists() {
            return Err(LauncherError::Generic {
                code: "ERR_INSTANCE_NOT_FOUND".into(),
                message: format!("Instance '{instance_id}' not found."),
            });
        }
        let manifest = ac::helpers::read_manifest(&manifest_path)?;
        let registry_revision = self.compute_registry_revision()?;
        Ok(InstanceLoadResult {
            instance_dir,
            manifest,
            registry_revision,
        })
    }

    /// Prepare a removal `PreparedPlan` (pure logic, no network needed).
    ///
    /// Reverse-dependency planning is computed here rather than left to the
    /// caller. This is the core-owned replacement for the desktop adapter's
    /// `prepare_remove` and the CLI's inline removal code.
    pub fn prepare_removal(
        manifest: &InstanceManifest,
        filename: &str,
        registry_revision: String,
    ) -> PreparedPlan {
        let target = all_installed(manifest).find(|item| {
            item.filename == filename
                || effective_filename(item) == filename
                || item
                    .registry_id
                    .as_deref()
                    .map(|id| id.eq_ignore_ascii_case(filename))
                    .unwrap_or(false)
                || item
                    .modrinth_id
                    .as_deref()
                    .map(|id| id.eq_ignore_ascii_case(filename))
                    .unwrap_or(false)
        });

        let (target_filename, reverse_dependents) = match target {
            Some(target) => {
                let aliases = AliasMap::from_pairs(&[]);
                let installed: Vec<InstalledMod> = all_installed(manifest).cloned().collect();
                let removal = ac::dependency_ops::build_removal_plan_with_aliases(
                    &installed, target, &aliases,
                );
                (
                    effective_filename(target),
                    removal
                        .dependents
                        .into_iter()
                        .map(|d| ReverseDepInfo {
                            mod_jar_id: d.mod_id,
                            filename: d.filename,
                            requirement: d.requirement,
                            impact: Some("Would lose a required dependency".into()),
                        })
                        .collect(),
                )
            }
            None => (filename.to_string(), vec![]),
        };

        PreparedPlan {
            operation: ResolvedOperation::Remove {
                target_filename,
                reverse_dependents,
                content_type: None,
            },
            dependencies: vec![],
            conflicts: vec![],
            registry_revision,
        }
    }

    /// Resolve an install intent into a read-only plan.
    ///
    /// Uses the core-owned [`Resolver`] to prepare curated, Modrinth, or
    /// manual artifacts and dependency dispositions, then normalizes through
    /// [`InstallPipeline`] to produce the final plan.
    pub async fn resolve(
        &self,
        intent: InstallIntent,
        reporter: &dyn ProgressReporter,
    ) -> LauncherResult<ResolvedInstallPlan> {
        let load = self.load_instance(&intent.target_instance)?;
        let mut resolver = Resolver::new(self.ctx.clone());
        if let Some(providers) = &self.providers {
            resolver = resolver.with_providers(providers.clone());
        }
        if let Some(token) = std::env::var("GITHUB_TOKEN")
            .ok()
            .filter(|value| !value.is_empty())
        {
            resolver = resolver.with_github_token(token);
        } else if let Some(token) = ac::auth::get_token() {
            resolver = resolver.with_stored_github_token(token);
        }
        let prepared = resolver.resolve(&intent, &load.manifest).await?;
        let mut plan = InstallPipeline
            .resolve_plan(intent, &load.instance_dir, prepared, reporter)
            .await
            .map_err(|e| LauncherError::Generic {
                code: "ERR_RESOLVE".into(),
                message: e,
            })?;
        self.append_curated_conflict_warnings(&mut plan, &load.manifest);
        self.append_curator_warnings(&mut plan);
        Ok(plan)
    }

    /// Execute a fully-resolved plan with freshness checks.
    ///
    /// Re-validates instance state and registry revision before delegating to
    /// `InstallPipeline::execute_plan`.
    pub async fn execute(
        &self,
        plan: &ResolvedInstallPlan,
        reporter: &dyn ProgressReporter,
        cancel: &CancellationToken,
    ) -> InstallOutcome {
        let load = match self.load_instance(&plan.intent.target_instance) {
            Ok(load) => load,
            Err(error) => {
                return InstallOutcome::Failed {
                    error: format!("Instance not accessible before execution: {error}"),
                    rollback_performed: false,
                    snapshot_id: None,
                    hash_confirmation: None,
                };
            }
        };
        let registry_db = self
            .ctx
            .paths
            .registry_db()
            .is_file()
            .then(|| self.ctx.paths.registry_db());
        let loader_service = crate::loader_service::LoaderService::new(self.ctx.clone());
        let download_cache =
            crate::download_cache::DownloadCache::new(self.ctx.paths.download_cache_dir());
        let guard = match self
            .mutation_guard_async(&plan.intent.target_instance, "install")
            .await
        {
            Ok(guard) => guard,
            Err(error) => {
                return InstallOutcome::Failed {
                    error: error.to_string(),
                    rollback_performed: false,
                    snapshot_id: None,
                    hash_confirmation: None,
                };
            }
        };
        InstallPipeline
            .execute_plan_with_scheduler(
                &guard,
                plan,
                &load.instance_dir,
                &load.registry_revision,
                reporter,
                cancel,
                crate::install_pipeline::InstallExecutionResources {
                    scheduler: Some(&self.ctx.task_scheduler),
                    registry_db_path: registry_db.as_deref(),
                    loader_service: Some(&loader_service),
                    download_cache: Some(&download_cache),
                    content_store: Some(&self.ctx),
                },
            )
            .await
    }

    /// Resolve a caller-prepared reconciliation plan through the pipeline.
    ///
    /// Used by trusted adapters (e.g., desktop lockfile repair/import) that
    /// have already computed the backend-resolved operations. The service
    /// handles instance loading, registry revision, and pipeline normalization.
    pub async fn resolve_prepared(
        &self,
        intent: InstallIntent,
        mut prepared: PreparedPlan,
        reporter: &dyn ProgressReporter,
    ) -> LauncherResult<ResolvedInstallPlan> {
        let load = self.load_instance(&intent.target_instance)?;
        prepared.registry_revision = load.registry_revision;
        let mut plan = InstallPipeline
            .resolve_plan(intent, &load.instance_dir, prepared, reporter)
            .await
            .map_err(|e| LauncherError::Generic {
                code: "ERR_RESOLVE".into(),
                message: e,
            })?;
        self.append_curated_conflict_warnings(&mut plan, &load.manifest);
        self.append_curator_warnings(&mut plan);
        Ok(plan)
    }

    /// Warn when this plan would create a curator-flagged bad pairing.
    ///
    /// Best-effort by design: a missing or unreadable `registry.db` yields no
    /// warnings rather than an error. The registry is optional, and failing an
    /// install because the advisory data could not be read would be worse than
    /// the advice being absent.
    fn append_curated_conflict_warnings(
        &self,
        plan: &mut ResolvedInstallPlan,
        manifest: &InstanceManifest,
    ) {
        let conflicts = match ac::registry::RegistryService::new(self.ctx.clone()).known_conflicts()
        {
            Ok(conflicts) if !conflicts.is_empty() => conflicts,
            _ => return,
        };
        plan.warnings
            .extend(curated_conflict_warnings(plan, manifest, &conflicts));
    }

    /// Show the curator's install warnings for the items this plan writes.
    ///
    /// No registry means there is nothing to warn about. Any other read failure
    /// becomes a plan warning, so the review never says "nothing to check" when
    /// the check did not run. It never blocks the install.
    fn append_curator_warnings(&self, plan: &mut ResolvedInstallPlan) {
        match ac::registry::RegistryService::new(self.ctx.clone()).curator_warnings() {
            Ok(warnings) => {
                plan.warnings
                    .extend(curator_warning_plan_warnings(plan, &warnings));
            }
            Err(LauncherError::RegistryMissing) => {}
            Err(error) => plan.warnings.push(crate::install_pipeline::PlanWarning {
                code: CURATOR_WARNINGS_UNAVAILABLE_CODE.into(),
                message: format!(
                    "Agora could not check this install against curator warnings: {error}"
                ),
            }),
        }
    }

    /// Check that the instance is not locked.
    /// Pin or unpin an installed entry against updates.
    ///
    /// A pinned entry is skipped by "Update All" and shows no update badge, but
    /// stays installed and can still be updated deliberately. Matching is by
    /// filename across every content array, the same identity the enable/disable
    /// path uses.
    ///
    /// Returns `Ok(false)` when no entry matches, so a caller can distinguish
    /// "nothing to do" from a write failure.
    pub fn set_update_pinned(
        &self,
        instance_id: &str,
        filename: &str,
        pinned: bool,
    ) -> LauncherResult<bool> {
        let sanitized = ac::paths::sanitize_id(instance_id);
        let _guard = self.mutation_guard(&sanitized, "set_update_pinned")?;
        let manifest_path = self.ctx.paths.instance_manifest(&sanitized)?;
        let mut manifest = ac::helpers::read_manifest(&manifest_path)?;

        let mut changed = false;
        for entry in manifest
            .mods
            .iter_mut()
            .chain(manifest.resourcepacks.iter_mut())
            .chain(manifest.shaders.iter_mut())
            .chain(manifest.datapacks.iter_mut())
            .chain(manifest.worlds.iter_mut())
        {
            if entry.filename == filename && entry.update_pinned != pinned {
                entry.update_pinned = pinned;
                changed = true;
            }
        }
        if changed {
            ac::helpers::atomic_write_manifest(&manifest_path, &manifest)?;
            // The cached result was computed before this entry was pinned, so
            // it still counts toward the instance card's update badge. Drop it
            // and let the next check rebuild without the pinned entry. Failing
            // to clear only leaves a stale badge, so it must not fail the pin.
            let _ = crate::update_cache::delete_cached_instance_updates_ctx(&self.ctx, &sanitized);
        }
        Ok(changed)
    }

    pub fn check_not_locked(&self, instance_id: &str) -> LauncherResult<()> {
        let manifest_path = self.ctx.paths.instance_manifest(instance_id)?;
        if !manifest_path.exists() {
            return Ok(());
        }
        let manifest = ac::helpers::read_manifest(&manifest_path)?;
        if manifest.is_locked {
            return Err(LauncherError::InstanceLocked);
        }
        Ok(())
    }

    /// Download, verify, and install a single artifact into an instance.
    ///
    /// This is a convenience method for direct installs that bypass the full
    /// pipeline (snapshot, staging, health scan). For production use, prefer
    /// `resolve_and_execute` through the pipeline.
    ///
    /// Every hash in the request was parsed when it was built, so a blank or
    /// malformed one never reaches the download.
    pub async fn install_artifact(&self, request: &InstallRequest) -> LauncherResult<InstalledMod> {
        let instance_id = request.instance_id.as_str();
        self.check_not_locked(instance_id)?;

        let dir = self.ctx.paths.instance_dir(instance_id)?;
        ac::helpers::check_disk_space(&dir)?;

        let bytes =
            ac::download::download_mod_bytes(&self.ctx.http_clients, &request.target.download_url)
                .await?;
        let guard = self.mutation_guard(instance_id, "install_artifact")?;

        self.commit_verified_artifact(&guard, request, bytes)
    }

    /// Verify downloaded bytes and, only if every check passes, write them into
    /// the instance and record them in its manifest.
    pub(crate) fn commit_verified_artifact(
        &self,
        guard: &agora_core::instance_runtime::InstanceGuard,
        request: &InstallRequest,
        bytes: Vec<u8>,
    ) -> LauncherResult<InstalledMod> {
        let instance_id = request.instance_id.as_str();
        let filename = request.target.filename.as_str();
        let download_url = request.target.download_url.as_str();
        let version = request.target.version.as_deref();
        let registry_id = request.source.registry_id();
        let modrinth_id = request.source.modrinth_id();
        let hashes = &request.hashes;
        if guard.instance_id() != instance_id {
            return Err(LauncherError::Generic {
                code: "ERR_INSTANCE_GUARD".into(),
                message:
                    "The instance guard authorises a different instance; nothing was installed."
                        .into(),
            });
        }
        // The name becomes a path under the instance: it must be one portable
        // file name, whatever the source or the pack claims.
        if !agora_core::app_paths::is_safe_file_name(filename) {
            return Err(LauncherError::Generic {
                code: "ERR_INVALID_FILENAME".into(),
                message: format!("'{filename}' is not a safe file name; nothing was installed."),
            });
        }
        let dir = self.ctx.paths.instance_dir(instance_id)?;
        let manifest_path = self.ctx.paths.instance_manifest(instance_id)?;
        let mut manifest = agora_core::helpers::read_manifest(&manifest_path)?;

        // Curator pin, the hash a modpack lists and the hash remembered from an
        // earlier install all ask the user before a mismatch is installed. See
        // agora_core::artifact_hash.
        let mut expectations = Vec::new();
        if let Some(pack) = &hashes.pack_sha256 {
            expectations.push(ConfirmableHash {
                origin: HashOrigin::PackPin,
                sha256: pack.to_string(),
            });
        }
        if let Some(pin) = &hashes.curator_sha256 {
            expectations.push(ConfirmableHash {
                origin: HashOrigin::CuratorPin,
                sha256: pin.to_string(),
            });
        }
        if let Some(remembered) = registry_id.and_then(|id| {
            manifest
                .mods
                .iter()
                .chain(manifest.resourcepacks.iter())
                .chain(manifest.shaders.iter())
                .chain(manifest.datapacks.iter())
                .chain(manifest.worlds.iter())
                .find(|item| {
                    !item.hash_verified
                        && item.registry_id.as_deref() == Some(id)
                        && item.version.as_deref() == version
                        && item.filename == filename
                })
        }) {
            expectations.push(ConfirmableHash {
                origin: HashOrigin::PreviousInstall,
                sha256: remembered.sha256.clone(),
            });
        }
        let check = verify_download(
            &bytes,
            hashes.published_sha1.as_ref().map(Sha1::as_str),
            hashes.published_sha256.as_ref().map(Sha256::as_str),
            hashes.published_sha512.as_ref().map(Sha512::as_str),
            &expectations,
            request.accept_hash_confirmation,
            filename,
            version,
        )?;
        let installed_sha256 = check.sha256;
        let hash_verified = check.verified;
        let content_type = if request.target.content_type.is_empty() {
            "mod"
        } else {
            request.target.content_type.as_str()
        };
        let target_dir = dir.join(agora_core::helpers::content_subdir(content_type));
        let item_path = target_dir.join(filename);
        let expect = expect_for_install(&manifest, &item_path, filename)?;
        (|| -> LauncherResult<InstalledMod> {
            // The verified bytes go into the shared store first, then the store places them into
            // the instance as a link or a copy. Storing from a temporary file keeps the pipeline's
            // verified bytes as the object's bytes.
            let incoming = dir.join(".agora").join("store-incoming");
            std::fs::create_dir_all(&incoming).map_err(|_| LauncherError::InstanceCreateFailed)?;
            let temp = incoming.join(format!("{}.bin", uuid::Uuid::new_v4().simple()));
            std::fs::write(&temp, &bytes).map_err(|_| LauncherError::InstanceCreateFailed)?;
            let (provider, provider_id) = request.source.provenance();
            let stored = crate::content_bridge::store_file(
                &self.ctx,
                &temp,
                &crate::content_bridge::StoreDetails {
                    content_type,
                    filename,
                    version,
                    page_url: Some(download_url),
                    description: None,
                    catalog_id: registry_id.or(modrinth_id),
                    provider: Some(provider),
                    provider_id: Some(provider_id),
                    provider_origin: None,
                },
            );
            let _ = std::fs::remove_file(&temp);
            let stored = stored?;
            if !stored.sha256.eq_ignore_ascii_case(&installed_sha256) {
                return Err(LauncherError::Generic {
                code: "ERR_CONTENT_STORE".into(),
                message:
                    "The stored object does not match the verified download; nothing was installed."
                        .into(),
            });
            }
            // The stored object holds the bytes the placement will: read its metadata there.
            let metadata = crate::jar_metadata::parse_jar_metadata_for_loader_at_mc(
                &self.ctx.paths.content_object_path(&stored.sha256),
                &manifest.loader,
                &manifest.minecraft_version,
            );

            let installed_mod = InstalledMod {
                provider: None,
                update_pinned: false,
                pack_managed: false,
                installed_as_dependency: false,
                filename: filename.to_string(),
                registry_id: registry_id.map(|s| s.to_string()),
                modrinth_id: modrinth_id.map(|s| s.to_string()),
                source: request.source.stored_source().to_string(),
                source_url: Some(download_url.to_string()),
                version: version.map(|s| s.to_string()),
                sha256: installed_sha256,
                hash_verified,
                installed_at: chrono::Utc::now().to_rfc3339(),
                java_packages: metadata.java_packages,
                mod_jar_id: metadata.mod_jar_id,
                depends_on: metadata.depends_on,
                optional_deps: metadata.optional_deps,
                incompatible_deps: metadata.incompatible_deps,
                provided_mod_ids: metadata
                    .provided_mods
                    .into_iter()
                    .map(|provided| provided.mod_id)
                    .collect(),
                enabled: true,
                content_type: content_type.to_string(),
                content_item_id: Some(stored.item_id),
                object_sha256: Some(stored.sha256),
            };

            agora_core::helpers::remove_from_content_array(&mut manifest, filename);
            agora_core::helpers::push_to_content_array(&mut manifest, &installed_mod);
            let path = crate::content_bridge::content_path(content_type, filename)?;
            let sha256 = installed_mod.sha256.clone();
            crate::content_bridge::staged_change(guard, &dir, &[&target_dir], |stage| {
                crate::content_bridge::stage_object(&self.ctx, stage, &path, &sha256, expect)?;
                stage.write_manifest(&manifest)
            })?;
            Ok(installed_mod)
        })()
    }

    /// Remove an artifact from an instance by filename.
    ///
    /// Deletes the file from whichever content subdirectory it resides in and
    /// updates the manifest atomically.
    pub fn remove_artifact(&self, instance_id: &str, filename: &str) -> LauncherResult<bool> {
        self.check_not_locked(instance_id)?;
        let guard = self.mutation_guard(instance_id, "remove_artifact")?;

        if filename.contains("..") || filename.contains('/') || filename.contains('\\') {
            return Err(LauncherError::Generic {
                code: "ERR_INVALID_FILENAME".to_string(),
                message: "Filename contains invalid characters.".to_string(),
            });
        }

        let dir = self.ctx.paths.instance_dir(instance_id)?;
        let manifest_path = self.ctx.paths.instance_manifest(instance_id)?;
        if !manifest_path.exists() {
            return Ok(false);
        }
        let mut manifest = agora_core::helpers::read_manifest(&manifest_path)?;
        let Some(entry) = all_installed(&manifest)
            .find(|item| item.filename == filename || effective_filename(item) == filename)
            .cloned()
        else {
            // Nothing recorded means Agora did not place this file. Leave it alone.
            return Ok(false);
        };
        let live = dir
            .join(agora_core::helpers::content_subdir(&entry.content_type))
            .join(effective_filename(&entry));
        if !live.is_file() {
            // The recorded file is gone. Never search another folder for the same name.
            return Ok(false);
        }
        let recorded = crate::content_bridge::recorded_sha256(&entry).map(str::to_string);
        let matches = match recorded.as_deref() {
            Some(recorded) => crate::content_bridge::file_matches_recorded(&live, recorded)?,
            None => false,
        };
        let (true, Some(recorded)) = (matches, recorded) else {
            return Err(LauncherError::Generic {
                code: "ERR_FILE_CHANGED".into(),
                message: format!(
                    "'{filename}' was changed outside Agora, so it was left in place. Move it away, then remove it again."
                ),
            });
        };
        agora_core::helpers::remove_from_content_array(&mut manifest, &entry.filename);
        let path =
            crate::content_bridge::content_path(&entry.content_type, &effective_filename(&entry))?;
        // The folder the install placed it in gets the same right the install gave it, so a placed
        // link can be moved out of it (and back, if the removal is rolled back).
        let folder = dir.join(ac::helpers::content_subdir(&entry.content_type));
        crate::content_bridge::staged_change(&guard, &dir, &[&folder], |stage| {
            stage.remove(&path, ac::staged_apply::Expect::Agora(recorded))?;
            stage.write_manifest(&manifest)
        })
        .map_err(|error| LauncherError::Generic {
            code: "ERR_REMOVE_FAILED".into(),
            message: format!("Could not remove '{filename}': {error}"),
        })?;
        Ok(true)
    }

    /// Add a manually-dropped .jar file into an instance's `mods/` folder.
    ///
    /// Security: the source path must resolve to one of the user's allowlisted
    /// drop directories (Downloads, Desktop, Documents, or system temp).
    pub fn add_manual_artifact(
        &self,
        instance_id: &str,
        source_path: &str,
    ) -> LauncherResult<InstalledMod> {
        self.check_not_locked(instance_id)?;
        let guard = self.mutation_guard(instance_id, "add_manual_artifact")?;

        let src = std::path::Path::new(source_path);
        let ext = src
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        if ext.as_deref() != Some("jar") {
            return Err(LauncherError::Generic {
                code: "ERR_INVALID_FILENAME".to_string(),
                message: "Only .jar files can be added manually.".to_string(),
            });
        }
        let file_name =
            src.file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| LauncherError::Generic {
                    code: "ERR_INVALID_FILENAME".to_string(),
                    message: "Could not determine a valid file name.".to_string(),
                })?;
        if file_name.contains("..") || file_name.contains('/') || file_name.contains('\\') {
            return Err(LauncherError::Generic {
                code: "ERR_INVALID_FILENAME".to_string(),
                message: "Filename contains invalid characters.".to_string(),
            });
        }

        let dir = self.ctx.paths.instance_dir(instance_id)?;
        let mods_dir = dir.join("mods");
        let dest = mods_dir.join(file_name);
        let manifest_path = self.ctx.paths.instance_manifest(instance_id)?;

        let resolved = std::fs::canonicalize(source_path).map_err(|_| LauncherError::Generic {
            code: "ERR_INVALID_SOURCE".to_string(),
            message: "Source file does not exist or cannot be resolved.".to_string(),
        })?;

        let mut roots: Vec<std::path::PathBuf> = Vec::new();
        for r in [
            dirs::download_dir(),
            dirs::desktop_dir(),
            dirs::document_dir(),
            Some(std::env::temp_dir()),
        ]
        .into_iter()
        .flatten()
        {
            if let Ok(c) = std::fs::canonicalize(&r) {
                roots.push(c);
            }
        }
        let allowed = roots.iter().any(|root| resolved.starts_with(root));
        if !allowed {
            return Err(LauncherError::Generic {
                code: "ERR_SOURCE_NOT_ALLOWED".to_string(),
                message: "Source file is outside the allowed drop directories \
                          (Downloads, Desktop, Documents, or system temp)."
                    .to_string(),
            });
        }

        if !manifest_path.exists() {
            return Err(LauncherError::Generic {
                code: "ERR_MANIFEST_MISSING".to_string(),
                message: "Instance manifest not found. Create the instance first.".to_string(),
            });
        }
        let mut manifest = agora_core::helpers::read_manifest(&manifest_path)?;

        let expect = expect_for_install(&manifest, &dest, file_name)?;
        (|| -> LauncherResult<InstalledMod> {
            let stored = crate::content_bridge::store_file(
                &self.ctx,
                &resolved,
                &crate::content_bridge::StoreDetails {
                    content_type: "mod",
                    filename: file_name,
                    version: None,
                    page_url: None,
                    description: None,
                    catalog_id: None,
                    provider: Some("manual"),
                    provider_id: None,
                    provider_origin: None,
                },
            )?;
            // The stored object holds the bytes the placement will: read its metadata there.
            let metadata = crate::jar_metadata::parse_jar_metadata_for_loader_at_mc(
                &self.ctx.paths.content_object_path(&stored.sha256),
                &manifest.loader,
                &manifest.minecraft_version,
            );
            let installed_mod = InstalledMod {
                hash_verified: false,
                provider: None,
                update_pinned: false,
                pack_managed: false,
                installed_as_dependency: false,
                filename: file_name.to_string(),
                registry_id: None,
                modrinth_id: None,
                source: "manual_drag_drop".to_string(),
                source_url: None,
                version: None,
                sha256: stored.sha256.clone(),
                installed_at: chrono::Utc::now().to_rfc3339(),
                java_packages: metadata.java_packages,
                mod_jar_id: metadata.mod_jar_id,
                depends_on: metadata.depends_on,
                optional_deps: metadata.optional_deps,
                incompatible_deps: metadata.incompatible_deps,
                provided_mod_ids: metadata
                    .provided_mods
                    .into_iter()
                    .map(|provided| provided.mod_id)
                    .collect(),
                enabled: true,
                content_type: "mod".to_string(),
                content_item_id: Some(stored.item_id),
                object_sha256: Some(stored.sha256),
            };
            agora_core::helpers::remove_from_content_array(&mut manifest, file_name);
            agora_core::helpers::push_to_content_array(&mut manifest, &installed_mod);
            let path = crate::content_bridge::content_path("mod", file_name)?;
            let sha256 = installed_mod.sha256.clone();
            crate::content_bridge::staged_change(&guard, &dir, &[&mods_dir], |stage| {
                crate::content_bridge::stage_object(&self.ctx, stage, &path, &sha256, expect)?;
                stage.write_manifest(&manifest)
            })?;
            Ok(installed_mod)
        })()
    }

    /// Add an item already in the shared store to a Minecraft instance, with no network.
    ///
    /// The stored object is placed by the store's link-or-copy rule and the manifest entry is
    /// rebuilt from what the item recorded. Adding the same item twice returns the existing entry.
    pub fn install_stored_item(
        &self,
        instance_id: &str,
        item_id: &str,
    ) -> LauncherResult<InstalledMod> {
        self.check_not_locked(instance_id)?;
        let guard = self.mutation_guard(instance_id, "install_stored_item")?;
        crate::content_bridge::add_item_to_instance(&self.ctx, &guard, instance_id, item_id)
    }

    /// Convenience: resolve and immediately execute (one-shot).
    ///
    /// Useful for the CLI and non-interactive callers.
    pub async fn resolve_and_execute(
        &self,
        intent: InstallIntent,
        reporter: &dyn ProgressReporter,
        cancel: &CancellationToken,
    ) -> LauncherResult<InstallOutcome> {
        let plan = self.resolve(intent, reporter).await?;
        Ok(self.execute(&plan, reporter, cancel).await)
    }

    fn compute_registry_revision(&self) -> LauncherResult<String> {
        Ok(self
            .ctx
            .registry_digest()?
            .unwrap_or_else(|| "registry-unavailable".into()))
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Whose file is at `target`, before a direct install places one there: nothing (an add), or a
/// file Agora recorded that still matches its recorded hash (Agora's to replace). Anything else is
/// the user's: the install refuses and names it, and the file is never touched.
fn expect_for_install(
    manifest: &InstanceManifest,
    target: &std::path::Path,
    filename: &str,
) -> LauncherResult<ac::staged_apply::Expect> {
    match target.symlink_metadata() {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ac::staged_apply::Expect::Absent)
        }
        Err(error) => {
            return Err(LauncherError::Generic {
                code: "ERR_INSTALL_FAILED".into(),
                message: format!("Could not check '{filename}' before installing: {error}"),
            })
        }
    }
    let entry = all_installed(manifest)
        .find(|item| item.filename == filename || effective_filename(item) == filename);
    let Some(entry) = entry else {
        return Err(LauncherError::Generic {
            code: "ERR_FILE_EXISTS".into(),
            message: format!(
                "'{filename}' already exists in the instance, and Agora did not install it. Move it away, then install again."
            ),
        });
    };
    let recorded = crate::content_bridge::recorded_sha256(entry);
    let matches = match recorded {
        Some(recorded) => crate::content_bridge::file_matches_recorded(target, recorded)?,
        None => false,
    };
    match (matches, recorded) {
        (true, Some(recorded)) => Ok(ac::staged_apply::Expect::Agora(recorded.to_string())),
        _ => Err(LauncherError::Generic {
            code: "ERR_FILE_CHANGED".into(),
            message: format!(
                "'{filename}' was changed outside Agora, so it was left alone. Move it away, then install again."
            ),
        }),
    }
}

fn all_installed(manifest: &InstanceManifest) -> impl Iterator<Item = &InstalledMod> {
    manifest
        .mods
        .iter()
        .chain(manifest.resourcepacks.iter())
        .chain(manifest.shaders.iter())
        .chain(manifest.datapacks.iter())
        .chain(manifest.worlds.iter())
}

fn effective_filename(item: &InstalledMod) -> String {
    if item.enabled || item.filename.ends_with(".disabled") {
        item.filename.clone()
    } else {
        format!("{}.disabled", item.filename)
    }
}

/// Plan-warning code for a curator install warning on an item the plan writes.
/// The desktop review keys off this code to show these warnings up front.
pub const CURATOR_WARNING_CODE: &str = "WARN_CURATOR_WARNING";

/// Plan-warning code for a registry that could not be read for curator warnings.
pub const CURATOR_WARNINGS_UNAVAILABLE_CODE: &str = "WARN_CURATOR_WARNINGS_UNAVAILABLE";

/// Curator install warnings for the items `plan` writes, one per item.
///
/// Only planned adds are checked, so the warning sits in the review that comes
/// before anything is written. A warned item pulled in as a dependency counts
/// too. Ids match exactly, the same key the annotation lookup uses.
pub(crate) fn curator_warning_plan_warnings(
    plan: &ResolvedInstallPlan,
    warnings: &[agora_core::registry::CuratorWarning],
) -> Vec<crate::install_pipeline::PlanWarning> {
    use crate::install_pipeline::{PlanWarning, ResolvedArtifact};
    use std::collections::HashSet;

    let mut reported: HashSet<&str> = HashSet::new();
    let mut out = Vec::new();
    for add in &plan.files_to_add {
        let metadata = match &add.artifact {
            ResolvedArtifact::Download(download) => &download.metadata,
            ResolvedArtifact::LocalFile(local) => &local.metadata,
        };
        let Some(item_id) = metadata.registry_id.as_deref() else {
            continue;
        };
        if !reported.insert(item_id) {
            continue;
        }
        let Some(warning) = warnings.iter().find(|w| w.item_id == item_id) else {
            continue;
        };
        out.push(PlanWarning {
            code: CURATOR_WARNING_CODE.into(),
            message: format!("Curator warning for {}: {}", warning.name, warning.warning),
        });
    }
    out
}

/// Curated-conflict warnings for the content set `plan` would leave behind.
///
/// Only pairs that this plan *creates* are reported: at least one side has to
/// be something the plan is adding. A conflict already sitting in the instance
/// is pre-launch health's job — repeating it on an unrelated install would be
/// noise attached to the wrong action.
///
/// Version-windowed facts are evaluated against the versions that will actually
/// be present, so installing 0.4 of a mod flagged from 0.5 stays quiet.
fn curated_conflict_warnings(
    plan: &ResolvedInstallPlan,
    manifest: &InstanceManifest,
    conflicts: &[agora_core::registry::KnownConflict],
) -> Vec<crate::install_pipeline::PlanWarning> {
    use crate::install_pipeline::{PlanWarning, ResolvedArtifact};
    use std::collections::HashMap;

    let removed: std::collections::HashSet<&str> = plan
        .files_to_remove
        .iter()
        .map(|f| f.filename.as_str())
        .collect();

    // id -> (version, is_incoming). An id that is both installed and being
    // updated counts as incoming: the plan is what put the new version there.
    let mut present: HashMap<String, (Option<String>, bool)> = HashMap::new();
    for item in all_installed(manifest) {
        if removed.contains(item.filename.as_str()) {
            continue;
        }
        for id in [item.registry_id.as_deref(), item.modrinth_id.as_deref()]
            .into_iter()
            .flatten()
        {
            present.insert(id.to_ascii_lowercase(), (item.version.clone(), false));
        }
    }
    for add in &plan.files_to_add {
        let metadata = match &add.artifact {
            ResolvedArtifact::Download(download) => &download.metadata,
            ResolvedArtifact::LocalFile(local) => &local.metadata,
        };
        for id in [
            metadata.registry_id.as_deref(),
            metadata.modrinth_id.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            present.insert(id.to_ascii_lowercase(), (metadata.version.clone(), true));
        }
    }

    let mut warnings = Vec::new();
    for conflict in conflicts {
        let Some((a_version, a_incoming)) = present.get(&conflict.mod_a_id.to_ascii_lowercase())
        else {
            continue;
        };
        let Some((b_version, b_incoming)) = present.get(&conflict.mod_b_id.to_ascii_lowercase())
        else {
            continue;
        };
        if !a_incoming && !b_incoming {
            continue;
        }
        if !conflict.applies_to_versions(a_version.as_deref(), b_version.as_deref()) {
            continue;
        }
        let mitigation = if conflict.mitigated_by.is_empty() {
            String::new()
        } else {
            format!(" Known mitigation: {}.", conflict.mitigated_by.join(", "))
        };
        warnings.push(PlanWarning {
            code: "WARN_KNOWN_CONFLICT".into(),
            message: format!(
                "'{}' and '{}' are a known conflict (severity: {}).{}{}",
                conflict.mod_a_id,
                conflict.mod_b_id,
                conflict.severity,
                conflict
                    .notes
                    .as_deref()
                    .map(|n| format!(" {n}"))
                    .unwrap_or_default(),
                mitigation
            ),
        });
    }
    warnings
}

#[cfg(test)]
mod curated_conflict_tests {
    use super::*;
    use crate::install_pipeline::{
        ArtifactMetadata, ArtifactSource, DiskSpaceEstimate, FileAdd, FileRemove, HashSpec,
        InstallAction, OptionalDepsPolicy, PlanOverrides, RequestSource, ResolvedArtifact,
        ResolvedDownload, ResolvedOperation, SnapshotPlan, SourceType,
    };
    use agora_core::dependency_ops::VersionGrammar;
    use agora_core::registry::KnownConflict;

    fn conflict(a: &str, b: &str, a_versions: &[&str]) -> KnownConflict {
        KnownConflict {
            mod_a_id: a.into(),
            mod_b_id: b.into(),
            severity: "hard".into(),
            mitigated_by: vec![],
            notes: None,
            mod_a_versions: a_versions.iter().map(|s| s.to_string()).collect(),
            mod_b_versions: vec![],
            version_grammar: VersionGrammar::Fabric,
        }
    }

    fn installed(registry_id: &str, filename: &str, version: Option<&str>) -> InstalledMod {
        InstalledMod {
            provider: None,
            filename: filename.into(),
            registry_id: Some(registry_id.into()),
            modrinth_id: None,
            source: "registry".into(),
            source_url: None,
            version: version.map(str::to_string),
            sha256: String::new(),
            hash_verified: true,
            installed_at: String::new(),
            java_packages: vec![],
            mod_jar_id: Some(registry_id.into()),
            provided_mod_ids: vec![],
            enabled: true,
            content_type: "mod".into(),
            update_pinned: false,
            pack_managed: false,
            installed_as_dependency: false,
            depends_on: vec![],
            optional_deps: vec![],
            incompatible_deps: vec![],
            content_item_id: None,
            object_sha256: None,
        }
    }

    fn manifest_with(mods: Vec<InstalledMod>) -> InstanceManifest {
        InstanceManifest {
            manifest_version: agora_core::models::CURRENT_MANIFEST_VERSION,
            game_data: Default::default(),
            pack_origin: None,
            instance_id: "inst".into(),
            name: "Inst".into(),
            created_from_pack: None,
            minecraft_version: "1.21".into(),
            loader: "fabric".into(),
            loader_version: "0.16.0".into(),
            is_locked: false,
            mods,
            resourcepacks: vec![],
            shaders: vec![],
            datapacks: vec![],
            worlds: vec![],
            user_preferences: serde_json::json!({}),
        }
    }

    fn adding(registry_id: &str, version: Option<&str>) -> FileAdd {
        FileAdd {
            target_filename: format!("{registry_id}.jar"),
            staging_filename: format!("{registry_id}.jar.part"),
            artifact: ResolvedArtifact::Download(ResolvedDownload {
                item_id: registry_id.into(),
                version_id: "v".into(),
                source: ArtifactSource::Download {
                    url: "https://example.com/x.jar".into(),
                },
                hashes: HashSpec {
                    values: vec![],
                    ..Default::default()
                },
                size: 0,
                filename: format!("{registry_id}.jar"),
                metadata: ArtifactMetadata {
                    provider: None,
                    source_type: SourceType::Curated,
                    registry_id: Some(registry_id.into()),
                    modrinth_id: None,
                    content_type: "mod".into(),
                    version: version.map(str::to_string),
                    download_strategy: None,
                    pinned_host: None,
                },
            }),
            hashes: HashSpec {
                values: vec![],
                ..Default::default()
            },
            size: 0,
            installed_as_dependency: false,
            replaces_missing_file: false,
        }
    }

    fn plan_adding(adds: Vec<FileAdd>, removes: Vec<FileRemove>) -> ResolvedInstallPlan {
        ResolvedInstallPlan {
            fingerprint: String::new(),
            intent: InstallIntent {
                action: InstallAction::Install {
                    source_type: SourceType::Curated,
                    item_id: "x".into(),
                    candidate_version: None,
                },
                target_instance: "inst".into(),
                optional_deps: OptionalDepsPolicy::ExcludeAll,
                requested_by: RequestSource::Interactive,
                overrides: PlanOverrides::default(),
            },
            operation: ResolvedOperation::Remove {
                target_filename: "noop.jar".into(),
                reverse_dependents: vec![],
                content_type: None,
            },
            dependencies: vec![],
            conflicts: vec![],
            files_to_add: adds,
            files_to_remove: removes,
            files_to_disable: vec![],
            files_to_promote: vec![],
            files_to_enable: vec![],
            snapshot: SnapshotPlan {
                label: String::new(),
                estimated_bytes: 0,
            },
            disk_estimate: DiskSpaceEstimate::zero(),
            warnings: vec![],
            blocking_errors: vec![],
            pending_choices: vec![],
            loader_change: None,
            created_at: String::new(),
            instance_state_hash: String::new(),
            registry_revision: String::new(),
        }
    }

    #[test]
    fn installing_the_second_half_of_a_known_pair_warns() {
        let manifest = manifest_with(vec![installed("sodium", "sodium.jar", Some("0.5.3"))]);
        let plan = plan_adding(vec![adding("optifine", Some("1.0"))], vec![]);
        let warnings =
            curated_conflict_warnings(&plan, &manifest, &[conflict("optifine", "sodium", &[])]);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].code, "WARN_KNOWN_CONFLICT");
    }

    #[test]
    fn a_conflict_already_present_is_not_re_reported_on_an_unrelated_install() {
        // Pre-launch health owns this one. Attaching it to an unrelated
        // install would blame the wrong action.
        let manifest = manifest_with(vec![
            installed("sodium", "sodium.jar", Some("0.5.3")),
            installed("optifine", "optifine.jar", Some("1.0")),
        ]);
        let plan = plan_adding(vec![adding("lithium", Some("0.12"))], vec![]);
        assert!(curated_conflict_warnings(
            &plan,
            &manifest,
            &[conflict("optifine", "sodium", &[])]
        )
        .is_empty());
    }

    #[test]
    fn a_version_window_that_excludes_the_incoming_version_stays_quiet() {
        let manifest = manifest_with(vec![installed("sodium", "sodium.jar", Some("0.5.3"))]);
        let plan = plan_adding(vec![adding("optifine", Some("0.9"))], vec![]);
        // Flagged only from 1.0 onward; 0.9 is going in.
        let facts = [conflict("optifine", "sodium", &[">=1.0"])];
        assert!(curated_conflict_warnings(&plan, &manifest, &facts).is_empty());

        let newer = plan_adding(vec![adding("optifine", Some("1.2"))], vec![]);
        assert_eq!(
            curated_conflict_warnings(&newer, &manifest, &facts).len(),
            1
        );
    }

    #[test]
    fn removing_the_other_half_in_the_same_plan_clears_the_warning() {
        let manifest = manifest_with(vec![installed("sodium", "sodium.jar", Some("0.5.3"))]);
        let plan = plan_adding(
            vec![adding("optifine", Some("1.0"))],
            vec![FileRemove {
                filename: "sodium.jar".into(),
                content_type: Some("mod".into()),
            }],
        );
        assert!(curated_conflict_warnings(
            &plan,
            &manifest,
            &[conflict("optifine", "sodium", &[])]
        )
        .is_empty());
    }

    #[test]
    fn an_empty_plan_and_an_empty_fact_set_produce_nothing() {
        let manifest = manifest_with(vec![installed("sodium", "sodium.jar", Some("0.5.3"))]);
        let empty_plan = plan_adding(vec![], vec![]);
        assert!(curated_conflict_warnings(
            &empty_plan,
            &manifest,
            &[conflict("optifine", "sodium", &[])]
        )
        .is_empty());
        let plan = plan_adding(vec![adding("optifine", Some("1.0"))], vec![]);
        assert!(curated_conflict_warnings(&plan, &manifest_with(vec![]), &[]).is_empty());
    }

    #[test]
    fn ids_match_case_insensitively() {
        let manifest = manifest_with(vec![installed("Sodium", "sodium.jar", Some("0.5.3"))]);
        let plan = plan_adding(vec![adding("OptiFine", Some("1.0"))], vec![]);
        assert_eq!(
            curated_conflict_warnings(&plan, &manifest, &[conflict("optifine", "sodium", &[])])
                .len(),
            1
        );
    }

    // -------------------------------------------------------------------
    // Curator install warnings: one plan warning per planned item
    // -------------------------------------------------------------------

    fn curator_warning(
        item_id: &str,
        name: &str,
        text: &str,
    ) -> agora_core::registry::CuratorWarning {
        agora_core::registry::CuratorWarning {
            item_id: item_id.into(),
            name: name.into(),
            warning: text.into(),
        }
    }

    #[test]
    fn a_planned_warned_item_gets_a_plan_warning_with_its_text() {
        let plan = plan_adding(vec![adding("xaeros-minimap", Some("24.0"))], vec![]);
        let warnings = curator_warning_plan_warnings(
            &plan,
            &[curator_warning(
                "xaeros-minimap",
                "Xaero's Minimap",
                "Do not use XaeroPlus with this mod.",
            )],
        );
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].code, CURATOR_WARNING_CODE);
        assert_eq!(
            warnings[0].message,
            "Curator warning for Xaero's Minimap: Do not use XaeroPlus with this mod."
        );
    }

    #[test]
    fn an_unplanned_warned_item_stays_quiet() {
        let plan = plan_adding(vec![adding("sodium", Some("0.5.3"))], vec![]);
        assert!(curator_warning_plan_warnings(
            &plan,
            &[curator_warning(
                "xaeros-minimap",
                "Xaero's Minimap",
                "Do not use XaeroPlus."
            )]
        )
        .is_empty());
    }

    #[test]
    fn removing_a_warned_item_does_not_warn() {
        // Only what the plan writes is reviewed. A removal writes nothing new.
        let removal = crate::install_pipeline::FileRemove {
            filename: "xaeros-minimap.jar".into(),
            content_type: None,
        };
        let plan = plan_adding(vec![], vec![removal]);
        assert!(curator_warning_plan_warnings(
            &plan,
            &[curator_warning(
                "xaeros-minimap",
                "Xaero's Minimap",
                "Do not use XaeroPlus."
            )]
        )
        .is_empty());
    }

    #[test]
    fn a_warned_item_named_twice_is_reported_once() {
        let plan = plan_adding(
            vec![
                adding("xaeros-minimap", None),
                adding("xaeros-minimap", None),
            ],
            vec![],
        );
        let warnings = curator_warning_plan_warnings(
            &plan,
            &[curator_warning(
                "xaeros-minimap",
                "Xaero's Minimap",
                "Do not use XaeroPlus.",
            )],
        );
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn an_empty_warning_list_and_an_empty_plan_produce_nothing() {
        let plan = plan_adding(vec![adding("xaeros-minimap", None)], vec![]);
        assert!(curator_warning_plan_warnings(&plan, &[]).is_empty());
        let empty = plan_adding(vec![], vec![]);
        assert!(curator_warning_plan_warnings(
            &empty,
            &[curator_warning(
                "xaeros-minimap",
                "Xaero's Minimap",
                "Do not use XaeroPlus."
            )]
        )
        .is_empty());
    }

    #[test]
    fn a_near_miss_or_wrong_case_id_does_not_attach_the_warning() {
        // Identity is the registry id exactly, the same key the annotation
        // lookup uses. A similar id must not put a warning on another mod.
        let warning = [curator_warning(
            "xaeros-minimap",
            "Xaero's Minimap",
            "Do not use XaeroPlus.",
        )];
        let near = plan_adding(vec![adding("xaeros-minimap-extra", None)], vec![]);
        assert!(curator_warning_plan_warnings(&near, &warning).is_empty());
        let wrong_case = plan_adding(vec![adding("Xaeros-Minimap", None)], vec![]);
        assert!(curator_warning_plan_warnings(&wrong_case, &warning).is_empty());
    }
}
