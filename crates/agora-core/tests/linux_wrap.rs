//! `agora games wrap`: the Steam launch options wrapper, run for real on Linux (MASTER_SPEC
//! §26.5).
//!
//! These tests need unprivileged user namespaces and a Linux file system for the overlay. They are
//! skipped only on a host the `AGORA_LINUX_VIEW_KNOWN_RESTRICTED` environment variable marks as
//! known-restricted, exactly like `linux_view.rs`. A shell script stands in for Proton: the
//! wrapper runs it with Steam's environment, writes into an invented Steam tree, and the session,
//! per-user mounts and crash answer are all checked.
#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agora_core::ctx::CoreContext;
use agora_core::game_base::BaseMode;
use agora_core::game_deploy::add_content;
use agora_core::game_discovery::{DiscoveredInstall, InstallCapabilities};
use agora_core::game_instance::create;
use agora_core::game_registry::{
    GameRegistry, IdentifiedInstall, PackageSource, RuntimeResolution,
};
use agora_core::game_wrap::{self, WrapError, WrapOutcome, PROTON_UNTESTED};
use agora_core::linux_view::KNOWN_RESTRICTED_ENV;
use agora_game_api::crash::{
    BuiltInPack, BuiltInParser, BuiltInParserError, Coverage, CoverageState, Fact, ParserOutput,
    ParserReads, ParserRequest, Producer, CRASH_CONTRACT_VERSION,
};
use agora_game_api::{
    DeploymentStrategy, GameDefinition, GameId, GamePackage, GamePath, InstallId, InstallKind,
    LaunchRecipe, LaunchValue, PackageDefinition, RelPath, RuntimeIdentity, StoreId,
    StoreIdentifier, UserDataLocation, UserFileMapping, UserFileStrategy,
};
use agora_plugin_host::QuickJsHost;
use tempfile::TempDir;

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct TestPackage(PackageDefinition);

impl GamePackage for TestPackage {
    fn definition(&self) -> &PackageDefinition {
        &self.0
    }
}

fn game_id() -> GameId {
    GameId::new("wrap-test-game").unwrap()
}

fn test_definition() -> GameDefinition {
    GameDefinition {
        mo2_game_name: None,
        session: Default::default(),
        id: game_id(),
        extends: None,
        name: "Wrap Test Game".into(),
        import_ids: Default::default(),
        stores: vec![StoreIdentifier {
            store: StoreId::new("steam").unwrap(),
            product: "4242".into(),
        }],
        version_sources: vec![],
        deployment: DeploymentStrategy::VirtualFileSystem,
        content_rules: vec![],
        framework_ids: vec![],
        tool_ids: vec![],
        launch: Some(LaunchRecipe {
            executable: GamePath::Runtime {
                path: RelPath::new("Game.bin").unwrap(),
            },
            arguments: vec![LaunchValue::Literal {
                value: "-test".into(),
            }],
            environment: Default::default(),
            working_directory: GamePath::Runtime {
                path: RelPath::default(),
            },
        }),
        user_files: vec![UserFileMapping {
            source: GamePath::UserData {
                location: UserDataLocation::Documents,
                path: RelPath::new("Saved Games/Wrap Test/config.txt").unwrap(),
            },
            instance_path: RelPath::new("user/config.txt").unwrap(),
            strategy: UserFileStrategy::JournaledSwap,
            stores: vec![StoreId::new("steam").unwrap()],
        }],
        save_paths: vec![],
        linked_archive_patterns: vec![],
        declared_writes: vec![],
        excluded_paths: vec![],
        load_orders: Vec::new(),
        runtime_files: Vec::new(),
        save_location: Vec::new(),
        ini_checks: Vec::new(),
        launch_alternatives: Vec::new(),
        content_layout: None,
        copy_patterns: Vec::new(),
    }
}

