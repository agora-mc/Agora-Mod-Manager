//! Every request path must ask the gate again before it follows a redirect
//! (MASTER_SPEC §32.9).
//!
//! These live in their own integration binary because the gate, and the proxy the blocking path
//! reads, are process-wide. Two of them are end-to-end: a loopback proxy answers the first request,
//! turns Lockdown Mode on, and then expects the redirect to be refused before it is sent. The rest
//! of the paths cannot be driven round a loopback proxy — their URL policy requires an allowlisted
//! HTTPS host, and faking one over TLS is not something a test here can do — so they are held to
//! the same rule structurally: the hop inside each redirect loop must consult the gate.

use agora_core::ctx::Ctx;
use agora_core::db;
use agora_core::http_client::{self, ClientCategory, HostPolicy, HttpClients, ProxySetting};
use agora_core::network_gate::{self, SettingsGate};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

/// The gate and the proxy are process-wide, so these two cases must not run at once: one case
/// installing its gate would answer the other's request. `cargo nextest` gives each test its own
/// process, `cargo test` does not.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn read_request(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(stream);
    let mut request = String::new();
    reader.read_line(&mut request).unwrap();
    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line).unwrap();
        assert!(bytes > 0, "proxy received incomplete headers");
        if line == "\r\n" {
            return request;
        }
    }
}

/// A proxy that answers the first request with a redirect, after committing Lockdown Mode.
///
/// It returns the requests it received: one means the redirect was refused before it was sent.
fn proxy_that_locks_down_after(
    db_path: std::path::PathBuf,
    redirect: &'static str,
) -> (u16, std::thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let handle = std::thread::spawn(move || {
        let mut requests: Vec<String> = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let mut stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    // A second connection is the redirect this test says must not happen, so an
                    // idle listener after the first request ends the loop.
                    if !requests.is_empty() || std::time::Instant::now() > deadline {
                        return requests;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                }
                Err(error) => panic!("proxy could not accept a connection: {error}"),
            };
            // A Windows listener set non-blocking hands its connections the same mode, and reading
            // a request that has not arrived yet would fail rather than wait.
            stream.set_nonblocking(false).unwrap();
            requests.push(read_request(&mut stream));
            if requests.len() == 1 {
                let conn = db::local_state_connection(&db_path).unwrap();
                db::set_setting(&conn, "network_lockdown_enabled", &serde_json::json!(true))
                    .unwrap();
                stream.write_all(redirect.as_bytes()).unwrap();
                continue;
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .unwrap();
            return requests;
        }
    });
    (port, handle)
}

#[test]
fn a_blocking_get_asks_the_gate_again_before_following_a_redirect() {
    let _serial = serial();
    let root = tempfile::tempdir().unwrap();
    let ctx = Ctx::for_testing(root.path().to_owned());
    let db_path = ctx.paths.local_state_db();
    db::init_local_state_db(&db_path).unwrap();
    let (port, server) = proxy_that_locks_down_after(
        db_path.clone(),
        "HTTP/1.1 302 Found\r\nLocation: /follow\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    network_gate::install(Arc::new(SettingsGate::for_context(&ctx)));
    http_client::configure_proxy(ProxySetting::Manual {
        url: format!("http://127.0.0.1:{port}"),
        username: None,
    });

    let clients = HttpClients::for_testing(reqwest::Client::new());
    let result = http_client::blocking_checked_get_bytes_with_policy(
        &clients,
        ClientCategory::ConsentedContent,
        "http://settings-cache.invalid/start",
        HostPolicy::UserConsented,
    );
    let requests = server.join().unwrap();
    assert_eq!(
        requests.len(),
        1,
        "a blocking request reached the proxy after Lockdown committed: {requests:?}"
    );
    let denied = result.expect_err("Lockdown must deny the redirect");
    assert_eq!(denied.code(), "ERR_NETWORK_LOCKDOWN", "{denied}");
}

/// The redirect hop inside each of the four redirect-following loops consults the gate.
///
/// A structural guard, because three of the four paths cannot be driven round a loopback proxy:
/// their URL policy requires an allowlisted HTTPS host, and a test cannot present a trusted
/// certificate for one. Adding a redirect loop without the hop check should fail here rather than
/// leave a request reachable under Lockdown.
#[test]
fn every_redirect_hop_consults_the_network_gate() {
    let _serial = serial();
    let source = include_str!("../src/http_client.rs");
    for function in [
        "checked_request_with_policy",
        "checked_send",
        "checked_request_with_headers",
        "blocking_checked_request_with_policy",
    ] {
        let body = body_of(source, function);
        let calls: Vec<usize> = body
            .match_indices("network_gate::authorize(")
            .map(|(at, _)| at)
            .collect();
        assert_eq!(
            calls.len(),
            2,
            "{function} must ask the gate once for the request and once for each redirect hop; \
             found {} call(s)",
            calls.len()
        );
        let redirection = body
            .find("is_redirection()")
            .unwrap_or_else(|| panic!("{function} no longer inspects the status"));
        assert!(
            calls[1] > redirection,
            "{function} checks the gate only before the first request, so a redirect is followed \
             without asking again"
        );
    }
}

/// The body of a function, found by its name and closed by counting braces.
fn body_of<'a>(source: &'a str, function: &str) -> &'a str {
    let start = source
        .find(&format!("fn {function}("))
        .unwrap_or_else(|| panic!("{function} is gone; this guard needs rewriting, not deleting"));
    let open = start + source[start..].find('{').expect("a body");
    let mut depth = 0usize;
    for (offset, character) in source[open..].char_indices() {
        match character {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &source[open..=open + offset];
                }
            }
            _ => {}
        }
    }
    panic!("{function} has no closing brace");
}
