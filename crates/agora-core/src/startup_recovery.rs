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
//! 4. Interrupted staged applies in every instance ([`staged_apply::recover_instances`](crate::staged_apply::recover_instances)).
//! 5. Interrupted Recently deleted (trash) entries
//!    ([`game_instance::trash::recover_interrupted_reporting`]).
//! 6. Pre-B3a snapshot object migration ([`migrate_snapshot_objects`]).
//!
//! The one recovery that does NOT run here is the interrupted plugin install
//! ([`plugins::install::recover_interrupted_installs`]): it must finish before the plugin
//! records are read to build the context's game registry, so `CoreContext::initialize` runs it
//! during construction and returns its lines as startup warnings, which every adapter already
//! reports. It decides through [`crate::recovery::decide`], the one recovery decision every
//! journal moves onto (MASTER_SPEC §32.11). The local database's own recovery likewise runs when
//! the database is opened, inside context construction, and its result comes back in the startup
//! report.
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
//! * the writer holds that same lock for the whole operation: a tool swap holds the game/store
//!   user-files lock from before its journal is written, through the copy, the spawned tool's
//!   run and the post-exit capture, until the journal is cleared, so the windows in between
//!   (after engage before spawn, during post-exit capture) are covered by the lock, not only by
//!   the recorded process identities;
//! * where the journal also records process identities (tool swaps, user-files sessions), a live
//!   recorded process additionally refuses the recovery;
//! * where liveness is unknown — a journal with no recorded identity at all (a legacy journal,
//!   or a self-capture that failed) — recovery does not act on it automatically: it is reported
//!   as needing a hand, and the explicit restore command is the human override.
//!
//! Everything is reported, never raised: a journal that needs a hand comes back as a warning line,
//! and startup continues.

use crate::content_store::migrate::{migrate_snapshot_objects, MigrationOutcome};
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
    let mut report = run_before_snapshot_migration(ctx);
    recover_snapshot_objects(ctx, &mut report);
    report
}

/// Run steps 1–4 when the caller is handling the snapshot migration itself.
pub fn run_before_snapshot_migration(ctx: &Ctx) -> StartupRecoveryReport {
    let mut report = StartupRecoveryReport::default();

    // 1. Game packages (game-specific recovery stays behind the registry hook).
    report.warnings.extend(ctx.games.recover_at_startup(ctx));

    // 2. Interrupted tool swaps. Nowait: a journal whose lock another Agora process holds belongs
    //    to a live run and is left alone and reported.
    for recovery in crate::game_tool_swap::recover_all_nowait(ctx) {
        match recovery {
            Recovery::Restored(journal) => report.actions.push(format!(
                "Put back the real Data folder of {} after an interrupted run of {}.",
                journal.game, journal.tool
            )),
            Recovery::Stuck(message) => report
                .warnings
                .push(format!("A tool swap was not put back: {message}")),
        }
    }

    // 3. User-files sessions of runs that are no longer alive.
    let (actions, warnings) = crate::game_user_files::recover_finished_sessions(ctx);
    report.actions.extend(actions);
    report.warnings.extend(warnings);

    // 4. Interrupted staged applies (MASTER_SPEC §32.4), in every instance: before the commit point
    //    the old instance is put back, after it the cleanup is finished. Nowait: an instance
    //    another Agora is changing right now is left alone and reported.
    let (actions, warnings) = crate::staged_apply::recover_instances(ctx);
    report.actions.extend(actions);
    report.warnings.extend(warnings);

    // 5. Interrupted Recently deleted (trash) deletes and restores.
    let (actions, warnings) = crate::game_instance::trash::recover_interrupted_reporting(ctx);
    report.actions.extend(actions);
    report.warnings.extend(warnings);

    report
}

// 6. Move legacy snapshot objects while the content store is idle.
fn recover_snapshot_objects(ctx: &Ctx, report: &mut StartupRecoveryReport) {
    match migrate_snapshot_objects(ctx) {
        Ok(MigrationOutcome::NothingToDo) => {}
        Ok(MigrationOutcome::Busy) => report.warnings.push(
            "Snapshot object migration will retry on startup because the content store is busy."
                .into(),
        ),
        Ok(MigrationOutcome::Done(migration)) => {
            report.actions.push(format!(
                "Snapshot object migration: {} moved, {} already in the store (legacy copy removed), {} repaired, {} legacy entries kept.",
                migration.moved,
                migration.already_present,
                migration.replaced_damaged,
                migration.mismatched.len() + migration.left_alone.len()
            ));
            for path in migration.mismatched {
                report.warnings.push(format!(
                    "Kept legacy snapshot object {} because its SHA-256 differs from its name.",
                    path.display()
                ));
            }
            for (path, reason) in migration.left_alone {
                report.warnings.push(format!(
                    "Kept legacy snapshot entry {} ({reason}).",
                    path.display()
                ));
            }
        }
        Ok(MigrationOutcome::NeedsHand(reason)) => report
            .warnings
            .push(format!("Snapshot object migration needs a hand. {reason}")),
        Err(error) => report
            .warnings
            .push(format!("Snapshot object migration stopped. {error}")),
    }
}
