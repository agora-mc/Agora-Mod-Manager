//! Test seams, carried on the context (MASTER_SPEC §32.9).
//!
//! A seam is state a test installs to reach a window, a fault or a measurement inside production
//! code: a pause between two steps, a journal write made to fail, a volume reading stood in for.
//! Production never installs one, so an unset seam costs one cheap check.
//!
//! **A seam belongs to the context it was installed on.** Clones of a context share their seams, so
//! an operation that hands a clone to another thread still reaches its hook, on any thread. Two
//! contexts never share one. A seam in a process-wide `static` let one test's hook catch another
//! test's code, because plain `cargo test` runs a binary's tests as threads in one process; a seam
//! in a `thread_local!` could not reach code the operation ran on another thread. Both failed only
//! on the platform whose CI did not run one process per test.
//!
//! Every test that installs a seam asserts the seam fired: a pause that never triggers must fail
//! the test rather than pass it silently.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

pub use crate::game_tool_swap::{RunHook, VolumeSeam, WriteFaultHook};

/// The test seams of one context. Clone it to share them: an operation that carries a clone of its
/// context onto another thread carries the same seams.
#[derive(Clone, Default)]
pub struct TestSeams(Arc<Seams>);

/// The seam slots. Every slot is empty in production; a seam is read under a short lock (or an
/// atomic for the counters), which is far below the cost of the I/O each one sits beside.
///
/// Each installed hook also has an adjacent atomic saying it is installed, so the check a
/// production operation makes on every pass is one atomic load and no lock.
#[derive(Default)]
struct Seams {
    volume: Mutex<Option<VolumeSeam>>,
    volume_installed: AtomicBool,
    volume_hits: AtomicUsize,
    run_hook: Mutex<Option<RunHook>>,
    run_hook_installed: AtomicBool,
    run_hook_hits: AtomicUsize,
    write_fault: Mutex<Option<WriteFaultHook>>,
    write_fault_installed: AtomicBool,
    write_fault_hits: AtomicUsize,
    plan_calls: AtomicUsize,
    update_fault: Mutex<Option<usize>>,
    update_fault_armed: AtomicBool,
    update_stops: AtomicUsize,
    #[cfg(debug_assertions)]
    base_build_pauses: Mutex<Vec<Arc<BaseBuildPause>>>,
    #[cfg(debug_assertions)]
    create_pin_pause: PauseSlot,
    #[cfg(debug_assertions)]
    restore_rename_pause: PauseSlot,
    /// Where a plugin install should stop, so a test sees the files and the journal exactly as a
    /// killed process leaves them.
    #[cfg(debug_assertions)]
    install_crash: Mutex<Option<crate::plugins::install::InstallCrashPoint>>,
}

/// Lock a slot, taking the value back from a panic that left it poisoned: a seam is test state, and
/// a test that panicked must not poison every later seam of the same operation.
fn slot<T>(slot: &Mutex<T>) -> MutexGuard<'_, T> {
    slot.lock().unwrap_or_else(|error| error.into_inner())
}

// ---------------------------------------------------------------------------
// Volume
// ---------------------------------------------------------------------------

impl TestSeams {
    /// Stand in for the volume check and the free space of this context's operations, or clear it.
    pub fn set_volume_seam(&self, seam: Option<VolumeSeam>) {
        *slot(&self.0.volume) = seam;
        self.0
            .volume_installed
            .store(seam.is_some(), Ordering::Release);
    }

    /// The volume stand-in of this context, empty in production.
    ///
    /// One atomic load when no stand-in is installed; the lock is taken only when a test installed
    /// one, which also counts the hit. A test asserts that count went up: an installation the code
    /// never consulted is a dead seam, not a passing test.
    pub(crate) fn volume_seam(&self) -> VolumeSeam {
        if !self.0.volume_installed.load(Ordering::Acquire) {
            return VolumeSeam::default();
        }
        let seam = slot(&self.0.volume).unwrap_or_default();
        self.0.volume_hits.fetch_add(1, Ordering::Relaxed);
        seam
    }

