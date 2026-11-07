//! General load orders (MASTER_SPEC §30.12): several lists per game, the three rule sets, script
//! formats and folders as items. The Creation Engine suites keep covering Skyrim.
//!
//! The script hooks run through a compiled test double, the same way a package would answer them.

use std::sync::{Arc, Mutex};

use agora_core::ctx::CoreContext;
use agora_core::game_base::BaseMode;
use agora_core::game_deploy::{add_content, deploy, DeployMode, DeployOutcome};
use agora_core::game_discovery::{DiscoveredInstall, DiscoveryReport, InstallCapabilities};
use agora_core::game_instance::{
    create, prepare_launch_with, GameInstanceRecord, LaunchOptions, VfsFailure,
};
use agora_core::game_launch::SystemLauncher;
use agora_core::game_load_order::{self, Finding, LoadOrderError};
use agora_core::game_plugins::{self, PluginListError};
use agora_core::game_registry::{
    GameRegistry, IdentifiedInstall, PackageSource, RuntimeResolution,
};
use agora_game_api::{
    DeploymentStrategy, GameContext, GameDefinition, GameError, GameFuture, GameId, GamePackage,
    GamePath, InstallId, InstallKind, LoadOrderDefinition, LoadOrderFormat, LoadOrderLine,
    LoadOrderRule, LoadOrderRules, PackageDefinition, RelPath, RuntimeIdentity, StoreId,
    StoreIdentifier, UserDataLocation, UserFileMapping, UserFileStrategy,
};
use tempfile::TempDir;

static TEST_LOCK: Mutex<()> = Mutex::new(());

/// What the compiled double answers for script lists.
#[derive(Clone, Default)]
struct ScriptHooks {
    rules: Vec<LoadOrderRule>,
    fail_rules: bool,
    fail_parse: bool,
    fail_render: bool,
}

struct TestPackage {
    definition: PackageDefinition,
    hooks: ScriptHooks,
}

impl GamePackage for TestPackage {
    fn definition(&self) -> &PackageDefinition {
        &self.definition
    }

    fn load_order_rules<'a>(
        &'a self,
        _context: GameContext,
        _list_id: &'a str,
    ) -> GameFuture<'a, Option<Vec<LoadOrderRule>>> {
        let hooks = self.hooks.clone();
        Box::pin(async move {
            if hooks.fail_rules {
                return Err(GameError {
                    code: "rules_failed".into(),
                    message: "the script rules failed on purpose".into(),
                });
            }
            Ok(Some(hooks.rules))
        })
    }

    fn parse_load_order<'a>(
        &'a self,
        _context: GameContext,
        _list_id: &'a str,
        bytes: Vec<u8>,
    ) -> GameFuture<'a, Option<Vec<LoadOrderLine>>> {
        let fail = self.hooks.fail_parse;
        Box::pin(async move {
            if fail {
                return Err(GameError {
                    code: "parse_failed".into(),
                    message: "the script parser failed on purpose".into(),
                });
            }
            let text = String::from_utf8_lossy(&bytes);
            let lines = text
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| {
                    let (name, active) = line.split_once('\t').unwrap_or((line, "true"));
                    LoadOrderLine {
                        name: name.to_string(),
                        active: active != "false",
                    }
                })
                .collect();
            Ok(Some(lines))
        })
    }

    fn render_load_order<'a>(
        &'a self,
        _context: GameContext,
        _list_id: &'a str,
        lines: Vec<LoadOrderLine>,
    ) -> GameFuture<'a, Option<Vec<u8>>> {
        let fail = self.hooks.fail_render;
        Box::pin(async move {
            if fail {
                return Err(GameError {
                    code: "render_failed".into(),
                    message: "the script renderer failed on purpose".into(),
                });
            }
            let mut out = String::new();
            for line in lines {
                out.push_str(&line.name);
                out.push('\t');
                out.push_str(if line.active { "true" } else { "false" });
                out.push('\n');
            }
            Ok(Some(out.into_bytes()))
        })
    }
}

fn package_of(def: &GameDefinition) -> PackageDefinition {
    PackageDefinition {
        id: format!("test.{}", def.id),
        version: semver::Version::new(0, 1, 0),
        api_range: semver::VersionReq::parse(">=0.1, <0.2").unwrap(),
        parents: vec![],
        game_templates: std::collections::BTreeMap::new(),
        behaviours: None,
        games: vec![def.clone()],
        frameworks: vec![],
        tools: vec![],
    }
}

