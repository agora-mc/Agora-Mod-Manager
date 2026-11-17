//! The Linux game view, run for real (MASTER_SPEC §26.5).
//!
//! These tests need unprivileged user namespaces and a Linux file system for the overlay's
//! upper and work folders. They are skipped, with the reason printed, only on a host that the
//! `AGORA_LINUX_VIEW_KNOWN_RESTRICTED` environment variable marks as known-restricted (CI's
//! Ubuntu job allows user namespaces instead; see the workflow). They never pass silently.
#![cfg(target_os = "linux")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agora_core::ctx::CoreContext;
use agora_core::game_base::{get_file_identity, BaseMode};
use agora_core::game_deploy::{add_content, deploy, deployment_dir, plan, DeployMode};
use agora_core::game_discovery::{DiscoveredInstall, InstallCapabilities};
use agora_core::game_instance::create;
use agora_core::game_registry::{
    GameRegistry, IdentifiedInstall, PackageSource, RuntimeResolution,
};
use agora_core::linux_view::{
    self, CommandSpec, OverlayMechanism, PerUserMount, ViewOutcome, ViewRequest, ViewRun,
    KNOWN_RESTRICTED_ENV, REFUSE_KERNEL_OVERLAY_ENV, REFUSE_OVERLAY_ENV,
};
use agora_game_api::{
    BaseReference, DeploymentStrategy, GameDefinition, GameId, GamePackage, GamePath, InstallId,
    InstallKind, LaunchRecipe, LaunchValue, PackageDefinition, RelPath, RuntimeIdentity, StoreId,
    StoreIdentifier,
};
use tempfile::TempDir;

/// The step-down test changes a process-wide environment variable, so the tests in this binary
/// take turns. Nextest runs each test in its own process anyway.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// Allocation tracking for the fork/exec child (L3).
//
// A process that forks while other threads hold the allocator lock must not allocate in the
// child. This allocator records, in a shared file, every allocation made by a pid other than the
// test process. The only such pid in a view run is the forked child before it execs, so a
// non-empty log is proof that the child allocated between `fork` and `execve`.
// ---------------------------------------------------------------------------

static TRACK_ALLOCATIONS: AtomicBool = AtomicBool::new(false);
static PARENT_PID: AtomicI32 = AtomicI32::new(0);
static ALLOCATION_LOG_FD: AtomicI32 = AtomicI32::new(-1);

unsafe extern "C" {
    #[link_name = "write"]
    fn raw_write(fd: i32, buf: *const std::ffi::c_void, count: usize) -> isize;
}

struct TrackingAllocator;

unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_if_child_allocates();
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
    }
}

#[global_allocator]
static ALLOCATOR: TrackingAllocator = TrackingAllocator;

fn record_if_child_allocates() {
    if !TRACK_ALLOCATIONS.load(Ordering::Relaxed) {
        return;
    }
    let parent = PARENT_PID.load(Ordering::Relaxed);
    if parent == 0 || std::process::id() as i32 == parent {
        return;
    }
    let fd = ALLOCATION_LOG_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        // A raw write, so recording an allocation cannot itself allocate.
        unsafe {
            raw_write(fd, b"x".as_ptr() as *const _, 1);
        }
    }
}

struct TestPackage(PackageDefinition);

impl GamePackage for TestPackage {
    fn definition(&self) -> &PackageDefinition {
        &self.0
    }
}

