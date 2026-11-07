//! The proof MASTER_SPEC §30.12 ends with: Cyberpunk 2077 is modded through the shipped tracer
//! package alone. A fake install, a fake `redMod.exe` and the real `packages/tracers/cyberpunk-2077`
//! files are used; no real game is touched.
//!
//! The package's rules, script format, script rule and classify hook are exercised through the
//! real QuickJS host, so a hook that never reached it cannot pass.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agora_core::app_paths::AppPaths;
use agora_core::content_store;
use agora_core::ctx::CoreContext;
use agora_core::game_base::BaseMode;
use agora_core::game_deploy::{self, add_to_instance, AddToInstance, DeployMode, FileSource};
use agora_core::game_discovery::{DiscoveredInstall, DiscoveryReport, InstallCapabilities};
use agora_core::game_instance::{
    self, create, get_manifest, prepare_launch_with, BeforeLaunchChoice, GameInstanceRecord,
    LaunchOptions, VfsFailure,
};
use agora_core::game_launch::{LaunchError, LaunchedGame, Launcher, PreparedLaunch};
use agora_core::game_load_order;
use agora_core::game_plugins;
use agora_core::game_registry::{GameRegistry, IdentifiedInstall, RuntimeResolution};
use agora_core::game_tools;
use agora_core::plugins::service::PluginService;
use agora_core::plugins::store::{self, PluginSource};
use agora_core::plugins::PLUGINS_EMERGENCY_STOP_SETTING;
use agora_game_api::{
    BaseReference, GameContext, GameDefinition, GameId, InstallId, InstallKind, LayerSource,
    LayerStack, LoadOrder, RuntimeIdentity, StoreId,
};
use agora_plugin_api::capability::CapabilitySet;
use agora_plugin_api::manifest::{PluginId, PluginManifest};
use agora_plugin_host::QuickJsHost;
use tempfile::TempDir;

const PLUGIN: &str = "agora-tracer.cyberpunk-2077";
const GAME: &str = "cyberpunk-2077";

fn real_package_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("packages")
        .join("tracers")
        .join("cyberpunk-2077")
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let path = entry.path();
        let dest = to.join(entry.file_name());
        if path.is_dir() {
            copy_dir(&path, &dest);
        } else {
            std::fs::copy(&path, &dest).unwrap();
        }
    }
}

fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime for the test")
        .block_on(future)
}

struct Harness {
    _tmp: TempDir,
    ctx: CoreContext,
    _service: PluginService,
    definition: GameDefinition,
    install: IdentifiedInstall,
}

