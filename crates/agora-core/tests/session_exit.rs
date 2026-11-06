//! Observing how a game's own process ends (MASTER_SPEC §30.6), with no game.
//!
//! `agora-session-crasher` stands in for the game and for `skse64_loader.exe`. Job objects are
//! Windows-only, so the loader-first cases that need to find the game in a job are `#[cfg(windows)]`;
//! the cases where the launched process is the game run everywhere, and the loader-first case on
//! other platforms asserts the honest answer: the exit could not be observed.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use agora_core::game_launch::{
    session_images, wait_for_exit_watching, LaunchedGame, SessionExitReport,
};
use agora_core::process_identity;
use agora_core::process_job;
#[cfg(windows)]
use agora_core::session_ledger::MatchedBy;
use agora_core::session_ledger::{verdict, ExitObservation, SessionVerdict, VerdictInput};
use tempfile::TempDir;

fn crasher() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_agora-session-crasher"))
}

fn exe_name(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}

/// A private folder holding copies of the crasher under the given names, so a loader and a game
/// have different image names, as they do for Skyrim.
struct Rig {
    dir: TempDir,
}

impl Rig {
    fn new(names: &[&str]) -> Self {
        let dir = tempfile::Builder::new()
            .prefix(&format!("agora-session-{}-", std::process::id()))
            .tempdir()
            .unwrap();
        for name in names {
            std::fs::copy(crasher(), dir.path().join(exe_name(name))).unwrap();
        }
        Self { dir }
    }

    fn exe(&self, name: &str) -> PathBuf {
        self.dir.path().join(exe_name(name))
    }

    fn flag(&self, name: &str) -> PathBuf {
        self.dir.path().join(format!("{name}.flag"))
    }

    fn raise(&self, name: &str) {
        std::fs::write(self.flag(name), b"").unwrap();
    }