fn test_definition() -> GameDefinition {
    GameDefinition {
        mo2_game_name: None,
        session: Default::default(),
        id: GameId::new("linux-view-test").unwrap(),
        extends: None,
        name: "Linux View Test Game".into(),
        import_ids: Default::default(),
        stores: vec![StoreIdentifier {
            store: StoreId::new("steam").unwrap(),
            product: "12345".into(),
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
    let install_dir = tmp.path().join("install");
    std::fs::create_dir_all(&install_dir).unwrap();
    for (name, bytes) in [
        ("Game.exe", b"fake game binary".as_slice()),
        ("base_only.txt", b"base only".as_slice()),
        ("shared.txt", b"base shared".as_slice()),
        ("delete_me.txt", b"base delete".as_slice()),
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
        install_id: InstallId::new("steam:12345").unwrap(),
        discovered: DiscoveredInstall {
            store,
            product: "12345".into(),
            name: def.name.clone(),
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

fn add_content_folder(ctx: &CoreContext, name: &str, files: &[(&str, &[u8])]) -> String {
    let folder = tempfile::Builder::new()
        .prefix("linux-view-mod-")
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

/// A pinned instance whose farm has a mod file over a base file, a base-only file, a mod-only
/// file and a file to delete, plus one file that exists only in the writable layer.
struct Fixture {
    _tmp: TempDir,
    ctx: CoreContext,
    def: GameDefinition,
    instance_id: String,
    base_location: PathBuf,
    content_objects: BTreeMap<String, PathBuf>,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::Builder::new()
            .prefix("agora-linux-view-")
            .tempdir_in("/tmp")
            .expect("a Linux temp folder under /tmp");
        let def = test_definition();
        let ctx = create_context(&tmp, &def);
        let install = install_fixture(&tmp, &def);
        let instance = create(
            &ctx,
            &install,
            &def,
            "Linux View",
            None,
            BaseMode::Linked,
            &|_| {},
        )
        .expect("create the instance");
        let item = add_content_folder(
            &ctx,
            "View Mod",
            &[
                ("shared.txt", b"mod shared"),
                ("mod_file.txt", b"mod only"),
                ("delete_me.txt", b"mod delete"),
            ],
        );
        add_content(&ctx, &instance.instance_id, &item, None, None).unwrap();
        let writable = tmp
            .path()
            .join("app_data")
            .join("instances")
            .join(&instance.instance_id);
        std::fs::create_dir_all(writable.join("writable")).unwrap();
        std::fs::write(
            writable.join("writable").join("written.txt"),
            b"only writable",
        )
        .unwrap();

        let base_id = match &instance.base {
            BaseReference::Pinned { id, .. } => id.clone(),
            BaseReference::Unpinned { .. } => panic!("the fixture instance must be pinned"),
        };
        let base: agora_core::game_base::BaseManifest = serde_json::from_str(
            &std::fs::read_to_string(ctx.paths.base_manifest_path(&base_id)).unwrap(),
        )
        .unwrap();
        let item_record = agora_core::content_store::get_item(&ctx, &item).unwrap();
        let content_objects = item_record
            .files
            .iter()
            .map(|file| {
                (
                    file.path.as_str().to_string(),
                    ctx.paths.content_object_path(&file.sha256),
                )
            })
            .collect();

        Self {
            _tmp: tmp,
            ctx,
            def,
            instance_id: instance.instance_id,
            base_location: base.location,
            content_objects,
        }
    }

    fn instance_dir(&self) -> PathBuf {
        self.ctx
            .paths
            .instance_dir(&self.instance_id)
            .expect("a valid instance id")
    }

    fn writable(&self) -> PathBuf {
        self.instance_dir().join("writable")
    }

    fn lower(&self) -> PathBuf {
        deploy(&self.ctx, &self.instance_id, &self.def, DeployMode::Virtual)
            .expect("deploy the virtual farm");
        deployment_dir(&self.ctx, &self.instance_id)
            .expect("read the deployment folder")
            .expect("the farm was just built")
    }

    fn content_object(&self, path: &str) -> &Path {
        self.content_objects
            .get(path)
            .map(PathBuf::as_path)
            .unwrap_or_else(|| panic!("no content object for {path}"))
    }

    fn command(&self, script: &str) -> CommandSpec {
        CommandSpec {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".to_string(), script.to_string()],
            environment: None,
            working_directory: None,
        }
    }
}

/// Take the `Ran` result, or fail loudly. Only a host explicitly marked known-restricted may
/// skip, and it prints why.
fn ran(outcome: ViewOutcome) -> Option<ViewRun> {
    match outcome {
        ViewOutcome::Ran(run) => Some(run),
        ViewOutcome::SteppedDown(step_down) => {
            let namespaces = step_down.reason.contains("user namespace")
                || step_down.reason.contains("uid")
                || step_down.reason.contains("gid");
            if namespaces && std::env::var_os(KNOWN_RESTRICTED_ENV).is_some() {
                eprintln!(
                    "SKIPPED on a host marked known-restricted by {KNOWN_RESTRICTED_ENV}: {}",
                    step_down.reason
                );
                None
            } else {
                panic!(
                    "the Linux view did not run ({}); set {KNOWN_RESTRICTED_ENV} only on a host \
                     known to disable unprivileged user namespaces, or fix the host",
                    step_down.reason
                );
            }
        }
    }
}

fn run_view(fixture: &Fixture, request: &ViewRequest) -> ViewOutcome {
    match linux_view::native().view_and_run(&fixture.ctx, &fixture.def, request) {
        Ok(outcome) => outcome,
        Err(error) => panic!("the view failed: {error}"),
    }
}

fn write(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, bytes).unwrap();
}

fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
}

#[test]
fn empty_input_is_an_error_never_a_clean_result() {
    let _serial = serial();
    let tmp = tempfile::Builder::new()
        .prefix("agora-linux-view-empty-")
        .tempdir_in("/tmp")
        .unwrap();
    let def = test_definition();
    let ctx = CoreContext::for_testing(tmp.path().join("app_data"));
    let missing = tmp.path().join("missing-target");
    let request = ViewRequest {
        instance_id: "no-such-instance".into(),
        target: missing.clone(),
        command: CommandSpec {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), "exit 0".into()],
            environment: None,
            working_directory: None,
        },
        per_user_files: vec![],
    };
    let error = linux_view::native()
        .view_and_run(&ctx, &def, &request)
        .expect_err("a missing target is not a view");
    assert!(
        matches!(error, linux_view::ViewError::InvalidTarget(ref path) if path == &missing),
        "{error}"
    );

    let target = tmp.path().join("target");
    std::fs::create_dir_all(&target).unwrap();
    let mut missing_copy = request.clone();
    missing_copy.target = target.clone();
    missing_copy.per_user_files = vec![PerUserMount {
        real_path: target.join("real.txt"),
        instance_copy: tmp.path().join("missing-copy.txt"),
    }];
    let error = linux_view::native()
        .view_and_run(&ctx, &def, &missing_copy)
        .expect_err("a missing per-user copy is not a view");
    assert!(
        matches!(error, linux_view::ViewError::UnreadablePerUserFile(_)),
        "{error}"
    );

    let mut no_program = request;
    no_program.target = target;
    no_program.command.program = PathBuf::new();
    let error = linux_view::native()
        .view_and_run(&ctx, &def, &no_program)
        .expect_err("an empty program is not a command");
    assert!(
        matches!(error, linux_view::ViewError::EmptyProgram),
        "{error}"
    );
}

#[test]
fn the_forked_child_allocates_nothing_before_it_execs() {
    let _serial = serial();
    let fixture = Fixture::new();
    let target = fixture._tmp.path().join("target");
    std::fs::create_dir_all(&target).unwrap();
    let request = ViewRequest {
        instance_id: fixture.instance_id.clone(),
        target,
        command: fixture.command("exit 0\n"),
        per_user_files: vec![],
    };

    let mut log = tempfile::tempfile().unwrap();
    ALLOCATION_LOG_FD.store(log.as_raw_fd(), Ordering::SeqCst);
    PARENT_PID.store(std::process::id() as i32, Ordering::SeqCst);
    TRACK_ALLOCATIONS.store(true, Ordering::SeqCst);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_view(&fixture, &request)
    }));
    TRACK_ALLOCATIONS.store(false, Ordering::SeqCst);
    ALLOCATION_LOG_FD.store(-1, Ordering::SeqCst);
    PARENT_PID.store(0, Ordering::SeqCst);

    use std::io::{Read, Seek, SeekFrom};
    log.seek(SeekFrom::Start(0)).unwrap();
    let mut recorded = Vec::new();
    log.read_to_end(&mut recorded).unwrap();
    assert!(
        recorded.is_empty(),
        "the forked child allocated {} time(s) between fork and exec",
        recorded.len()
    );
    // Still fail loudly on a host that cannot run the view, like the other tests.
    let _ = ran(outcome.expect("the view must not panic"));
}