fn mapping(path: &str) -> UserFileMapping {
    UserFileMapping::new(
        GamePath::UserData {
            location: UserDataLocation::LocalAppData,
            path: RelPath::new(format!("Test Game/{path}")).unwrap(),
        },
        RelPath::new(format!("user/{path}")).unwrap(),
        UserFileStrategy::JournaledSwap,
    )
    .with_stores(vec![StoreId::steam()])
}

fn lines_rule(id: &str, file: &str) -> LoadOrderDefinition {
    LoadOrderDefinition {
        id: id.into(),
        name: id.into(),
        file: RelPath::new(format!("user/{file}")).unwrap(),
        items: agora_game_api::LoadOrderItems {
            folder: RelPath::new("Data").unwrap(),
            patterns: vec!["*.esp".into(), "*.esm".into(), "*.esl".into()],
            folders: false,
        },
        format: LoadOrderFormat::Lines {
            active_prefix: "*".into(),
            header: vec!["# header".into()],
        },
        rules: LoadOrderRules {
            kind: "none".into(),
            entry: None,
            implicit: Vec::new(),
            implicit_list_file: None,
            loot: None,
        },
    }
}

fn script_rule(id: &str, file: &str, kind: &str) -> LoadOrderDefinition {
    LoadOrderDefinition {
        id: id.into(),
        name: id.into(),
        file: RelPath::new(format!("user/{file}")).unwrap(),
        items: agora_game_api::LoadOrderItems {
            folder: RelPath::new("Data").unwrap(),
            patterns: vec!["*.esp".into(), "*.esm".into(), "*.esl".into()],
            folders: false,
        },
        format: LoadOrderFormat::Script {
            parse: "parse".into(),
            render: "render".into(),
        },
        rules: LoadOrderRules {
            kind: kind.into(),
            entry: Some("rules".into()),
            implicit: Vec::new(),
            implicit_list_file: None,
            loot: None,
        },
    }
}

fn definition(lists: Vec<LoadOrderDefinition>) -> GameDefinition {
    let mut user_files = Vec::new();
    for rule in &lists {
        let file = rule
            .file
            .as_str()
            .strip_prefix("user/")
            .unwrap_or(rule.file.as_str())
            .to_string();
        user_files.push(mapping(&file));
    }
    GameDefinition {
        mo2_game_name: None,
        session: Default::default(),
        id: GameId::new("test-game").unwrap(),
        extends: None,
        name: "Test Game".into(),
        stores: vec![StoreIdentifier {
            store: StoreId::steam(),
            product: "12345".into(),
        }],
        version_sources: vec![],
        deployment: DeploymentStrategy::VirtualFileSystem,
        content_rules: vec![],
        content_layout: None,
        framework_ids: vec![],
        tool_ids: vec![],
        launch: Some(agora_game_api::LaunchRecipe {
            executable: GamePath::Runtime {
                path: RelPath::new("Game.exe").unwrap(),
            },
            arguments: vec![],
            environment: Default::default(),
            working_directory: GamePath::Runtime {
                path: RelPath::default(),
            },
        }),
        log_paths: vec![],
        crash_paths: vec![],
        user_files,
        save_paths: vec![],
        linked_archive_patterns: vec![],
        declared_writes: vec![],
        excluded_paths: vec![],
        load_orders: lists,
        runtime_files: Vec::new(),
        save_location: Vec::new(),
        launch_alternatives: Vec::new(),
        copy_patterns: Vec::new(),
    }
}

fn install_of(dir: &std::path::Path) -> IdentifiedInstall {
    let store_id = StoreId::steam();
    let runtime = RuntimeIdentity {
        game: GameId::new("test-game").unwrap(),
        store: store_id.clone(),
        version: "1.0.0".into(),
        build: None,
    };
    let volume = agora_core::game_discovery::volume::VolumeDetector::new().get_volume_info(dir);
    let discovered = DiscoveredInstall {
        store: store_id,
        product: "12345".into(),
        name: "Test Game".into(),
        kind: InstallKind::BaseGame,
        parent_product: None,
        location: dir.to_path_buf(),
        store_version: Some(runtime.version.clone()),
        store_build: None,
        executables: vec!["Game.exe".into()],
        capabilities: InstallCapabilities {
            executables_readable: true,
            accepts_new_files: true,
            relocatable: true,
        },
        volume,
    };
    IdentifiedInstall {
        game: runtime.game.clone(),
        install_id: InstallId::new("steam:12345").unwrap(),
        discovered,
        add_ons: vec![],
        runtime: RuntimeResolution::Identified {
            runtime,
            source: "executable".into(),
        },
    }
}

