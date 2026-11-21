use super::*;
use agora_core::download::sha512_hex;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

const FABRIC_PROFILE: &str = include_str!("../../../tests/fixtures/fabric-1.21.1-0.20.1.json");
const QUILT_PROFILE: &[u8] = include_bytes!("../../../tests/fixtures/quilt-1.21.1-0.30.1.json");
const JAR: &[u8] = b"invented library bytes";

#[derive(Clone, Copy)]
pub(super) enum MavenReply {
    Match,
    Mismatch,
    Missing,
    Sha256,
    Sha1,
    Malformed,
}

// Read one HTTP request without waiting for EOF, then close the connection.
pub(super) async fn respond(mut socket: TcpStream, profile: &[u8], reply: MavenReply) {
    let mut request = String::new();
    let mut reader = BufReader::new(&mut socket);
    reader.read_line(&mut request).await.unwrap();
    let path = request.split_whitespace().nth(1).unwrap();
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).await.unwrap() == 0 || header == "\r\n" {
            break;
        }
    }
    let (extension, checksum) = match reply {
        MavenReply::Mismatch => (".jar.sha512", sha512_hex(b"another library")),
        MavenReply::Sha256 => (".jar.sha256", sha256_hex(JAR)),
        MavenReply::Sha1 => (".jar.sha1", sha1_hex(JAR)),
        MavenReply::Malformed => (".jar.sha512", "invalid checksum".into()),
        _ => (".jar.sha512", sha512_hex(JAR)),
    };
    let good_sha256 = sha256_hex(JAR);
    let body = if path.ends_with("/profile/json") {
        Some(profile)
    } else if path.ends_with(".jar") {
        Some(JAR)
    } else if path.ends_with(extension) && !matches!(reply, MavenReply::Missing) {
        Some(checksum.as_bytes())
    } else if path.ends_with(".jar.sha256")
        && matches!(reply, MavenReply::Mismatch | MavenReply::Malformed)
    {
        Some(good_sha256.as_bytes())
    } else {
        None
    };
    let (status, body) = body.map_or(("404 Not Found", &b""[..]), |b| ("200 OK", b));
    let headers = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    socket.write_all(headers.as_bytes()).await.unwrap();
    socket.write_all(body).await.unwrap();
}

async fn create(profile: Value, reply: MavenReply, allow_unverified: bool) -> LauncherResult<()> {
    create_for(
        "fabric",
        "0.20.1",
        serde_json::to_vec(&profile).unwrap(),
        reply,
        allow_unverified,
    )
    .await
}

async fn create_for(
    loader: &str,
    loader_version: &str,
    profile: Vec<u8>,
    reply: MavenReply,
    allow_unverified: bool,
) -> LauncherResult<()> {
    let temp = tempfile::tempdir().unwrap();
    let ctx = crate::testing_context(temp.path().to_owned());
    let manifest_path = ctx.paths.instance_manifest("test-instance").unwrap();
    std::fs::create_dir_all(ctx.paths.instances_root()).unwrap();
    agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
    let base = ctx.paths.minecraft_runtime_root().join("versions/1.21.1");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(
        base.join("1.21.1.json"),
        br#"{"id":"1.21.1","mainClass":"invented.game.Main","type":"release","libraries":[]}"#,
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let library_count = serde_json::from_slice::<Value>(&profile)
        .ok()
        .and_then(|value| value["libraries"].as_array().map(Vec::len));
    let server = tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            respond(socket, &profile, reply).await;
        }
    });
    let request = serde_json::from_value(json!({
        "name": "Test Instance", "instance_id": "test-instance",
        "minecraft_version": "1.21.1", "loader": loader, "loader_version": loader_version
    }))
    .unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let service = crate::instance_service::InstanceService::new(ctx.clone());
    let result = crate::loader_download::TEST_LOADER_ORIGIN
        .scope((client, origin), async {
            if allow_unverified {
                service.create_with_unverified_loader(request, true).await
            } else {
                service.create(request).await
            }
        })
        .await;
    server.abort();
    let _ = server.await;
    if result.is_ok() {
        assert!(manifest_path.is_file());
        let root = ctx.paths.minecraft_runtime_root();
        let tuple = LoaderTuple {
            loader: loader.into(),
            loader_version: loader_version.into(),
            minecraft_version: "1.21.1".into(),
        };
        let receipt = installed_profile::read_receipt(&ctx.paths.loader_receipts(), &tuple)
            .unwrap()
            .unwrap();
        let pins = if matches!(reply, MavenReply::Missing) {
            &receipt.generated_artifact_sha256
        } else {
            &receipt.curated_artifact_sha256
        };
        assert!(!pins.is_empty());
        assert_eq!(Some(pins.len()), library_count);
        for (path, sha256) in pins {
            assert_eq!(
                std::fs::read(root.join("libraries").join(path)).unwrap(),
                JAR
            );
            assert_eq!(sha256, &sha256_hex(JAR));
        }
    } else {
        assert!(!manifest_path.exists());
    }
    result.map(|_| ())
}

fn fabric_profile() -> Value {
    serde_json::from_str(FABRIC_PROFILE).unwrap()
}

