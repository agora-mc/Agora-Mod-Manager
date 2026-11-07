//! Installing a curated catalog entry for another game into an instance (MASTER_SPEC §26.8).
//!
//! The steps, in order, and where each one can stop:
//!
//! 1. The entry must be for the instance's game. Its compatibility entry is the first one whose
//!    `stores` include the instance's store and whose `game_versions` include its version.
//! 2. Each framework the entry requires must be installed, and at least `min_version`. A framework
//!    whose version cannot be read is a warning, and the launch check still guards it. An unmet
//!    requirement refuses the install unless the caller opted in with
//!    [`InstallRequest::allow_missing_frameworks`]. A dry run refuses nothing: it names what is
//!    unmet in [`CatalogPlan::unmet_frameworks`], so a preview can show it. The launch check reports
//!    the same requirements afterwards ([`requirement_findings`]).
//! 3. The file is chosen. `github_release` takes the newest non-draft, non-prerelease release with
//!    an asset matching the compatibility entry's `asset` glob. `direct_hash` is the entry's URL.
//! 4. An entry this instance already has (any release) is reported and nothing changes.
//! 5. Under `--dry-run` the plan is returned here. If the content store already holds this release
//!    file under a hash known before the download, that copy is used ([`stored_copy`]). Otherwise
//!    the caller is told what is about to be downloaded, and the bytes are fetched through the
//!    existing host policy.
//! 6. The bytes are checked through [`crate::artifact_hash::verify_download`], which is the hash
//!    policy in code. They are imported with a `ContentSource::Catalog`, then placed through
//!    [`crate::game_deploy::decide_placement`], the same rule `games instance content add` uses.
//!
//! Network access goes through [`CatalogTransport`], so the steps above can be tested with a fake.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use agora_game_api::{
    BaseReference, FrameworkId, GameDefinition, GameId, LayerSource, RuntimeIdentity,
};
use async_trait::async_trait;
use serde::Serialize;

use crate::artifact_hash::{
    first_unmet, is_sha256_hex, verify_download, ConfirmableHash, HashOrigin,
};
use crate::content_store::{self, ContentItem, ContentSource};
use crate::ctx::Ctx;
use crate::error::{LauncherError, LauncherResult};
use crate::game_deploy::{self, PlacementDecision};
use crate::game_frameworks::{self, FrameworkStatus};
use crate::game_instance::{self, GameInstanceManifest};
use crate::github_release::{self, GitHubAuth, GitHubRelease};
use crate::http_client::HttpClients;
use crate::registry::{
    self, DownloadSource, FrameworkRequirement, GameCatalogItem, GameCompatibility, RegistryService,
};

/// The network side of an install: a page of a repository's releases, and the bytes of a file.
#[async_trait]
pub trait CatalogTransport: Send + Sync {
    /// One page of `owner/repo`'s releases, and the total number of pages.
    async fn releases_page(
        &self,
        repo: &str,
        page: u32,
    ) -> LauncherResult<(Vec<GitHubRelease>, u32)>;
    /// The bytes at `url`. `pinned_host` is the host of the entry's signed source, for a
    /// `direct_hash` file; `None` for a GitHub release file.
    async fn fetch(&self, url: &str, pinned_host: Option<&str>) -> LauncherResult<Vec<u8>>;
}

/// The real transport: GitHub's API and downloads through the checked HTTP clients.
pub struct HttpTransport<'a> {
    pub clients: &'a HttpClients,
    pub auth: GitHubAuth,
}

#[async_trait]
impl CatalogTransport for HttpTransport<'_> {
    async fn releases_page(
        &self,
        repo: &str,
        page: u32,
    ) -> LauncherResult<(Vec<GitHubRelease>, u32)> {
        github_release::list_releases_page(self.clients, repo, page, &self.auth).await
    }

    async fn fetch(&self, url: &str, pinned_host: Option<&str>) -> LauncherResult<Vec<u8>> {
        match pinned_host {
            Some(host) => crate::download::download_pinned_bytes(self.clients, url, host).await,
            None => crate::download::download_mod_bytes(self.clients, url).await,
        }
    }
}

/// What the caller asked for.
pub struct InstallRequest<'a> {
    pub instance_id: &'a str,
    pub item: &'a GameCatalogItem,
    /// Accept a hash that differs from a curator pin or an earlier install. Never accepts a
    /// published hash that differs, and never overrides a framework refusal: that is
    /// `allow_missing_frameworks`.
    pub install_anyway: bool,
    /// Install although a framework the entry requires is unmet, as the user chose. The plan names
    /// every such requirement. It changes nothing about how the file's hash is checked.
    pub allow_missing_frameworks: bool,
    /// Plan only: nothing is downloaded or stored. An unmet framework does not refuse a dry run; the
    /// plan names it.
    pub dry_run: bool,
}

/// The compatibility entry that was chosen, the frameworks checked, and the file to fetch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CatalogPlan {
    pub item_id: String,
    pub item_name: String,
    pub instance_id: String,
    /// Index of the chosen compatibility entry in the manifest's order.
    pub compatibility_index: usize,
    pub compatibility: GameCompatibility,
    /// The requirements that are met.
    pub frameworks: Vec<FrameworkCheck>,
    /// The requirements that are not. Empty unless this is a dry run, or the caller set
    /// [`InstallRequest::allow_missing_frameworks`]; otherwise they refuse the install.
    pub unmet_frameworks: Vec<UnmetFramework>,
    pub source: PlannedSource,
    pub warnings: Vec<String>,
}

/// One required framework, as the instance has it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FrameworkCheck {
    pub framework: String,
    pub name: String,
    pub min_version: Option<String>,
    /// The version found, or `None` when the framework is present but its version is unreadable.
    pub found_version: Option<String>,
}

/// Why a required framework is unmet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FrameworkProblem {
    /// The game's files have no marker for it.
    Absent,
    /// It is installed, at a version below the one required.
    TooOld { found: String },
    /// The game definition declares no way to detect it, so nobody can tell.
    Undetectable,
    /// The game does not declare this framework. `malformed` says why, when the entry's id is not
    /// even a valid framework id.
    Undeclared { malformed: Option<String> },
}

/// A catalog entry for the same game that looks like the framework itself, so a person can add it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CatalogEntryRef {
    pub id: String,
    pub name: String,
}

/// One required framework the instance does not satisfy, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnmetFramework {
    /// The id as the entry wrote it.
    pub framework: String,
    /// The game's name for it, or the id when the game does not declare it.
    pub name: String,
    pub min_version: Option<String>,
    pub problem: FrameworkProblem,
    /// Where the catalog can provide it, when it can.
    pub catalog_entry: Option<CatalogEntryRef>,
    /// The warning, in a player's words.
    pub message: String,
}