fn create_context(tmp: &TempDir, def: &GameDefinition) -> CoreContext {
    let ctx = CoreContext::for_testing(tmp.path().join("app_data"));
    agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
    let package = PackageDefinition {
        id: format!("test.{}", def.id),
        version: semver::Version::new(0, 1, 0),
        api_range: semver::VersionReq::parse(">=0.1, <0.2").unwrap(),
        parents: vec![],
        game_templates: BTreeMap::new(),
        behaviours: None,
        games: vec![def.clone()],
        frameworks: vec![],
        tools: vec![],
    };
    let mut builder = GameRegistry::builder();
    builder
        .add(
            PackageSource::Compiled {
                crate_name: "test".into(),
            },
            Arc::new(TestPackage(package)),
        )
        .expect("register the test package");
    ctx.with_games(Arc::new(builder.build()))
}

fn install_fixture(tmp: &TempDir, def: &GameDefinition) -> IdentifiedInstall {
    let install_dir = tmp.path().join("Steam/steamapps/common/Wrap Test Game");
    std::fs::create_dir_all(&install_dir).unwrap();
    for (name, bytes) in [
        ("Game.bin", b"fake game binary".as_slice()),
        ("base_only.txt", b"base only".as_slice()),
        ("shared.txt", b"base shared".as_slice()),
    ] {
        std::fs::write(install_dir.join(name), bytes).unwrap();
    }
    let store = StoreId::new("steam").unwrap();
    let runtime = RuntimeIdentity {
        game: def.id.clone(),
        store: store.clone(),
        version: "1.0.0".into(),
        build: None,
    };
    let detector = agora_core::game_discovery::volume::VolumeDetector::new();
    let volume = detector.get_volume_info(&install_dir);
    IdentifiedInstall {
        game: def.id.clone(),
        install_id: InstallId::new("steam:4242").unwrap(),
        discovered: DiscoveredInstall {
            store,
            product: "4242".into(),
            name: def.name.clone(),
            kind: InstallKind::BaseGame,
            parent_product: None,
            location: install_dir,
            store_version: Some(runtime.version.clone()),
            store_build: None,
            executables: vec!["Game.bin".into()],
            capabilities: InstallCapabilities {
                executables_readable: true,
                accepts_new_files: true,
                relocatable: true,
            },
            volume,
        },
        add_ons: vec![],
        runtime: RuntimeResolution::Identified {
            runtime,
            source: "executable".into(),
        },
    }
}

fn add_content_folder(ctx: &CoreContext, name: &str, files: &[(&str, &[u8])]) -> String {
    let folder = tempfile::Builder::new()
        .prefix("wrap-test-mod-")
        .tempdir_in("/tmp")
        .unwrap();
    for (rel, bytes) in files {
        let path = folder.path().join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, bytes).unwrap();
    }
    agora_core::content_store::add_folder(ctx, folder.path(), Some(name))
        .unwrap()
        .item()
        .item_id
        .clone()
}

struct Fixture {
    _tmp: TempDir,
    ctx: CoreContext,
    instance_id: String,
    install: PathBuf,
    prefix: PathBuf,
    real_config: PathBuf,
    instance_config: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::Builder::new()
            .prefix("agora-linux-wrap-")
            .tempdir_in("/tmp")
            .expect("a Linux temp folder under /tmp");
        let def = test_definition();
        let ctx = create_context(&tmp, &def);
        let install = install_fixture(&tmp, &def);
        let instance = create(
            &ctx,
            &install,
            &def,
            "Wrap Test",
            None,
            BaseMode::Linked,
            &|_| {},
        )
        .expect("create the instance");
        let item = add_content_folder(
            &ctx,
            "Wrap Mod",
            &[("mod.txt", b"mod output"), ("shared.txt", b"mod shared")],
        );
        add_content(&ctx, &instance.instance_id, &item, None, None).unwrap();

        let prefix = tmp.path().join("Steam/steamapps/compatdata/4242/pfx");
        let user = prefix.join("drive_c/users/steamuser");
        std::fs::create_dir_all(user.join("Documents/Saved Games/Wrap Test")).unwrap();
        let real_config = user.join("Documents/Saved Games/Wrap Test/config.txt");
        std::fs::write(&real_config, b"player original").unwrap();
        let instance_config = ctx
            .paths
            .instance_dir(&instance.instance_id)
            .unwrap()
            .join("user/config.txt");
        std::fs::create_dir_all(instance_config.parent().unwrap()).unwrap();
        std::fs::write(&instance_config, b"instance config").unwrap();

