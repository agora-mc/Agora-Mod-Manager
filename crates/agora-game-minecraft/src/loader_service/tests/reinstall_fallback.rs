// Tests for the reinstall fallback in `ensure_entry`: when an existing
// loader install fails adoption, Agora reinstalls to heal it, but the
// reason must not be lost — a warning reaches the operation log, and a
// failed reinstall (e.g. offline) reports the original reason.

use super::*;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;

const FABRIC_PROFILE: &str = include_str!("../../../tests/fixtures/fabric-1.21.1-0.20.1.json");

/// The reason adoption gives for the damaged profile written below.
const ADOPT_REASON: &str = "mainClass is empty";

/// Minimal context with a collecting progress sink and a local state DB.
fn test_ctx(root: &std::path::Path) -> (Ctx, Arc<CollectingProgressSink>) {
    let sink = Arc::new(CollectingProgressSink::new());
    let ctx = crate::testing_context(root.to_owned()).with_progress_sink(sink.clone());
    crate::content_bridge::init_local_state(&ctx);
    (ctx, sink)
}

/// Base version JSON the fabric profile inherits from (same minimal shape
/// the generated-profiles tests use).
fn write_base_version(minecraft_root: &std::path::Path) {
    let dir = minecraft_root.join("versions/1.21.1");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("1.21.1.json"),
        br#"{"id":"1.21.1","mainClass":"invented.game.Main","type":"release","libraries":[]}"#,
    )
    .unwrap();
}

/// A pre-existing fabric install whose profile is damaged so adoption
/// refuses it (empty mainClass). No receipt is written.
fn write_damaged_fabric_profile(minecraft_root: &std::path::Path) {
    let mut profile: Value = serde_json::from_str(FABRIC_PROFILE).unwrap();
    let profile_id = profile["id"].as_str().unwrap().to_string();
    assert_eq!(profile_id, "fabric-loader-0.20.1-1.21.1");
    profile["mainClass"] = Value::String(String::new());
    let dir = minecraft_root.join("versions").join(&profile_id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{profile_id}.json")),
        serde_json::to_vec(&profile).unwrap(),
    )
    .unwrap();
}

/// The reason given when an install adopts but is trusted only through
/// Maven libraries without published hashes the user has not opted into.
const UNVERIFIED_REASON: &str =
    "loader has libraries with no published hash and unverified loader libraries are not allowed";

/// Spawns the local Maven responder that serves the intact fabric profile.
async fn spawn_fabric_maven(
    reply: super::generated_profiles::MavenReply,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let profile = super::generated_profiles::served_fabric_profile(reply);
    let server = tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            super::generated_profiles::respond(socket, &profile, reply).await;
        }
    });
    (origin, server)
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
}

fn adopt_warning_messages(sink: &CollectingProgressSink) -> Vec<String> {
    sink.events()
        .into_iter()
        .filter(|event| {
            event
                .message
                .contains("existing install could not be verified:")
        })
        .map(|event| event.message)
        .collect()
}

fn unverified_reinstall_warnings(sink: &CollectingProgressSink) -> Vec<String> {
    sink.events()
        .into_iter()
        .filter(|event| {
            event
                .message
                .contains("unverified loader libraries are not allowed")
        })
        .map(|event| event.message)
        .collect()
}

/// Runs the fabric 0.20.1 install through the loader service.
async fn ensure(
    service: &LoaderService,
    origin: String,
    allow_unverified: bool,
) -> LauncherResult<InstallReceiptSummary> {
    crate::loader_download::TEST_LOADER_ORIGIN
        .scope((http_client(), origin), async {
            service
                .ensure_installed_with_options(
                    "fabric",
                    "1.21.1",
                    "0.20.1",
                    false,
                    allow_unverified,
                )
                .await
        })
        .await
}

/// A damaged install that fails adoption must be healed by reinstalling,
/// and a warning naming the adoption reason must reach the operation log.
#[tokio::test]
async fn adopt_failure_warns_then_reinstalls() {
    let temp = tempfile::tempdir().unwrap();
    let (ctx, sink) = test_ctx(temp.path());
    let minecraft_root = ctx.paths.minecraft_runtime_root();
    write_base_version(&minecraft_root);
    write_damaged_fabric_profile(&minecraft_root);

    let (origin, server) = spawn_fabric_maven(super::generated_profiles::MavenReply::Match).await;
    let service = LoaderService::new(ctx.clone());
    let summary = crate::loader_download::TEST_LOADER_ORIGIN
        .scope((http_client(), origin), async {
            service
                .ensure_installed_with_options("fabric", "1.21.1", "0.20.1", false, false)
                .await
        })
        .await
        .expect("the damaged install must heal by reinstalling");
    server.abort();
    let _ = server.await;

    assert_eq!(summary.tuple.loader, "fabric");
    // The heal really happened: the profile on disk is the intact one.
    let healed = std::fs::read_to_string(
        minecraft_root
            .join("versions/fabric-loader-0.20.1-1.21.1/fabric-loader-0.20.1-1.21.1.json"),
    )
    .unwrap();
    assert!(
        healed.contains("net.fabricmc.loader.impl.launch.knot.KnotClient"),
        "the reinstalled profile must replace the damaged one"
    );

    let warnings = adopt_warning_messages(&sink);
    assert_eq!(
        warnings.len(),
        1,
        "exactly one adoption-failure warning must be recorded, got: {:?}",
        sink.events()
            .into_iter()
            .map(|event| event.message)
            .collect::<Vec<_>>()
    );
    let warning = &warnings[0];
    assert!(
        warning.contains(&format!(
            "existing install could not be verified: {ADOPT_REASON}"
        )),
        "the warning must name the adoption reason, got: {warning}"
    );
    assert!(
        warning.contains("reinstalling"),
        "the warning must say a reinstall will follow, got: {warning}"
    );
}