impl Harness {
    fn new(script_override: Option<&str>) -> Self {
        let tmp = TempDir::new().unwrap();
        let paths = AppPaths::from_root(tmp.path().join("data"));
        paths.create_required_dirs().unwrap();
        agora_core::db::init_local_state_db(&paths.local_state_db()).unwrap();

        // The real package files, with the script optionally replaced for a failure test.
        let plugin_dir = paths.plugin_packages_root().join(PLUGIN);
        copy_dir(&real_package_dir(), &plugin_dir);
        if let Some(script) = script_override {
            std::fs::write(plugin_dir.join("main.js"), script).unwrap();
        }
        let manifest: PluginManifest =
            serde_json::from_slice(&std::fs::read(plugin_dir.join("agora-plugin.json")).unwrap())
                .expect("the shipped manifest parses");
        let mut granted = CapabilitySet::new();
        for name in &manifest.capabilities.required {
            granted.insert(name.parse().expect("a known capability"));
        }
        {
            let conn = agora_core::db::local_state_connection(&paths.local_state_db()).unwrap();
            agora_core::db::set_setting(
                &conn,
                PLUGINS_EMERGENCY_STOP_SETTING,
                &serde_json::Value::Bool(false),
            )
            .unwrap();
            store::upsert(
                &conn,
                &manifest,
                &granted,
                &PluginSource::Package,
                &plugin_dir,
                true,
                "2026-11-06T00:00:00Z",
            )
            .unwrap();
        }

        let (ctx, warnings) =
            CoreContext::initialize(paths, GameRegistry::builder()).expect("the context starts");
        assert!(
            !warnings.iter().any(|warning| warning.contains(PLUGIN)),
            "{warnings:?}"
        );

        let service = PluginService::headless(ctx.clone(), Arc::new(QuickJsHost::new()));
        service.reload().expect("the plugin records a load");
        service
            .set_scripts_allowed(&PluginId::parse(PLUGIN).unwrap(), true)
            .expect("scripts are allowed for the test");

        let game_id = GameId::new(GAME).unwrap();
        let definition = ctx
            .games
            .game(&game_id)
            .expect("the shipped game package is registered")
            .clone();

        let install_dir = tmp.path().join("game");
        for dir in [
            "bin/x64",
            "tools/redmod/bin",
            "r6/logs",
            "red4ext/logs",
            "archive/pc/mod",
            "mods",
        ] {
            std::fs::create_dir_all(install_dir.join(dir)).unwrap();
        }
        let helper = PathBuf::from(env!("CARGO_BIN_EXE_agora-tool-test-helper"));
        std::fs::copy(&helper, install_dir.join("bin/x64/Cyberpunk2077.exe")).unwrap();
        std::fs::copy(&helper, install_dir.join("tools/redmod/bin/redMod.exe")).unwrap();
        // A RED4ext install without its loader: the diagnostics check has something to say.
        std::fs::copy(&helper, install_dir.join("red4ext/RED4ext.dll")).unwrap();
        std::fs::write(
            install_dir.join("red4ext/logs/red4ext.log"),
            b"RED4ext initialized successfully\n",
        )
        .unwrap();

        let runtime = RuntimeIdentity {
            game: game_id.clone(),
            store: StoreId::gog(),
            version: "2.31".into(),
            build: None,
        };
        let volume =
            agora_core::game_discovery::volume::VolumeDetector::new().get_volume_info(&install_dir);
        let discovered = DiscoveredInstall {
            store: StoreId::gog(),
            product: "1423049311".into(),
            name: "Cyberpunk 2077".into(),
            kind: InstallKind::BaseGame,
            parent_product: None,
            location: install_dir,
            store_version: Some(runtime.version.clone()),
            store_build: None,
            executables: vec!["bin/x64/Cyberpunk2077.exe".into()],
            capabilities: InstallCapabilities {
                executables_readable: true,
                accepts_new_files: true,
                relocatable: true,
            },
            volume,
        };
        let install = IdentifiedInstall {
            game: game_id,
            install_id: InstallId::new("gog:1423049311").unwrap(),
            discovered,
            add_ons: vec![],
            runtime: RuntimeResolution::Identified {
                runtime,
                source: "executable".into(),
            },
        };

        Self {
            _tmp: tmp,
            ctx,
            _service: service,
            definition,
            install,
        }
    }

    fn create(&self, name: &str) -> GameInstanceRecord {
        create(
            &self.ctx,
            &self.install,
            &self.definition,
            name,
            None,
            BaseMode::Linked,
            &|_| {},
        )
        .expect("the instance is created")
    }

    fn add(&self, instance: &str, name: &str, files: &[(&str, &[u8])]) -> (String, AddToInstance) {
        let dir = TempDir::new().unwrap();
        for (rel, bytes) in files {
            let path = dir.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
        let outcome = content_store::add_folder(&self.ctx, dir.path(), Some(name)).unwrap();
        let item = outcome.item().item_id.clone();
        let added = add_to_instance(&self.ctx, instance, &item).expect("the item is placed");
        (item, added)
    }
}

/// Runs a tool or a game from its staging folder so writes are captured, without the VFS DLL.
struct StagingLauncher;

impl Launcher for StagingLauncher {
    fn locate_vfs_dll(&self) -> Result<PathBuf, String> {
        Ok(PathBuf::from(r"C:\agora\agora_vfs.dll"))
    }