        Self {
            _tmp: tmp,
            ctx,
            instance_id: instance.instance_id,
            install: install.discovered.location,
            prefix,
            real_config,
            instance_config,
        }
    }

    fn crash_log(&self) -> PathBuf {
        self.prefix
            .join("drive_c/users/steamuser/Documents/Saved Games/Wrap Test/crash.log")
    }

    fn pending(&self) {
        game_wrap::write_pending(
            &self.ctx,
            &game_wrap::PendingLaunch {
                instance_id: self.instance_id.clone(),
                game: game_id().as_str().to_string(),
                created_unix_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as i64,
            },
        )
        .unwrap();
    }

    fn live<'a>(
        &'a self,
        packs: &'a [agora_core::crash_packs::LoadedCrashPack],
        built_in: &'a [BuiltInPack],
        host: &'a QuickJsHost,
    ) -> agora_core::crash_live::LivePacks<'a> {
        agora_core::crash_live::LivePacks {
            packs,
            built_in,
            host,
            instances: None,
        }
    }
}

struct CrashParser;

impl BuiltInParser for CrashParser {
    fn parse(
        &self,
        _request: &ParserRequest,
        _reads: &dyn ParserReads,
    ) -> Result<ParserOutput, BuiltInParserError> {
        Ok(ParserOutput {
            contract: CRASH_CONTRACT_VERSION,
            producer: Producer {
                format: "wrapdemo".to_string(),
                version: None,
            },
            facts: vec![Fact {
                kind: "wrapdemo.crash".to_string(),
                attrs: BTreeMap::new(),
                refs: Vec::new(),
                span: None,
            }],
            bucket: Vec::new(),
            coverage: vec![Coverage {
                kind: "wrapdemo.crash".to_string(),
                state: CoverageState::Checked,
                why: None,
            }],
            notes: Vec::new(),
        })
    }
}

fn crash_pack() -> BuiltInPack {
    BuiltInPack {
        file: "wrapdemo/pack.json".to_string(),
        json: serde_json::json!({
            "contract": 1,
            "games": [game_id().as_str()],
            "sources": [{
                "id": "crash_log",
                "where": {
                    "root": "user_data",
                    "location": "documents",
                    "path": "Saved Games/Wrap Test"
                },
                "match": { "glob": "crash.log" },
                "format": "wrapdemo",
                "bind": [{ "time": { "before_s": 5, "after_s": 5 } }],
                "role": "primary"
            }],
            "parsers": [{
                "id": "wrapdemo",
                "entry": { "built_in": { "name": "wrapdemo" } },
                "detect": { "name_glob": "crash.log" }
            }],
            "rules": [{
                "id": "wrapdemo_rule",
                "version": 1,
                "scope": { "producer": { "format": "wrapdemo" } },
                "when": { "fact": { "kind": "wrapdemo.crash" } },
                "effects": [{ "finding": { "scope": "environment", "level": "possible" } }],
                "text": { "headline": { "en": "The invented crash log was read." } }
            }]
        })
        .to_string(),
        parsers: vec![("wrapdemo".to_string(), Box::new(CrashParser))],
    }
}

fn script(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(
        path,
        format!(
            "#!/bin/sh
{body}"
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).unwrap();
}

/// Restore the process working directory when a test that imitates Steam's launch leaves.
struct RestoreDirectory(PathBuf);

impl Drop for RestoreDirectory {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.0);
    }
}