impl UnmetFramework {
    fn new(requirement: &FrameworkRequirement, name: String, problem: FrameworkProblem) -> Self {
        let mut unmet = Self {
            framework: requirement.framework.clone(),
            name,
            min_version: requirement.min_version.clone(),
            problem,
            catalog_entry: None,
            message: String::new(),
        };
        unmet.message = format!(
            "This mod needs {}{}. {}",
            unmet.needs(),
            unmet.reason(),
            unmet.advice()
        );
        unmet
    }

    /// The framework and the version it must be: "SKSE 2.2.6 or newer".
    pub fn needs(&self) -> String {
        match &self.min_version {
            Some(minimum) => format!("{} {minimum} or newer", self.name),
            None => self.name.clone(),
        }
    }

    /// Why it is unmet, written to follow [`Self::needs`].
    pub fn reason(&self) -> String {
        match &self.problem {
            FrameworkProblem::Absent => ", and this instance doesn't have it".to_string(),
            FrameworkProblem::TooOld { found } => format!(", and this instance has {found}"),
            FrameworkProblem::Undetectable => {
                ", and Agora can't tell whether this instance has it".to_string()
            }
            FrameworkProblem::Undeclared { .. } => {
                ", which this game doesn't list, so Agora can't check for it".to_string()
            }
        }
    }

    /// What it means for the mod.
    pub fn advice(&self) -> &'static str {
        match self.problem {
            FrameworkProblem::Absent | FrameworkProblem::Undeclared { .. } => {
                "It probably won't work until you add it."
            }
            FrameworkProblem::TooOld { .. } => "It probably won't work until you update it.",
            FrameworkProblem::Undetectable => "It probably won't work unless you have it.",
        }
    }

    /// The error that refuses the install when the user has not opted in.
    fn refusal(&self, item_id: &str, instance_id: &str, game: &str) -> (&'static str, String) {
        let name = &self.name;
        match &self.problem {
            FrameworkProblem::Absent => (
                FRAMEWORK_MISSING,
                format!(
                    "'{item_id}' needs {}, and instance '{instance_id}' does not have it. Install {name} first.",
                    self.needs()
                ),
            ),
            FrameworkProblem::TooOld { found } => (
                FRAMEWORK_TOO_OLD,
                format!(
                    "'{item_id}' needs {}, and instance '{instance_id}' has {name} {found}. Update {name} first.",
                    self.needs()
                ),
            ),
            FrameworkProblem::Undetectable => (
                FRAMEWORK_UNDETECTABLE,
                format!(
                    "'{item_id}' needs {name}, and Agora cannot tell whether the instance has it: the game definition declares no way to detect it."
                ),
            ),
            FrameworkProblem::Undeclared {
                malformed: Some(reason),
            } => (FRAMEWORK_BAD_REQUIREMENT, format!("{name}: {reason}")),
            FrameworkProblem::Undeclared { malformed: None } => (
                FRAMEWORK_UNKNOWN,
                format!("'{item_id}' requires framework '{name}', which {game} does not declare."),
            ),
        }
    }
}

const FRAMEWORK_MISSING: &str = "ERR_CATALOG_FRAMEWORK_MISSING";
const FRAMEWORK_TOO_OLD: &str = "ERR_CATALOG_FRAMEWORK_TOO_OLD";
const FRAMEWORK_UNDETECTABLE: &str = "ERR_CATALOG_FRAMEWORK_UNDETECTABLE";
const FRAMEWORK_UNKNOWN: &str = "ERR_CATALOG_UNKNOWN_FRAMEWORK";
const FRAMEWORK_BAD_REQUIREMENT: &str = "ERR_CATALOG_BAD_REQUIREMENT";

/// Whether `code` is a refusal that [`InstallRequest::allow_missing_frameworks`] overrides.
pub fn is_framework_refusal(code: &str) -> bool {
    matches!(
        code,
        FRAMEWORK_MISSING
            | FRAMEWORK_TOO_OLD
            | FRAMEWORK_UNDETECTABLE
            | FRAMEWORK_UNKNOWN
            | FRAMEWORK_BAD_REQUIREMENT
    )
}

/// The file the plan will fetch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "strategy", rename_all = "snake_case")]
pub enum PlannedSource {
    GithubRelease {
        repo: String,
        tag: String,
        asset: String,
        download_url: String,
        /// The SHA-256 GitHub published for the asset, when it published one.
        published_sha256: Option<String>,
        size: Option<u64>,
        /// The curator's pin for this exact release file, when there is one.
        pin: Option<ConfirmableHash>,
    },
    DirectHash {
        url: String,
        file: String,
        /// The entry's manifest hash, which names this one file.
        sha256: String,
        /// The host of the entry's signed source, which the download is pinned to.
        pinned_host: String,
        size: Option<u64>,
    },
}

impl PlannedSource {
    /// The file name the bytes are stored under.
    pub fn file_name(&self) -> &str {
        match self {
            PlannedSource::GithubRelease { asset, .. } => asset,
            PlannedSource::DirectHash { file, .. } => file,
        }
    }

    /// The release tag, for a GitHub file. `None` for a `direct_hash` file.
    pub fn release(&self) -> Option<&str> {
        match self {
            PlannedSource::GithubRelease { tag, .. } => Some(tag),
            PlannedSource::DirectHash { .. } => None,
        }
    }

    /// Where the file comes from, as a person reads it.
    pub fn describe(&self) -> String {
        match self {
            PlannedSource::GithubRelease { repo, tag, .. } => {
                format!("GitHub {repo} release {tag}")
            }
            PlannedSource::DirectHash { url, .. } => format!("direct download {url}"),
        }
    }
}

/// What the hash check found, in words a person can read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HashReport {
    /// The lowercase SHA-256 of the downloaded bytes, which the install records.
    pub sha256: String,
    /// Whether a hash the source published, or the curator's manifest, checked the bytes.
    pub verified: bool,
    /// How the bytes were checked, or why they were not.
    pub basis: String,
    /// When an expectation did not match and `--install-anyway` let the install go on: whether it
    /// was the curator's pin or an earlier install.
    pub confirmed_past: Option<HashOrigin>,
}

