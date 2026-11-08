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
//! Diagnostics remain read-only. Item voting mutates only the authenticated
//! user's direct reaction on a registry-mapped voting issue.

use crate::error::{LauncherError, LauncherResult};

use agora_core::governance::{github_headers, GovernanceService};
pub use agora_core::governance::{
    resolve_github_app_slug, resolve_governance_config as core_resolve_config,
    resolve_governance_environment, resolve_governance_repo,
    run_governance_diagnostics as core_sync_checks, DiagnosticCheck, DiagnosticStatus,
    GovernanceConfig, GovernanceEnvironment, GovernanceEvent, GovernanceSummary,
};
use agora_core::registry::RegistryService;
use tauri::AppHandle;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemVote {
    Upvote,
    Downvote,
}

impl ItemVote {
    fn graphql_content(self) -> &'static str {
        match self {
            Self::Upvote => "THUMBS_UP",
            Self::Downvote => "THUMBS_DOWN",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ItemVoteState {
    pub vote: Option<ItemVote>,
    pub conflicted: bool,
}

#[derive(Debug, Clone)]
struct VoteTarget {
    owner: String,
    repository: String,
    issue_number: i32,
}

#[derive(Debug, serde::Deserialize)]
struct GraphQlEnvelope<T> {
    data: Option<T>,
    errors: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, serde::Deserialize)]
struct VoteQueryData {
    repository: Option<VoteQueryRepository>,
}

#[derive(Debug, serde::Deserialize)]
struct VoteQueryRepository {
    issue: Option<VoteQueryIssue>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct VoteQueryIssue {
    id: String,
    reaction_groups: Vec<VoteReactionGroup>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct VoteReactionGroup {
    content: String,
    viewer_has_reacted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VoteMutation {
    Add(ItemVote),
    Remove(ItemVote),
}

#[derive(Debug)]
struct VoteSnapshot {
    issue_node_id: String,
    upvote: bool,
    downvote: bool,
}

static VOTE_MUTATION_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

fn resolve_vote_target(app: &AppHandle, item_id: &str) -> LauncherResult<VoteTarget> {
    let ctx = crate::core_context(app)?;
    let registry = RegistryService::new(ctx);
    let item = registry
        .get_item_by_id(item_id)?
        .ok_or_else(|| LauncherError::Generic {
            code: "ERR_GOVERNANCE_ITEM_NOT_FOUND".to_string(),
            message: "This curated item is no longer available in the registry.".to_string(),
        })?;
    if item.is_immune {
        return Err(LauncherError::Generic {
            code: "ERR_GOVERNANCE_VOTE_IMMUNE".to_string(),
            message: "This curator-protected item does not accept votes.".to_string(),
        });
    }

    let governance = GovernanceService::new(registry);
    let summary =
        governance
            .get_governance_summary(item_id)?
            .ok_or_else(|| LauncherError::Generic {
                code: "ERR_GOVERNANCE_NO_VOTE_ISSUE".to_string(),
                message: "Voting is not available for this item in the current registry build."
                    .to_string(),
            })?;
    let issue_number = summary
        .vote_issue_number
        .and_then(|number| i32::try_from(number).ok())
        .filter(|number| *number > 0)
        .ok_or_else(|| LauncherError::Generic {
            code: "ERR_GOVERNANCE_NO_VOTE_ISSUE".to_string(),
            message: "This item does not have a voting issue.".to_string(),
        })?;

    let config = governance.config();
    let (owner, repository) =
        config
            .repository
            .split_once('/')
            .ok_or_else(|| LauncherError::Generic {
                code: "ERR_GOVERNANCE_CONFIG".to_string(),
                message: "The governance repository configuration is invalid.".to_string(),
            })?;
    if owner.is_empty() || repository.is_empty() || repository.contains('/') {
        return Err(LauncherError::Generic {
            code: "ERR_GOVERNANCE_CONFIG".to_string(),
            message: "The governance repository configuration is invalid.".to_string(),
        });
    }

    Ok(VoteTarget {
        owner: owner.to_string(),
        repository: repository.to_string(),
        issue_number,
    })
}

async fn github_graphql<T: serde::de::DeserializeOwned>(
    app: &AppHandle,
    body: &serde_json::Value,
    operation: &str,
) -> LauncherResult<T> {
    let ctx = crate::core_context(app)?;
    let mut token = crate::auth::get_valid_access_token(app)
        .await
        .ok_or(LauncherError::AuthRequired)?;
    let request_body = serde_json::to_vec(body).map_err(|_| LauncherError::Generic {
        code: "ERR_GOVERNANCE_VOTE".to_string(),
        message: "Failed to encode the vote request.".to_string(),
    })?;

    for attempt in 0..2 {
        let _permit = agora_core::github_ratelimit::acquire_github_permit().await;
        let response = agora_core::http_client::checked_send(
            &ctx.http_clients,
            agora_core::http_client::ClientCategory::GitHub,
            reqwest::Method::POST,
            "https://api.github.com/graphql",
            &github_headers(&token, None),
            Some(request_body.clone()),
            Some("application/json"),
        )
        .await
        .map_err(|_| LauncherError::NetworkOffline)?;

        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            if attempt == 0
                && agora_core::auth::try_refresh_after_401_with_token(&token)
                    .await
                    .is_ok()
            {
                token = crate::auth::get_valid_access_token(app)
                    .await
                    .ok_or(LauncherError::AuthExpired)?;
                continue;
            }
            let _ = crate::auth::clear_token(app);
            return Err(LauncherError::AuthExpired);
        }
        if agora_core::github_ratelimit::is_rate_limit_response(&response) {
            let retry_after = agora_core::github_ratelimit::parse_retry_after(&response);
            agora_core::github_ratelimit::report_rate_limit(retry_after).await;
            return Err(LauncherError::Generic {
                code: "ERR_RATE_LIMITED".to_string(),
                message: "GitHub rate limited the vote request. Try again shortly.".to_string(),
            });
        }
        if response.status() == reqwest::StatusCode::FORBIDDEN {
            return Err(LauncherError::Generic {
                code: "ERR_GOVERNANCE_VOTE_FORBIDDEN".to_string(),
                message: "GitHub did not grant permission to change issue reactions.".to_string(),
            });
        }
        if !response.status().is_success() {
            return Err(LauncherError::Generic {
                code: "ERR_GOVERNANCE_VOTE".to_string(),
                message: format!("GitHub could not complete the {operation} request."),
            });
        }

        let response_text = agora_core::http_client::checked_response_text(
            response,
            agora_core::http_client::ClientCategory::GitHub,
        )
        .await
        .map_err(|_| LauncherError::Generic {
            code: "ERR_GOVERNANCE_VOTE".to_string(),
            message: "GitHub returned an invalid vote response.".to_string(),
        })?;
        let envelope: GraphQlEnvelope<T> =
            serde_json::from_str(&response_text).map_err(|_| LauncherError::Generic {
                code: "ERR_GOVERNANCE_VOTE".to_string(),
                message: "GitHub returned an invalid vote response.".to_string(),
            })?;
        if envelope.errors.is_some_and(|errors| !errors.is_empty()) {
            return Err(LauncherError::Generic {
                code: "ERR_GOVERNANCE_VOTE".to_string(),
                message: format!("GitHub rejected the {operation} request."),
            });
        }
        return envelope.data.ok_or_else(|| LauncherError::Generic {
            code: "ERR_GOVERNANCE_VOTE".to_string(),
            message: "GitHub returned an empty vote response.".to_string(),
        });
    }

    Err(LauncherError::AuthExpired)
}

fn state_from_snapshot(snapshot: &VoteSnapshot) -> ItemVoteState {
    match (snapshot.upvote, snapshot.downvote) {
        (true, false) => ItemVoteState {
            vote: Some(ItemVote::Upvote),
            conflicted: false,
        },
        (false, true) => ItemVoteState {
            vote: Some(ItemVote::Downvote),
            conflicted: false,
        },
        (true, true) => ItemVoteState {
            vote: None,
            conflicted: true,
        },
        (false, false) => ItemVoteState {
            vote: None,
            conflicted: false,
        },
    }
}

fn plan_vote_mutations(snapshot: &VoteSnapshot, desired: Option<ItemVote>) -> Vec<VoteMutation> {
    let mut operations = Vec::new();
    if snapshot.upvote && desired != Some(ItemVote::Upvote) {
        operations.push(VoteMutation::Remove(ItemVote::Upvote));
    }
    if snapshot.downvote && desired != Some(ItemVote::Downvote) {
        operations.push(VoteMutation::Remove(ItemVote::Downvote));
    }
    if desired == Some(ItemVote::Upvote) && !snapshot.upvote {
        operations.push(VoteMutation::Add(ItemVote::Upvote));
    }
    if desired == Some(ItemVote::Downvote) && !snapshot.downvote {
        operations.push(VoteMutation::Add(ItemVote::Downvote));
    }
    operations
}

async fn fetch_vote_snapshot(app: &AppHandle, target: &VoteTarget) -> LauncherResult<VoteSnapshot> {
    let body = serde_json::json!({
        "query": r#"
            query ItemVote($owner: String!, $repository: String!, $number: Int!) {
              repository(owner: $owner, name: $repository) {
                issue(number: $number) {
                  id
                  reactionGroups {
                    content
                    viewerHasReacted
                  }
                }
              }
            }
        "#,
        "variables": {
            "owner": &target.owner,
            "repository": &target.repository,
            "number": target.issue_number,
        },
    });
    let data: VoteQueryData = github_graphql(app, &body, "vote lookup").await?;
    let issue = data
        .repository
        .and_then(|repository| repository.issue)
        .ok_or_else(|| LauncherError::Generic {
            code: "ERR_GOVERNANCE_VOTE_ISSUE_GONE".to_string(),
            message: "The voting issue could not be found.".to_string(),
        })?;

    let mut snapshot = VoteSnapshot {
        issue_node_id: issue.id,
        upvote: false,
        downvote: false,
    };
    for group in issue.reaction_groups {
        if !group.viewer_has_reacted {
            continue;
        }
        match group.content.as_str() {
            "THUMBS_UP" => snapshot.upvote = true,
            "THUMBS_DOWN" => snapshot.downvote = true,
            _ => {}
        }
    }
    Ok(snapshot)
}

async fn apply_vote_mutation(
    app: &AppHandle,
    issue_node_id: &str,
    operation: VoteMutation,
) -> LauncherResult<()> {
    let (query, vote) = match operation {
        VoteMutation::Add(vote) => (
            r#"mutation ItemVote($subjectId: ID!, $content: ReactionContent!) {
                 addReaction(input: { subjectId: $subjectId, content: $content }) {
                   reaction { content }
                 }
               }"#,
            vote,
        ),
        VoteMutation::Remove(vote) => (
            r#"mutation ItemVote($subjectId: ID!, $content: ReactionContent!) {
                 removeReaction(input: { subjectId: $subjectId, content: $content }) {
                   reaction { content }
                 }
               }"#,
            vote,
        ),
    };
    let body = serde_json::json!({
        "query": query,
        "variables": {
            "subjectId": issue_node_id,
            "content": vote.graphql_content(),
        },
    });
    let _: serde_json::Value = github_graphql(app, &body, "vote update").await?;
    Ok(())
}

/// Return the authenticated user's direct vote on an item's vote issue.
pub async fn get_item_vote(app: &AppHandle, item_id: String) -> LauncherResult<ItemVoteState> {
    let target_app = app.clone();
    let target = tokio::task::spawn_blocking(move || resolve_vote_target(&target_app, &item_id))
        .await
        .map_err(|_| LauncherError::Generic {
            code: "ERR_GOVERNANCE_QUERY".to_string(),
            message: "Voting target lookup failed.".to_string(),
        })??;
    let snapshot = fetch_vote_snapshot(app, &target).await?;
    Ok(state_from_snapshot(&snapshot))
}

/// Make the authenticated user's direct reaction match the requested vote.
pub async fn set_item_vote(
    app: &AppHandle,
    item_id: String,
    vote: Option<ItemVote>,
) -> LauncherResult<ItemVoteState> {
    let target_app = app.clone();
    let target = tokio::task::spawn_blocking(move || resolve_vote_target(&target_app, &item_id))
        .await
        .map_err(|_| LauncherError::Generic {
            code: "ERR_GOVERNANCE_QUERY".to_string(),
            message: "Voting target lookup failed.".to_string(),
        })??;
    let _vote_guard = VOTE_MUTATION_LOCK.lock().await;
    let snapshot = fetch_vote_snapshot(app, &target).await?;
    let mut applied = Vec::new();
    for operation in plan_vote_mutations(&snapshot, vote) {
        if let Err(error) = apply_vote_mutation(app, &snapshot.issue_node_id, operation).await {
            for completed in applied.into_iter().rev() {
                let inverse = match completed {
                    VoteMutation::Add(direction) => VoteMutation::Remove(direction),
                    VoteMutation::Remove(direction) => VoteMutation::Add(direction),
                };
                let _ = apply_vote_mutation(app, &snapshot.issue_node_id, inverse).await;
            }
            return Err(error);
        }
        applied.push(operation);
    }
    let updated = fetch_vote_snapshot(app, &target).await?;
    Ok(state_from_snapshot(&updated))
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

#[cfg(test)]
mod vote_tests {
    use super::*;

    fn snapshot(upvote: bool, downvote: bool) -> VoteSnapshot {
        VoteSnapshot {
            issue_node_id: "issue-node".to_string(),
            upvote,
            downvote,
        }
    }

    #[test]
    fn conflicted_reactions_are_reported_without_a_selected_vote() {
        assert_eq!(
            state_from_snapshot(&snapshot(true, true)),
            ItemVoteState {
                vote: None,
                conflicted: true,
            }
        );
    }

    #[test]
    fn switching_vote_removes_the_old_reaction_before_adding_the_new_one() {
        assert_eq!(
            plan_vote_mutations(&snapshot(true, false), Some(ItemVote::Downvote)),
            vec![
                VoteMutation::Remove(ItemVote::Upvote),
                VoteMutation::Add(ItemVote::Downvote),
            ]
        );
    }

    #[test]
    fn selecting_a_direction_repairs_conflicted_reactions() {
        assert_eq!(
            plan_vote_mutations(&snapshot(true, true), Some(ItemVote::Upvote)),
            vec![VoteMutation::Remove(ItemVote::Downvote)]
        );
    }

    #[test]
    fn clearing_vote_removes_every_existing_direction() {
        assert_eq!(
            plan_vote_mutations(&snapshot(true, true), None),
            vec![
                VoteMutation::Remove(ItemVote::Upvote),
                VoteMutation::Remove(ItemVote::Downvote),
            ]
        );
    }
}
