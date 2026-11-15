//! Framework files built for another game version are refused before launch (MASTER_SPEC §26.6):
//! the same check for a deployed launch, a base launch and `games instance check`.

use std::path::PathBuf;
use std::sync::Arc;

use agora_core::ctx::CoreContext;
use agora_core::game_base::BaseMode;
use agora_core::game_deploy::{add_content, deployment_dir, set_content_enabled};
use agora_core::game_discovery::{DiscoveredInstall, InstallCapabilities};
use agora_core::game_instance::{
    create, launch_checks, prepare_launch_with, runtime_findings, GameInstanceRecord,
    InstanceError, LaunchOptions, VfsFailure,
};
use agora_core::game_launch::{
    prepare_base_launch_with, LaunchError, LaunchedGame, Launcher, PreparedLaunch,
};
use agora_core::game_registry::{
    GameRegistry, IdentifiedInstall, PackageSource, RuntimeResolution,
};
use agora_game_api::{
    BaseReference, DeploymentStrategy, FrameworkDefinition, FrameworkDetection, FrameworkId,
    FrameworkVersionSource, GameDefinition, GameId, GamePackage, GamePath, IniCheck,
    IniMinimumFrom, InstallId, InstallKind, LaunchRecipe, LaunchValue, LoadOrderDefinition,
    PackageDefinition, RelPath, RuntimeFileProblem, RuntimeFileRule, RuntimeIdentity, StoreId,
    StoreIdentifier, UserDataLocation, UserFileMapping, UserFileStrategy,
};
use tempfile::TempDir;

const VERSION: &str = "1.6.1170.0";

/// The SKSE rule Skyrim SE declares, so the tests exercise the same shape as the real game.
fn skse_rule() -> RuntimeFileRule {
    RuntimeFileRule {
        id: "skse".into(),
        name: "Skyrim Script Extender (SKSE)".into(),
        family: "skse64_*.dll".into(),
        expected: "skse64_{1}_{2}_{3}.dll".into(),
        applies_to: vec![],
        repair: "Install the SKSE build for Skyrim {version}".into(),
        when_present: None,
    }
}

/// A second framework rule, whose family is present but built for another version.
fn other_framework_rule() -> RuntimeFileRule {
    RuntimeFileRule {
        id: "other-framework".into(),
        name: "Other Framework".into(),
        family: "other_*.dll".into(),
        expected: "other_1.dll".into(),
        applies_to: vec![],
        repair: "Install the Other Framework for this version".into(),
        when_present: None,
    }
}

/// A plugin list rule with no engine semantics, so a check needs no plugin headers.
fn plugin_list_rule() -> LoadOrderDefinition {
    LoadOrderDefinition {
        id: "plugins".into(),
        name: "Plugins".into(),
        file: RelPath::new("user/Plugins.txt").unwrap(),
        destination: Default::default(),
        items: agora_game_api::LoadOrderItems {
            folder: RelPath::new("Data").unwrap(),
            patterns: vec!["*.esm".into(), "*.esl".into(), "*.esp".into()],
            folders: false,
        },
        format: agora_game_api::LoadOrderFormat::Lines {
            active_prefix: "*".into(),
            header: vec![],
        },
        rules: agora_game_api::LoadOrderRules {
            kind: "none".into(),
            entry: None,
            implicit: Vec::new(),
            implicit_list_file: None,
            sorter: None,
            overlaps: None,
        },
        intro: None,
        item_heading: None,
    }
}

/// Write the instance's plugin list with `plugins`, each active.
fn write_plugin_list(s: &Setup, plugins: &[&str]) {
    let path = s
        .ctx
        .paths
        .instance_dir(&s.instance.instance_id)
        .unwrap()
        .join("user")
        .join("Plugins.txt");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let lines: Vec<String> = plugins.iter().map(|name| format!("*{name}")).collect();
    std::fs::write(path, lines.join("\n")).unwrap();
}