/// What the install did, after the plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum InstallState {
    /// `--dry-run`: the plan is all there is.
    DryRun,
    /// The instance already has an item from this entry. Nothing changed.
    AlreadyInstalled {
        installed_release: Option<String>,
        /// Whether the release it has is the release that would be installed.
        same_release: bool,
    },
    /// The file is in the content store and placed in the instance.
    Placed {
        content_item_id: String,
        mount_path: String,
        source_path: String,
        size: u64,
        hash: HashReport,
    },
    /// The archive has a FOMOD installer. It is in the content store but not in the instance.
    NeedsInstaller {
        content_item_id: String,
        size: u64,
        hash: HashReport,
        reason: String,
        top_level: Vec<String>,
        /// The command that runs the installer, as `content add` words it.
        installer_command: String,
    },
}

/// The whole result: the plan, what happened, and any warnings the install raised.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstallReport {
    pub plan: CatalogPlan,
    pub state: InstallState,
}

/// Plan, and unless `dry_run` or already installed, fetch, check, import and place the entry.
///
/// `announce` is called once, with the plan, right before the download starts, so a caller can
/// show the file name, the source and the size.
pub async fn install(
    ctx: &Ctx,
    transport: &dyn CatalogTransport,
    request: InstallRequest<'_>,
    announce: &mut dyn FnMut(&CatalogPlan),
) -> LauncherResult<InstallReport> {
    let manifest = game_instance::get_manifest(ctx, request.instance_id)
        .map_err(|e| generic("ERR_INSTANCE_NOT_FOUND", e))?;
    if request.item.game != manifest.game.as_str() {
        return Err(LauncherError::Generic {
            code: "ERR_CATALOG_WRONG_GAME".into(),
            message: format!(
                "'{}' is an entry for {}, and instance '{}' is a {} instance.",
                request.item.id,
                request.item.game,
                request.instance_id,
                manifest.game.as_str()
            ),
        });
    }
    let definition = ctx.games.game(&manifest.game).ok_or_else(|| {
        generic(
            "ERR_CATALOG_NO_GAME",
            format!("No game package defines {}.", manifest.game.as_str()),
        )
    })?;
    if definition.content_layout.is_none() {
        return Err(generic(
            "ERR_CATALOG_NO_CONTENT_LAYOUT",
            format!(
                "Game definition '{}' has no content layout, so Agora cannot place catalog content for it.",
                definition.id.as_str()
            ),
        ));
    }

    let mut plan = plan(
        ctx,
        &manifest,
        definition,
        request.item,
        request.instance_id,
        transport,
        request.dry_run || request.allow_missing_frameworks,
    )
    .await?;

    if let Some(installed) = installed_release(ctx, &manifest, &request.item.id)? {
        let same_release = installed.as_deref() == plan.source.release();
        return Ok(InstallReport {
            plan,
            state: InstallState::AlreadyInstalled {
                installed_release: installed,
                same_release,
            },
        });
    }
    if request.dry_run {
        return Ok(InstallReport {
            plan,
            state: InstallState::DryRun,
        });
    }

    // The store's own copy is used when it holds this release file under a hash known before the
    // download. Otherwise the file is fetched, checked and imported as before.
    let mut damaged = HashSet::new();
    let (content_item, size, hash) = match stored_copy(ctx, &mut plan, &mut damaged)? {
        Some(copy) => copy,
        None => {
            announce(&plan);
            let bytes = fetch_bytes(transport, &plan.source).await?;
            let hash = check_bytes(ctx, &plan, &bytes, request.install_anyway)?;

            let scratch = ScratchDir::new(ctx)?;
            let file_path = scratch.path.join(safe_file_name(plan.source.file_name()));
            std::fs::write(&file_path, &bytes).map_err(|e| generic("ERR_CATALOG_WRITE", e))?;

            // The objects a verify found damaged are replaced by this import, not trusted.
            let outcome = content_store::add_archive_replacing(
                ctx,
                &file_path,
                Some(&request.item.name),
                &damaged,
                |sha256, _display_path| ContentSource::Catalog {
                    item_id: request.item.id.clone(),
                    game: request.item.game.clone(),
                    release: plan.source.release().map(str::to_string),
                    asset: plan.source.file_name().to_string(),
                    sha256: sha256.to_string(),
                    verified: hash.verified,
                    added_at_unix_ms: now_unix_ms(),
                },
            )
            .map_err(|e| generic("ERR_CATALOG_IMPORT", e))?;
            let size = bytes.len() as u64;
            (outcome.item().clone(), size, hash)
        }
    };

    let layout = definition
        .content_layout
        .as_ref()
        .expect("checked above: the game has a content layout");
    let facts =
        crate::game_conditions::InstanceFacts::for_instance(ctx, request.instance_id, definition);
    let classifier = ctx
        .games
        .package_for(&definition.id)
        .map(|_| game_deploy::PlacementClassifier::new(ctx, request.instance_id, definition));
    let decision = {
        let classify = classifier.as_ref().map(|classifier| {
            |item_name: &str, files: &[agora_game_api::RelPath]| classifier.run(item_name, files)
        });
        let classify_ref: Option<agora_game_api::PlacementClassify<'_>> =
            classify.as_ref().map(|classify| {
                classify
                    as &dyn Fn(
                        &str,
                        &[agora_game_api::RelPath],
                    ) -> Option<agora_game_api::Suggestion>
            });
        game_deploy::decide_placement_with(
            &content_item,
            layout,
            &definition.content_rules,
            Some(&facts),
            classify_ref,
        )
    };
    let state = match decision {
        PlacementDecision::Place {
            mount_path,
            source_path,
            ..
        } => {
            let mount = (!mount_path.is_empty()).then_some(mount_path.as_str());
            let source = (!source_path.is_empty()).then_some(source_path.as_str());
            match game_deploy::add_content(
                ctx,
                request.instance_id,
                &content_item.item_id,
                mount,
                source,
            ) {
                Ok(_) => InstallState::Placed {
                    content_item_id: content_item.item_id.clone(),
                    mount_path,
                    source_path,
                    size,
                    hash: hash.clone(),
                },
                Err(game_deploy::DeployError::ContentAlreadyPresent(_)) => {
                    InstallState::AlreadyInstalled {
                        installed_release: plan.source.release().map(str::to_string),
                        same_release: true,
                    }
                }
                Err(e) => return Err(generic("ERR_CATALOG_PLACE", e)),
            }
        }
        PlacementDecision::Installer { reason, top_level } => InstallState::NeedsInstaller {
            installer_command: format!(
                "agora games content fomod install {} --instance {}",
                content_item.item_id, request.instance_id
            ),
            content_item_id: content_item.item_id.clone(),
            size,
            hash: hash.clone(),
            reason,
            top_level,
        },
        PlacementDecision::Unknown { top_level, note } => {
            if let Some(error) = classifier.and_then(game_deploy::PlacementClassifier::take_error) {
                return Err(LauncherError::Generic {
                    code: "ERR_CATALOG_CLASSIFY".into(),
                    message: format!(
                        "Cannot determine where '{}' goes, because the game's classify check could not run: {}. It is in the content store as {}.",
                        request.item.id, error.message, content_item.item_id
                    ),
                });
            }
            let note = note
                .as_deref()
                .map(|note| format!(" {note}"))
                .unwrap_or_default();
            return Err(LauncherError::Generic {
                code: "ERR_CATALOG_UNPLACEABLE".into(),
                message: format!(
                    "Cannot determine where '{}' goes; top-level entries: {}.{note} It is in the content store as {}. Place it with `agora games instance content add {} {} --into <path>`.",
                    request.item.id,
                    top_level.join(", "),
                    content_item.item_id,
                    request.instance_id,
                    content_item.item_id
                ),
            });
        }
    };
    Ok(InstallReport { plan, state })
}

