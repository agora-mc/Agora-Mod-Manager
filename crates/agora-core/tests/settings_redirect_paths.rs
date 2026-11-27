//! Exercise the four checked redirect paths against a loopback HTTP proxy.

use agora_core::ctx::Ctx;
use agora_core::db;
use agora_core::error::LauncherResult;
use agora_core::http_client::{self, ClientCategory, HostPolicy, HttpClients, ProxySetting};
use agora_core::network_gate::{self, SettingsGate};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

// The existing blocking proxy and gate are process-wide. Keep the fixtures in one binary and
// serialize their use of those resources.
static SERIAL: Mutex<()> = Mutex::new(());
const START: &str = "http://first.settings-cache.invalid/start";
const FOLLOW: &str = "http://second.settings-cache.invalid/follow";
const BODY: &[u8] = b"original body";
const AUTH: &str = "Bearer fixture-token";

#[derive(Debug)]
struct Request {
    line: String,
    headers: Vec<String>,
    body: Vec<u8>,
}

impl Request {
    fn authorization(&self) -> Option<&str> {
        self.headers.iter().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("authorization")
                .then(|| value.trim())
        })
    }
}

fn read_request(stream: &mut TcpStream) -> Request {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    assert!(
        reader.read_line(&mut line).unwrap() > 0,
        "expected a request line"
    );
    let mut headers = Vec::new();
    let mut length = 0;
    loop {
        let mut header = String::new();
        assert!(
            reader.read_line(&mut header).unwrap() > 0,
            "incomplete proxy request"
        );
        if header == "\r\n" {
            break;
        }
        let (name, value) = header.split_once(':').unwrap();
        if name.eq_ignore_ascii_case("content-length") {
            length = value.trim().parse().unwrap();
        }
        headers.push(header);
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    Request {
        line,
        headers,
        body,
    }
}

fn serve(
    listener: TcpListener,
    stop: mpsc::Receiver<()>,
    lockdown: Option<PathBuf>,
    status: u16,
) -> Vec<Request> {
    let mut requests = Vec::new();
    loop {
        let mut stream = match listener.accept() {
            Ok((stream, _)) => stream,
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
            if let Some(path) = &lockdown {
                let conn = db::local_state_connection(path).unwrap();
                db::set_setting(&conn, "network_lockdown_enabled", &serde_json::json!(true))
                    .unwrap();
            }
            // The transport cannot authorize the hop before this response is sent.
            write!(stream, "HTTP/1.1 {status} Redirect\r\nLocation: {FOLLOW}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        } else {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .unwrap();
            return requests;
        }
    }
}

#[derive(Clone, Copy)]
enum Path {
    Get,
    Post,
    AuthenticatedGet,
    BlockingGet,
}

impl Path {
    fn run(self, clients: &HttpClients) -> LauncherResult<reqwest::StatusCode> {
        let category = ClientCategory::ConsentedContent;
        if matches!(self, Self::BlockingGet) {
            return http_client::blocking_checked_request_with_policy(
                clients,
                category,
                START,
                HostPolicy::UserConsented,
            )
            .map(|response| response.status());
        }
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let response = match self {
                    Self::Get => {
                        http_client::checked_request_with_policy(
                            clients,
                            category,
                            START,
                            HostPolicy::UserConsented,
                        )
                        .await?
                    }
                    Self::Post => {
                        http_client::checked_send(
                            clients,
                            category,
                            reqwest::Method::POST,
                            START,
                            &[],
                            Some(BODY.to_vec()),
                            Some("text/plain"),
                        )
                        .await?
                    }
                    Self::AuthenticatedGet => {
                        http_client::checked_request_with_headers(
                            clients,
                            category,
                            START,
                            vec![("Authorization".into(), AUTH.into())],
                        )
                        .await?
                    }
                    Self::BlockingGet => unreachable!(),
                };
                Ok(response.status())
            })
    }
}