fn make_test_definition() -> GameDefinition {
    GameDefinition {
        mo2_game_name: None,
        session: Default::default(),
        id: GameId::new("skyrim-se").unwrap(),
        extends: None,
        name: "Skyrim Special Edition".into(),
        stores: vec![StoreIdentifier {
            store: StoreId::new("steam").unwrap(),
            product: "489830".into(),
        }],
        version_sources: vec![],
        deployment: DeploymentStrategy::VirtualFileSystem,
        content_rules: vec![],
        framework_ids: vec![],
        tool_ids: vec![],
        launch: Some(LaunchRecipe {
            executable: GamePath::Runtime {
                path: RelPath::new("Game.exe").unwrap(),
            },
            arguments: vec![LaunchValue::Literal {
                value: "-test".into(),
            }],
            environment: Default::default(),
            working_directory: GamePath::Runtime {
                path: RelPath::default(),
            },
        }),
        user_files: vec![],
        save_paths: vec![],
        linked_archive_patterns: vec!["Data/*.bsa".into()],
        declared_writes: vec![],
        excluded_paths: vec![],
        load_orders: Vec::new(),
        launch_alternatives: Vec::new(),
        runtime_files: vec![skse_rule()],
        save_location: Vec::new(),
        ini_checks: Vec::new(),
        content_layout: None,
        copy_patterns: Vec::new(),
    }
}

struct TestPackage(PackageDefinition);

impl GamePackage for TestPackage {
    fn definition(&self) -> &PackageDefinition {
        &self.0
    }
}

fn create_test_context(tmp: &TempDir, def: &GameDefinition) -> CoreContext {
    create_test_context_with(tmp, def, Vec::new())
}

fn create_test_context_with(
    tmp: &TempDir,
    def: &GameDefinition,
    frameworks: Vec<FrameworkDefinition>,
) -> CoreContext {
    // These tests read INI files the user keeps under Documents. On a machine where that
    // location cannot be resolved (a bare Linux account has no XDG user dirs, for one), the
    // check reports that it could not run. Point the test at its own root so the check is
    // hermetic; the value is process-wide, so set it once (the tests run in threads).
    static USER_DATA_ROOT: std::sync::Once = std::sync::Once::new();
    USER_DATA_ROOT.call_once(|| {
        std::env::set_var(
            "AGORA_TEST_USER_DATA_ROOT",
            std::env::temp_dir().join("agora-runtime-files-user-data"),
        );
    });
    let ctx = CoreContext::for_testing(tmp.path().join("app_data"));
    agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
    let mut builder = GameRegistry::builder();
    let pkg_def = PackageDefinition {
        id: format!("test.{}", def.id),
        version: semver::Version::new(0, 1, 0),
        api_range: semver::VersionReq::parse(">=0.1, <0.2").unwrap(),
        parents: vec![],
        game_templates: std::collections::BTreeMap::new(),
        behaviours: None,
        games: vec![def.clone()],
        frameworks,
        tools: vec![],
    };
    builder
        .add(
            PackageSource::Compiled {
                crate_name: "test".into(),
            },
            Arc::new(TestPackage(pkg_def)),
        )
        .expect("register test package");
    ctx.with_games(Arc::new(builder.build()))
}