/// The plan: the compatibility entry, the frameworks, and the file. Makes no change. An unmet
/// framework refuses here, before any network access, unless `keep_unmet`.
async fn plan(
    ctx: &Ctx,
    manifest: &GameInstanceManifest,
    definition: &GameDefinition,
    item: &GameCatalogItem,
    instance_id: &str,
    transport: &dyn CatalogTransport,
    keep_unmet: bool,
) -> LauncherResult<CatalogPlan> {
    let runtime = instance_runtime(manifest).ok_or_else(|| {
        generic(
            "ERR_CATALOG_NO_RUNTIME",
            format!("Instance '{instance_id}' has no runtime version, so no entry can be matched to it."),
        )
    })?;
    let (compatibility_index, compatibility) = choose_compatibility(item, &runtime)?;

    let mut warnings = Vec::new();
    let mut frameworks = Vec::new();
    let mut unmet_frameworks = Vec::new();
    for requirement in &compatibility.requires {
        let found = look_up_framework(
            ctx,
            &manifest.game,
            instance_id,
            definition,
            &requirement.framework,
        )?;
        match judge_requirement(found, requirement, &item.id) {
            RequirementOutcome::Met { check, warning } => {
                warnings.extend(warning);
                frameworks.push(check);
            }
            RequirementOutcome::Unmet(unmet) => unmet_frameworks.push(unmet),
        }
    }
    if !keep_unmet {
        if let Some(refusal) = refuse_unmet(&unmet_frameworks, item, instance_id, &manifest.game) {
            return Err(refusal);
        }
    }
    if !unmet_frameworks.is_empty() {
        link_catalog_entries(ctx, &manifest.game, Some(&item.id), &mut unmet_frameworks);
    }

    let source = plan_source(item, &compatibility, transport).await?;
    Ok(CatalogPlan {
        item_id: item.id.clone(),
        item_name: item.name.clone(),
        instance_id: instance_id.to_string(),
        compatibility_index,
        compatibility,
        frameworks,
        unmet_frameworks,
        source,
        warnings,
    })
}

/// The refusal for these unmet requirements, under the first one's code and naming them all. `None`
/// when there are none.
fn refuse_unmet(
    unmet: &[UnmetFramework],
    item: &GameCatalogItem,
    instance_id: &str,
    game: &GameId,
) -> Option<LauncherError> {
    let refusals: Vec<(&str, String)> = unmet
        .iter()
        .map(|unmet| unmet.refusal(&item.id, instance_id, game.as_str()))
        .collect();
    let code = refusals.first()?.0;
    let message = refusals
        .iter()
        .map(|(_, message)| message.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    Some(generic(code, message))
}

/// A framework as the game and the instance have it, before any one requirement is applied.
#[derive(Clone)]
enum FrameworkLookup {
    Declared {
        name: String,
        status: FrameworkStatus,
    },
    NotDeclared {
        malformed: Option<String>,
    },
}

/// The framework `id` for this game and instance. An error means the instance could not be read,
/// which says nothing about the framework.
fn look_up_framework(
    ctx: &Ctx,
    game: &GameId,
    instance_id: &str,
    definition: &GameDefinition,
    id: &str,
) -> LauncherResult<FrameworkLookup> {
    let framework_id = match FrameworkId::new(id) {
        Ok(framework_id) => framework_id,
        Err(error) => {
            return Ok(FrameworkLookup::NotDeclared {
                malformed: Some(error.to_string()),
            })
        }
    };
    let Some(declared) = ctx.games.framework(game, &framework_id) else {
        return Ok(FrameworkLookup::NotDeclared { malformed: None });
    };
    Ok(FrameworkLookup::Declared {
        name: declared.name.clone(),
        status: game_frameworks::framework_status(ctx, instance_id, definition, declared)?,
    })
}

enum RequirementOutcome {
    Met {
        check: FrameworkCheck,
        warning: Option<String>,
    },
    Unmet(UnmetFramework),
}

/// One requirement against what the instance has. The install's refusal and the launch check both
/// come from here, so they cannot disagree.
fn judge_requirement(
    found: FrameworkLookup,
    requirement: &FrameworkRequirement,
    item_id: &str,
) -> RequirementOutcome {
    let (name, status) = match found {
        FrameworkLookup::Declared { name, status } => (name, status),
        FrameworkLookup::NotDeclared { malformed } => {
            return RequirementOutcome::Unmet(UnmetFramework::new(
                requirement,
                requirement.framework.clone(),
                FrameworkProblem::Undeclared { malformed },
            ))
        }
    };
    let mut warning = None;
    let found_version = match status {
        FrameworkStatus::Absent => {
            return RequirementOutcome::Unmet(UnmetFramework::new(
                requirement,
                name,
                FrameworkProblem::Absent,
            ))
        }
        FrameworkStatus::NotDetectable => {
            return RequirementOutcome::Unmet(UnmetFramework::new(
                requirement,
                name,
                FrameworkProblem::Undetectable,
            ))
        }
        FrameworkStatus::Present {
            version: Some(found),
            ..
        } => {
            if let Some(minimum) = requirement.min_version.as_deref() {
                match game_frameworks::version_at_least(&found, minimum) {
                    Some(true) => {}
                    Some(false) => {
                        return RequirementOutcome::Unmet(UnmetFramework::new(
                            requirement,
                            name,
                            FrameworkProblem::TooOld { found },
                        ))
                    }
                    None => {
                        warning = Some(format!(
                            "Could not compare {name} {found} with the {minimum} that '{item_id}' needs."
                        ))
                    }
                }
            }
            Some(found)
        }
        FrameworkStatus::Present {
            version: None,
            unreadable,
        } => {
            warning = Some(format!(
                "Could not read the version of {name} ({}), so it was not checked against {}. The launch check still guards a mismatched build.",
                unreadable.unwrap_or_else(|| "unknown reason".into()),
                requirement.min_version.as_deref().unwrap_or("any version")
            ));
            None
        }
    };
    RequirementOutcome::Met {
        check: FrameworkCheck {
            framework: requirement.framework.clone(),
            name,
            min_version: requirement.min_version.clone(),
            found_version,
        },
        warning,
    }
}

/// Point each unmet framework at the catalog entry that provides it, when one can be told apart.
/// A registry that cannot be read only means no link, so it is not an error.
fn link_catalog_entries(
    ctx: &Ctx,
    game: &GameId,
    exclude: Option<&str>,
    unmet: &mut [UnmetFramework],
) {
    let entries = RegistryService::new(ctx.clone())
        .list_game_items(game.as_str())
        .unwrap_or_default();
    for framework in unmet {
        framework.catalog_entry = framework_catalog_entry(&entries, exclude, framework);
    }
}

/// The one entry that is this framework: its id is the framework's id, optionally followed by a
/// number (`skse` and `skse64`), or its name is the framework's name, ignoring case. A manifest says
/// which frameworks an entry needs, not which it provides, so this matches narrowly, and two
/// candidates give no link rather than a guess.
fn framework_catalog_entry(
    entries: &[GameCatalogItem],
    exclude: Option<&str>,
    framework: &UnmetFramework,
) -> Option<CatalogEntryRef> {
    let id = framework.framework.to_lowercase();
    let name = framework.name.to_lowercase();
    let mut candidates = entries.iter().filter(|entry| {
        let entry_id = entry.id.to_lowercase();
        Some(entry.id.as_str()) != exclude
            && (entry_id
                .strip_prefix(&id)
                .is_some_and(|rest| rest.bytes().all(|b| b.is_ascii_digit()))
                || entry.name.to_lowercase() == name)
    });
    let only = candidates.next()?;
    candidates.next().is_none().then(|| CatalogEntryRef {
        id: only.id.clone(),
        name: only.name.clone(),
    })
}

/// What the launch check says about the frameworks an instance's catalog mods need.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RequirementFinding {
    /// These mods need a framework the instance does not satisfy.
    Unmet {
        needed_by: Vec<String>,
        unmet: UnmetFramework,
    },
    /// What these mods need could not be checked. Never read as "all is well".
    NotChecked {
        needed_by: Vec<String>,
        reason: String,
    },
}