    fn launch(&self, prepared: &PreparedLaunch) -> Result<LaunchedGame, LaunchError> {
        let mut plain = prepared.clone();
        if let Some(vfs) = &prepared.vfs {
            plain.resolved.cwd = vfs.upper.clone();
        }
        plain.vfs = None;
        agora_core::game_launch::launch(&plain)
    }

    fn launch_capturing(&self, prepared: &PreparedLaunch) -> Result<LaunchedGame, LaunchError> {
        let mut plain = prepared.clone();
        if let Some(vfs) = &prepared.vfs {
            plain.resolved.cwd = vfs.upper.clone();
        }
        plain.vfs = None;
        agora_core::game_launch::launch_capturing(&plain)
    }
}

fn prepare(h: &Harness, instance: &str) -> Result<PreparedLaunch, game_instance::InstanceError> {
    prepare_launch_with(
        &h.ctx,
        instance,
        &h.definition,
        LaunchOptions {
            launch_anyway: false,
            plain: false,
            deployment: None,
            on_vfs_failure: VfsFailure::FallBack,
            before_launch: BeforeLaunchChoice::Run,
        },
        &DiscoveryReport::default,
        &StagingLauncher,
    )
}

fn args(prepared: &PreparedLaunch) -> Vec<String> {
    prepared
        .resolved
        .args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

fn log(h: &Harness, instance: &str) -> String {
    std::fs::read_to_string(game_instance::launch_log_path(&h.ctx, instance).unwrap())
        .unwrap_or_default()
}

fn needs(h: &Harness, instance: &str) -> Vec<game_tools::BeforeLaunchNeed> {
    let facts =
        agora_core::game_conditions::InstanceFacts::for_instance(&h.ctx, instance, &h.definition);
    game_tools::before_launch_needs(&h.ctx, instance, &h.definition, &facts, &mut Vec::new())
        .expect("the tool needs are readable")
}

fn mount_of(added: AddToInstance) -> (String, String) {
    match added {
        AddToInstance::Added {
            layer,
            placement_reason,
            ..
        } => (
            layer.mount_path.as_str().to_string(),
            placement_reason.unwrap_or_default(),
        ),
        other => panic!("expected the item to be placed, got {other:?}"),
    }
}

fn generated_layer(h: &Harness, instance: &str) -> Option<(String, String)> {
    get_manifest(&h.ctx, instance)
        .unwrap()
        .layers
        .layers()
        .iter()
        .find_map(|layer| match &layer.source {
            LayerSource::Generated {
                tool, generation, ..
            } if tool.as_str() == "redmod-deploy" => {
                Some((generation.clone(), layer.id.to_string()))
            }
            _ => None,
        })
}

#[test]
fn the_shipped_package_places_orders_deploys_and_launches() {
    let h = Harness::new(None);
    let instance = h.create("Proof");
    let id = &instance.instance_id;

    // 1. A REDmod is a folder with info.json, and mounts under mods/<folder>.
    let (_, redmod) = h.add(
        id,
        "!Patch",
        &[
            ("!Patch/info.json", b"{}"),
            ("!Patch/archives/patch.archive", b"archive"),
        ],
    );
    let (mount, reason) = mount_of(redmod);
    assert_eq!(mount, "mods/!Patch");
    assert!(reason.contains("REDmod"), "{reason}");

    // 2. Only *.archive mounts in archive/pc/mod.
    let (_, archive) = h.add(id, "Legacy", &[("LegacyMod.archive", b"archive")]);
    let (mount, reason) = mount_of(archive);
    assert_eq!(mount, "archive/pc/mod");
    assert!(reason.contains("Archive mod"), "{reason}");

    // 3. A root-shaped pack falls through to content_layout and stays at the game root.
    let (_, root) = h.add(id, "RootPack", &[("r6/tweaks/MyMod.tweak", b"tweak")]);
    let (mount, reason) = mount_of(root);
    assert_eq!(mount, "");
    assert!(reason.contains("game-root"), "{reason}");

    // 4. A loose *.reds set has no marker, so data cannot place it: classify does.
    let (_, reds) = h.add(
        id,
        "CoolRedscript",
        &[("CoolRedscript/init.reds", b"// redscript")],
    );
    let (mount, reason) = mount_of(reds);
    assert_eq!(mount, "r6/scripts/CoolRedscript");
    assert!(reason.contains("reds"), "{reason}");

    // A second REDmod so the order has two entries.
    let (_, beta) = h.add(id, "Alpha", &[("Alpha/info.json", b"{}")]);
    assert_eq!(mount_of(beta).0, "mods/Alpha");

    // The list is a script format: core renders it with the package's renderer.
    game_plugins::import_list_in(
        &h.ctx,
        id,
        &h.definition,
        Some("redmod-mods"),
        &[("Alpha".into(), true), ("!Patch".into(), true)],
        &[],
        &[],
    )
    .expect("the list is written");
    let list_path = h
        .ctx
        .paths
        .instance_dir(id)
        .unwrap()
        .join("mods/modlist.txt");
    assert_eq!(
        std::fs::read(&list_path).unwrap(),
        b"Alpha\n!Patch\n",
        "the script renderer writes one folder per line"
    );

    // The script rule pulls a !-prefixed mod before the others, and the wrong order is a finding.
    let wrong = game_load_order::order_in(&h.ctx, id, &h.definition, Some("redmod-mods"))
        .expect("the order reads");
    assert!(
        wrong
            .findings
            .iter()
            .any(|finding| finding.message().contains("!Patch")),
        "{:?}",
        wrong.findings
    );

    game_plugins::write_order_locked_in(
        &h.ctx,
        id,
        &h.definition,
        Some("redmod-mods"),
        &[("!Patch".into(), true), ("Alpha".into(), true)],
    )
    .expect("the order is reordered");
    assert_eq!(std::fs::read(&list_path).unwrap(), b"!Patch\nAlpha\n");
    let right = game_load_order::order_in(&h.ctx, id, &h.definition, Some("redmod-mods"))
        .expect("the order reads");
    assert!(right.findings.is_empty(), "{:?}", right.findings);

    // The tool has never run, so a launch needs it.
    assert_eq!(needs(&h, id).len(), 1);
    let first = prepare(&h, id).expect("the first launch prepares");
    let first_args = args(&first);
    assert!(
        first_args.iter().any(|arg| arg == "-modded"),
        "a modded launch passes -modded: {first_args:?}"
    );
    assert!(log(&h, id).contains("Before launch: ran REDmod deploy."));

    // The tool's output is a generated layer, and the game reads it above the base.
    let (generation, _) = generated_layer(&h, id).expect("the deploy made a generated layer");
    let generation_dir = game_tools::generation_dir(
        &h.ctx.paths.instance_dir(id).unwrap(),
        "redmod-deploy",
        &generation,
    )
    .unwrap();
    let recorded = std::fs::read_to_string(generation_dir.join("redmod-deploy-args.txt")).unwrap();
    assert!(recorded.starts_with("deploy\n-root="), "{recorded}");
    let plan = game_deploy::plan(&h.ctx, id, &h.definition, DeployMode::Links).unwrap();
    assert!(plan.files.iter().any(|file| {
        file.path.as_str() == "r6/cache/modded/final.redscripts"
            && matches!(&file.source, FileSource::Generated { tool, .. } if tool == "redmod-deploy")
    }));

    // A second launch does not run it again while nothing it reads changed.
    assert!(needs(&h, id).is_empty());
    prepare(&h, id).expect("the second launch prepares");
    assert_eq!(
        log(&h, id)
            .matches("Before launch: ran REDmod deploy.")
            .count(),
        1
    );

    // A legacy archive does not need a REDmod deploy...
    h.add(id, "LegacyTwo", &[("Other.archive", b"archive")]);
    assert!(needs(&h, id).is_empty());
    // ...but a new REDmod does.
    h.add(id, "Beta", &[("Beta/info.json", b"{}")]);
    assert_eq!(needs(&h, id).len(), 1);
    prepare(&h, id).expect("the third launch prepares");
    assert_eq!(
        log(&h, id)
            .matches("Before launch: ran REDmod deploy.")
            .count(),
        2
    );

    // With no REDmod at all, neither the deploy nor -modded happens.
    let empty = h.create("Vanilla");
    assert!(needs(&h, &empty.instance_id).is_empty());
    let vanilla = prepare(&h, &empty.instance_id).expect("a vanilla launch prepares");
    assert!(
        !args(&vanilla).iter().any(|arg| arg == "-modded"),
        "{:?}",
        args(&vanilla)
    );
    assert!(generated_layer(&h, &empty.instance_id).is_none());
    assert!(!log(&h, &empty.instance_id).contains("Before launch"));
}

#[test]
fn diagnostics_reads_the_red4ext_log_and_flags_a_missing_loader() {
    let h = Harness::new(None);
    // The base copy is made when the instance is created, so the error is in the log first.
    std::fs::write(
        h.install
            .discovered
            .location
            .join("red4ext/logs/red4ext.log"),
        b"RED4ext initialized
Failed to load plugin Foo
",
    )
    .unwrap();
    let instance = h.create("Diagnostics");
    let game_id = GameId::new(GAME).unwrap();
    let package = h.ctx.games.package_for(&game_id).unwrap().clone();
    let context = GameContext {
        instance: instance.instance_id.clone(),
        runtime: RuntimeIdentity {
            game: game_id,
            store: StoreId::gog(),
            version: "2.31".into(),
            build: None,
        },
        base: Some(BaseReference::Unpinned {
            install: InstallId::new("gog:1423049311").unwrap(),
            reason: "test".into(),
        }),
        frameworks: Vec::new(),
        layers: LayerStack::default(),
        load_order: LoadOrder::default(),
        settings: Default::default(),
    };

    let diagnostics = block_on(package.diagnostics(context.clone())).expect("diagnostics run");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "RED4EXT_NO_LOADER"),
        "{diagnostics:?}"
    );