/// A Skyrim-like install at [`VERSION`] holding `extra_files` (relative paths) beside its game.
fn make_install(tmp: &TempDir, extra_files: &[&str]) -> IdentifiedInstall {
    let install_dir = tmp.path().join("install");
    std::fs::create_dir_all(install_dir.join("Data")).unwrap();
    std::fs::write(install_dir.join("Game.exe"), b"fake game binary").unwrap();
    std::fs::write(install_dir.join("Data").join("Skyrim.bsa"), b"BSA DATA").unwrap();
    for extra in extra_files {
        let path = install_dir.join(extra);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"fake framework file").unwrap();
    }

    let store = StoreId::new("steam").unwrap();
    let runtime = RuntimeIdentity {
        game: GameId::new("skyrim-se").unwrap(),
        store: store.clone(),
        version: VERSION.into(),
        build: None,
    };
    let volume =
        agora_core::game_discovery::volume::VolumeDetector::new().get_volume_info(&install_dir);
    IdentifiedInstall {
        game: runtime.game.clone(),
        install_id: InstallId::new("steam:489830").unwrap(),
        discovered: DiscoveredInstall {
            store,
            product: "489830".into(),
            name: "Skyrim Special Edition".into(),
            kind: InstallKind::BaseGame,
            parent_product: None,
            location: install_dir,
            store_version: Some(runtime.version.clone()),
            store_build: None,
            executables: vec!["Game.exe".into()],
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

struct Setup {
    _tmp: TempDir,
    ctx: CoreContext,
    def: GameDefinition,
    instance: GameInstanceRecord,
}

/// An instance at [`VERSION`] over an install holding `extra_install_files`.
fn setup(extra_install_files: &[&str]) -> Setup {
    let tmp = TempDir::new().unwrap();
    let def = make_test_definition();
    let ctx = create_test_context(&tmp, &def);
    let install = make_install(&tmp, extra_install_files);
    let instance = create(
        &ctx,
        &install,
        &def,
        "Runtime Files",
        None,
        BaseMode::Linked,
        &|_| {},
    )
    .expect("create instance");
    Setup {
        _tmp: tmp,
        ctx,
        def,
        instance,
    }
}

/// A content item holding `files` at the root of the game, added to the instance. Returns its id.
fn add_root_content(ctx: &CoreContext, instance_id: &str, name: &str, files: &[&str]) -> String {
    let mod_dir = tempfile::tempdir().unwrap();
    for file in files {
        std::fs::write(mod_dir.path().join(file), b"fake mod file").unwrap();
    }
    let outcome = agora_core::content_store::add_folder(ctx, mod_dir.path(), Some(name)).unwrap();
    let item_id = outcome.item().item_id.clone();
    add_content(ctx, instance_id, &item_id, None, None).unwrap();
    item_id
}

/// The virtual file system is not installed: a launch falls back to linked files.
struct NoVfs;

impl Launcher for NoVfs {
    fn locate_vfs_dll(&self) -> Result<PathBuf, String> {
        Err("no agora_vfs.dll in this test".into())
    }
    fn launch(&self, _prepared: &PreparedLaunch) -> Result<LaunchedGame, LaunchError> {
        Err(LaunchError::ProcessCapture(
            "tests do not start games".into(),
        ))
    }
}

/// The virtual file system is "found" (its path is never opened: preparing a launch does not
/// start anything), so a launch runs under it.
struct FakeVfs;

impl Launcher for FakeVfs {
    fn locate_vfs_dll(&self) -> Result<PathBuf, String> {
        Ok(PathBuf::from("agora_vfs_test_stub.dll"))
    }
    fn launch(&self, _prepared: &PreparedLaunch) -> Result<LaunchedGame, LaunchError> {
        Err(LaunchError::ProcessCapture(
            "tests do not start games".into(),
        ))
    }
}

fn options(launch_anyway: bool) -> LaunchOptions {
    LaunchOptions {
        launch_anyway,
        on_vfs_failure: VfsFailure::FallBack,
        ..Default::default()
    }
}

fn prepare(
    s: &Setup,
    launch_anyway: bool,
    launcher: &dyn Launcher,
) -> Result<PreparedLaunch, InstanceError> {
    prepare_launch_with(
        &s.ctx,
        &s.instance.instance_id,
        &s.def,
        options(launch_anyway),
        &agora_core::game_discovery::discover_all,
        launcher,
    )
}

fn assert_skse_mismatch(err: InstanceError, found: &str) {
    match err {
        InstanceError::LaunchError(LaunchError::RuntimeMismatch { findings }) => {
            assert_eq!(findings.len(), 1, "{findings:?}");
            let finding = &findings[0];
            assert_eq!(finding.rule_id, "skse");
            assert_eq!(finding.expected, "skse64_1_6_1170.dll");
            assert_eq!(finding.found, vec![found.to_string()]);
            assert_eq!(finding.problem, RuntimeFileProblem::WrongVersion);
        }
        other => panic!("expected RuntimeMismatch, got {other:?}"),
    }
}

#[test]
fn deployed_content_with_skse_for_another_version_refuses_the_launch() {
    let s = setup(&[]);
    add_root_content(
        &s.ctx,
        &s.instance.instance_id,
        "SKSE 1.6.1179",
        &["skse64_1_6_1179.dll"],
    );

    let err = prepare(&s, false, &NoVfs).unwrap_err();
    assert_skse_mismatch(err, "skse64_1_6_1179.dll");
}

#[test]
fn launch_anyway_carries_the_findings_on_the_prepared_launch() {
    let s = setup(&[]);
    add_root_content(
        &s.ctx,
        &s.instance.instance_id,
        "SKSE 1.6.1179",
        &["skse64_1_6_1179.dll"],
    );

    let prepared = prepare(&s, true, &NoVfs).expect("launch anyway proceeds");
    assert_eq!(prepared.runtime_findings.len(), 1);
    assert_eq!(prepared.runtime_findings[0].rule_id, "skse");
}

#[test]
fn a_file_in_the_writable_layer_counts_under_the_virtual_file_system() {
    let s = setup(&[]);
    // The game wrote this under the virtual file system: the plan leaves the writable layer out
    // in that mode, so the check has to find it there too.
    let writable = s
        .ctx
        .paths
        .instance_dir(&s.instance.instance_id)
        .unwrap()
        .join("writable");
    std::fs::create_dir_all(&writable).unwrap();
    std::fs::write(writable.join("skse64_1_6_1179.dll"), b"written by the game").unwrap();

    let err = prepare(&s, false, &FakeVfs).unwrap_err();
    assert_skse_mismatch(err, "skse64_1_6_1179.dll");
}

#[test]
fn a_clean_instance_launches_with_no_findings() {
    let s = setup(&[]);
    let prepared = prepare(&s, false, &NoVfs).expect("no framework to refuse");
    assert!(prepared.runtime_findings.is_empty());
    assert!(runtime_findings(&s.ctx, &s.instance.instance_id, &s.def)
        .unwrap()
        .is_empty());
}

#[test]
fn check_reports_what_launch_refuses_and_deploys_nothing() {
    let s = setup(&[]);
    add_root_content(
        &s.ctx,
        &s.instance.instance_id,
        "SKSE 1.6.1179",
        &["skse64_1_6_1179.dll"],
    );
    assert!(deployment_dir(&s.ctx, &s.instance.instance_id)
        .unwrap()
        .is_none());

    let findings = runtime_findings(&s.ctx, &s.instance.instance_id, &s.def).unwrap();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].rule_id, "skse");
    assert_eq!(findings[0].found, vec!["skse64_1_6_1179.dll"]);

    // Checking built nothing: the game folder is still not there.
    assert!(deployment_dir(&s.ctx, &s.instance.instance_id)
        .unwrap()
        .is_none());
}