#[test]
fn merged_view_and_write_isolation() {
    let _serial = serial();
    let fixture = Fixture::new();
    let out = fixture._tmp.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let real_user = fixture._tmp.path().join("user/Plugins.txt");
    write(&real_user, b"real per-user");
    let instance_user = fixture.instance_dir().join("user/Plugins.txt");
    write(&instance_user, b"instance per-user");

    let lower = fixture.lower();
    let base_before = read(&fixture.base_location.join("base_only.txt"));
    let lower_before = read(&lower.join("mod_file.txt"));
    let content_before = read(fixture.content_object("mod_file.txt"));
    let content_identity = get_file_identity(fixture.content_object("mod_file.txt")).unwrap();

    let target = fixture._tmp.path().join("target");
    std::fs::create_dir_all(&target).unwrap();
    let mut request = ViewRequest {
        instance_id: fixture.instance_id.clone(),
        target: target.clone(),
        command: fixture.command(concat!(
            "set -e\n",
            "cat \"$TARGET/shared.txt\" > \"$OUT/merged-shared\"\n",
            "cat \"$TARGET/base_only.txt\" > \"$OUT/merged-base\"\n",
            "cat \"$TARGET/mod_file.txt\" > \"$OUT/merged-mod\"\n",
            "cat \"$TARGET/written.txt\" > \"$OUT/merged-written\"\n",
            "cat \"$REAL\" > \"$OUT/merged-user\"\n",
            "printf 'changed base\\n' > \"$TARGET/base_only.txt\"\n",
            "printf 'changed mod\\n' > \"$TARGET/mod_file.txt\"\n",
            "printf 'new in upper\\n' > \"$TARGET/new_in_upper.txt\"\n",
            "rm \"$TARGET/delete_me.txt\"\n",
        )),
        per_user_files: vec![PerUserMount {
            real_path: real_user.clone(),
            instance_copy: instance_user.clone(),
        }],
    };
    request.command.environment = Some(vec![
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("TARGET".into(), target.display().to_string()),
        ("OUT".into(), out.display().to_string()),
        ("REAL".into(), real_user.display().to_string()),
    ]);

    let run = ran(run_view(&fixture, &request)).expect("the fixture host must run the view");
    eprintln!("linux view mechanism: {}", run.mechanism.as_str());
    assert!(run.status.success(), "the command failed: {:?}", run.status);

    assert_eq!(read(&out.join("merged-shared")), b"mod shared");
    assert_eq!(read(&out.join("merged-base")), b"base only");
    assert_eq!(read(&out.join("merged-mod")), b"mod only");
    assert_eq!(read(&out.join("merged-written")), b"only writable");
    assert_eq!(read(&out.join("merged-user")), b"instance per-user");

    // Writes copy up into the writable layer, including the two that came from a lower file.
    assert_eq!(
        read(&fixture.writable().join("base_only.txt")),
        b"changed base\n"
    );
    assert_eq!(
        read(&fixture.writable().join("mod_file.txt")),
        b"changed mod\n"
    );
    assert_eq!(
        read(&fixture.writable().join("new_in_upper.txt")),
        b"new in upper\n"
    );

    // The delete is a whiteout in the upper layer; the lower and base files are untouched.
    let whiteout = std::fs::symlink_metadata(fixture.writable().join("delete_me.txt"))
        .expect("a delete must leave a whiteout in the writable layer");
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    if whiteout.file_type().is_char_device() {
        assert_eq!(whiteout.rdev(), 0, "the whiteout must be device 0:0");
    } else {
        assert!(
            whiteout.file_type().is_file() && whiteout.len() == 0,
            "the upper entry is neither a char-device whiteout nor an empty marker: {whiteout:?}"
        );
    }
    assert_eq!(read(&lower.join("delete_me.txt")), b"mod delete");
    assert_eq!(
        read(&fixture.base_location.join("delete_me.txt")),
        b"base delete"
    );
    assert_eq!(read(&lower.join("base_only.txt")), b"base only");
    assert_eq!(read(&lower.join("mod_file.txt")), lower_before);
    assert_eq!(
        read(&fixture.base_location.join("base_only.txt")),
        base_before
    );
    assert_eq!(read(fixture.content_object("mod_file.txt")), content_before);
    assert_eq!(
        get_file_identity(fixture.content_object("mod_file.txt")).unwrap(),
        content_identity,
        "the content object was replaced instead of copied up"
    );
    assert_eq!(read(&real_user), b"real per-user");
}

