//! Turning a plugin on brings the plugins it depends on with it, and a plugin another one needs
//! cannot be turned off on its own. Uses the two real official plugins, as a player installs them.

use std::path::Path;
use std::sync::Arc;

use agora_core::ctx::CoreContext;
use agora_core::plugins::{NoopUiSink, PluginService};
use agora_plugin_api::manifest::PluginId;
use agora_plugin_host::QuickJsHost;
use tempfile::TempDir;

fn service() -> (TempDir, PluginService) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = CoreContext::for_testing(dir.path().to_path_buf());
    ctx.paths.create_required_dirs().unwrap();
    agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
    let service = PluginService::new(ctx, Arc::new(QuickJsHost::new()), Arc::new(NoopUiSink));
    (dir, service)
}

fn id(raw: &str) -> PluginId {
    PluginId::parse(raw).unwrap()
}

fn official(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/official")
        .join(name)
}

fn install_both(service: &PluginService) {
    service
        .add_development_folder(&official("creation-engine"), true)
        .expect("the base installs");
    service
        .add_development_folder(&official("skyrim-se"), true)
        .expect("Skyrim installs");
}

fn enabled(service: &PluginService, plugin: &PluginId) -> bool {
    service
        .list()
        .into_iter()
        .find(|summary| summary.id == plugin.as_str())
        .map(|summary| summary.enabled)
        .unwrap_or(false)
}

#[test]
fn enabling_skyrim_brings_the_creation_engine_base() {
    let (_dir, service) = service();
    install_both(&service);
    let base = id("agora.creation-engine");
    let skyrim = id("agora.skyrim-se");

    // Start from both off, turning Skyrim off first because it needs the base.
    service.set_enabled(&skyrim, false).unwrap();
    service.set_enabled(&base, false).unwrap();
    assert!(!enabled(&service, &base) && !enabled(&service, &skyrim));

    service.set_enabled(&skyrim, true).unwrap();
    assert!(enabled(&service, &skyrim));
    assert!(
        enabled(&service, &base),
        "the base came along with the plugin that needs it"
    );
}

#[test]
fn the_base_cannot_be_disabled_while_skyrim_needs_it() {
    let (_dir, service) = service();
    install_both(&service);
    let base = id("agora.creation-engine");
    let skyrim = id("agora.skyrim-se");

    let error = service
        .set_enabled(&base, false)
        .expect_err("turning the base off under Skyrim must be refused");
    let message = error.to_string();
    assert!(
        message.contains("Skyrim Special Edition") && message.contains("Turn"),
        "{message}"
    );
    assert!(
        enabled(&service, &base),
        "the refused change changed nothing"
    );
    assert!(enabled(&service, &skyrim));

    // Turning Skyrim off first makes the base turnable off.
    service.set_enabled(&skyrim, false).unwrap();
    service.set_enabled(&base, false).unwrap();
    assert!(!enabled(&service, &base));
}