/// When the heal reinstall fails (here: the loader Maven origin is
/// unreachable), the error must include the original adoption reason, not
/// only the reinstall failure.
#[tokio::test]
async fn failed_reinstall_error_carries_adopt_reason() {
    let temp = tempfile::tempdir().unwrap();
    let (ctx, sink) = test_ctx(temp.path());
    let minecraft_root = ctx.paths.minecraft_runtime_root();
    write_base_version(&minecraft_root);
    write_damaged_fabric_profile(&minecraft_root);

    // Reserve then release a port so the loader origin is unreachable.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);

    let service = LoaderService::new(ctx.clone());
    let error = crate::loader_download::TEST_LOADER_ORIGIN
        .scope((http_client(), origin), async {
            service
                .ensure_installed_with_options("fabric", "1.21.1", "0.20.1", false, false)
                .await
        })
        .await
        .expect_err("the offline reinstall must fail");

    let message = error.to_string();
    assert!(
        message.contains(&format!(
            "existing install could not be verified: {ADOPT_REASON}"
        )),
        "the reinstall error must carry the adoption reason, got: {message}"
    );
    assert!(
        message.contains("; reinstall failed:"),
        "the reinstall error must also name the reinstall failure, got: {message}"
    );
    assert!(
        !adopt_warning_messages(&sink).is_empty(),
        "the adoption warning must be recorded even when the reinstall fails"
    );
}

/// An install that adopts but is trusted only through no-hash Maven
/// libraries must warn when the user has not opted in, then heal by
/// reinstalling (here: against an origin that now publishes checksums).
#[tokio::test]
async fn unverified_libraries_without_opt_in_warn_then_reinstall_heals() {
    let temp = tempfile::tempdir().unwrap();
    let (ctx, sink) = test_ctx(temp.path());
    let minecraft_root = ctx.paths.minecraft_runtime_root();
    write_base_version(&minecraft_root);
    let service = LoaderService::new(ctx.clone());

    // Phase 1: fresh install accepting libraries without published hashes.
    let (origin, server) = spawn_fabric_maven(super::generated_profiles::MavenReply::Missing).await;
    ensure(&service, origin, true)
        .await
        .expect("install must succeed with the unverified-loader opt-in");
    server.abort();
    let _ = server.await;

    let tuple = LoaderTuple {
        loader: "fabric".into(),
        minecraft_version: "1.21.1".into(),
        loader_version: "0.20.1".into(),
    };
    let receipt = installed_profile::read_receipt(&ctx.paths.loader_receipts(), &tuple)
        .unwrap()
        .unwrap();
    assert!(
        !receipt.generated_artifact_sha256.is_empty(),
        "the recorded install must carry no-hash libraries to exercise the fallthrough"
    );

    // Phase 2: the same install re-checked without the opt-in must warn and
    // reinstall. The origin now publishes checksums, so the heal verifies.
    let (origin, server) = spawn_fabric_maven(super::generated_profiles::MavenReply::Match).await;
    let summary = ensure(&service, origin, false)
        .await
        .expect("the reinstall must heal the install with published hashes");
    server.abort();
    let _ = server.await;

    assert_eq!(summary.tuple.loader, "fabric");
    // The fresh install (phase 1) had nothing to verify, so no adoption
    // warning may have been recorded for it.
    assert!(
        adopt_warning_messages(&sink).is_empty(),
        "a fresh install must not claim an existing install failed verification, got: {:?}",
        adopt_warning_messages(&sink)
    );
    let warnings = unverified_reinstall_warnings(&sink);
    assert_eq!(
        warnings.len(),
        1,
        "exactly one unverified-libraries warning must be recorded, got: {warnings:?}"
    );
    assert!(
        warnings[0].contains(UNVERIFIED_REASON) && warnings[0].contains("reinstalling"),
        "the warning must name the reason and announce the reinstall, got: {}",
        warnings[0]
    );
}

/// The same fallthrough with an unreachable loader Maven origin: the
/// reinstall failure must carry the unverified-libraries reason.
#[tokio::test]
async fn unverified_libraries_reinstall_failure_carries_reason() {
    let temp = tempfile::tempdir().unwrap();
    let (ctx, sink) = test_ctx(temp.path());
    let minecraft_root = ctx.paths.minecraft_runtime_root();
    write_base_version(&minecraft_root);
    let service = LoaderService::new(ctx.clone());

    // Phase 1: fresh install with the unverified-loader opt-in.
    let (origin, server) = spawn_fabric_maven(super::generated_profiles::MavenReply::Missing).await;
    ensure(&service, origin, true)
        .await
        .expect("install must succeed with the unverified-loader opt-in");
    server.abort();
    let _ = server.await;

    // Reserve then release a port so the loader origin is unreachable.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);

    let error = ensure(&service, origin, false)
        .await
        .expect_err("the offline reinstall must fail");

    let message = error.to_string();
    assert!(
        message.contains(UNVERIFIED_REASON),
        "the reinstall error must carry the unverified-libraries reason, got: {message}"
    );
    assert!(
        message.contains("; reinstall failed:"),
        "the reinstall error must also name the reinstall failure, got: {message}"
    );
    assert!(
        !unverified_reinstall_warnings(&sink).is_empty(),
        "the unverified-libraries warning must be recorded even when the reinstall fails"
    );
    assert!(
        adopt_warning_messages(&sink).is_empty(),
        "a fresh install must not claim an existing install failed verification"
    );
}