fn latest_session(
    ctx: &CoreContext,
    instance_id: &str,
) -> (String, Option<String>, Option<String>) {
    let conn = agora_core::db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
    conn.query_row(
        "SELECT session_id, ended_at, verdict FROM launch_history
          WHERE instance_id = ?1 ORDER BY started_at DESC LIMIT 1",
        [instance_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .unwrap()
}

fn ran_or_skip(outcome: WrapOutcome) -> Option<WrapOutcome> {
    match outcome {
        WrapOutcome::Ran {
            stepped_down: None, ..
        } => Some(outcome),
        WrapOutcome::Ran {
            stepped_down: Some(reason),
            ..
        } => {
            if std::env::var_os(KNOWN_RESTRICTED_ENV).is_some() {
                eprintln!("SKIPPED on a host marked known-restricted: stepped down: {reason}");
                None
            } else {
                panic!("the view stepped down: {reason}");
            }
        }
        other => Some(other),
    }
}

#[test]
fn no_command_is_an_error() {
    let _serial = serial();
    let tmp = tempfile::Builder::new()
        .prefix("agora-linux-wrap-empty-")
        .tempdir_in("/tmp")
        .unwrap();
    let def = test_definition();
    let ctx = create_context(&tmp, &def);
    let host = QuickJsHost::new();
    let live = agora_core::crash_live::LivePacks {
        packs: &[],
        built_in: &[],
        host: &host,
        instances: None,
    };
    let error = game_wrap::wrap(&ctx, &live, &[]).expect_err("no command is not a wrapper run");
    assert!(matches!(error, WrapError::NoCommand), "{error}");
}

#[test]
fn without_an_instance_the_command_runs_untouched_and_says_why() {
    let _serial = serial();
    let fixture = Fixture::new();
    let out = fixture._tmp.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let marker = out.join("passthrough");
    let command = fixture._tmp.path().join("plain.sh");
    script(
        &command,
        &format!("printf ran > '{}'\nexit 7\n", marker.display()),
    );
    std::env::set_var("SteamAppId", "4242");
    let host = QuickJsHost::new();
    let live = fixture.live(&[], &[], &host);
    let outcome = game_wrap::wrap(
        &fixture.ctx,
        &live,
        &[command.display().to_string(), "an-argument".into()],
    )
    .unwrap();
    match outcome {
        WrapOutcome::PassedThrough { reason, exit_code } => {
            assert!(reason.contains("no pending launch"), "{reason}");
            assert_eq!(exit_code, Some(7));
        }
        other => panic!("the command should have run untouched: {other:?}"),
    }
    assert_eq!(std::fs::read(&marker).unwrap(), b"ran");
}

#[test]
fn the_pending_launch_picks_the_instance_opens_the_session_and_mounts_user_files() {
    let _serial = serial();
    let fixture = Fixture::new();
    fixture.pending();
    let out = fixture._tmp.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let merged = out.join("merged");
    let config_read = out.join("config-read");
    let command = fixture._tmp.path().join("fake-proton.sh");
    script(
        &command,
        &format!(
            "set -e\ncat '{install}/mod.txt' > '{merged}'\ncat '{config}' > '{config_read}'\nprintf 'game wrote' > '{config}'\nexit 0\n",
            install = fixture.install.display(),
            merged = merged.display(),
            config = fixture.real_config.display(),
            config_read = config_read.display(),
        ),
    );
    std::env::set_var("SteamAppId", "4242");
    let host = QuickJsHost::new();
    let live = fixture.live(&[], &[], &host);
    let outcome = game_wrap::wrap(&fixture.ctx, &live, &[command.display().to_string()]).unwrap();
    let Some(WrapOutcome::Ran {
        instance_id,
        exit_code,
        ..
    }) = ran_or_skip(outcome)
    else {
        return;
    };
    assert_eq!(instance_id, fixture.instance_id);
    assert_eq!(exit_code, Some(0));

    assert_eq!(std::fs::read(&merged).unwrap(), b"mod output");
    assert_eq!(std::fs::read(&config_read).unwrap(), b"instance config");
    // The bind mount is the instance's copy, and the player's real file is untouched.
    assert_eq!(
        std::fs::read(&fixture.real_config).unwrap(),
        b"player original"
    );
    assert_eq!(
        std::fs::read(&fixture.instance_config).unwrap(),
        b"game wrote"
    );

    let (session, ended_at, verdict) = latest_session(&fixture.ctx, &fixture.instance_id);
    assert!(!session.is_empty());
    assert!(ended_at.is_some(), "the wrapper must close its session");
    assert_eq!(verdict.as_deref(), Some("{\"kind\":\"normal\"}"));
}

#[test]
fn a_missing_instance_copy_is_seeded_from_the_players_file() {
    let _serial = serial();
    let fixture = Fixture::new();
    fixture.pending();
    std::fs::remove_file(&fixture.instance_config).unwrap();
    let out = fixture._tmp.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let read = out.join("read");
    let command = fixture._tmp.path().join("fake-proton-seed.sh");
    script(
        &command,
        &format!(
            "set -e\ncat '{config}' > '{read}'\nprintf 'game wrote' > '{config}'\n",
            config = fixture.real_config.display(),
            read = read.display(),
        ),
    );
    std::env::set_var("SteamAppId", "4242");
    let host = QuickJsHost::new();
    let live = fixture.live(&[], &[], &host);
    let outcome = game_wrap::wrap(&fixture.ctx, &live, &[command.display().to_string()]).unwrap();
    let Some(WrapOutcome::Ran { .. }) = ran_or_skip(outcome) else {
        return;
    };

    assert_eq!(
        std::fs::read(&read).unwrap(),
        b"player original",
        "the first launch must see the player's settings"
    );
    assert_eq!(
        std::fs::read(&fixture.real_config).unwrap(),
        b"player original",
        "the player's real file was changed"
    );
    assert_eq!(
        std::fs::read(&fixture.instance_config).unwrap(),
        b"game wrote"
    );
}

#[test]
fn the_wrapper_never_creates_a_file_in_the_real_prefix() {
    let _serial = serial();
    let fixture = Fixture::new();
    fixture.pending();
    std::fs::remove_file(&fixture.real_config).unwrap();
    std::fs::remove_file(&fixture.instance_config).unwrap();
    let out = fixture._tmp.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let seen = out.join("seen");
    let command = fixture._tmp.path().join("fake-proton-absent.sh");
    script(
        &command,
        &format!(
            "if [ -e '{config}' ]; then printf present; else printf missing; fi > '{seen}'\n",
            config = fixture.real_config.display(),
            seen = seen.display(),
        ),
    );
    std::env::set_var("SteamAppId", "4242");
    let host = QuickJsHost::new();
    let live = fixture.live(&[], &[], &host);
    let outcome = game_wrap::wrap(&fixture.ctx, &live, &[command.display().to_string()]).unwrap();
    let Some(WrapOutcome::Ran { .. }) = ran_or_skip(outcome) else {
        return;
    };

    assert_eq!(
        std::fs::read(&seen).unwrap(),
        b"missing",
        "the wrapper created the real file before the game ran"
    );
    assert!(
        !fixture.real_config.exists(),
        "the wrapper created a file in the real prefix"
    );
    assert!(
        !fixture.instance_config.exists(),
        "the wrapper created an empty instance copy for a file neither side has"
    );
}

#[test]
fn an_instance_copy_is_not_bound_when_the_real_file_is_absent() {
    let _serial = serial();
    let fixture = Fixture::new();
    fixture.pending();
    std::fs::remove_file(&fixture.real_config).unwrap();
    let command = fixture._tmp.path().join("fake-proton-copy-only.sh");
    script(&command, "exit 0\n");
    std::env::set_var("SteamAppId", "4242");
    let host = QuickJsHost::new();
    let live = fixture.live(&[], &[], &host);
    let outcome = game_wrap::wrap(&fixture.ctx, &live, &[command.display().to_string()]).unwrap();
    let Some(WrapOutcome::Ran { .. }) = ran_or_skip(outcome) else {
        return;
    };

    assert!(
        !fixture.real_config.exists(),
        "the wrapper created the bind target in the real prefix"
    );
    assert_eq!(
        std::fs::read(&fixture.instance_config).unwrap(),
        b"instance config"
    );
}

#[test]
fn a_relative_write_lands_in_the_view_not_the_real_game_folder() {
    let _serial = serial();
    let fixture = Fixture::new();
    fixture.pending();
    let command = fixture._tmp.path().join("fake-proton-relative.sh");
    script(
        &command,
        "set -e\nprintf 'relative write' > relative-from-view.txt\nexit 0\n",
    );
    std::env::set_var("SteamAppId", "4242");
    let host = QuickJsHost::new();
    let live = fixture.live(&[], &[], &host);

    // Steam starts the command with the game folder as its working directory. The inherited
    // cwd stays on the folder underneath the mount unless the view re-enters it.
    let _restore = RestoreDirectory(std::env::current_dir().unwrap());
    std::env::set_current_dir(&fixture.install).unwrap();
    let outcome = game_wrap::wrap(&fixture.ctx, &live, &[command.display().to_string()]).unwrap();
    let Some(WrapOutcome::Ran { .. }) = ran_or_skip(outcome) else {
        return;
    };

    assert!(
        !fixture.install.join("relative-from-view.txt").exists(),
        "the relative write landed in the real game folder, not the view"
    );
    let writable = fixture
        .ctx
        .paths
        .instance_dir(&fixture.instance_id)
        .unwrap()
        .join("writable");
    assert_eq!(
        std::fs::read(writable.join("relative-from-view.txt")).unwrap(),
        b"relative write"
    );
}

#[test]
fn a_crash_log_written_in_the_prefix_gets_an_answer() {
    let _serial = serial();
    let fixture = Fixture::new();
    fixture.pending();
    let crash_log = fixture.crash_log();
    let command = fixture._tmp.path().join("fake-proton-crash.sh");
    script(
        &command,
        &format!(
            "printf 'an invented crash\\n' > '{}'\nexit 1\n",
            crash_log.display()
        ),
    );
    std::env::set_var("SteamAppId", "4242");
    let built_in = vec![crash_pack()];
    let host = QuickJsHost::new();
    let live = fixture.live(&[], &built_in, &host);
    let outcome = game_wrap::wrap(&fixture.ctx, &live, &[command.display().to_string()]).unwrap();
    let Some(WrapOutcome::Ran { exit_code, .. }) = ran_or_skip(outcome) else {
        return;
    };
    assert_eq!(exit_code, Some(1));
    assert!(
        crash_log.is_file(),
        "the script wrote its log in the prefix"
    );

    let conn = agora_core::db::local_state_connection(&fixture.ctx.paths.local_state_db()).unwrap();
    let (session, _, _) = latest_session(&fixture.ctx, &fixture.instance_id);
    let answer = agora_core::crash_live::answer_for_session(&conn, &session)
        .unwrap()
        .expect("a crash inside the session must get an answer");
    let agora_core::crash_live::StoredAnswer::Analyzed { answer, files, .. } = answer.answer else {
        panic!("expected an analyzed answer, got {:?}", answer.answer);
    };
    assert!(
        answer.findings.iter().any(|finding| finding
            .headline
            .get("en")
            .is_some_and(|line| line.contains("invented crash log"))),
        "the invented rule must fire: findings {:?}, could_not_check {:?}, artifacts {:?}, files {:?}",
        answer.findings,
        answer.could_not_check,
        answer.artifacts,
        files
    );
}

#[test]
fn crash_investigate_search_finds_a_log_inside_the_proton_prefix() {
    let _serial = serial();
    let fixture = Fixture::new();
    let crash_log = fixture.crash_log();
    std::fs::create_dir_all(crash_log.parent().unwrap()).unwrap();
    std::fs::write(&crash_log, b"an invented crash\n").unwrap();

    let built_in = vec![crash_pack()];
    let host = QuickJsHost::new();
    let live = fixture.live(&[], &built_in, &host);
    let window = agora_core::crash_live::investigation_window(&fixture.ctx, &fixture.instance_id);
    let session = agora_core::crash_engine::evidence::CrashSession {
        session_id: format!("investigate-{}", fixture.instance_id),
        pid: None,
        working_dir: None,
        window,
    };
    let evidence = agora_core::crash_live::collect_instance_evidence(
        &fixture.ctx,
        &fixture.instance_id,
        &live,
        &session,
        &[],
    )
    .expect("the shared search must read the instance");
    assert_eq!(
        evidence.game_packs().count(),
        1,
        "the invented crash pack must apply to the instance"
    );

    let files = evidence.files();
    let found = files
        .iter()
        .find(|file| file.name == "crash.log")
        .unwrap_or_else(|| {
            panic!(
                "the shared search used by crash investigate must find the prefix's crash.log, \
                 found {:?}",
                files.iter().map(|file| &file.name).collect::<Vec<_>>()
            )
        });
    assert_eq!(found.bytes, b"an invented crash\n");
    assert!(
        evidence
            .collection
            .bound
            .iter()
            .any(|bound| bound.path == crash_log),
        "the evidence must be read from the Proton prefix, not from the Linux home folders"
    );
}

#[test]
fn the_untested_proton_line_is_the_one_the_cli_prints() {
    assert_eq!(
        PROTON_UNTESTED,
        "Launching through Proton on Linux has not been tested on real hardware yet."
    );
}
