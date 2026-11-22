use super::*;
use agora_core::download::sha512_hex;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

const FABRIC_PROFILE: &str = include_str!("../../../tests/fixtures/fabric-1.21.1-0.20.1.json");
const FABRIC_263_PROFILE: &str = include_str!("../../../tests/fixtures/fabric-26.3-0.20.1.json");
const QUILT_PROFILE: &[u8] = include_bytes!("../../../tests/fixtures/quilt-1.21.1-0.30.1.json");
const QUILT_263_PROFILE: &[u8] =
    include_bytes!("../../../tests/fixtures/quilt-26.3-0.31.0-beta.6.json");
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

/// Which receipt map a successful install should have pinned every library in:
/// `Curated` when a hash the source published was checked, `Generated` when the
/// bytes were accepted only because the user allowed an unhashed download.
#[derive(Clone, Copy)]
enum PinKind {
    Curated,
    Generated,
}

async fn create(profile: Value, reply: MavenReply, allow_unverified: bool) -> LauncherResult<()> {
    create_for(
        "1.21.1",
        "fabric",
        "0.20.1",
        serde_json::to_vec(&profile).unwrap(),
        reply,
        allow_unverified,
        PinKind::Curated,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn create_for(
    mc_version: &str,
    loader: &str,
    loader_version: &str,
    profile: Vec<u8>,
    reply: MavenReply,
    allow_unverified: bool,
    expect_pins: PinKind,
) -> LauncherResult<()> {
    let temp = tempfile::tempdir().unwrap();
    let ctx = crate::testing_context(temp.path().to_owned());
    let manifest_path = ctx.paths.instance_manifest("test-instance").unwrap();
    std::fs::create_dir_all(ctx.paths.instances_root()).unwrap();
    agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
    let base = ctx
        .paths
        .minecraft_runtime_root()
        .join(format!("versions/{mc_version}"));
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(
        base.join(format!("{mc_version}.json")),
        format!(
            r#"{{"id":"{mc_version}","mainClass":"invented.game.Main","type":"release","libraries":[]}}"#
        ),
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
        "minecraft_version": mc_version, "loader": loader, "loader_version": loader_version
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
            minecraft_version: mc_version.into(),
        };
        let receipt = installed_profile::read_receipt(&ctx.paths.loader_receipts(), &tuple)
            .unwrap()
            .unwrap();
        let (pins, others) = match expect_pins {
            PinKind::Curated => (
                &receipt.curated_artifact_sha256,
                &receipt.generated_artifact_sha256,
            ),
            PinKind::Generated => (
                &receipt.generated_artifact_sha256,
                &receipt.curated_artifact_sha256,
            ),
        };
        assert!(!pins.is_empty());
        assert_eq!(Some(pins.len()), library_count);
        assert!(
            others.is_empty(),
            "every library took the path this test is about: {others:?}"
        );
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

/// Publish the digests of the bytes the test server serves for every library.
///
/// The real 1–2 MB Maven jars cannot be fixtures, so the captured profile's
/// digests describe jars the test never sees. A test about the structural,
/// sidecar or receipt paths replaces them with digests of its invented jar;
/// the digests themselves are exercised by
/// [`refuses_bytes_that_contradict_the_sha256_the_profile_publishes`].
fn publish_served_digests(profile: &mut Value, bytes: &[u8]) {
    for library in profile["libraries"].as_array_mut().unwrap() {
        library["sha1"] = json!(sha1_hex(bytes));
        library["sha256"] = json!(sha256_hex(bytes));
        library["sha512"] = json!(sha512_hex(bytes));
        library["size"] = json!(bytes.len());
    }
}

/// A profile from a meta server that published no library digests at all: the
/// shape Quilt's profiles have. The sidecar tests need a library with nothing
/// published, and the captured Fabric profiles do publish digests.
fn without_library_digests(profile: &mut Value) {
    for library in profile["libraries"].as_array_mut().unwrap() {
        for field in ["sha1", "sha256", "sha512", "size"] {
            library.as_object_mut().unwrap().remove(field);
        }
    }
}

/// The captured Fabric profile as the local Maven responder describes it.
///
/// The responder serves [`JAR`] for every library, so a reply that publishes a
/// sidecar needs a profile whose digests describe those bytes. `Missing` is a
/// source that published nothing: its profile carries no digest either, or the
/// test would no longer be about an unhashed download.
pub(super) fn served_fabric_profile(reply: MavenReply) -> Vec<u8> {
    let mut profile = fabric_profile();
    if matches!(reply, MavenReply::Missing) {
        without_library_digests(&mut profile);
    } else {
        publish_served_digests(&mut profile, JAR);
    }
    serde_json::to_vec(&profile).unwrap()
}

fn fabric_profile() -> Value {
    serde_json::from_str(FABRIC_PROFILE).unwrap()
}

fn fabric_263_profile() -> Value {
    serde_json::from_str(FABRIC_263_PROFILE).unwrap()
}

#[tokio::test]
async fn creates_fabric_instance_from_captured_profile() {
    let mut profile = fabric_profile();
    publish_served_digests(&mut profile, JAR);
    create_for(
        "1.21.1",
        "fabric",
        "0.20.1",
        serde_json::to_vec(&profile).unwrap(),
        MavenReply::Match,
        false,
        PinKind::Curated,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn creates_fabric_instance_after_asm_update() {
    let mut profile = fabric_profile();
    profile["libraries"][0]["name"] = json!("org.ow2.asm:asm:9.10.2");
    publish_served_digests(&mut profile, JAR);
    create(profile, MavenReply::Match, false).await.unwrap();
}

/// Every loader at 26.3, from the real generated profile: no mappings artifact
/// (26.1 and later are unobfuscated), and the libraries carry digests.
#[tokio::test]
async fn creates_fabric_instance_from_captured_263_profile() {
    let mut profile = fabric_263_profile();
    assert!(
        !profile["libraries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|library| library["name"]
                .as_str()
                .is_some_and(|name| name.starts_with("net.fabricmc:intermediary"))),
        "an unobfuscated release's profile carries no mappings artifact"
    );
    publish_served_digests(&mut profile, JAR);
    create_for(
        "26.3",
        "fabric",
        "0.20.1",
        serde_json::to_vec(&profile).unwrap(),
        MavenReply::Match,
        false,
        PinKind::Curated,
    )
    .await
    .unwrap();
}

/// The real Quilt 26.3 profile publishes no digest for any library, so a
/// library with no sidecar either is the case the override exists for: the
/// install names the jar and stops, and takes the user's word only when they
/// give it.
#[tokio::test]
async fn creates_quilt_instance_from_captured_263_profile() {
    let refused = create_for(
        "26.3",
        "quilt",
        "0.31.0-beta.6",
        QUILT_263_PROFILE.to_vec(),
        MavenReply::Missing,
        false,
        PinKind::Generated,
    )
    .await
    .unwrap_err();
    assert!(
        refused.to_string().contains("has no published hash"),
        "{refused}"
    );
    assert!(
        refused
            .to_string()
            .contains("sponge-mixin-0.17.4+mixin.0.8.7.jar"),
        "the refusal must name the jar: {refused}"
    );

    create_for(
        "26.3",
        "quilt",
        "0.31.0-beta.6",
        QUILT_263_PROFILE.to_vec(),
        MavenReply::Missing,
        true,
        PinKind::Generated,
    )
    .await
    .unwrap();

    create_for(
        "26.3",
        "quilt",
        "0.31.0-beta.6",
        QUILT_263_PROFILE.to_vec(),
        MavenReply::Match,
        false,
        PinKind::Curated,
    )
    .await
    .unwrap();
}

/// Fabric publishes a SHA-256 on each library, which is the strongest digest
/// the profile itself carries. Bytes that contradict it are refused, and the
/// missing-checksum override does not reach them: it is for a source that
/// published nothing, not for one whose own digest disagrees with its bytes.
#[tokio::test]
async fn refuses_bytes_that_contradict_the_sha256_the_profile_publishes() {
    // The captured profile's digests describe the real jars; the server serves
    // this test's invented bytes and no sidecar.
    for allow_unverified in [false, true] {
        let error = create_for(
            "26.3",
            "fabric",
            "0.20.1",
            serde_json::to_vec(&fabric_263_profile()).unwrap(),
            MavenReply::Missing,
            allow_unverified,
            PinKind::Curated,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error.code().as_str(), "ERR_HASH_MISMATCH"),
            "an override cannot accept bytes its own profile contradicts: {error:?}"
        );
        assert!(
            error.to_string().contains("SHA-256")
                && error.to_string().contains("maven.fabricmc.net"),
            "the refusal names the digest kind and the jar: {error}"
        );
    }
}

/// A library may publish its digests in either place: on the library entry
/// (Fabric's profile does) or on `downloads.artifact`. A profile that uses
/// both must not lose the one that is not on the artifact — reading only the
/// nested copy treats the library as unhashed and installs bytes the profile
/// contradicts once the override is given.
#[tokio::test]
async fn a_library_with_digests_in_both_places_keeps_all_of_them() {
    let mut profile = fabric_263_profile();
    let coordinate = "org.ow2.asm:asm:9.10.1";
    let path = "org/ow2/asm/asm/9.10.1/asm-9.10.1.jar";
    let published = profile["libraries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|library| library["name"] == coordinate)
        .and_then(|library| library["sha256"].as_str())
        .unwrap_or_else(|| panic!("the captured profile publishes {coordinate}'s SHA-256"))
        .to_string();
    // Every other library publishes nothing, so the mixed one is the only
    // library this install can refuse on.
    without_library_digests(&mut profile);
    let library = profile["libraries"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|library| library["name"] == coordinate)
        .unwrap_or_else(|| panic!("the captured profile lists {coordinate}"));
    library["sha256"] = json!(published);
    // The same library, now also naming its artifact without digests there.
    library["downloads"] = json!({
        "artifact": {
            "path": path,
            "url": format!("https://maven.fabricmc.net/{path}")
        }
    });

    let error = create_for(
        "26.3",
        "fabric",
        "0.20.1",
        serde_json::to_vec(&profile).unwrap(),
        MavenReply::Missing,
        true,
        PinKind::Curated,
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.code(),
        "ERR_HASH_MISMATCH",
        "the library entry's digest still describes the download: {error:?}"
    );
    assert!(
        error.to_string().contains("SHA-256") && error.to_string().contains("asm-9.10.1.jar"),
        "the refusal names the digest kind and the jar: {error}"
    );
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
    // A source that published nothing: no sidecar, and a profile without the
    // library's own digests (which the captured Fabric profiles do carry).
    let mut profile = fabric_profile();
    without_library_digests(&mut profile);
    let missing = create(profile.clone(), MavenReply::Missing, false)
        .await
        .unwrap_err();
    assert!(
        missing.to_string().contains("no published hash"),
        "{missing}"
    );
    create_for(
        "1.21.1",
        "fabric",
        "0.20.1",
        serde_json::to_vec(&profile).unwrap(),
        MavenReply::Missing,
        true,
        PinKind::Generated,
    )
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
    let mut profile = fabric_profile();
    without_library_digests(&mut profile);
    for reply in [MavenReply::Sha256, MavenReply::Sha1] {
        create(profile.clone(), reply, false).await.unwrap();
    }
}

#[tokio::test]
async fn creates_quilt_instance_from_captured_profile() {
    create_for(
        "1.21.1",
        "quilt",
        "0.30.1",
        QUILT_PROFILE.to_vec(),
        MavenReply::Match,
        false,
        PinKind::Curated,
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
            "1.21.1",
            "quilt",
            "0.30.1",
            serde_json::to_vec(&profile).unwrap(),
            MavenReply::Match,
            true,
            PinKind::Curated,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains(rule), "{coordinate}: {err}");
    }
}