#[test]
fn a_base_launch_with_skse_for_another_version_is_refused_and_can_be_forced() {
    // The base holds the mismatched SKSE, and nothing is deployed on top of it.
    let s = setup(&["skse64_1_6_1179.dll"]);
    let base_id = match &s.instance.base {
        BaseReference::Pinned { id, .. } => id.clone(),
        other => panic!("expected a pinned base, got {other:?}"),
    };
    let manifest_text = std::fs::read_to_string(s.ctx.paths.base_manifest_path(&base_id)).unwrap();
    let manifest: agora_core::game_base::BaseManifest =
        serde_json::from_str(&manifest_text).unwrap();
    assert_eq!(manifest.runtime.version, VERSION);

    match prepare_base_launch_with(&manifest, &s.def, false, false) {
        Err(LaunchError::RuntimeMismatch { findings }) => {
            assert_eq!(findings.len(), 1);
            assert_eq!(findings[0].found, vec!["skse64_1_6_1179.dll"]);
        }
        other => panic!("expected RuntimeMismatch, got {other:?}"),
    }

    let forced =
        prepare_base_launch_with(&manifest, &s.def, true, false).expect("launch anyway proceeds");
    assert_eq!(forced.runtime_findings.len(), 1);
}

#[test]
fn a_rule_that_cannot_be_checked_launches_with_a_warning_not_a_refusal() {
    // A definition bug: component {5} does not exist in a four-part version. The instance is
    // otherwise clean for this rule's family, so without the bug it would launch.
    let mut s = setup(&[]);
    s.def.runtime_files = vec![RuntimeFileRule {
        id: "broken".into(),
        name: "Broken rule".into(),
        family: "skse64_*.dll".into(),
        expected: "skse64_{1}_{2}_{5}.dll".into(),
        applies_to: vec![],
        repair: "Fix the game definition".into(),
        when_present: None,
    }];
    add_root_content(
        &s.ctx,
        &s.instance.instance_id,
        "SKSE 1.6.1179",
        &["skse64_1_6_1179.dll"],
    );

    let prepared =
        prepare(&s, false, &NoVfs).expect("a rule that cannot be checked does not refuse");
    assert_eq!(prepared.runtime_findings.len(), 1, "{prepared:?}");
    let finding = &prepared.runtime_findings[0];
    assert_eq!(finding.rule_id, "broken");
    assert!(matches!(
        finding.problem,
        RuntimeFileProblem::CannotCheck { .. }
    ));
    assert!(!finding.refuses_launch());
}