impl RequirementFinding {
    /// One line for a person.
    pub fn summary(&self) -> String {
        match self {
            RequirementFinding::Unmet { needed_by, unmet } => {
                let verb = if needed_by.len() == 1 {
                    "needs"
                } else {
                    "need"
                };
                format!(
                    "{} {verb} {}{}. {}",
                    needed_by.join(", "),
                    unmet.needs(),
                    unmet.reason(),
                    unmet.advice()
                )
            }
            RequirementFinding::NotChecked { needed_by, reason } => format!(
                "Agora could not check which frameworks {} need ({reason}).",
                needed_by.join(", ")
            ),
        }
    }
}

/// The enabled catalog mods of an instance whose required frameworks are unmet, judged afresh from
/// the catalog and the files the game will see. Grouped by requirement, so five mods that need one
/// missing framework are one finding. This is what still tells the truth after "install anyway",
/// and after a framework is removed later. A mod the catalog no longer lists has no known
/// requirement, so it gives nothing.
pub fn requirement_findings(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
) -> Vec<RequirementFinding> {
    let manifest = match game_instance::get_manifest(ctx, instance_id) {
        Ok(manifest) => manifest,
        Err(error) => {
            return vec![RequirementFinding::NotChecked {
                needed_by: vec![format!("the mods in instance '{instance_id}'")],
                reason: error.to_string(),
            }]
        }
    };
    let installed = enabled_catalog_items(ctx, &manifest);
    if installed.is_empty() {
        return Vec::new();
    }
    let Some(runtime) = instance_runtime(&manifest) else {
        return Vec::new();
    };
    let connection = match RegistryService::new(ctx.clone()).connection() {
        Ok(connection) => connection,
        Err(error) => {
            return vec![RequirementFinding::NotChecked {
                needed_by: installed.into_iter().map(|(_, name)| name).collect(),
                reason: error.to_string(),
            }]
        }
    };

    let mut groups: Vec<(UnmetFramework, Vec<String>)> = Vec::new();
    let mut unchecked: Vec<(String, String)> = Vec::new();
    let mut looked_up: HashMap<String, Result<FrameworkLookup, String>> = HashMap::new();
    for (item_id, fallback_name) in installed {
        let item = match registry::get_game_item(&connection, &item_id) {
            Ok(Some(item)) => item,
            Ok(None) => continue,
            Err(error) => {
                unchecked.push((fallback_name, error.to_string()));
                continue;
            }
        };
        let Ok((_, compatibility)) = choose_compatibility(&item, &runtime) else {
            continue;
        };
        for requirement in &compatibility.requires {
            // Reading a framework plans the instance's files, so it is done once per framework.
            let found = looked_up
                .entry(requirement.framework.clone())
                .or_insert_with(|| {
                    look_up_framework(
                        ctx,
                        &manifest.game,
                        instance_id,
                        definition,
                        &requirement.framework,
                    )
                    .map_err(|error| error.to_string())
                });
            let found = match found {
                Ok(found) => found.clone(),
                Err(reason) => {
                    unchecked.push((item.name.clone(), reason.clone()));
                    continue;
                }
            };
            if let RequirementOutcome::Unmet(unmet) =
                judge_requirement(found, requirement, &item.id)
            {
                match groups.iter_mut().find(|(group, _)| *group == unmet) {
                    Some((_, needed_by)) => needed_by.push(item.name.clone()),
                    None => groups.push((unmet, vec![item.name.clone()])),
                }
            }
        }
    }

    let mut findings = Vec::new();
    for (mut unmet, needed_by) in groups {
        link_catalog_entries(ctx, &manifest.game, None, std::slice::from_mut(&mut unmet));
        findings.push(RequirementFinding::Unmet { needed_by, unmet });
    }
    if let Some((_, reason)) = unchecked.first() {
        findings.push(RequirementFinding::NotChecked {
            reason: reason.clone(),
            needed_by: unchecked.iter().map(|(name, _)| name.clone()).collect(),
        });
    }
    findings
}