struct Harness {
    _lock: std::sync::MutexGuard<'static, ()>,
    _tmp: TempDir,
    _user_data: TempDir,
    ctx: CoreContext,
    def: GameDefinition,
    install: IdentifiedInstall,
}

impl Drop for Harness {
    fn drop(&mut self) {
        std::env::remove_var("AGORA_TEST_USER_DATA_ROOT");
    }
}

impl Harness {
    fn new(lists: Vec<LoadOrderDefinition>) -> Self {
        Self::with_hooks(lists, ScriptHooks::default())
    }

    fn with_hooks(lists: Vec<LoadOrderDefinition>, hooks: ScriptHooks) -> Self {
        let lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let user_data = TempDir::new().unwrap();
        std::env::set_var("AGORA_TEST_USER_DATA_ROOT", user_data.path());

        let install_dir = tmp.path().join("install");
        std::fs::create_dir_all(install_dir.join("Data")).unwrap();
        std::fs::write(install_dir.join("Game.exe"), b"fake game binary").unwrap();

        let def = definition(lists);
        let ctx = CoreContext::for_testing(tmp.path().join("app_data"));
        agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
        let mut builder = GameRegistry::builder();
        builder
            .add(
                PackageSource::Compiled {
                    crate_name: "test".into(),
                },
                Arc::new(TestPackage {
                    definition: package_of(&def),
                    hooks,
                }),
            )
            .expect("register test package");
        let ctx = ctx.with_games(Arc::new(builder.build()));
        let install = install_of(&install_dir);
        Self {
            _lock: lock,
            _tmp: tmp,
            _user_data: user_data,
            ctx,
            def,
            install,
        }
    }

    fn instance(&self, name: &str) -> GameInstanceRecord {
        create(
            &self.ctx,
            &self.install,
            &self.def,
            name,
            None,
            BaseMode::Linked,
            &|_| {},
        )
        .expect("create instance")
    }