    /// How many times an installed volume stand-in was read with this context.
    pub fn volume_hits(&self) -> usize {
        self.0.volume_hits.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// Tool swap hooks
// ---------------------------------------------------------------------------

impl TestSeams {
    /// Install the hook that fires at the swap's windows, or clear it.
    pub fn set_run_hook(&self, hook: Option<RunHook>) {
        let installed = hook.is_some();
        *slot(&self.0.run_hook) = hook;
        self.0
            .run_hook_installed
            .store(installed, Ordering::Release);
    }

    /// Clear the run hook.
    pub fn clear_run_hook(&self) {
        self.set_run_hook(None);
    }

    /// Install the hook that makes chosen journal writes fail, or clear it.
    pub fn set_write_fault_hook(&self, hook: Option<WriteFaultHook>) {
        let installed = hook.is_some();
        *slot(&self.0.write_fault) = hook;
        self.0
            .write_fault_installed
            .store(installed, Ordering::Release);
    }

    /// Clear the journal write-fault hook.
    pub fn clear_write_fault_hook(&self) {
        self.set_write_fault_hook(None);
    }

    /// The run hook of this context, cloned out so a hook may install or clear one. One atomic load
    /// when none is installed, and every time the hook is handed out counts one hit.
    pub(crate) fn run_hook(&self) -> Option<RunHook> {
        if !self.0.run_hook_installed.load(Ordering::Acquire) {
            return None;
        }
        let hook = slot(&self.0.run_hook).clone();
        if hook.is_some() {
            self.0.run_hook_hits.fetch_add(1, Ordering::Relaxed);
        }
        hook
    }

    /// How many times an installed run hook fired with this context.
    pub fn run_hook_hits(&self) -> usize {
        self.0.run_hook_hits.load(Ordering::Relaxed)
    }

    /// Whether a journal write to `path` fails right now. `false` unless this context's test
    /// installed a fault hook that names the path. One atomic load when none is installed; a write
    /// the hook fails counts one hit.
    pub fn write_fault_fires(&self, path: &Path) -> bool {
        if !self.0.write_fault_installed.load(Ordering::Acquire) {
            return false;
        }
        let hook = slot(&self.0.write_fault).clone();
        let fires = hook.is_some_and(|hook| hook(path));
        if fires {
            self.0.write_fault_hits.fetch_add(1, Ordering::Relaxed);
        }
        fires
    }

    /// How many journal writes an installed fault hook made fail with this context.
    pub fn write_fault_hits(&self) -> usize {
        self.0.write_fault_hits.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// Deploy
// ---------------------------------------------------------------------------

impl TestSeams {
    /// Count one run of the deployment planner.
    pub(crate) fn note_plan_call(&self) {
        self.0.plan_calls.fetch_add(1, Ordering::Relaxed);
    }

    /// How many times the planner has run with this context. The product never reads it; the
    /// benchmark and the launch tests count the plans one operation makes.
    pub fn plan_calls(&self) -> usize {
        self.0.plan_calls.load(Ordering::Relaxed)
    }

    /// The test seam for crash safety: the next in-place update with this context stops once it
    /// has made `changes` changes to the farm and leaves its marker behind, as a process stop
    /// would. `None` turns the seam off.
    pub fn set_update_fault_after(&self, changes: Option<usize>) {
        *slot(&self.0.update_fault) = changes;
        self.0
            .update_fault_armed
            .store(changes.is_some(), Ordering::Release);
    }

    /// Take the armed stop for one in-place update.
    ///
    /// The update that claims it is the only one the stop can reach: a second update run on a
    /// clone of the same context gets `None`, so one update's stops can never be spent by another
    /// (two instances updated at once share the context, not the stop).
    pub(crate) fn claim_update_fault(&self) -> Option<usize> {
        if !self.0.update_fault_armed.load(Ordering::Acquire) {
            return None;
        }
        let claimed = slot(&self.0.update_fault).take();
        if claimed.is_none() {
            self.0.update_fault_armed.store(false, Ordering::Release);
        }
        claimed
    }

    /// Count one stop the armed fault made.
    pub(crate) fn note_update_stop(&self) {
        self.0.update_stops.fetch_add(1, Ordering::Relaxed);
    }

    /// How many in-place updates the armed fault stopped with this context.
    pub fn update_stops(&self) -> usize {
        self.0.update_stops.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// Debug-only pauses
// ---------------------------------------------------------------------------

/// Where a base build's test pause fires.
#[cfg(debug_assertions)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BaseBuildPauseKind {
    /// Right after the folder is renamed into place.
    AfterRename,
    /// After the early pinning check of a damaged record, before its locked re-check.
    BeforePinRecheck,
}

#[cfg(debug_assertions)]
struct BaseBuildPause {
    kind: BaseBuildPauseKind,
    root: PathBuf,
    reached: AtomicBool,
}

/// A registered base-build pause. `reached` says the build arrived at its window and is waiting;
/// `release` stops asking and lets it go.
#[cfg(debug_assertions)]
pub struct BaseBuildPauseHandle {
    seams: TestSeams,
    pause: Arc<BaseBuildPause>,
}

#[cfg(debug_assertions)]
impl BaseBuildPauseHandle {
    /// Whether the registered build has reached its pause and is waiting.
    pub fn reached(&self) -> bool {
        self.pause.reached.load(Ordering::SeqCst)
    }

    /// Stop asking; the parked build, if any, continues from its pause.
    pub fn release(&self) {
        let mut pauses = slot(&self.seams.0.base_build_pauses);
        pauses.retain(|pause| !Arc::ptr_eq(pause, &self.pause));
    }
}

/// A pause armed on one name (a base id, an instance id) that a later step waits at and an
/// explicit call releases.
///
/// Keyed by name so a registration left behind by a finished test never parks an unrelated
/// operation in the same context.
#[cfg(debug_assertions)]
#[derive(Default)]
struct PauseSlot {
    target: Mutex<Option<String>>,
    reached: AtomicBool,
}

#[cfg(debug_assertions)]
impl PauseSlot {
    fn set(&self, target: Option<&str>) {
        *slot(&self.target) = target.map(str::to_string);
        self.reached.store(false, Ordering::SeqCst);
    }

    fn reached(&self) -> bool {
        self.reached.load(Ordering::SeqCst)
    }

    fn wait_if_target(&self, name: &str) {
        if !self.is_target(name) {
            return;
        }
        // Reached stays true for this arming: a test whose operation never arrived must assert it
        // and fail loudly.
        self.reached.store(true, Ordering::SeqCst);
        while self.is_target(name) {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        self.reached.store(false, Ordering::SeqCst);
    }

    fn is_target(&self, name: &str) -> bool {
        slot(&self.target).as_deref() == Some(name)
    }
}

#[cfg(debug_assertions)]
impl TestSeams {
    /// Test seam: make the next plugin install stop (panic) at `point`, the way a killed process
    /// leaves an install. `None` disarms it.
    ///
    /// A test asserts the seam fired: an install that never reached the point must fail the test,
    /// not pass silently.
    pub fn set_install_crash(&self, point: Option<crate::plugins::install::InstallCrashPoint>) {
        *slot(&self.0.install_crash) = point;
    }

    /// Stop the install here if this context asked it to.
    pub(crate) fn install_crash_point(&self, point: crate::plugins::install::InstallCrashPoint) {
        if *slot(&self.0.install_crash) == Some(point) {
            panic!("a test cut this plugin install short at {point:?}");
        }
    }

    /// Test seam: make the next base build under `base_root` stop right after it renames its
    /// folder into place, until the returned handle is released.
    pub fn pause_base_build_after_rename(&self, base_root: &Path) -> BaseBuildPauseHandle {
        self.register_base_build_pause(BaseBuildPauseKind::AfterRename, base_root)
    }

    /// Test seam: make the next base build under `base_root` stop after its early pinning check of
    /// a damaged record and before its locked re-check, until the returned handle is released.
    pub fn pause_base_build_before_pin_recheck(&self, base_root: &Path) -> BaseBuildPauseHandle {
        self.register_base_build_pause(BaseBuildPauseKind::BeforePinRecheck, base_root)
    }

    fn register_base_build_pause(
        &self,
        kind: BaseBuildPauseKind,
        base_root: &Path,
    ) -> BaseBuildPauseHandle {
        let pause = Arc::new(BaseBuildPause {
            kind,
            root: base_root.to_path_buf(),
            reached: AtomicBool::new(false),
        });
        slot(&self.0.base_build_pauses).push(pause.clone());
        BaseBuildPauseHandle {
            seams: self.clone(),
            pause,
        }
    }

    /// Stop the build here if this context registered a pause of `kind` under `base_root`.
    pub(crate) fn base_build_pause_point(&self, kind: BaseBuildPauseKind, base_root: &Path) {
        let pause = slot(&self.0.base_build_pauses)
            .iter()
            .find(|pause| pause.kind == kind && pause.root == base_root)
            .cloned();
        let Some(pause) = pause else {
            return;
        };
        // Reached stays true for this registration: a test whose build never arrived must assert
        // it and fail loudly.
        pause.reached.store(true, Ordering::SeqCst);
        loop {
            let still_asked = slot(&self.0.base_build_pauses)
                .iter()
                .any(|registered| Arc::ptr_eq(registered, &pause));
            if !still_asked {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Test seam: make the next create that pins `base_id` stop after choosing it, until this is
    /// called with `None`.
    pub fn set_create_pause_before_publishing_pin(&self, base_id: Option<&str>) {
        self.0.create_pin_pause.set(base_id);
    }

    /// Whether a paused create has reached the window and is waiting.
    pub fn create_pause_before_publishing_pin_reached(&self) -> bool {
        self.0.create_pin_pause.reached()
    }

    /// Stop the create here if this context asked it to.
    pub(crate) fn create_pin_pause_point(&self, base_id: &str) {
        self.0.create_pin_pause.wait_if_target(base_id);
    }

    /// Test seam: make the next restore of `instance_id` pause right after its rename, until this
    /// is called with `None`.
    pub fn set_restore_pause_after_rename_for(&self, instance_id: Option<&str>) {
        self.0.restore_rename_pause.set(instance_id);
    }

    /// Whether a paused restore has reached its rename and is waiting.
    pub fn restore_pause_after_rename_reached(&self) -> bool {
        self.0.restore_rename_pause.reached()
    }

    /// Stop the restore here if this context asked it to.
    pub(crate) fn restore_rename_pause_point(&self, instance_id: &str) {
        self.0.restore_rename_pause.wait_if_target(instance_id);
    }
}