/// The catalog entries behind an instance's enabled layers, as `(entry id, stored item name)`, each
/// once. An item a FOMOD installer derived counts as the archive it came from. The store's ids are
/// listed once: a lookup without them rescans the folder, per layer, on setups of hundreds of mods.
fn enabled_catalog_items(ctx: &Ctx, manifest: &GameInstanceManifest) -> Vec<(String, String)> {
    let Ok(ids) = content_store::item_ids(ctx) else {
        return Vec::new();
    };
    let mut found: Vec<(String, String)> = Vec::new();
    for layer in manifest.layers.layers() {
        let LayerSource::Content { content } = &layer.source else {
            continue;
        };
        if !layer.enabled {
            continue;
        }
        let Ok(item) = content_store::get_item_indexed(ctx, &ids, content) else {
            continue;
        };
        let origin = crate::content_fomod::recorded_install(&item)
            .and_then(|(from_item, _)| content_store::get_item_indexed(ctx, &ids, from_item).ok());
        for source in origin.iter().chain([&item]).flat_map(|i| &i.sources) {
            if let ContentSource::Catalog { item_id, .. } = source {
                if !found.iter().any(|(id, _)| id == item_id) {
                    found.push((item_id.clone(), item.name.clone()));
                }
            }
        }
    }
    found
}

/// The instance's runtime identity: the one it records, or its pinned base's.
pub(crate) fn instance_runtime(manifest: &GameInstanceManifest) -> Option<RuntimeIdentity> {
    manifest
        .runtime_identity
        .clone()
        .or_else(|| match &manifest.base {
            BaseReference::Pinned { runtime, .. } => Some(runtime.clone()),
            BaseReference::Unpinned { .. } => None,
        })
}

/// The first compatibility entry whose stores include the runtime's store and whose game versions
/// include its version.
fn choose_compatibility(
    item: &GameCatalogItem,
    runtime: &RuntimeIdentity,
) -> LauncherResult<(usize, GameCompatibility)> {
    let store = runtime.store.as_str();
    let found = item
        .game_compatibility
        .iter()
        .enumerate()
        .find(|(_, compat)| {
            compat.stores.iter().any(|s| s == store)
                && compat
                    .game_versions
                    .iter()
                    .any(|pattern| version_matches(pattern, &runtime.version))
        });
    match found {
        Some((index, compat)) => Ok((index, compat.clone())),
        None => {
            let supported: Vec<String> = item
                .game_compatibility
                .iter()
                .map(|compat| {
                    format!(
                        "{} on {}",
                        compat.game_versions.join(", "),
                        compat.stores.join("/")
                    )
                })
                .collect();
            Err(generic(
                "ERR_CATALOG_NO_COMPATIBLE_ENTRY",
                format!(
                    "'{}' supports {}. This instance is {} {} on {}, which none of those matches.",
                    item.id,
                    supported.join("; "),
                    runtime.game.as_str(),
                    runtime.version,
                    store
                ),
            ))
        }
    }
}

/// Whether `version` matches `pattern`: the same, or a `*` on whole dot-separated components.
pub fn version_matches(pattern: &str, version: &str) -> bool {
    let pattern_parts: Vec<&str> = pattern.split('.').collect();
    let version_parts: Vec<&str> = version.split('.').collect();
    pattern_parts.len() == version_parts.len()
        && pattern_parts
            .iter()
            .zip(&version_parts)
            .all(|(p, v)| *p == "*" || p == v)
}

/// The file to fetch, from the entry's first download source that core can use.
async fn plan_source(
    item: &GameCatalogItem,
    compatibility: &GameCompatibility,
    transport: &dyn CatalogTransport,
) -> LauncherResult<PlannedSource> {
    let chosen = item
        .download_sources
        .iter()
        .find(|source| matches!(source.strategy.as_str(), "github_release" | "direct_hash"));
    let Some(source) = chosen else {
        return Err(generic(
            "ERR_CATALOG_NO_SOURCE",
            format!(
                "'{}' has no download source Agora can install from.",
                item.id
            ),
        ));
    };
    match source.strategy.as_str() {
        "github_release" => github_source(item, source, compatibility, transport).await,
        _ => direct_source(item, source),
    }
}

async fn github_source(
    item: &GameCatalogItem,
    source: &DownloadSource,
    compatibility: &GameCompatibility,
    transport: &dyn CatalogTransport,
) -> LauncherResult<PlannedSource> {
    let repo = source.identifier.clone();
    let glob = compatibility.asset.clone().ok_or_else(|| {
        generic(
            "ERR_CATALOG_NO_ASSET_PATTERN",
            format!(
                "'{}' names no asset to pick for this compatibility entry.",
                item.id
            ),
        )
    })?;
    let chosen = find_newest_asset(&repo, &glob, transport).await?;
    let Some((release, asset)) = chosen else {
        return Err(generic(
            "ERR_CATALOG_NO_ASSET",
            format!(
                "No release of {repo} has a file matching '{glob}' (drafts and prereleases are not considered)."
            ),
        ));
    };
    let published_sha256 = asset
        .digest
        .as_deref()
        .map(|digest| digest.strip_prefix("sha256:").unwrap_or(digest).to_string());
    let pin = source
        .pins
        .iter()
        .find(|pin| pin.tag == release.tag_name && pin.asset == asset.name)
        .map(|pin| ConfirmableHash {
            origin: HashOrigin::CuratorPin,
            sha256: pin.sha256.clone(),
        });
    Ok(PlannedSource::GithubRelease {
        repo,
        tag: release.tag_name.clone(),
        asset: asset.name.clone(),
        download_url: asset.browser_download_url.clone(),
        published_sha256,
        size: asset.size,
        pin,
    })
}

fn direct_source(item: &GameCatalogItem, source: &DownloadSource) -> LauncherResult<PlannedSource> {
    let url = source.identifier.clone();
    let parsed = reqwest::Url::parse(&url).map_err(|_| {
        generic(
            "ERR_CATALOG_BAD_URL",
            format!("'{}' has a download URL that is not valid.", item.id),
        )
    })?;
    let file = parsed
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .filter(|segment| !segment.is_empty())
        .ok_or_else(|| {
            generic(
                "ERR_CATALOG_BAD_URL",
                format!("'{}' has a download URL with no file name.", item.id),
            )
        })?
        .to_string();
    // An empty manifest hash is no hash at all: it must not pass as a checksum, nor as a file that
    // "matches anything".
    let sha256 = item
        .sha256
        .clone()
        .filter(|hash| !hash.trim().is_empty())
        .ok_or_else(|| {
            generic(
                "ERR_CATALOG_NO_HASH",
                format!(
                    "'{}' is a direct download with no sha256 in its manifest.",
                    item.id
                ),
            )
        })?;
    let pinned_host = parsed.host_str().unwrap_or_default().to_string();
    Ok(PlannedSource::DirectHash {
        url,
        file,
        sha256,
        pinned_host,
        size: None,
    })
}

