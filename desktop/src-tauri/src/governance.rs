//! Governance Tauri adapters, diagnostics, and authenticated item voting.
//!
//! Pure logic in `agora_core::governance`; this module adds read-only GitHub
//! API checks for diagnostics (checks 5-13 from the spec).
//!
//! # Network checks (read-only)
//! - `repository_metadata_readable` - GET /repos/:owner/:repo
//! - `issues_enabled` - check repo features from metadata
//! - `discussions_enabled` - check repo features from metadata
//! - `triage_category_exists` - GraphQL query for DiscussionCategory
//! - Required labels and Issue forms via read-only REST requests
//!
//! Diagnostics remain read-only. Item voting lives in
//! `agora_core::governance_vote`; it mutates only the authenticated user's
//! direct reaction on a registry-mapped voting issue.

use crate::error::LauncherResult;

use agora_core::governance::GovernanceService;
pub use agora_core::governance::{
    resolve_github_app_slug, resolve_governance_config as core_resolve_config,
    resolve_governance_environment, resolve_governance_repo,
    run_governance_diagnostics as core_sync_checks, DiagnosticCheck, DiagnosticStatus,
    GovernanceConfig, GovernanceEnvironment, GovernanceEvent, GovernanceSummary,
};
pub use agora_core::governance_vote::{ItemVote, ItemVoteState};
use agora_core::registry::RegistryService;
use tauri::AppHandle;

// --- Config ---

/// Return the resolved governance configuration.
pub fn get_governance_config(app: &AppHandle) -> GovernanceConfig {
    let ctx = match crate::core_context(app) {
        Ok(c) => c,
        Err(_) => return core_resolve_config(None, None),
    };
    let registry = RegistryService::new(ctx);
    let svc = GovernanceService::new(registry);
    svc.config()
}

// --- Summary (per-item) ---

/// Fetch the governance summary for a single item.
pub fn get_governance_summary(
    app: &AppHandle,
    item_id: &str,
) -> LauncherResult<Option<GovernanceSummary>> {
    let ctx = crate::core_context(app)?;
    let registry = RegistryService::new(ctx);
    let svc = GovernanceService::new(registry);
    svc.get_governance_summary(item_id)
}

/// Return the authenticated user's direct vote on an item's vote issue.
pub async fn get_item_vote(app: &AppHandle, item_id: String) -> LauncherResult<ItemVoteState> {
    let ctx = crate::core_context(app)?;
    agora_core::governance_vote::get_item_vote(&ctx, &item_id).await
}

/// Make the authenticated user's direct reaction match the requested vote.
pub async fn set_item_vote(
    app: &AppHandle,
    item_id: String,
    vote: Option<ItemVote>,
) -> LauncherResult<ItemVoteState> {
    let ctx = crate::core_context(app)?;
    agora_core::governance_vote::set_item_vote(&ctx, &item_id, vote).await
}

// --- Events ---

/// List governance events, optionally filtered by item_id.
pub fn list_governance_events(
    app: &AppHandle,
    item_id: Option<&str>,
) -> LauncherResult<Vec<GovernanceEvent>> {
    let ctx = crate::core_context(app)?;
    let registry = RegistryService::new(ctx);
    let svc = GovernanceService::new(registry);
    svc.list_governance_events(item_id, 100)
}

// --- Diagnostics (sync) ---

/// Run sync governance diagnostics (core checks only).
pub fn run_sync_diagnostics(app: &AppHandle) -> Vec<DiagnosticCheck> {
    let ctx = match crate::core_context(app) {
        Ok(ctx) => ctx,
        Err(_) => return Vec::new(),
    };
    let registry = RegistryService::new(ctx);
    let svc = GovernanceService::new(registry);
    svc.run_diagnostics()
}

// --- Diagnostics (async network checks) ---

/// Run async network-based diagnostic checks. The checks live in
/// `agora_core::governance`, shared with the CLI; this adapter only supplies
/// the core context. Without a context the checks cannot run, so they are
/// reported as unavailable rather than dropped.
pub async fn run_network_diagnostics(app: &AppHandle) -> Vec<DiagnosticCheck> {
    let ctx = match crate::core_context(app) {
        Ok(ctx) => ctx,
        Err(error) => {
            eprintln!("[governance] core context unavailable for network diagnostics: {error}");
            let has_token = agora_core::auth::get_valid_access_token().await.is_some();
            return agora_core::governance::network_checks_unavailable(has_token);
        }
    };
    agora_core::governance::run_network_diagnostics(&ctx).await
}

#[cfg(test)]
mod http_policy_tests {
    #[test]
    fn governance_does_not_construct_or_use_a_raw_http_client() {
        // This guards the architectural rule that governance requests must go
        // through agora_core's checked HTTP helpers, rather than testing HTTP
        // behavior itself.
        let source = include_str!("governance.rs");
        let reqwest_client = ["reqwest", "::Client"].concat();
        assert!(!source.contains(&reqwest_client));
        assert!(!source.contains(&["reqwest", "::Client", "::builder"].concat()));
        assert!(!source.contains(&["reqwest", "::Client", "::new"].concat()));
        let bypass_client = ["github", "_client", "()"].concat();
        assert!(!source.contains(&bypass_client));
        for method in ["get", "post"] {
            assert!(!source.contains(&format!(".{method}(")));
        }
    }
}
