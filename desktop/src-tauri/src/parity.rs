//! Views the CLI had and the desktop did not: the local database's health (`agora doctor`) and
//! the catalog's governance lists (`agora registry under-review`, `audit-log`, `resolutions`).
//! Thin wrappers over the core functions the CLI calls; `scripts/cli_coverage.json` keeps the two
//! in step.

use agora_core::error::{LauncherError, LauncherResult};
use agora_core::registry::{AuditLogEntry, RegistryService, UnderReviewItem};

fn task_failed(what: &str) -> LauncherError {
    LauncherError::Generic {
        code: "ERR_TASK_FAILED".into(),
        message: format!("{what} did not finish."),
    }
}

/// The local database's integrity, schema version, backups and files kept aside after a
/// recovery, as `agora doctor` reports them. The Microsoft account store's state comes from
/// `msa_account_store_status`.
#[tauri::command]
pub async fn local_state_report(
    app: tauri::AppHandle,
) -> LauncherResult<agora_core::db::LocalStateReport> {
    let ctx = crate::core_context(&app)?;
    tokio::task::spawn_blocking(move || {
        agora_core::db::local_state_report(&ctx.paths.local_state_db())
    })
    .await
    .map_err(|_| task_failed("The database check"))
}

/// Catalog items under review, lowest score first.
#[tauri::command]
pub async fn registry_under_review(app: tauri::AppHandle) -> LauncherResult<Vec<UnderReviewItem>> {
    let ctx = crate::core_context(&app)?;
    tokio::task::spawn_blocking(move || RegistryService::new(ctx).list_under_review_items())
        .await
        .map_err(|_| task_failed("Reading the items under review"))?
}

/// The newest catalog audit log entries: compile runs and governance events.
#[tauri::command]
pub async fn registry_audit_log(
    app: tauri::AppHandle,
    limit: Option<i64>,
) -> LauncherResult<Vec<AuditLogEntry>> {
    let ctx = crate::core_context(&app)?;
    let limit = limit.unwrap_or(50).clamp(1, 500);
    tokio::task::spawn_blocking(move || RegistryService::new(ctx).list_audit_log(limit))
        .await
        .map_err(|_| task_failed("Reading the audit log"))?
}

/// The newest triage resolutions from the audit log.
#[tauri::command]
pub async fn registry_resolutions(
    app: tauri::AppHandle,
    limit: Option<u32>,
) -> LauncherResult<Vec<AuditLogEntry>> {
    let ctx = crate::core_context(&app)?;
    let limit = limit.unwrap_or(20).clamp(1, 200);
    tokio::task::spawn_blocking(move || RegistryService::new(ctx).list_recent_resolutions(limit))
        .await
        .map_err(|_| task_failed("Reading the resolutions"))?
}
