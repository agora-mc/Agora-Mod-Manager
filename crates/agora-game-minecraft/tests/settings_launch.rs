#![cfg(any(debug_assertions, feature = "test-support"))]

use agora_core::ctx::Ctx;
use agora_core::db;
use agora_core::network::NetworkPolicy;
use agora_core::network_gate::{self, SettingsGate};
use agora_game_minecraft::launch::{
    DownloadArtifact, Library, LibraryArtifact, LibraryDownloads, VersionDownloads, VersionInfo,
};
use agora_game_minecraft::launch_planner::{self, ResolvedJava, ResolvedLaunchPlan};
use agora_game_minecraft::launch_stage::{FileKind, FileProgress};
use base64::{engine::general_purpose::STANDARD, Engine};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

static SERIAL: Mutex<()> = Mutex::new(());
const HOST: &str = "libraries.minecraft.net";
const CLIENT: &[u8] = b"fixture client bytes";
const LIBRARY: &[u8] = b"fixture library bytes";

// Public, self-signed certificate for the loopback TLS fixture. Its Ed25519 seed is the
// repeated byte below. No real signing key or trust-store changes are needed.
const TLS_CERT: &str = "MIIBfTCCAS+gAwIBAgIUcIgbbdsviAEbk5+AVEjyfUOTJWMwBQYDK2VwMCIxIDAeBgNVBAMMF2xpYnJhcmllcy5taW5lY3JhZnQubmV0MB4XDTI2MTEyNzAwNTcwNloXDTM2MTEyNDAwNTcwNlowIjEgMB4GA1UEAwwXbGlicmFyaWVzLm1pbmVjcmFmdC5uZXQwKjAFBgMrZXADIQAhUvjRm3kdJEUyQuFfLqtst8/6e2pe0wCXlg4GmIHbEqN3MHUwHQYDVR0OBBYEFJ+Avk9/Kk/nwdC5qtJDCddETc/sMB8GA1UdIwQYMBaAFJ+Avk9/Kk/nwdC5qtJDCddETc/sMA8GA1UdEwEB/wQFMAMBAf8wIgYDVR0RBBswGYIXbGlicmFyaWVzLm1pbmVjcmFmdC5uZXQwBQYDK2VwA0EAaBGIisdNnGzk39nO5yml2iKmjVYVOp8AN6eB/KUDqdFkD2j6+tTEmCbot2h+G3yO/yMbJHt3rN948uVxjVfhCw==";

fn tls_server() -> Arc<rustls::ServerConfig> {
    let cert = CertificateDer::from(STANDARD.decode(TLS_CERT).unwrap());
    let mut key = vec![
        0x30, 0x2e, 0x02, 0x01, 0, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
    ];
    key.extend([0x42; 32]);
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key));
    Arc::new(
        rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .unwrap(),
    )
}

fn request_line(stream: impl Read) -> String {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    assert!(reader.read_line(&mut line).unwrap() > 0);
    loop {
        let mut header = String::new();
        assert!(reader.read_line(&mut header).unwrap() > 0);
        if header == "\r\n" {
            return line;
        }
    }
}

fn enable_lockdown(path: &Path) {
    let writer = db::local_state_connection(path).unwrap();
    db::set_setting(
        &writer,
        "network_lockdown_enabled",
        &serde_json::json!(true),
    )
    .unwrap();
}

