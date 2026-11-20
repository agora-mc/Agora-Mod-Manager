//! The one startup recovery pass (MASTER_SPEC §32.11).
//!
//! Several features keep a crash journal on disk so an operation cut off by a crash, power loss or
//! kill can be finished or undone the next time Agora starts. This module is the single entry
//! point that runs every one of those recoveries, in a fixed order, and reports what it did:
//! every adapter (CLI, desktop, MCP listener) calls [`run_at_startup`] exactly once at startup and
//! then reports the returned lines its own way — the CLI prints them, the desktop shows them with
//! its other startup notices, the MCP listener logs them to stderr.
//!
//! # What runs here, and in what order
//!
//! 1. The game packages' own recovery, behind the registry hook
//!    ([`GameRegistry::recover_at_startup`](crate::game_registry::GameRegistry::recover_at_startup)):
//!    game-specific recovery stays in the game's package (the Minecraft package recovers its
//!    interrupted launcher-import jobs there).
//! 2. Interrupted tool swaps ([`game_tool_swap::recover_all_nowait`]).
//! 3. User-files sessions of finished runs ([`game_user_files::recover_finished_sessions`]).
//! 4. Interrupted Recently deleted (trash) entries
//!    ([`game_instance::trash::recover_interrupted_reporting`]).
//!
//! The one recovery that does NOT run here is the interrupted plugin install
//! ([`plugins::install::recover_interrupted_installs`]): it must finish before the plugin
//! records are read to build the context's game registry, so `CoreContext::initialize` runs it
//! during construction and returns its lines as startup warnings, which every adapter already
//! reports. The local database's own recovery likewise runs when the database is opened, inside
//! context construction, and its result comes back in the startup report.
//!
//! Journals that are recovered lazily instead of here, on purpose: the content-store conversion
//! journal is finished at the next conversion of the same instance (it needs that instance's
//! manifest, and runs under its lock), and the snapshot restore journal at the next restore of the
//! same instance. Both are safe to leave until then: their files sit inside the instance folder
//! and nothing else reads them.
//!
//! # Two processes
//!
//! The CLI and the desktop can run at the same time, so recovery must never "recover" an operation
//! another live process is still in the middle of. Every pass here tells a dead operation from a
//! live one before touching anything:
//!
//! * it never waits on a lock: each journal's lock (the per-instance lock, the per-game/store
//!   user-files lock) is taken with a zero timeout, and a journal whose lock is held is **left
//!   alone and reported**, never recovered underneath the live holder — the OS releases the lock
//!   when the holder dies, so a crashed holder never blocks recovery;
//! * where the journal also records process identities (tool swaps, user-files sessions), a live
//!   recorded process additionally refuses the recovery.
//!
//! Everything is reported, never raised: a journal that needs a hand comes back as a warning line,
//! and startup continues.

use crate::ctx::Ctx;
use crate::game_tool_swap::Recovery;

/// What the startup recovery pass did and what it could not do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartupRecoveryReport {
    /// What recovery did — a swap put back, a session's files restored, an entry finished. One
    /// line each, ready to show.
    pub actions: Vec<String>,
    /// What recovery left alone or could not do, and why — a journal that needs a hand, or one
    /// whose lock a live operation in another process holds. One line each, ready to show.
    pub warnings: Vec<String>,
}

impl StartupRecoveryReport {
    /// Whether the pass found nothing to do and nothing to report.
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty() && self.warnings.is_empty()
    }
}

/// Run every startup recovery once, in the fixed order documented above. Called once at startup by
/// every adapter; the adapter reports the result its own way. Never raises: each journal's own
/// outcome is a line in the report.
pub fn run_at_startup(ctx: &Ctx) -> StartupRecoveryReport {
    let mut report = StartupRecoveryReport::default();

    // 1. Game packages (game-specific recovery stays behind the registry hook).
    report
        .warnings
        .extend(ctx.games.recover_at_startup(ctx));

    // 2. Interrupted tool swaps. Nowait: a journal whose lock another Agora process holds belongs
    //    to a live run and is left alone and reported.
    for recovery in crate::game_tool_swap::recover_all_nowait(ctx) {
        match recovery {
            Recovery::Restored(journal) => report.actions.push(format!(
                "Put back the real Data folder of {} after an interrupted run of {}.",
                journal.game, journal.tool
            )),
            Recovery::Stuck(message) => {
                report.warnings.push(format!("A tool swap was not put back: {message}"))
            }
        }
    }

    // 3. User-files sessions of runs that are no longer alive.
    let (actions, warnings) = crate::game_user_files::recover_finished_sessions(ctx);
    report.actions.extend(actions);
    report.warnings.extend(warnings);

    // 4. Interrupted Recently deleted (trash) deletes and restores.
    let (actions, warnings) = crate::game_instance::trash::recover_interrupted_reporting(ctx);
    report.actions.extend(actions);
    report.warnings.extend(warnings);

    report
}