/// True when `fuse-overlayfs` is somewhere on this process's `PATH`.
fn fuse_overlayfs_installed() -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            let candidate = dir.join("fuse-overlayfs");
            candidate.is_file()
        })
    })
}

#[test]
fn a_delete_inside_the_view_stays_deleted_in_a_links_deploy() {
    let _serial = serial();
    let fixture = Fixture::new();
    let out = fixture._tmp.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let target = fixture._tmp.path().join("target");
    std::fs::create_dir_all(&target).unwrap();

    let mut request = ViewRequest {
        instance_id: fixture.instance_id.clone(),
        target: target.clone(),
        command: fixture.command(
            "rm \"$TARGET/delete_me.txt\"
",
        ),
        per_user_files: vec![],
    };
    request.command.environment = Some(vec![
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("TARGET".into(), target.display().to_string()),
    ]);
    let run = ran(run_view(&fixture, &request)).expect("the fixture host must run the view");
    assert!(run.status.success(), "the command failed: {:?}", run.status);

    // Kernel overlayfs records the deletion in the writable layer as a char device 0:0.
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let whiteout = std::fs::symlink_metadata(fixture.writable().join("delete_me.txt"))
        .expect("a delete must leave a whiteout in the writable layer");
    assert!(
        whiteout.file_type().is_char_device() && whiteout.rdev() == 0,
        "expected a kernel overlay whiteout, got {whiteout:?}"
    );

    // Deploying the same instance with links (or copies) must not bring the file back.
    let links = plan(
        &fixture.ctx,
        &fixture.instance_id,
        &fixture.def,
        DeployMode::Links,
    )
    .unwrap();
    assert!(
        !links
            .files
            .iter()
            .any(|file| file.path.as_str() == "delete_me.txt"),
        "the file the game deleted inside the view came back in a links deploy"
    );
    assert!(links
        .files
        .iter()
        .any(|file| file.path.as_str() == "base_only.txt"));
}