fn exchange(
    path: Path,
    lockdown: bool,
    redirect_status: u16,
) -> (LauncherResult<reqwest::StatusCode>, Vec<Request>) {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let root = tempfile::tempdir().unwrap();
    let ctx = Ctx::for_testing(root.path().to_owned());
    let db_path = ctx.paths.local_state_db();
    db::init_local_state_db(&db_path).unwrap();
    network_gate::install(Arc::new(SettingsGate::for_context(&ctx)));

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    http_client::configure_proxy(ProxySetting::Manual {
        url: proxy.clone(),
        username: None,
    });
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(proxy).unwrap())
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let looked_up = Arc::new(AtomicUsize::new(0));
    let clients = HttpClients::for_testing(client)
        .with_test_host_policy(HostPolicy::UserConsented)
        .with_test_dns({
            let looked_up = looked_up.clone();
            Arc::new(move |url| {
                assert!(matches!(
                    url.host_str(),
                    Some("first.settings-cache.invalid" | "second.settings-cache.invalid")
                ));
                looked_up.fetch_add(1, Ordering::SeqCst);
                Ok(vec!["93.184.216.34:80".parse().unwrap()])
            })
        });
    let (finished, stop) = mpsc::channel();
    let server = std::thread::spawn(move || {
        serve(listener, stop, lockdown.then_some(db_path), redirect_status)
    });
    let result = path.run(&clients);
    drop(finished);
    let requests = server.join().unwrap();
    assert!(
        looked_up.load(Ordering::SeqCst) > 0,
        "the test DNS hook did not run"
    );
    assert!(
        !requests.is_empty(),
        "the initial request did not reach the proxy: {result:?}"
    );
    (result, requests)
}

fn assert_lockdown_blocks_hop(path: Path) {
    let (result, requests) = exchange(path, true, 307);
    assert_eq!(
        requests.len(),
        1,
        "request escaped after Lockdown committed: {requests:?}"
    );
    assert!(requests[0].line.contains(START));
    let deny = result.expect_err("the redirect must return a Lockdown denial");
    assert_eq!(deny.code(), "ERR_NETWORK_LOCKDOWN", "{deny}");
}

#[test]
fn c3_fix_async_get_rechecks_lockdown_before_following_a_redirect() {
    assert_lockdown_blocks_hop(Path::Get);
}

#[test]
fn c3_fix_post_rechecks_lockdown_before_following_a_redirect() {
    assert_lockdown_blocks_hop(Path::Post);
}

#[test]
fn c3_fix_authenticated_get_rechecks_lockdown_before_following_a_redirect() {
    assert_lockdown_blocks_hop(Path::AuthenticatedGet);
}

#[test]
fn c3_fix_blocking_get_rechecks_lockdown_before_following_a_redirect() {
    assert_lockdown_blocks_hop(Path::BlockingGet);
}

#[test]
fn c3_fix_a_307_or_308_replays_the_original_post_body() {
    for status in [307, 308] {
        let (result, requests) = exchange(Path::Post, false, status);
        assert_eq!(result.unwrap(), reqwest::StatusCode::OK);
        assert_eq!(requests.len(), 2);
        for (request, url) in requests.iter().zip([START, FOLLOW]) {
            assert!(
                request.line.starts_with(&format!("POST {url} ")),
                "{request:?}"
            );
            assert_eq!(request.body, BODY, "{request:?}");
        }
    }
}

#[test]
fn c3_fix_authenticated_get_strips_authorization_on_a_cross_origin_hop() {
    let (result, requests) = exchange(Path::AuthenticatedGet, false, 302);
    assert_eq!(result.unwrap(), reqwest::StatusCode::OK);
    assert_eq!(requests.len(), 2);
    assert!(requests[0].line.contains(START) && requests[1].line.contains(FOLLOW));
    assert_eq!(requests[0].authorization(), Some(AUTH));
    assert_eq!(requests[1].authorization(), None, "{:?}", requests[1]);
}
