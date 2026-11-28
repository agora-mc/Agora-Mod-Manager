//! The outbound gate belongs to the context, not to the process.
//!
//! `cargo test` runs many tests in one process, so a gate held in one process-wide slot is
//! whichever context installed last: a test that turns Lockdown on, and another that builds a
//! permissive testing context, answer for each other. Production makes one context per process,
//! which hides the problem — but a second context in one process must still answer by its own
//! settings, or the gate is not really reading the user's database.

use agora_core::ctx::Ctx;
use agora_core::db;
use agora_core::http_client::{self, ClientCategory};
use agora_core::network_gate::SettingsGate;
use serde_json::json;
use std::sync::Arc;

/// A context whose clients ask their own settings gate, with Lockdown as given.
fn context(lockdown: bool) -> (tempfile::TempDir, Ctx) {
    let root = tempfile::tempdir().unwrap();
    let ctx = Ctx::for_testing(root.path().to_owned());
    db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
    let conn = db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
    db::set_setting(&conn, "network_lockdown_enabled", &json!(lockdown)).unwrap();
    let gate = Arc::new(SettingsGate::for_context(&ctx));
    (root, ctx.with_network_gate(gate))
}

/// The code a request in this context is refused with, or `None` when the gate let it through.
async fn refusal(ctx: &Ctx) -> Option<String> {
    let error = http_client::checked_get_bytes(
        &ctx.http_clients,
        ClientCategory::GitHub,
        "https://example.invalid/",
    )
    .await
    .err()?;
    let code = error.code().to_string();
    // Anything the gate refuses carries its own code; anything it lets through fails later, at URL
    // validation or on the wire, and that is not a gate refusal.
    code.starts_with("ERR_NETWORK_").then_some(code)
}

async fn assert_each_context_answers_for_itself(locked: &Ctx, open: &Ctx) {
    assert_eq!(
        refusal(locked).await.as_deref(),
        Some("ERR_NETWORK_LOCKDOWN"),
        "the context with Lockdown on must refuse its own requests"
    );
    assert_ne!(
        refusal(open).await.as_deref(),
        Some("ERR_NETWORK_LOCKDOWN"),
        "the context with Lockdown off must not be refused by its neighbour's settings"
    );
    // And again, in the other order, so neither answer depends on which ran first.
    assert_ne!(
        refusal(open).await.as_deref(),
        Some("ERR_NETWORK_LOCKDOWN"),
        "a request from the open context must not change the locked one's answer"
    );
    assert_eq!(
        refusal(locked).await.as_deref(),
        Some("ERR_NETWORK_LOCKDOWN"),
        "the locked context must still be locked after the other one made a request"
    );
}

#[tokio::test]
async fn a_locked_context_created_first_answers_by_its_own_settings() {
    let (_locked_root, locked) = context(true);
    let (_open_root, open) = context(false);
    assert_each_context_answers_for_itself(&locked, &open).await;
}

#[tokio::test]
async fn a_locked_context_created_last_answers_by_its_own_settings() {
    let (_open_root, open) = context(false);
    let (_locked_root, locked) = context(true);
    assert_each_context_answers_for_itself(&locked, &open).await;
}