#[test]
fn fuse_overlay_runs_when_the_kernel_refuses() {
    let _serial = serial();
    if !fuse_overlayfs_installed() {
        eprintln!("SKIPPED: fuse-overlayfs is not on PATH on this host");
        return;
    }
    struct ClearEnv;
    impl Drop for ClearEnv {
        fn drop(&mut self) {
            std::env::remove_var(REFUSE_KERNEL_OVERLAY_ENV);
        }
    }
    std::env::set_var(REFUSE_KERNEL_OVERLAY_ENV, "1");
    let _clear = ClearEnv;

    let fixture = Fixture::new();
    let out = fixture._tmp.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let target = fixture._tmp.path().join("target");
    write(&target.join("outside.txt"), b"outside");

    let lower = fixture.lower();
    let base_before = read(&fixture.base_location.join("base_only.txt"));
    let lower_before = read(&lower.join("mod_file.txt"));

    let mut request = ViewRequest {
        instance_id: fixture.instance_id.clone(),
        target: target.clone(),
        command: fixture.command(concat!(
            "set -e
",
            "cat \"$TARGET/shared.txt\" > \"$OUT/merged-shared\"
",
            "cat \"$TARGET/base_only.txt\" > \"$OUT/merged-base\"
",
            "cat \"$TARGET/mod_file.txt\" > \"$OUT/merged-mod\"
",
            "cat \"$TARGET/written.txt\" > \"$OUT/merged-written\"
",
            "printf 'changed base\n' > \"$TARGET/base_only.txt\"
",
            "printf 'new in upper\n' > \"$TARGET/new_in_upper.txt\"
",
            "rm \"$TARGET/delete_me.txt\"
",
        )),
        per_user_files: vec![],
    };
    request.command.environment = Some(vec![
        (
            "PATH".into(),
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
        ),
        ("TARGET".into(), target.display().to_string()),
        ("OUT".into(), out.display().to_string()),
    ]);

    let run = ran(run_view(&fixture, &request)).expect("the fixture host must run the view");
    assert_eq!(
        run.mechanism,
        OverlayMechanism::Fuse,
        "the kernel overlay was refused, so the fuse path must have mounted the view"
    );
    assert!(run.status.success(), "the command failed: {:?}", run.status);

    assert_eq!(read(&out.join("merged-shared")), b"mod shared");
    assert_eq!(read(&out.join("merged-base")), b"base only");
    assert_eq!(read(&out.join("merged-mod")), b"mod only");
    assert_eq!(read(&out.join("merged-written")), b"only writable");

    assert_eq!(
        read(&fixture.writable().join("base_only.txt")),
        b"changed base
"
    );
    assert_eq!(
        read(&fixture.writable().join("new_in_upper.txt")),
        b"new in upper
"
    );
    assert!(
        fixture.writable().join("delete_me.txt").exists(),
        "a delete must leave a whiteout in the writable layer"
    );
    assert_eq!(
        read(&fixture.base_location.join("base_only.txt")),
        base_before
    );
    assert_eq!(read(&lower.join("delete_me.txt")), b"mod delete");
    assert_eq!(read(&lower.join("mod_file.txt")), lower_before);
    assert_eq!(read(&target.join("outside.txt")), b"outside");
    assert!(
        !target.join("new_in_upper.txt").exists(),
        "the overlay was visible outside the namespace"
    );
}

