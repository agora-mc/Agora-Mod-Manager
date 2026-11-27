//! This test has its own process because the current HTTP gate is installed globally.

use agora_core::ctx::Ctx;
use agora_core::db;
use agora_core::http_client::{self, ClientCategory, HostPolicy, HttpClients};
use agora_core::network_gate::SettingsGate;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

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

#[tokio::test]
async fn lockdown_committed_after_the_first_request_blocks_the_redirect() {
    let root = tempfile::tempdir().unwrap();
    let ctx = Ctx::for_testing(root.path().to_owned());
    let db_path = ctx.paths.local_state_db();
    db::init_local_state_db(&db_path).unwrap();
    let gate: std::sync::Arc<dyn agora_core::network_gate::NetworkGate> =
        Arc::new(SettingsGate::for_context(&ctx));

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let proxy = reqwest::Proxy::all(format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let client = reqwest::Client::builder()
        .proxy(proxy)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let dns_calls = Arc::new(AtomicUsize::new(0));
    let clients = HttpClients::for_testing(client)
        .with_gate(gate)
        .with_test_dns({
            let dns_calls = Arc::clone(&dns_calls);
            Arc::new(move |url| {
                assert_eq!(url.host_str(), Some("settings-cache.invalid"));
                dns_calls.fetch_add(1, Ordering::SeqCst);
                // Used only by URL policy. Transport connects exclusively to the local proxy.
                Ok(vec!["93.184.216.34:80".parse().unwrap()])
            })
        });

    let (finished, stop) = mpsc::channel::<()>();
    let server = std::thread::spawn(move || {
        let mut requests = Vec::new();
        loop {
            let mut stream = match listener.accept() {
                // On Windows an accepted socket inherits the listener's non-blocking mode; the
                // request read below needs a blocking one.
                Ok((stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    stream
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if !matches!(
                        stop.recv_timeout(Duration::from_millis(10)),
                        Err(mpsc::RecvTimeoutError::Timeout)
                    ) {
                        return requests;
                    }
                    continue;
                }
                Err(error) => panic!("proxy could not accept a connection: {error}"),
            };
            requests.push(read_request(&mut stream));
            if requests.len() == 1 {
                assert!(requests[0].starts_with("GET http://settings-cache.invalid/start "));
                // No Location reaches the client until the write has committed.
                let conn = db::local_state_connection(&db_path).unwrap();
                db::set_setting(&conn, "network_lockdown_enabled", &serde_json::json!(true))
                    .unwrap();
                stream.write_all(b"HTTP/1.1 302 Found\r\nLocation: /follow\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            } else {
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    )
                    .unwrap();
                return requests;
            }
        }
    });

    let result = http_client::checked_get_bytes_with_policy(
        &clients,
        ClientCategory::ConsentedContent,
        "http://settings-cache.invalid/start",
        HostPolicy::UserConsented,
    )
    .await;
    drop(finished);
    let requests = server.join().unwrap();
    assert!(
        dns_calls.load(Ordering::SeqCst) > 0,
        "test DNS hook never fired"
    );
    assert!(
        !requests.is_empty(),
        "proxy never received the initial request: {result:?}"
    );
    assert_eq!(
        requests.len(),
        1,
        "a request reached the proxy after Lockdown committed: {requests:?}"
    );
    let denied = result.expect_err("Lockdown must deny the redirect");
    assert_eq!(denied.code(), "ERR_NETWORK_LOCKDOWN", "{denied}");
}