#[test]
fn a_disabled_content_layer_with_another_versions_skse_gives_no_finding() {
    let s = setup(&[]);
    let item_id = add_root_content(
        &s.ctx,
        &s.instance.instance_id,
        "SKSE 1.6.1179",
        &["skse64_1_6_1179.dll"],
    );
    set_content_enabled(&s.ctx, &s.instance.instance_id, &item_id, false).unwrap();

    assert!(runtime_findings(&s.ctx, &s.instance.instance_id, &s.def)
        .unwrap()
        .is_empty());
    let prepared = prepare(&s, false, &NoVfs).expect("the disabled layer is not in the game");
    assert!(prepared.runtime_findings.is_empty());
}

#[test]
fn launch_checks_list_the_frameworks_and_plugin_count_that_passed() {
    let mut s = setup(&["skse64_1_6_1170.dll"]);
    s.def.load_orders = vec![plugin_list_rule()];
    write_plugin_list(&s, &["A.esp", "B.esp"]);

    let checks = launch_checks(&s.ctx, &s.instance, &s.def).expect("the launch checks run");
    assert!(checks.runtime.is_empty(), "{:?}", checks.runtime);
    assert!(checks.load_order.is_empty(), "{:?}", checks.load_order);
    assert_eq!(checks.passed.len(), 2, "{:?}", checks.passed);
    assert_eq!(checks.passed[0].kind, "framework");
    assert_eq!(checks.passed[0].title, "Skyrim Script Extender (SKSE)");
    assert!(checks.passed[0].detail.contains("skse64_1_6_1170.dll"));
    assert_eq!(checks.passed[1].kind, "load_order");
    assert_eq!(checks.passed[1].title, "Plugin load order");
    assert_eq!(checks.passed[1].detail, "2 plugins in a valid order.");
}

#[test]
fn launch_checks_say_zero_plugins_rather_than_omitting_the_load_order() {
    let mut s = setup(&[]);
    s.def.runtime_files = Vec::new();
    s.def.load_orders = vec![plugin_list_rule()];

    let checks = launch_checks(&s.ctx, &s.instance, &s.def).expect("the launch checks run");
    assert_eq!(checks.passed.len(), 1, "{:?}", checks.passed);
    assert_eq!(checks.passed[0].kind, "load_order");
    assert_eq!(checks.passed[0].detail, "0 plugins in a valid order.");
}

#[test]
fn launch_checks_list_a_passing_framework_beside_a_blocking_one() {
    let mut s = setup(&["skse64_1_6_1170.dll", "other_2.dll"]);
    s.def.runtime_files = vec![skse_rule(), other_framework_rule()];

    let checks = launch_checks(&s.ctx, &s.instance, &s.def).expect("the launch checks run");
    assert_eq!(checks.runtime.len(), 1, "{:?}", checks.runtime);
    assert_eq!(checks.runtime[0].rule_id, "other-framework");
    assert_eq!(checks.passed.len(), 1, "{:?}", checks.passed);
    assert_eq!(checks.passed[0].kind, "framework");
    assert_eq!(checks.passed[0].title, "Skyrim Script Extender (SKSE)");
}

#[test]
fn launch_checks_with_nothing_declared_pass_nothing() {
    let mut s = setup(&[]);
    s.def.runtime_files = Vec::new();

    let checks = launch_checks(&s.ctx, &s.instance, &s.def).expect("the launch checks run");
    assert!(checks.passed.is_empty(), "{:?}", checks.passed);
}