#[test]
fn fuse_finds_its_mount_when_the_game_folder_has_a_space() {
    let _serial = serial();
    if !fuse_overlayfs_installed() {
        eprintln!("SKIPPED: fuse-overlayfs is not on PATH on this host");
        return;
    }
    struct ClearEnv;
    impl Drop for ClearEnv {
        fn drop(&mut self) {
            std::env::remove_var(REFUSE_KERNEL_OVERLAY_ENV);
        }
    }
    std::env::set_var(REFUSE_KERNEL_OVERLAY_ENV, "1");
    let _clear = ClearEnv;

    let fixture = Fixture::new();
    let out = fixture._tmp.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    // `/proc/self/mountinfo` escapes this space as `\040`; a check for the raw path never finds
    // the fuse mount, so the view would time out and step down instead.
    let target = fixture._tmp.path().join("a game folder");
    std::fs::create_dir_all(&target).unwrap();

    let mut request = ViewRequest {
        instance_id: fixture.instance_id.clone(),
        target: target.clone(),
        command: fixture.command("cat \"$TARGET/shared.txt\" > \"$OUT/read\"\n"),
        per_user_files: vec![],
    };
    request.command.environment = Some(vec![
        (
            "PATH".into(),
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
        ),
        ("TARGET".into(), target.display().to_string()),
        ("OUT".into(), out.display().to_string()),
    ]);

    let run = ran(run_view(&fixture, &request)).expect("the fixture host must run the view");
    assert_eq!(run.mechanism, OverlayMechanism::Fuse);
    assert!(run.status.success(), "the command failed: {:?}", run.status);
    assert_eq!(read(&out.join("read")), b"mod shared");
}