    // A log line that reports an error is visible, and reading it proves game:read works.
    let log_path = h._tmp.path().join("game/red4ext/logs/red4ext.log");
    std::fs::write(
        &log_path,
        b"RED4ext initialized\nFailed to load plugin Foo\n",
    )
    .unwrap();
    let diagnostics = block_on(package.diagnostics(context)).expect("diagnostics run");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "RED4EXT_LOG_ERROR"
                && diagnostic.message.contains("Failed to load plugin Foo")),
        "{diagnostics:?}"
    );
}

#[test]
fn a_throwing_rules_hook_is_a_warning_not_a_crash() {
    let source = std::fs::read_to_string(real_package_dir().join("main.js")).unwrap();
    let throwing = source.replacen(
        "export function redmodRules(args) {",
        "export function redmodRules(args) { throw new Error('rules failed on purpose');",
        1,
    );
    assert_ne!(throwing, source, "the test found the rules hook");

    let h = Harness::new(Some(&throwing));
    let instance = h.create("Broken hook");
    let id = &instance.instance_id;
    h.add(id, "Alpha", &[("Alpha/info.json", b"{}")]);
    game_plugins::import_list_in(
        &h.ctx,
        id,
        &h.definition,
        Some("redmod-mods"),
        &[("Alpha".into(), true)],
        &[],
        &[],
    )
    .unwrap();

    let order = game_load_order::order_in(&h.ctx, id, &h.definition, Some("redmod-mods")).unwrap();
    assert!(
        order
            .findings
            .iter()
            .any(|finding| matches!(finding, game_load_order::Finding::RulesFailed { .. })),
        "{:?}",
        order.findings
    );
    // A failure to judge the order is a warning: the launch still prepares.
    let prepared = prepare(&h, id).expect("a broken rules hook does not stop the launch");
    assert!(prepared
        .load_order_findings
        .iter()
        .any(|finding| matches!(finding, game_load_order::Finding::RulesFailed { .. })));
}