#[test]
fn an_unpinned_instance_passes_nothing() {
    let tmp = TempDir::new().unwrap();
    let def = make_test_definition();
    let ctx = create_test_context(&tmp, &def);
    let mut install = make_install(&tmp, &["skse64_1_6_1170.dll"]);
    install.discovered.capabilities.executables_readable = false;
    let instance = create(
        &ctx,
        &install,
        &def,
        "Unpinned",
        None,
        BaseMode::Linked,
        &|_| {},
    )
    .expect("create unpinned instance");

    let checks = launch_checks(&ctx, &instance, &def).expect("the launch checks run");
    assert!(checks.passed.is_empty(), "{:?}", checks.passed);
}

#[test]
fn an_installed_framework_missing_a_required_file_warns_in_check_and_launch() {
    let tmp = TempDir::new().unwrap();
    let mut def = make_test_definition();
    def.framework_ids = vec![FrameworkId::new("skse").unwrap()];
    let framework = FrameworkDefinition {
        id: FrameworkId::new("skse").unwrap(),
        game: GameId::new("skyrim-se").unwrap(),
        name: "Skyrim Script Extender".into(),
        version: "2.2.6".into(),
        supported_runtimes: Vec::new(),
        required_frameworks: Vec::new(),
        content: vec!["catalog.skse".into()],
        launch: None,
        detection: Some(FrameworkDetection {
            marker: RelPath::new("skse64_loader.exe").unwrap(),
            required: vec![RelPath::new("Data/Scripts/SKSE.pex").unwrap()],
            version: Some(FrameworkVersionSource::PeFileVersion),
        }),
    };
    let ctx = create_test_context_with(&tmp, &def, vec![framework]);
    // The loader is there, as on the real install, but its Papyrus scripts are not.
    let install = make_install(&tmp, &["skse64_1_6_1170.dll", "skse64_loader.exe"]);
    let instance = create(
        &ctx,
        &install,
        &def,
        "Half SKSE",
        None,
        BaseMode::Linked,
        &|_| {},
    )
    .expect("create instance");
    let s = Setup {
        _tmp: tmp,
        ctx,
        def,
        instance,
    };

    let findings = runtime_findings(&s.ctx, &s.instance.instance_id, &s.def).unwrap();
    let missing = findings
        .iter()
        .find(|finding| finding.problem == RuntimeFileProblem::MissingFiles)
        .expect("the instance check names the missing framework files");
    assert_eq!(missing.rule_id, "skse");
    assert_eq!(missing.found, vec!["Data/Scripts/SKSE.pex"]);
    assert!(!missing.refuses_launch(), "a missing file only warns");
    assert!(
        !missing.repair_from_source,
        "the source install lacks the file too: {:?}",
        missing.repair
    );
    assert!(
        missing
            .repair
            .contains("Install Skyrim Script Extender so all of its files are present"),
        "{}",
        missing.repair
    );
    assert!(
        missing.repair.contains("catalog.skse"),
        "the framework's content key is shown: {}",
        missing.repair
    );

    let checks = launch_checks(&s.ctx, &s.instance, &s.def).expect("the launch checks run");
    assert!(
        checks
            .runtime
            .iter()
            .any(|finding| finding.problem == RuntimeFileProblem::MissingFiles),
        "{:?}",
        checks.runtime
    );

    let prepared =
        prepare(&s, false, &NoVfs).expect("a missing framework file does not refuse the launch");
    assert!(
        prepared
            .runtime_findings
            .iter()
            .any(|finding| finding.problem == RuntimeFileProblem::MissingFiles),
        "{:?}",
        prepared.runtime_findings
    );
}