#[test]
fn target_is_unchanged_outside_the_namespace() {
    let _serial = serial();
    let fixture = Fixture::new();
    let out = fixture._tmp.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let target = fixture._tmp.path().join("target");
    write(&target.join("outside.txt"), b"outside");

    let request = ViewRequest {
        instance_id: fixture.instance_id.clone(),
        target: target.clone(),
        command: fixture.command(concat!(
            "printf started > \"$OUT/started\"\n",
            "while [ ! -e \"$OUT/release\" ]; do sleep 0.02; done\n",
            "printf inside > \"$TARGET/inside.txt\"\n",
            "printf seen > \"$OUT/seen\"\n",
        )),
        per_user_files: vec![],
    };
    let mut request = request;
    request.command.environment = Some(vec![
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("TARGET".into(), target.display().to_string()),
        ("OUT".into(), out.display().to_string()),
    ]);

    let ctx = fixture.ctx.clone();
    let def = fixture.def.clone();
    let handle = std::thread::spawn(move || {
        linux_view::native()
            .view_and_run(&ctx, &def, &request)
            .expect("the view must run")
    });

    let deadline = Instant::now() + Duration::from_secs(20);
    while !out.join("started").exists() {
        if Instant::now() > deadline {
            std::fs::write(out.join("release"), b"").unwrap();
            let _ = handle.join();
            panic!("the command never reported that it started");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(read(&target.join("outside.txt")), b"outside");
    assert!(
        !target.join("inside.txt").exists(),
        "the overlay was visible outside the namespace"
    );

    std::fs::write(out.join("release"), b"").unwrap();
    let outcome = handle.join().expect("the view thread");
    let run = ran(outcome).expect("the fixture host must run the view");
    assert!(run.status.success());
    assert_eq!(read(&target.join("outside.txt")), b"outside");
    assert!(
        !target.join("inside.txt").exists(),
        "the target changed after the view ended"
    );
    assert_eq!(read(&out.join("seen")), b"seen");
    assert_eq!(read(&fixture.writable().join("inside.txt")), b"inside");
}

/// The namespace's uid 0 is the player's own host uid, not host root. It is deliberately kept
/// (rather than mapping the player's uid directly) because overlay copy-up keeps a lower file's
/// read-only mode: content-store objects are 0444, and an unprivileged in-namespace uid cannot
/// write the copy-up, so a game could not change a mod's own config. This pins both facts, so a
/// change of mapping must also solve copy-up.
#[test]
fn the_view_maps_its_root_to_the_player_so_read_only_content_stays_writable() {
    let _serial = serial();
    let fixture = Fixture::new();
    let out = fixture._tmp.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let target = fixture._tmp.path().join("target");
    std::fs::create_dir_all(&target).unwrap();
    let target_display = target.display().to_string();

    let mut request = ViewRequest {
        instance_id: fixture.instance_id.clone(),
        target,
        command: fixture.command(concat!(
            "id -u > \"$OUT/uid\"\n",
            "printf changed > \"$TARGET/mod_file.txt\"\n",
        )),
        per_user_files: vec![],
    };
    request.command.environment = Some(vec![
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("OUT".into(), out.display().to_string()),
        ("TARGET".into(), target_display),
    ]);

    let run = ran(run_view(&fixture, &request)).expect("the fixture host must run the view");
    assert!(run.status.success(), "the command failed: {:?}", run.status);
    assert_eq!(
        String::from_utf8(read(&out.join("uid"))).unwrap().trim(),
        "0",
        "the game's uid inside the namespace changed; solve read-only copy-up first"
    );
    assert_eq!(read(&fixture.writable().join("mod_file.txt")), b"changed");
    assert_eq!(
        read(fixture.content_object("mod_file.txt")),
        b"mod only",
        "the read-only content object was written through its copy-up"
    );
}

#[test]
fn per_user_file_is_the_instance_copy_and_the_real_file_stays() {
    let _serial = serial();
    let fixture = Fixture::new();
    let out = fixture._tmp.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let target = fixture._tmp.path().join("target");
    std::fs::create_dir_all(&target).unwrap();
    let real_user = fixture._tmp.path().join("user/Plugins.txt");
    write(&real_user, b"real per-user");
    let instance_user = fixture.instance_dir().join("user/Plugins.txt");
    write(&instance_user, b"instance per-user");

    let mut request = ViewRequest {
        instance_id: fixture.instance_id.clone(),
        target: target.clone(),
        command: fixture.command(concat!(
            "cat \"$REAL\" > \"$OUT/read\"\n",
            "printf 'written by the game' > \"$REAL\"\n",
        )),
        per_user_files: vec![PerUserMount {
            real_path: real_user.clone(),
            instance_copy: instance_user.clone(),
        }],
    };
    request.command.environment = Some(vec![
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("OUT".into(), out.display().to_string()),
        ("REAL".into(), real_user.display().to_string()),
    ]);

    let run = ran(run_view(&fixture, &request)).expect("the fixture host must run the view");
    assert!(run.status.success());
    assert_eq!(read(&out.join("read")), b"instance per-user");
    assert_eq!(read(&real_user), b"real per-user");
    assert_eq!(read(&instance_user), b"written by the game");
}

#[test]
fn a_refused_overlay_reports_the_step_down_and_runs_nothing() {
    let _serial = serial();
    struct ClearEnv;
    impl Drop for ClearEnv {
        fn drop(&mut self) {
            std::env::remove_var(REFUSE_OVERLAY_ENV);
        }
    }
    std::env::set_var(REFUSE_OVERLAY_ENV, "1");
    let _clear = ClearEnv;

    let fixture = Fixture::new();
    let out = fixture._tmp.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let target = fixture._tmp.path().join("target");
    write(&target.join("outside.txt"), b"outside");
    let mut request = ViewRequest {
        instance_id: fixture.instance_id.clone(),
        target: target.clone(),
        command: fixture.command("printf ran > \"$OUT/ran\"\n"),
        per_user_files: vec![],
    };
    request.command.environment = Some(vec![
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("OUT".into(), out.display().to_string()),
    ]);

    let outcome = run_view(&fixture, &request);
    match outcome {
        ViewOutcome::SteppedDown(step_down) => {
            assert_eq!(step_down.next, DeployMode::Links);
            assert!(
                step_down.reason.contains(REFUSE_OVERLAY_ENV),
                "the reason must name the refusal: {}",
                step_down.reason
            );
        }
        ViewOutcome::Ran(run) => panic!(
            "the view ran (exit {:?}) even though the overlay was refused",
            run.status.code()
        ),
    }
    assert!(!out.join("ran").exists(), "the command ran without a view");
    assert_eq!(read(&target.join("outside.txt")), b"outside");
}