/// The newest release, page by page, with a non-draft, non-prerelease asset matching `glob`
/// (case-insensitive). Within a page, releases are taken by `published_at`, newest first.
async fn find_newest_asset(
    repo: &str,
    glob: &str,
    transport: &dyn CatalogTransport,
) -> LauncherResult<Option<(GitHubRelease, crate::github_release::GitHubReleaseAsset)>> {
    let pattern = glob.to_ascii_lowercase();
    let mut page = 1;
    loop {
        let (mut releases, total_pages) = transport.releases_page(repo, page).await?;
        releases.sort_by(|a, b| b.published_at.cmp(&a.published_at));
        for release in releases {
            if release.draft || release.prerelease {
                continue;
            }
            let found = release
                .assets
                .iter()
                .find(|asset| {
                    agora_game_api::glob_match(&pattern, &asset.name.to_ascii_lowercase())
                })
                .cloned();
            if let Some(asset) = found {
                return Ok(Some((release, asset)));
            }
        }
        if page >= total_pages {
            return Ok(None);
        }
        page += 1;
    }
}

/// The bytes of the planned file.
async fn fetch_bytes(
    transport: &dyn CatalogTransport,
    source: &PlannedSource,
) -> LauncherResult<Vec<u8>> {
    match source {
        PlannedSource::GithubRelease { download_url, .. } => {
            transport.fetch(download_url, None).await
        }
        PlannedSource::DirectHash {
            url, pinned_host, ..
        } => transport.fetch(url, Some(pinned_host)).await,
    }
}

/// Check the bytes against what their source published, the curator's pin, and an earlier install.
fn check_bytes(
    ctx: &Ctx,
    plan: &CatalogPlan,
    bytes: &[u8],
    install_anyway: bool,
) -> LauncherResult<HashReport> {
    let published = match &plan.source {
        PlannedSource::GithubRelease {
            published_sha256, ..
        } => published_sha256.clone(),
        PlannedSource::DirectHash { sha256, .. } => Some(sha256.clone()),
    };
    let mut expectations = Vec::new();
    if let PlannedSource::GithubRelease { pin: Some(pin), .. } = &plan.source {
        expectations.push(pin.clone());
    }
    if let Some(remembered) = remembered_hash(
        ctx,
        &plan.item_id,
        plan.source.release(),
        plan.source.file_name(),
    )? {
        expectations.push(ConfirmableHash {
            origin: HashOrigin::PreviousInstall,
            sha256: remembered,
        });
    }

    let actual = crate::download::sha256_hex(bytes);
    let unmet = first_unmet(&actual, &expectations).map(|expected| expected.origin);
    let confirmed_past = if install_anyway { unmet } else { None };
    let check = verify_download(
        bytes,
        None,
        published.as_deref(),
        &expectations,
        install_anyway,
        plan.source.file_name(),
        plan.source.release(),
    )?;

    let basis = if check.verified {
        match &plan.source {
            PlannedSource::GithubRelease { .. } => {
                "verified by the SHA-256 GitHub published for this file".to_string()
            }
            PlannedSource::DirectHash { .. } => {
                "verified by the SHA-256 in the curator's manifest".to_string()
            }
        }
    } else {
        let mut basis = format!(
            "not verified: GitHub published no checksum for {}, so Agora could not verify it",
            plan.source.file_name()
        );
        if let (None, Some(origin)) = (confirmed_past, expectations.first().map(|e| e.origin)) {
            basis.push_str(&format!("; the bytes match {}", describe_origin(origin)));
        }
        basis
    };
    let basis = match confirmed_past {
        Some(origin) => format!(
            "{basis}; installed with --install-anyway although they differ from {}",
            describe_origin(origin)
        ),
        None => basis,
    };
    Ok(HashReport {
        sha256: check.sha256,
        verified: check.verified,
        basis,
        confirmed_past,
    })
}

fn describe_origin(origin: HashOrigin) -> &'static str {
    match origin {
        HashOrigin::CuratorPin => "the curator's pin for this release file",
        HashOrigin::PreviousInstall => "the hash from an earlier install of this release file",
        HashOrigin::PackPin => "the hash listed in the modpack",
    }
}

/// The SHA-256 the source vouches for before any download: GitHub's published digest, the curator's
/// pin for this release file, or the manifest hash of a `direct_hash` file. An empty digest is
/// nothing published. `None` when there is no such hash, or when the ones there are malformed or
/// disagree; the download path then decides, and refuses a malformed published hash.
fn trusted_hash(source: &PlannedSource) -> Option<String> {
    let (published, pin) = match source {
        PlannedSource::GithubRelease {
            published_sha256,
            pin,
            ..
        } => (
            published_sha256
                .as_deref()
                .map(str::trim)
                .filter(|h| !h.is_empty()),
            pin.as_ref().map(|pin| pin.sha256.as_str()),
        ),
        PlannedSource::DirectHash { sha256, .. } => (Some(sha256.as_str()), None),
    };
    let mut trusted = Vec::new();
    for hash in published.into_iter().chain(pin) {
        if !is_sha256_hex(hash) {
            return None;
        }
        trusted.push(hash.to_ascii_lowercase());
    }
    let first = trusted.first()?.clone();
    trusted.iter().all(|hash| *hash == first).then_some(first)
}