#[test]
fn a_runtime_file_that_installed_plugins_need_is_reported_missing() {
    let mut s = setup(&["Data/SKSE/Plugins/EngineFixes.dll"]);
    s.def.runtime_files = vec![RuntimeFileRule {
        id: "address-library".into(),
        name: "Address Library for SKSE Plugins".into(),
        family: "Data/SKSE/Plugins/version*-*.bin".into(),
        expected: "Data/SKSE/Plugins/versionlib-{1}-{2}-{3}-{4}.bin".into(),
        applies_to: vec!["1.6.*".into()],
        repair: "Install the Address Library build for Skyrim {version}".into(),
        when_present: Some("Data/SKSE/Plugins/*.dll".into()),
    }];

    let findings = runtime_findings(&s.ctx, &s.instance.instance_id, &s.def).unwrap();
    let missing = findings
        .iter()
        .find(|finding| finding.problem == RuntimeFileProblem::Missing)
        .expect("the installed SKSE plugin makes the missing library speak");
    assert_eq!(
        missing.expected,
        "Data/SKSE/Plugins/versionlib-1-6-1170-0.bin"
    );
    assert!(
        missing
            .found
            .iter()
            .any(|file| file.ends_with("EngineFixes.dll")),
        "{:?}",
        missing.found
    );
    assert!(!missing.refuses_launch(), "a missing library only warns");

    let checks = launch_checks(&s.ctx, &s.instance, &s.def).expect("the launch checks run");
    assert!(
        checks
            .runtime
            .iter()
            .any(|finding| finding.problem == RuntimeFileProblem::Missing),
        "{:?}",
        checks.runtime
    );
}

#[test]
fn a_missing_framework_file_points_at_the_base_repair_when_the_install_has_it() {
    let tmp = TempDir::new().unwrap();
    let mut def = make_test_definition();
    def.framework_ids = vec![FrameworkId::new("skse").unwrap()];
    let framework = FrameworkDefinition {
        id: FrameworkId::new("skse").unwrap(),
        game: GameId::new("skyrim-se").unwrap(),
        name: "Skyrim Script Extender".into(),
        version: "2.2.6".into(),
        supported_runtimes: Vec::new(),
        required_frameworks: Vec::new(),
        content: Vec::new(),
        launch: None,
        detection: Some(FrameworkDetection {
            marker: RelPath::new("skse64_loader.exe").unwrap(),
            required: vec![RelPath::new("Data/Scripts/SKSE.pex").unwrap()],
            version: Some(FrameworkVersionSource::PeFileVersion),
        }),
    };
    let ctx = create_test_context_with(&tmp, &def, vec![framework]);
    let install = make_install(&tmp, &["skse64_1_6_1170.dll", "skse64_loader.exe"]);
    let source = install.discovered.location.clone();
    let instance = create(
        &ctx,
        &install,
        &def,
        "Copied SKSE",
        None,
        BaseMode::Copied,
        &|_| {},
    )
    .expect("create a copied instance");
    let base_id = match &instance.base {
        BaseReference::Pinned { id, .. } => id.clone(),
        other => panic!("expected a pinned base, got {other:?}"),
    };

    // The script arrives in the source install after the base was pinned, so the base has kept a
    // copy without it but a repair can still copy it.
    std::fs::create_dir_all(source.join("Data/Scripts")).unwrap();
    std::fs::write(source.join("Data/Scripts/SKSE.pex"), b"pex").unwrap();

    let findings = runtime_findings(&ctx, &instance.instance_id, &def).unwrap();
    let missing = findings
        .iter()
        .find(|finding| finding.problem == RuntimeFileProblem::MissingFiles)
        .expect("the instance check names the missing framework files");
    assert!(
        missing.repair_from_source,
        "the source install has the file: {:?}",
        missing.repair
    );
    assert!(
        missing
            .repair
            .contains(&format!("agora games base repair-framework {base_id} skse")),
        "{}",
        missing.repair
    );
}