fn proxy_worker(
    listener: TcpListener,
    stop: mpsc::Receiver<()>,
    redirect: Option<PathBuf>,
) -> Vec<String> {
    let config = tls_server();
    let mut requests = Vec::new();
    loop {
        let mut socket = match listener.accept() {
            Ok((socket, _)) => socket,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if !matches!(
                    stop.recv_timeout(Duration::from_millis(10)),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    return requests;
                }
                continue;
            }
            Err(error) => panic!("loopback proxy accept failed: {error}"),
        };
        socket.set_nonblocking(false).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        assert!(request_line(&mut socket).starts_with(&format!("CONNECT {HOST}:443 ")));
        socket
            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
            .unwrap();

        let mut tls = rustls::StreamOwned::new(
            rustls::ServerConnection::new(config.clone()).unwrap(),
            socket,
        );
        requests.push(request_line(&mut tls));
        if requests.len() == 1 {
            if let Some(path) = &redirect {
                assert!(requests[0].starts_with("GET /client.jar "));
                enable_lockdown(path);
                tls.write_all(b"HTTP/1.1 302 Found\r\nLocation: /redirected.jar\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                tls.flush().unwrap();
                continue;
            }
        }
        let body = if requests.last().unwrap().starts_with("GET /library.jar ") {
            LIBRARY
        } else {
            CLIENT
        };
        write!(
            tls,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        tls.write_all(body).unwrap();
        tls.flush().unwrap();
        if requests.len() == 2 {
            return requests;
        }
    }
}

fn context(root: &Path) -> Ctx {
    let ctx = Ctx::for_testing(root.to_owned());
    db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
    network_gate::install(Arc::new(SettingsGate::for_context(&ctx)));
    ctx
}

fn plan(ctx: &Ctx) -> ResolvedLaunchPlan {
    let root = ctx.paths.root();
    let conn = db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
    ResolvedLaunchPlan {
        instance_id: "fixture".into(),
        version_id: "fixture".into(),
        base_version_id: "fixture".into(),
        loader: None,
        java: ResolvedJava {
            path: root.join("unused-java"),
            major_version: 21,
            required_major_version: 21,
            native_arch: "64".into(),
            incompatible_override: false,
        },
        version: VersionInfo {
            downloads: Some(VersionDownloads {
                client: Some(DownloadArtifact {
                    url: format!("https://{HOST}/client.jar"),
                    sha1: Some(agora_core::download::sha1_hex(CLIENT)),
                    size: Some(CLIENT.len() as i64),
                }),
                ..VersionDownloads::default()
            }),
            libraries: vec![Library {
                name: "test:fixture:1".into(),
                downloads: Some(LibraryDownloads {
                    artifact: Some(LibraryArtifact {
                        path: "test/fixture/library.jar".into(),
                        url: format!("https://{HOST}/library.jar"),
                        sha1: Some(agora_core::download::sha1_hex(LIBRARY)),
                        size: Some(LIBRARY.len() as i64),
                        ..LibraryArtifact::default()
                    }),
                    classifiers: None,
                }),
                ..Library::default()
            }],
            ..VersionInfo::default()
        },
        game_dir: root.join("game"),
        assets_dir: root.join("assets"),
        cache_dir: root.join("cache"),
        network_policy: NetworkPolicy::from_db(&conn),
        adopted_profile: None,
    }
}

async fn exercise_materialization(redirect: bool) {
    let root = tempfile::tempdir().unwrap();
    let ctx = context(root.path());
    let plan = plan(&ctx);
    let cache = plan.cache_dir.clone();
    let db_path = ctx.paths.local_state_db();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let proxy = reqwest::Proxy::all(format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let redirect_db = redirect.then(|| db_path.clone());
    let (finished, stop) = mpsc::channel();
    let worker = std::thread::spawn(move || proxy_worker(listener, stop, redirect_db));
    let pause_hits = AtomicUsize::new(0);
    let after_client = |progress: FileProgress| {
        if progress.kind == FileKind::ClientJar && progress.done == 1 {
            assert_eq!(
                std::fs::read(cache.join("versions/fixture/fixture.jar")).unwrap(),
                CLIENT
            );
            enable_lockdown(&db_path);
            pause_hits.fetch_add(1, Ordering::SeqCst);
        }
    };
    let progress: Option<&agora_game_minecraft::launch_stage::FileProgressFn<'_>> =
        (!redirect).then_some(&after_client);
    let result = launch_planner::materialize_with_test_proxy(plan, progress, proxy).await;
    drop(finished);
    let requests = worker.join().unwrap();

    assert!(
        requests
            .first()
            .is_some_and(|line| line.starts_with("GET /client.jar ")),
        "the initial launch download never reached the fixture: {:?}; {requests:?}",
        result.as_ref().err()
    );
    if !redirect {
        assert_eq!(
            pause_hits.load(Ordering::SeqCst),
            1,
            "the progress seam must pause after the verified first download"
        );
    }
    let conn = db::local_state_connection(&db_path).unwrap();
    assert_eq!(
        db::get_setting(&conn, "network_lockdown_enabled").unwrap(),
        Some(serde_json::json!(true)),
        "the interposed writer must commit Lockdown"
    );
    assert_eq!(
        requests.len(),
        1,
        "a launch request escaped the live gate after Lockdown committed: {requests:?}"
    );
    let error = result.expect_err("the live gate must refuse the remaining launch request");
    assert_eq!(error.code(), "ERR_NETWORK_LOCKDOWN", "{error}");
}

#[test]
fn c3_fix_launch_lockdown_after_the_first_download_refuses_the_second() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(exercise_materialization(false));
}

#[test]
fn c3_fix_launch_lockdown_before_location_refuses_the_redirect() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(exercise_materialization(true));
}

fn assert_installer_refused(loader: &str) {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let root = tempfile::tempdir().unwrap();
    let ctx = context(root.path());
    let executable = fake_java(root.path());
    let started = root.path().join("installer-started");
    let archive = root.path().join("fixture.jar");
    std::fs::write(&archive, b"the stand-in executable ignores this fixture").unwrap();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            use agora_game_minecraft::loader_service::run_installer_for_testing;
            // First prove the fake executable leaves a marker when the spawn path runs.
            let baseline = run_installer_for_testing(&executable, &archive, loader, root.path())
                .await
                .unwrap();
            assert_eq!(baseline, 17);
            assert!(
                started.is_file(),
                "fixture did not observe the healthy spawn"
            );
            std::fs::remove_file(&started).unwrap();
            enable_lockdown(&ctx.paths.local_state_db());
            let result =
                run_installer_for_testing(&executable, &archive, loader, root.path()).await;
            assert!(
                !started.exists(),
                "{loader} installer process started after Lockdown committed: {result:?}"
            );
            let error = result.expect_err(
                "an installer capable of downloading dependencies must not start under Lockdown",
            );
            assert_eq!(error.code(), "ERR_NETWORK_LOCKDOWN", "{error}");
        });
}

#[cfg(windows)]
fn fake_java(root: &Path) -> PathBuf {
    let path = root.join("fake-java.cmd");
    std::fs::write(
        &path,
        b"@echo off\r\necho started>installer-started\r\nexit /b 17\r\n",
    )
    .unwrap();
    path
}

#[cfg(unix)]
fn fake_java(root: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = root.join("fake-java");
    std::fs::write(
        &path,
        b"#!/bin/sh\necho started > installer-started\nexit 17\n",
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path
}

#[test]
fn c3_fix_lockdown_refuses_to_spawn_forge_installer() {
    assert_installer_refused("forge");
}

#[test]
fn c3_fix_lockdown_refuses_to_spawn_neoforge_installer() {
    assert_installer_refused("neoforge");
}