#[tokio::test]
async fn creates_fabric_instance_from_captured_profile() {
    create_for(
        "fabric",
        "0.20.1",
        FABRIC_PROFILE.as_bytes().to_vec(),
        MavenReply::Match,
        false,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn creates_fabric_instance_after_asm_update() {
    let mut profile = fabric_profile();
    profile["libraries"][0]["name"] = json!("org.ow2.asm:asm:9.10.2");
    create(profile, MavenReply::Match, false).await.unwrap();
}

#[tokio::test]
async fn refuses_invalid_fabric_structure() {
    let mut host = fabric_profile();
    host["libraries"][0]["url"] = json!("https://invented.invalid/");
    let mut jvm = fabric_profile();
    jvm["arguments"]["jvm"]
        .as_array_mut()
        .unwrap()
        .push(json!("-Dunexpected=true"));
    let mut diagnostic = fabric_profile();
    diagnostic["arguments"]["jvm"][1] = json!("-XX:+UseBASE64Intrinsics");
    let mut class = fabric_profile();
    class["mainClass"] = json!("invented.loader.Main");
    let mut loader = fabric_profile();
    loader["libraries"][7]["name"] = json!("net.fabricmc:fabric-loader:0.20.0");
    let mut mappings = fabric_profile();
    mappings["libraries"][6]["name"] = json!("net.fabricmc:intermediary:1.21.2");
    let mut id = fabric_profile();
    id["id"] = json!("fabric-loader-0.20.1-1.21.2");
    let mut parent = fabric_profile();
    parent.as_object_mut().unwrap().remove("inheritsFrom");
    let mut game = fabric_profile();
    game["arguments"]["game"] = json!(["--gameDir"]);
    for (profile, rule) in [
        (host, "host"),
        (jvm, "arguments.jvm"),
        (diagnostic, "arguments.jvm"),
        (class, "mainClass"),
        (loader, "loader artifact"),
        (mappings, "mappings artifact"),
        (id, "id must"),
        (parent, "inheritsFrom"),
        (game, "arguments.game"),
    ] {
        let error = create(profile, MavenReply::Match, true).await.unwrap_err();
        assert!(error.to_string().contains(rule), "{error}");
    }
}

#[tokio::test]
async fn rejects_strongest_published_checksum_on_mismatch() {
    let mismatch = create(fabric_profile(), MavenReply::Mismatch, true)
        .await
        .unwrap_err();
    assert!(
        mismatch.to_string().contains("SHA-512 mismatch"),
        "{mismatch}"
    );
}

#[tokio::test]
async fn missing_checksums_require_an_explicit_override() {
    let missing = create(fabric_profile(), MavenReply::Missing, false)
        .await
        .unwrap_err();
    assert!(
        missing.to_string().contains("no published hash"),
        "{missing}"
    );
    create(fabric_profile(), MavenReply::Missing, true)
        .await
        .unwrap();
}

#[tokio::test]
async fn refuses_malformed_present_checksum() {
    let malformed = create(fabric_profile(), MavenReply::Malformed, true)
        .await
        .unwrap_err();
    assert!(
        malformed.to_string().contains("Invalid published SHA-512"),
        "{malformed}"
    );
}

#[tokio::test]
async fn uses_weaker_published_checksums_only_on_not_found() {
    for reply in [MavenReply::Sha256, MavenReply::Sha1] {
        create(fabric_profile(), reply, false).await.unwrap();
    }
}

#[tokio::test]
async fn creates_quilt_instance_from_captured_profile() {
    create_for(
        "quilt",
        "0.30.1",
        QUILT_PROFILE.to_vec(),
        MavenReply::Match,
        false,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn refuses_invalid_quilt_libraries() {
    for (coordinate, edit, rule) in [
        (
            "org.quiltmc:hashed:1.21.1",
            Some(("name", "org.quiltmc:hashed:1.21")),
            "mappings artifact",
        ),
        (
            "net.fabricmc:intermediary:1.21.1",
            Some(("name", "net.fabricmc:intermediary:1.21")),
            "mappings artifact",
        ),
        (
            "org.quiltmc:hashed:1.21.1",
            None,
            "Missing mappings artifact",
        ),
        (
            "org.quiltmc:quilt-loader:0.30.1",
            Some(("name", "org.quiltmc:quilt-loader:0.30.0")),
            "loader artifact",
        ),
        (
            "org.quiltmc:quilt-config:1.3.3",
            Some(("url", "https://invented.invalid/")),
            "host allowlist",
        ),
    ] {
        let mut profile: Value = serde_json::from_slice(QUILT_PROFILE).unwrap();
        let libraries = profile["libraries"].as_array_mut().unwrap();
        let index = libraries
            .iter()
            .position(|library| library["name"] == coordinate)
            .unwrap();
        if let Some((field, value)) = edit {
            libraries[index][field] = json!(value);
        } else {
            libraries.remove(index);
        }
        let err = create_for(
            "quilt",
            "0.30.1",
            serde_json::to_vec(&profile).unwrap(),
            MavenReply::Match,
            true,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains(rule), "{coordinate}: {err}");
    }
}