/// The Skyrim INI rules the official package declares, added to a test definition that has the
/// per-user file mapping they read.
fn add_skyrim_ini_checks(def: &mut GameDefinition) {
    def.user_files = vec![UserFileMapping::new(
        GamePath::UserData {
            location: UserDataLocation::Documents,
            path: RelPath::new("My Games/Skyrim Special Edition/Skyrim.ini").unwrap(),
        },
        RelPath::new("user/Skyrim.ini").unwrap(),
        UserFileStrategy::JournaledSwap,
    )];
    def.ini_checks = vec![
        IniCheck {
            file: "user/Skyrim.ini".into(),
            section: "General".into(),
            key: "uGridsToLoad".into(),
            default: Some("5".into()),
            expect: Some("5".into()),
            one_of: Vec::new(),
            odd: true,
            at_least: None,
            at_least_from: None,
            message: "uGridsToLoad is {value}; 5 is the default and other values crash.".into(),
            source: "https://en.uesp.net/wiki/Skyrim_Mod:INI_Files".into(),
        },
        IniCheck {
            file: "user/Skyrim.ini".into(),
            section: "General".into(),
            key: "uExterior Cell Buffer".into(),
            default: Some("36".into()),
            expect: None,
            one_of: Vec::new(),
            odd: false,
            at_least: None,
            at_least_from: Some(IniMinimumFrom {
                section: "General".into(),
                key: "uGridsToLoad".into(),
                default: Some("5".into()),
                add: 1,
                square: true,
            }),
            message: "uExterior Cell Buffer is {value}; it must be at least {minimum}.".into(),
            source: "https://en.uesp.net/wiki/Skyrim_Mod:INI_Files".into(),
        },
    ];
}

fn write_instance_ini(s: &Setup, body: &str) {
    let path = s
        .ctx
        .paths
        .instance_dir(&s.instance.instance_id)
        .unwrap()
        .join("user")
        .join("Skyrim.ini");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

fn ini_messages(s: &Setup) -> Vec<String> {
    launch_checks(&s.ctx, &s.instance, &s.def)
        .expect("the launch checks run")
        .diagnostics
        .into_iter()
        .map(|diagnostic| diagnostic.message)
        .collect()
}

#[test]
fn a_risky_ugrids_value_warns_in_the_check_and_the_launch() {
    let mut s = setup(&[]);
    add_skyrim_ini_checks(&mut s.def);
    write_instance_ini(
        &s,
        "[General]\r\nuGridsToLoad=11\r\nuExterior Cell Buffer=144\r\n",
    );

    let messages = ini_messages(&s);
    assert!(
        messages
            .iter()
            .any(|message| message.contains("uGridsToLoad is 11")),
        "{messages:?}"
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("source: https://")),
        "{messages:?}"
    );

    let prepared = prepare(&s, false, &NoVfs).expect("the launch is prepared");
    assert!(
        prepared
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("uGridsToLoad is 11")),
        "{:?}",
        prepared.diagnostics
    );
}

#[test]
fn the_default_ugrids_and_buffer_are_quiet() {
    let mut s = setup(&[]);
    add_skyrim_ini_checks(&mut s.def);
    write_instance_ini(
        &s,
        "[General]\r\nuGridsToLoad=5\r\nuExterior Cell Buffer=36\r\n",
    );
    assert!(ini_messages(&s).is_empty(), "{:?}", ini_messages(&s));
}

#[test]
fn a_buffer_below_the_ugrids_square_warns_with_the_minimum() {
    let mut s = setup(&[]);
    add_skyrim_ini_checks(&mut s.def);
    write_instance_ini(
        &s,
        "[General]\r\nuGridsToLoad=7\r\nuExterior Cell Buffer=36\r\n",
    );
    let messages = ini_messages(&s);
    assert!(
        messages.iter().any(|message| {
            message.contains("uExterior Cell Buffer is 36") && message.contains("at least 64")
        }),
        "{messages:?}"
    );
}

#[test]
fn an_absent_ini_file_is_judged_by_the_declared_defaults() {
    let mut s = setup(&[]);
    add_skyrim_ini_checks(&mut s.def);
    // No copy is written: the defaults are 5 and 36, which agree.
    assert!(ini_messages(&s).is_empty(), "{:?}", ini_messages(&s));
}

#[test]
fn an_even_ugrids_value_is_reported_as_well_as_a_non_default_one() {
    let mut s = setup(&[]);
    add_skyrim_ini_checks(&mut s.def);
    write_instance_ini(
        &s,
        "[General]\r\nuGridsToLoad=6\r\nuExterior Cell Buffer=49\r\n",
    );
    let messages = ini_messages(&s);
    assert!(
        messages
            .iter()
            .any(|message| message.contains("uGridsToLoad is 6")),
        "{messages:?}"
    );
}

#[test]
fn a_definition_without_ini_checks_reports_none() {
    let s = setup(&[]);
    assert!(ini_messages(&s).is_empty());
}