    fn wait_for(&self, name: &str) {
        let path = self.flag(name);
        let deadline = Instant::now() + Duration::from_secs(20);
        while !path.exists() {
            assert!(Instant::now() < deadline, "{name} never appeared");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Start `exe` the way a launch does: in a job (on Windows), with its identity captured.
fn start(exe: &Path, args: &[String]) -> LaunchedGame {
    let mut command = Command::new(exe);
    command
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let (child, job) = process_job::spawn_in_job(&mut command).expect("the crasher starts");
    let mut identity = process_identity::capture(child.id()).expect("its identity");
    identity.job = job;
    LaunchedGame {
        child,
        identity,
        program: exe.to_path_buf(),
        output: None,
    }
}

fn args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|p| p.to_string()).collect()
}

fn watch(
    dir: &Path,
    images: &[&str],
    mut launched: LaunchedGame,
) -> std::thread::JoinHandle<SessionExitReport> {
    let dir = dir.to_path_buf();
    let images: Vec<String> = images.iter().map(|i| exe_name(i)).collect();
    std::thread::spawn(move || {
        wait_for_exit_watching(
            &dir,
            &mut launched,
            &images,
            Duration::from_millis(20),
            Duration::from_millis(300),
        )
    })
}

/// Run `exe_base` with `mode` to its end under a watch for `images`. The process is held until the
/// watch has started, so its identity can be captured however quickly it would have ended.
fn run_to_end(rig: &Rig, exe_base: &str, mode: &[&str], images: &[&str]) -> SessionExitReport {
    let mut full = args(&["--hold-until"]);
    full.push(rig.flag("go").display().to_string());
    full.extend(mode.iter().map(|m| m.to_string()));
    let launched = start(&rig.exe(exe_base), &full);
    let handle = watch(rig.dir.path(), images, launched);
    rig.raise("go");
    handle.join().unwrap()
}

fn judge(report: &SessionExitReport, stop_requested: bool) -> SessionVerdict {
    verdict(&VerdictInput {
        exit: &report.exit,
        stop_requested,
        runtime_ms: 600_000,
        normal_exit_codes: &[0],
        artifact: None,
        froze: false,
    })
}

fn code_of(report: &SessionExitReport) -> u32 {
    match &report.exit {
        ExitObservation::Code { code, .. } => *code,
        other => panic!("expected an exit code, got {other:?}"),
    }
}

// -- the launched process is the game (everywhere) ---------------------------------------------

#[test]
fn a_normal_quit_is_read_as_exit_zero() {
    let rig = Rig::new(&["game"]);
    let report = run_to_end(&rig, "game", &["exit", "0"], &["game"]);
    assert_eq!(code_of(&report), 0);
    assert_eq!(judge(&report, false), SessionVerdict::Normal);
}

#[test]
fn exit_one_is_recorded_as_one_and_is_not_called_a_crash() {
    let rig = Rig::new(&["game"]);
    let report = run_to_end(&rig, "game", &["exit", "1"], &["game"]);
    assert_eq!(code_of(&report), 1);
    assert_ne!(judge(&report, false), SessionVerdict::Normal);
    assert!(!matches!(
        judge(&report, false),
        SessionVerdict::Crashed { .. }
    ));
}

#[test]
fn a_stop_requested_during_the_run_is_stopped_whatever_the_code() {
    let rig = Rig::new(&["game"]);
    let report = run_to_end(&rig, "game", &["exit", "1"], &["game"]);
    // The same observation, with and without the player's Stop.
    assert_eq!(judge(&report, true), SessionVerdict::Stopped);
    assert_ne!(judge(&report, false), SessionVerdict::Stopped);
}

#[test]
fn an_exit_before_the_game_process_is_seen_claims_nothing() {
    // The launched program exits at once with 3 and never starts a game.
    let rig = Rig::new(&["loader", "game"]);
    let report = run_to_end(&rig, "loader", &["exit", "3"], &["game"]);
    let ExitObservation::Unavailable { reason } = &report.exit else {
        panic!(
            "a game that was never seen has no exit code: {:?}",
            report.exit
        );
    };
    // It says what was observed: which program ended, with what, and what was not seen.
    assert!(reason.contains(&exe_name("game")), "{reason}");
    assert!(reason.contains("exit code 3"), "{reason}");
    assert_eq!(judge(&report, false), SessionVerdict::ClosedUnknown);
}

#[test]
fn a_launched_process_that_is_not_the_game_never_gives_its_code_to_the_game() {
    // Watching for "game", but the launched program is the loader and exits 0: that 0 is the
    // loader's, so the session must not become Normal.
    let rig = Rig::new(&["loader", "game"]);
    let report = run_to_end(&rig, "loader", &["exit", "0"], &["game"]);
    assert!(matches!(report.exit, ExitObservation::Unavailable { .. }));
    assert_ne!(judge(&report, false), SessionVerdict::Normal);
}

#[test]
fn an_empty_image_list_watches_nothing_and_claims_nothing() {
    let rig = Rig::new(&["loader"]);
    let report = run_to_end(&rig, "loader", &["exit", "0"], &[]);
    assert!(matches!(report.exit, ExitObservation::Unavailable { .. }));
    assert_ne!(judge(&report, false), SessionVerdict::Normal);
}

#[test]
fn the_games_process_names_come_from_data_then_the_recipe() {
    use agora_game_api::{GamePath, LaunchRecipe, RelPath};
    let mut def = common::skyrim_definition();
    let loader = Path::new("skse64_loader.exe");
    // No recipe, nothing declared: the launched program is all there is.
    assert_eq!(session_images(&def, loader), vec!["skse64_loader.exe"]);
    // The recipe's own executable is the game, not the loader that replaced it.
    def.launch = Some(LaunchRecipe {
        executable: GamePath::Runtime {
            path: RelPath::new("SkyrimSE.exe").unwrap(),
        },
        arguments: vec![],
        environment: Default::default(),
        working_directory: GamePath::Runtime {
            path: RelPath::default(),
        },
    });
    assert_eq!(session_images(&def, loader), vec!["SkyrimSE.exe"]);
    // Data wins over the recipe.
    def.session.processes = vec!["SkyrimVR.exe".into()];
    assert_eq!(session_images(&def, loader), vec!["SkyrimVR.exe"]);
    assert_eq!(def.session.normal_codes(), vec![0]);
    def.session.normal_exit_codes = vec![0, 3];
    assert_eq!(def.session.normal_codes(), vec![0, 3]);
}

// -- the loader exits first (the SKSE shape) ----------------------------------------------------

/// Start a loader that starts the game and exits, then run it to the end. `game_args` is how the
/// game ends once released. Returns the report.
fn skse_shape(game_args: &[&str], watch_dir_is_game_dir: bool) -> SessionExitReport {
    let rig = Rig::new(&["loader", "game"]);
    let elsewhere = TempDir::new().unwrap();
    let mut loader_args = args(&["--hold-until"]);
    loader_args.push(rig.flag("loader_go").display().to_string());
    loader_args.push("spawn".into());
    loader_args.push(rig.exe("game").display().to_string());
    loader_args.extend(args(&["--ready"]));
    loader_args.push(rig.flag("game_ready").display().to_string());
    loader_args.extend(args(&["--hold-until"]));
    loader_args.push(rig.flag("game_go").display().to_string());
    loader_args.extend(game_args.iter().map(|a| a.to_string()));

    let launched = start(&rig.exe("loader"), &loader_args);
    let dir = if watch_dir_is_game_dir {
        rig.dir.path().to_path_buf()
    } else {
        elsewhere.path().to_path_buf()
    };
    // The game is already in the job when the watch starts, so its first poll holds it; the
    // watch's later polls may be seconds apart under load, and a game that comes and goes between
    // two of them would otherwise be missed.
    rig.wait_for("game_ready");
    let handle = watch(&dir, &["game"], launched);

    // The loader has done its work and goes; the game keeps running.
    rig.raise("loader_go");
    std::thread::sleep(Duration::from_millis(700));
    assert!(
        !handle.is_finished(),
        "the watch ended when the loader did, while the game was still running"
    );
    rig.raise("game_go");
    handle.join().unwrap()
}

#[test]
#[cfg(windows)]
fn a_loader_that_exits_first_does_not_decide_the_verdict() {
    // The loader exited 0; the game then exited 7. The verdict follows the game.
    let report = skse_shape(&["exit", "7"], true);
    let ExitObservation::Code { code, image, .. } = &report.exit else {
        panic!("{:?}", report.exit);
    };
    assert_eq!(*code, 7);
    assert!(image.eq_ignore_ascii_case(&exe_name("game")), "{image}");
    assert_ne!(judge(&report, false), SessionVerdict::Normal);
    assert!(report.game_runtime_ms.is_some());
}

#[test]
#[cfg(windows)]
fn a_game_that_crashes_after_its_loader_exited_zero_is_a_crash() {
    let report = skse_shape(&["av"], true);
    assert_eq!(code_of(&report), 0xC000_0005);
    assert_eq!(
        judge(&report, false),
        SessionVerdict::Crashed {
            how: MatchedBy::ExitStatus
        }
    );
}

#[test]
#[cfg(windows)]
fn a_game_that_quits_normally_after_its_loader_exited_is_normal() {
    let report = skse_shape(&["exit", "0"], true);
    assert_eq!(code_of(&report), 0);
    assert_eq!(judge(&report, false), SessionVerdict::Normal);
}

#[test]
#[cfg(windows)]
fn the_watch_waits_for_a_game_running_outside_the_watched_folder() {
    // A store relaunch or a game outside the deployment folder: nothing runs from `dir`, but the
    // game Agora holds is still going.
    let report = skse_shape(&["exit", "9"], false);
    assert_eq!(code_of(&report), 9);
}

#[test]
#[cfg(not(windows))]
fn a_loader_that_exits_first_is_unobservable_where_there_are_no_jobs() {
    let report = skse_shape(&["exit", "0"], true);
    let ExitObservation::Unavailable { reason } = &report.exit else {
        panic!("{:?}", report.exit);
    };
    assert!(reason.contains("no job object"), "{reason}");
    assert_ne!(judge(&report, false), SessionVerdict::Normal);
}

// -- Stop (Windows: jobs) ----------------------------------------------------------------------

#[test]
#[cfg(windows)]
fn agoras_stop_ends_the_game_with_its_own_code_not_a_crashs() {
    let rig = Rig::new(&["game"]);
    let launched = start(
        &rig.exe("game"),
        &[
            "--ready".into(),
            rig.flag("ready").display().to_string(),
            "--hold-until".into(),
            rig.flag("never").display().to_string(),
            "exit".into(),
            "0".into(),
        ],
    );
    let job = launched.identity.job.clone().expect("a job");
    let handle = watch(rig.dir.path(), &["game"], launched);
    rig.wait_for("ready");
    std::thread::sleep(Duration::from_millis(200));
    assert!(process_job::terminate(&job).expect("terminated") >= 1);
    let report = handle.join().unwrap();
    assert_eq!(code_of(&report), process_job::STOP_EXIT_CODE);
    assert_ne!(
        code_of(&report),
        1,
        "a Stop must not look like a crash's exit 1"
    );
    // Judged on the code alone, with no flag at all, it is still a Stop.
    assert_eq!(judge(&report, false), SessionVerdict::Stopped);
}

#[test]
#[cfg(windows)]
fn the_fallback_stop_also_uses_the_stop_code() {
    let rig = Rig::new(&["game"]);
    let mut command = Command::new(rig.exe("game"));
    command
        .args(["--hold-until"])
        .arg(rig.flag("never"))
        .arg("exit")
        .arg("0");
    let mut child = command.spawn().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert!(process_job::terminate_process_for_stop(child.id()));
    let status = child.wait().unwrap();
    assert_eq!(
        status.code().map(|c| c as u32),
        Some(process_job::STOP_EXIT_CODE)
    );
}

// -- two games, one image name; recycled pids (Windows: jobs) ------------------------------------

#[test]
#[cfg(windows)]
fn two_games_with_one_image_name_each_report_their_own_exit() {
    let a = Rig::new(&["game"]);
    let b = Rig::new(&["game"]);
    let stranger = Rig::new(&["game"]);
    let hold_args = |rig: &Rig, code: &str| -> Vec<String> {
        vec![
            "--ready".into(),
            rig.flag("ready").display().to_string(),
            "--hold-until".into(),
            rig.flag("go").display().to_string(),
            "exit".into(),
            code.into(),
        ]
    };
    let launched_a = start(&a.exe("game"), &hold_args(&a, "6"));
    let launched_b = start(&b.exe("game"), &hold_args(&b, "5"));
    let job_a = launched_a.identity.job.clone().unwrap();
    let pid_a = launched_a.pid();
    let pid_b = launched_b.pid();
    // A third copy with the same image name that is in no job at all (another instance, or one the
    // player started themselves).
    let mut outsider = Command::new(stranger.exe("game"))
        .arg("--hold-until")
        .arg(stranger.flag("go"))
        .arg("exit")
        .arg("9")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let images = [exe_name("game")];
    // Held by membership, not by name: only A's own process is found in A's job.
    let held = process_job::hold_game_processes(&job_a, &images, &[]);
    assert_eq!(
        held.iter().map(|h| h.pid()).collect::<Vec<_>>(),
        vec![pid_a]
    );
    assert!(
        process_job::hold_game_processes(&job_a, &images, &[pid_a]).is_empty(),
        "a pid already held or ended is never taken again, so a recycled pid is a stranger"
    );
    assert_ne!(outsider.id(), pid_a);
    assert_ne!(outsider.id(), pid_b);
    drop(held);

    let watch_a = watch(a.dir.path(), &["game"], launched_a);
    let watch_b = watch(b.dir.path(), &["game"], launched_b);
    a.wait_for("ready");
    b.wait_for("ready");
    std::thread::sleep(Duration::from_millis(300));
    b.raise("go");
    let report_b = watch_b.join().unwrap();
    assert_eq!(code_of(&report_b), 5);
    assert!(!watch_a.is_finished(), "A is still running");
    a.raise("go");
    let report_a = watch_a.join().unwrap();
    assert_eq!(code_of(&report_a), 6);
    stranger.raise("go");
    let _ = outsider.wait();

    for (report, pid) in [(&report_a, pid_a), (&report_b, pid_b)] {
        let ExitObservation::Code { pid: seen, .. } = &report.exit else {
            panic!("{:?}", report.exit);
        };
        assert_eq!(*seen, pid);
    }
}

/// Agora's Stop ended the game, but the exit watcher still held a handle to it, so the process
/// table kept listing it and restoring the session refused: "cannot restore while session is
/// running". An ended process is not running, whoever still holds its handle.
#[test]
fn an_ended_process_someone_still_holds_is_not_running() {
    let dir = TempDir::new().unwrap();
    let exe = dir.path().join(crasher().file_name().unwrap());
    std::fs::copy(crasher(), &exe).unwrap();

    let mut held = Command::new(&exe)
        .args(["exit", "0"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    // `try_wait` sees the exit and keeps the handle open, as the watcher does.
    while held.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "the test program did not exit");
        std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(
        agora_core::game_launch::processes_running_from(dir.path()),
        Vec::new(),
        "an ended process counted as running"
    );
    drop(held);
}