/// A copy of this release file that the content store already holds, used in place of a download.
/// It is used only when all of these hold:
///
/// * a trusted hash is known before the download (see [`trusted_hash`]);
/// * an item in the store was imported from this entry's release file under that hash;
/// * every file of that item still matches the hash recorded for it (a full `verify_item`).
///
/// The store keeps an archive's extracted files, not the archive, so the archive's own bytes are not
/// hashed again: the files are checked against their recorded hashes, and the archive hash is the
/// one recorded when it was imported. A copy that fails the check is noted in `plan.warnings`, and
/// the file is downloaded as usual.
///
/// The size reported is the archive's, when the source gives it; otherwise the total size of the
/// stored files.
fn stored_copy(
    ctx: &Ctx,
    plan: &mut CatalogPlan,
    damaged: &mut HashSet<String>,
) -> LauncherResult<Option<(ContentItem, u64, HashReport)>> {
    let Some(expected) = trusted_hash(&plan.source) else {
        return Ok(None);
    };
    let release = plan.source.release().map(str::to_string);
    let file = plan.source.file_name().to_string();
    let entries = content_store::list_items(ctx).map_err(|e| generic("ERR_CONTENT_STORE", e))?;
    // A damaged record names no source, so it can match nothing here; the Library lists it.
    for item in entries.iter().filter_map(|entry| entry.readable()) {
        let imported_from_this_file = item.sources.iter().any(|source| {
            matches!(
                source,
                ContentSource::Catalog {
                    item_id,
                    release: r,
                    asset,
                    sha256,
                    ..
                } if *item_id == plan.item_id && *r == release && *asset == file && *sha256 == expected
            )
        });
        if !imported_from_this_file {
            continue;
        }
        let check =
            content_store::verify_item(ctx, &item.item_id, content_store::VerifyDepth::Full)
                .map_err(|e| generic("ERR_CONTENT_STORE", e))?;
        if !check.problems.is_empty() {
            // Name the objects the verify found bad, so the download's import replaces them.
            for problem in &check.problems {
                if let Some(file) = item
                    .files
                    .iter()
                    .find(|stored| stored.path.as_str() == problem.path)
                {
                    damaged.insert(file.sha256.clone());
                }
            }
            let paths: Vec<&str> = check
                .problems
                .iter()
                .map(|problem| problem.path.as_str())
                .take(3)
                .collect();
            plan.warnings.push(format!(
                "The content store's copy of {file} ({}) does not match its recorded hashes ({} problem(s), including {}). It was downloaded again instead.",
                item.item_id,
                check.problems.len(),
                paths.join(", ")
            ));
            continue;
        }

        let published = match &plan.source {
            PlannedSource::GithubRelease {
                published_sha256, ..
            } => published_sha256
                .as_deref()
                .is_some_and(|hash| !hash.trim().is_empty()),
            PlannedSource::DirectHash { .. } => true,
        };
        let verified_by = match &plan.source {
            PlannedSource::GithubRelease { .. } => {
                "verified by the SHA-256 GitHub published for this file"
            }
            PlannedSource::DirectHash { .. } => "verified by the SHA-256 in the curator's manifest",
        };
        let basis = if published {
            verified_by.to_string()
        } else {
            format!(
                "not verified: GitHub published no checksum for {file}, so Agora could not verify it; the bytes match the curator's pin for this release file"
            )
        };
        let size = match &plan.source {
            PlannedSource::GithubRelease {
                size: Some(size), ..
            } => *size,
            _ => item.total_size,
        };
        let hash = HashReport {
            sha256: expected.clone(),
            verified: published,
            basis: format!(
                "{basis}; reused the copy the content store already holds, so nothing was downloaded"
            ),
            confirmed_past: None,
        };
        return Ok(Some((item.clone(), size, hash)));
    }
    Ok(None)
}

/// The SHA-256 recorded when this release file was installed before without a published hash.
/// An item imported from a verified source never gives an expectation.
fn remembered_hash(
    ctx: &Ctx,
    item_id: &str,
    release: Option<&str>,
    asset: &str,
) -> LauncherResult<Option<String>> {
    let entries = content_store::list_items(ctx).map_err(|e| generic("ERR_CONTENT_STORE", e))?;
    // A damaged record names no source, so it remembers no hash; the Library lists it.
    for item in entries.iter().filter_map(|entry| entry.readable()) {
        for source in &item.sources {
            if let ContentSource::Catalog {
                item_id: id,
                release: r,
                asset: a,
                sha256,
                verified: false,
                ..
            } = source
            {
                if id == item_id && r.as_deref() == release && a == asset {
                    return Ok(Some(sha256.clone()));
                }
            }
        }
    }
    Ok(None)
}

/// The release of an item from this entry that the instance already has, if it has one.
fn installed_release(
    ctx: &Ctx,
    manifest: &GameInstanceManifest,
    item_id: &str,
) -> LauncherResult<Option<Option<String>>> {
    for layer in manifest.layers.layers() {
        let LayerSource::Content { content } = &layer.source else {
            continue;
        };
        let Ok(content_item) = content_store::get_item(ctx, content) else {
            continue;
        };
        for source in &content_item.sources {
            if let ContentSource::Catalog {
                item_id: id,
                release,
                ..
            } = source
            {
                if id == item_id {
                    return Ok(Some(release.clone()));
                }
            }
        }
    }
    Ok(None)
}

/// A file name the download can be stored under: the original name with anything unusual
/// replaced, so a hostile name cannot leave the scratch folder.
fn safe_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() || cleaned.starts_with('.') {
        format!("download{cleaned}")
    } else {
        cleaned
    }
}

/// A scratch folder under the content store for one download, removed when dropped.
struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    fn new(ctx: &Ctx) -> LauncherResult<Self> {
        // The pid and the millisecond alone collide when two installs start together in one
        // process; the counter makes every scratch folder distinct.
        static NEXT_SCRATCH: AtomicU64 = AtomicU64::new(0);
        let sequence = NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed);
        let path = ctx
            .paths
            .content_root()
            .join("catalog-downloads")
            .join(format!(
                "{}-{}-{sequence}",
                std::process::id(),
                now_unix_ms()
            ));
        std::fs::create_dir_all(&path).map_err(|e| generic("ERR_CATALOG_WRITE", e))?;
        Ok(Self { path })
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn generic(code: &str, error: impl std::fmt::Display) -> LauncherError {
    LauncherError::Generic {
        code: code.to_string(),
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_version_pattern_matches_exactly_or_by_whole_components() {
        assert!(version_matches("1.6.1179.0", "1.6.1179.0"));
        assert!(version_matches("1.6.1170.*", "1.6.1170.0"));
        assert!(!version_matches("1.6.1170.*", "1.6.1171.0"));
        assert!(!version_matches("1.6.1170.*", "1.6.1170"));
        assert!(!version_matches("1.6.11", "1.6.1170.0"));
        assert!(!version_matches("1.6.1179.0", "1.6.1179.0.1"));
    }

    #[test]
    fn a_hostile_file_name_is_stored_under_a_safe_name() {
        assert_eq!(safe_file_name("CrashLogger-1.2.7z"), "CrashLogger-1.2.7z");
        assert_eq!(safe_file_name("../../evil.7z"), "download.._.._evil.7z");
        assert_eq!(safe_file_name(""), "download");
        assert!(!safe_file_name("a\\b:c").contains('\\'));
    }

    #[test]
    fn two_scratch_dirs_at_once_do_not_share_a_folder() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::for_testing(dir.path().to_path_buf());
        let first = ScratchDir::new(&ctx).unwrap();
        let second = ScratchDir::new(&ctx).unwrap();
        assert_ne!(first.path, second.path);
        assert!(first.path.exists());
        assert!(second.path.exists());
    }
}