    fn item(&self, name: &str, files: &[(&str, &[u8])]) -> String {
        let dir = tempfile::tempdir().unwrap();
        for (rel, content) in files {
            let path = dir.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content).unwrap();
        }
        agora_core::content_store::add_folder(&self.ctx, dir.path(), Some(name))
            .unwrap()
            .item()
            .item_id
            .clone()
    }

    fn add(&self, inst: &GameInstanceRecord, item: &str) {
        add_content(&self.ctx, &inst.instance_id, item, None, None).unwrap();
    }

    fn deploy(&self, inst: &GameInstanceRecord) -> DeployOutcome {
        deploy(&self.ctx, &inst.instance_id, &self.def, DeployMode::Links).expect("deploy")
    }

    fn instance_dir(&self, inst: &GameInstanceRecord) -> std::path::PathBuf {
        self.ctx.paths.instance_dir(&inst.instance_id).unwrap()
    }

    fn list_path(&self, inst: &GameInstanceRecord, file: &str) -> std::path::PathBuf {
        self.instance_dir(inst).join("user").join(file)
    }

    fn bytes(&self, inst: &GameInstanceRecord, file: &str) -> Vec<u8> {
        std::fs::read(self.list_path(inst, file)).unwrap_or_default()
    }

    fn write(&self, inst: &GameInstanceRecord, file: &str, bytes: &[u8]) {
        let path = self.list_path(inst, file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
}

fn names(entries: &[game_load_order::LoadOrderEntry]) -> Vec<String> {
    entries.iter().map(|entry| entry.name.clone()).collect()
}

#[test]
fn two_lists_keep_separate_orders_and_files() {
    let h = Harness::new(vec![
        lines_rule("plugins", "Plugins.txt"),
        lines_rule("other", "Other.txt"),
    ]);
    let inst = h.instance("two-lists");
    let item = h.item("mod", &[("Data/A.esp", b"a"), ("Data/B.esp", b"b")]);
    h.add(&inst, &item);
    h.deploy(&inst);

    assert_eq!(
        h.bytes(&inst, "Plugins.txt"),
        b"# header\r\n*A.esp\r\n*B.esp\r\n"
    );
    assert_eq!(
        h.bytes(&inst, "Other.txt"),
        b"# header\r\n*A.esp\r\n*B.esp\r\n"
    );

    game_plugins::set_active_in(
        &h.ctx,
        &inst.instance_id,
        &h.def,
        Some("plugins"),
        "A.esp",
        false,
    )
    .unwrap();
    assert_eq!(
        h.bytes(&inst, "Plugins.txt"),
        b"# header\r\nA.esp\r\n*B.esp\r\n"
    );
    assert_eq!(
        h.bytes(&inst, "Other.txt"),
        b"# header\r\n*A.esp\r\n*B.esp\r\n"
    );

    let first =
        game_load_order::order_in(&h.ctx, &inst.instance_id, &h.def, Some("plugins")).unwrap();
    let second =
        game_load_order::order_in(&h.ctx, &inst.instance_id, &h.def, Some("other")).unwrap();
    assert_eq!(names(&first.entries), ["A.esp", "B.esp"]);
    assert!(!first.entries[0].active);
    assert!(
        second.entries[0].active,
        "the other list kept its own state"
    );
}

#[test]
fn reordering_one_list_never_writes_the_other() {
    let h = Harness::new(vec![
        lines_rule("plugins", "Plugins.txt"),
        lines_rule("other", "Other.txt"),
    ]);
    let inst = h.instance("reorder-one");
    let item = h.item("mod", &[("Data/A.esp", b"a"), ("Data/B.esp", b"b")]);
    h.add(&inst, &item);
    h.deploy(&inst);

    let other_before = h.bytes(&inst, "Other.txt");
    game_plugins::write_order_locked_in(
        &h.ctx,
        &inst.instance_id,
        &h.def,
        Some("plugins"),
        &[("B.esp".into(), true), ("A.esp".into(), true)],
    )
    .unwrap();

    assert_eq!(
        h.bytes(&inst, "Plugins.txt"),
        b"# header\r\n*B.esp\r\n*A.esp\r\n"
    );
    assert_eq!(h.bytes(&inst, "Other.txt"), other_before);
}

#[test]
fn an_undeclared_list_id_is_an_error_not_the_first_list() {
    let h = Harness::new(vec![lines_rule("plugins", "Plugins.txt")]);
    let inst = h.instance("bad-list");
    let err =
        game_load_order::order_in(&h.ctx, &inst.instance_id, &h.def, Some("nope")).unwrap_err();
    assert!(matches!(err, LoadOrderError::UnknownList { .. }), "{err}");
    let err = game_plugins::list_in(&h.ctx, &inst.instance_id, &h.def, Some("nope")).unwrap_err();
    assert!(matches!(err, PluginListError::UnknownList { .. }), "{err}");
}

#[test]
fn a_game_with_no_lists_deploys_and_launches() {
    let h = Harness::new(Vec::new());
    let inst = h.instance("no-lists");
    let item = h.item("content", &[("Data/A.esp", b"a")]);
    h.add(&inst, &item);
    h.deploy(&inst);

    assert!(game_load_order::orders(&h.ctx, &inst.instance_id, &h.def)
        .unwrap()
        .is_empty());
    assert!(
        game_load_order::check_all(&h.ctx, &inst.instance_id, &h.def)
            .unwrap()
            .is_empty()
    );

    let prepared = prepare_launch_with(
        &h.ctx,
        &inst.instance_id,
        &h.def,
        LaunchOptions {
            plain: true,
            on_vfs_failure: VfsFailure::FallBack,
            ..Default::default()
        },
        &|| DiscoveryReport {
            installs: vec![h.install.discovered.clone()],
            warnings: vec![],
        },
        &SystemLauncher,
    );
    assert!(prepared.is_ok(), "{prepared:?}");
}

#[test]
fn kind_none_accepts_any_order_and_has_no_sort() {
    let h = Harness::new(vec![lines_rule("plugins", "Plugins.txt")]);
    let inst = h.instance("none-rules");
    let item = h.item("mod", &[("Data/A.esp", b"a"), ("Data/B.esp", b"b")]);
    h.add(&inst, &item);
    h.deploy(&inst);

    assert!(
        game_load_order::check_in(&h.ctx, &inst.instance_id, &h.def, Some("plugins"))
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        game_load_order::sort_in(&h.ctx, &inst.instance_id, &h.def, Some("plugins"), false),
        Err(LoadOrderError::NoSort(_))
    ));
}

#[test]
fn a_failed_rules_hook_leaves_the_order_and_reports() {
    let mut rule = script_rule("plugins", "Script.txt", "script");
    rule.format = LoadOrderFormat::Lines {
        active_prefix: "*".into(),
        header: Vec::new(),
    };
    let h = Harness::with_hooks(
        vec![rule],
        ScriptHooks {
            fail_rules: true,
            ..ScriptHooks::default()
        },
    );
    let inst = h.instance("failed-rules");
    let item = h.item("mod", &[("Data/A.esp", b"a"), ("Data/B.esp", b"b")]);
    h.add(&inst, &item);
    h.deploy(&inst);
    let before = h.bytes(&inst, "Script.txt");

    let err = game_load_order::sort_in(&h.ctx, &inst.instance_id, &h.def, Some("plugins"), false)
        .unwrap_err();
    assert!(matches!(err, LoadOrderError::RulesScript { .. }), "{err}");
    assert_eq!(h.bytes(&inst, "Script.txt"), before, "nothing was written");

    let order =
        game_load_order::order_in(&h.ctx, &inst.instance_id, &h.def, Some("plugins")).unwrap();
    assert!(
        order
            .findings
            .iter()
            .any(|finding| matches!(finding, Finding::RulesFailed { .. })),
        "{:?}",
        order.findings
    );
}

#[test]
fn script_rules_with_a_cycle_are_reported_like_creation_engine_cycles() {
    let h = Harness::with_hooks(
        vec![script_rule("plugins", "Script.txt", "script")],
        ScriptHooks {
            rules: vec![
                LoadOrderRule::Requires {
                    item: "A.esp".into(),
                    master: "B.esp".into(),
                },
                LoadOrderRule::Requires {
                    item: "B.esp".into(),
                    master: "A.esp".into(),
                },
            ],
            ..ScriptHooks::default()
        },
    );
    let inst = h.instance("cycle");
    let item = h.item("mod", &[("Data/A.esp", b"a"), ("Data/B.esp", b"b")]);
    h.add(&inst, &item);
    h.deploy(&inst);

    let order =
        game_load_order::order_in(&h.ctx, &inst.instance_id, &h.def, Some("plugins")).unwrap();
    assert!(
        order.findings.iter().any(|finding| matches!(
            finding,
            Finding::MasterCycle { plugins } if plugins == &["A.esp".to_string(), "B.esp".to_string()]
        )),
        "{:?}",
        order.findings
    );
}

#[test]
fn a_rule_naming_a_missing_item_is_reported_not_ignored() {
    let h = Harness::with_hooks(
        vec![script_rule("plugins", "Script.txt", "script")],
        ScriptHooks {
            rules: vec![LoadOrderRule::Before {
                first: "Ghost.esp".into(),
                second: "A.esp".into(),
            }],
            ..ScriptHooks::default()
        },
    );
    let inst = h.instance("missing-item");
    let item = h.item("mod", &[("Data/A.esp", b"a")]);
    h.add(&inst, &item);
    h.deploy(&inst);

    let order =
        game_load_order::order_in(&h.ctx, &inst.instance_id, &h.def, Some("plugins")).unwrap();
    assert!(
        order.findings.iter().any(|finding| matches!(
            finding,
            Finding::MissingRuleItem { item, .. } if item == "Ghost.esp"
        )),
        "{:?}",
        order.findings
    );
}

#[test]
fn lines_with_no_active_prefix_treats_all_as_active() {
    let mut rule = lines_rule("plugins", "Plugins.txt");
    rule.format = LoadOrderFormat::Lines {
        active_prefix: String::new(),
        header: Vec::new(),
    };
    let h = Harness::new(vec![rule]);
    let inst = h.instance("all-active");
    let item = h.item("mod", &[("Data/A.esp", b"a"), ("Data/B.esp", b"b")]);
    h.add(&inst, &item);
    h.deploy(&inst);

    assert_eq!(h.bytes(&inst, "Plugins.txt"), b"A.esp\r\nB.esp\r\n");
    let list = game_plugins::list_in(&h.ctx, &inst.instance_id, &h.def, Some("plugins")).unwrap();
    assert!(list.entries.iter().all(|entry| entry.active));
}

#[test]
fn a_failed_render_leaves_the_file_untouched() {
    let h = Harness::with_hooks(
        vec![script_rule("plugins", "Script.txt", "none")],
        ScriptHooks {
            fail_render: true,
            ..ScriptHooks::default()
        },
    );
    let inst = h.instance("failed-render");
    let before = b"A.esp\ttrue\nB.esp\tfalse\n";
    h.write(&inst, "Script.txt", before);

    let err = game_plugins::set_active_in(
        &h.ctx,
        &inst.instance_id,
        &h.def,
        Some("plugins"),
        "B.esp",
        true,
    )
    .unwrap_err();
    assert!(matches!(err, PluginListError::Format { .. }), "{err}");
    assert_eq!(h.bytes(&inst, "Script.txt"), before);
}

#[test]
fn a_script_format_round_trips_through_a_test_double() {
    let h = Harness::new(vec![script_rule("plugins", "Script.txt", "none")]);
    let inst = h.instance("script-format");
    h.write(&inst, "Script.txt", b"A.esp\ttrue\nB.esp\tfalse\n");

    let list = game_plugins::list_in(&h.ctx, &inst.instance_id, &h.def, Some("plugins")).unwrap();
    assert_eq!(
        list.entries
            .iter()
            .map(|entry| (entry.name.as_str(), entry.active))
            .collect::<Vec<_>>(),
        [("A.esp", true), ("B.esp", false)]
    );

    game_plugins::write_order_locked_in(
        &h.ctx,
        &inst.instance_id,
        &h.def,
        Some("plugins"),
        &[("B.esp".into(), true), ("A.esp".into(), false)],
    )
    .unwrap();
    assert_eq!(h.bytes(&inst, "Script.txt"), b"B.esp\ttrue\nA.esp\tfalse\n");

    let again = game_plugins::list_in(&h.ctx, &inst.instance_id, &h.def, Some("plugins")).unwrap();
    assert_eq!(names_of(&again), ["B.esp", "A.esp"]);
    assert!(!again.entries[1].active);
}

fn names_of(list: &game_plugins::PluginList) -> Vec<String> {
    list.entries
        .iter()
        .map(|entry| entry.name.clone())
        .collect()
}

#[test]
fn a_render_hook_cannot_choose_the_output_path() {
    let h = Harness::new(vec![script_rule("plugins", "Script.txt", "none")]);
    let inst = h.instance("no-path");
    game_plugins::import_list_in(
        &h.ctx,
        &inst.instance_id,
        &h.def,
        Some("plugins"),
        &[("A.esp".into(), true)],
        &[],
        &[],
    )
    .unwrap();

    // The hook returns bytes only; core writes them to the list's declared file. Nothing the hook
    // returns can name another file, and no other file appears in the instance folder.
    assert_eq!(h.bytes(&inst, "Script.txt"), b"A.esp\ttrue\n");
    let files: Vec<String> = std::fs::read_dir(h.instance_dir(&inst))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert!(files.iter().all(|name| name != "A.esp\t true"));
    assert!(files.iter().any(|name| name == "user"));
}

#[test]
fn folder_items_list_folders_not_files() {
    let mut rule = lines_rule("mods", "mods.txt");
    rule.items.folder = RelPath::new("mods").unwrap();
    rule.items.patterns = vec!["*".into()];
    rule.items.folders = true;
    let h = Harness::new(vec![rule]);
    let inst = h.instance("folders");
    let item = h.item(
        "mod",
        &[
            ("mods/Alpha/mod.json", b"{}"),
            ("mods/Beta/mod.json", b"{}"),
        ],
    );
    h.add(&inst, &item);
    h.deploy(&inst);

    assert_eq!(
        h.bytes(&inst, "mods.txt"),
        b"# header\r\n*Alpha\r\n*Beta\r\n"
    );
    let order = game_load_order::order_in(&h.ctx, &inst.instance_id, &h.def, Some("mods")).unwrap();
    assert_eq!(names(&order.entries), ["Alpha", "Beta"]);
    assert!(order.entries.iter().all(|entry| entry.present));
}
