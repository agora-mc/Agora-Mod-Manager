//! A new instance, built aside and published whole (MASTER_SPEC §32.4, the whole-folder protocol).
//!
//! An importer builds the instance in a staging folder of its own below the instances root (same
//! disk, so publishing is one rename), then publishes it under its final name, then registers it.
//! Registration — the caller's database change (the instance's row) and this publication's receipt
//! in **one** transaction — is the commit point:
//!
//! - a crash before it leaves no instance: the next start removes the staging folder, or the
//!   published folder, which no row names and so nothing has used;
//! - a crash after it leaves the registered instance, and the next start only clears this
//!   protocol's own journal and marker from it.
//!
//! The journal and the ownership marker live **inside** the folder, so they move with it when it is
//! published, and the startup pass finds them while it looks at each instance folder anyway. The
//! folder's identity on disk is recorded when it is made; a rename keeps it, so recovery acts only
//! on the very folder this publication made, under either of its two names.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{
    database, forget_receipt, io, plain_directory, receipt_present, refused, StagedRecovery,
};
use crate::error::{LauncherError, LauncherResult};
use crate::lock_manager::{LockManager, LockResource};
use crate::process_identity;
use crate::recovery::{
    self, Decision, Envelope, JournalRead, JournalState, NoContentProof, OperationKind, OwnedPath,
    PathProof,
};

/// The publication's journal, inside the folder it describes.
pub const JOURNAL: &str = ".agora-new-instance.json";
/// The ownership marker, beside it.
const MARKER: &str = ".agora-new-instance.owner.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum State {
    /// Being built, or published and not yet registered: recovery removes it unless its receipt is
    /// in the database.
    Building,
}

impl JournalState for State {
    const KIND: OperationKind = OperationKind::NewInstance;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Journal {
    #[serde(flatten)]
    envelope: Envelope<State>,
    instance_id: String,
    staging: PathBuf,
    final_dir: PathBuf,
}

/// Start a new instance: create `staging` (which must not exist yet) below its instances root and
/// write the publication's marker and journal into it, before the caller puts anything there.
/// `final_dir` is where [`publish`] will move it. Returns the publication's operation id.
pub fn begin(staging: &Path, instance_id: &str, final_dir: &Path) -> LauncherResult<String> {
    if staging.parent().is_none() || staging.parent() != final_dir.parent() {
        return Err(refused(format!(
            "{} and {} must be folders side by side, so publishing is one rename",
            staging.display(),
            final_dir.display()
        )));
    }
    if let Some(parent) = staging.parent() {
        plain_directory(parent)?;
    }
    std::fs::create_dir(staging).map_err(|error| io(staging, error))?;
    recovery::sync_parent(staging).map_err(|error| io(staging, error))?;
    let resource = LockResource::Instance(instance_id.to_string());
    let mut envelope = Envelope::new(&resource, State::Building, Vec::new())?;
    let operation = envelope.operation_id.clone();
    recovery::write_marker(&staging.join(MARKER), &operation)?;
    let identity = recovery::path_identity(staging)
        .map_err(|error| io(staging, error))?
        .ok_or_else(|| refused(format!("{} vanished", staging.display())))?;
    // One folder under two names: each name is proven by the marker inside it, and both by the
    // folder's identity, which the rename keeps.
    let owned = |path: &Path| OwnedPath {
        path: path.to_path_buf(),
        proof: PathProof::MarkedIdentity {
            at: path.join(MARKER),
            identities: vec![identity],
        },
    };
    envelope.owned = vec![owned(staging), owned(final_dir)];
    let journal = Journal {
        envelope,
        instance_id: instance_id.to_string(),
        staging: staging.to_path_buf(),
        final_dir: final_dir.to_path_buf(),
    };
    recovery::write(&staging.join(JOURNAL), &journal)?;
    Ok(operation)
}

/// Give the built folder its final name. Refused, and nothing changed, when that name is taken.
/// The instance is not registered yet: until [`commit_with_receipt`] it is removed by recovery.
pub fn publish(staging: &Path, final_dir: &Path) -> LauncherResult<()> {
    let journal = read_journal(staging)?;
    if journal.final_dir != final_dir || journal.staging != staging {
        return Err(refused(format!(
            "{} was not begun to be published as {}",
            staging.display(),
            final_dir.display()
        )));
    }
    recovery::rename_directory(staging, final_dir).map_err(|error| LauncherError::Generic {
        code: "ERR_INSTANCE_EXISTS".into(),
        message: format!(
            "Could not publish {} as {}: {error}. No existing instance was changed.",
            staging.display(),
            final_dir.display()
        ),
    })
}

/// Register the published instance at `final_dir`: `change` (its row) and this publication's
/// receipt in one transaction on `conn`, the commit point. Then the journal and marker are cleared;
/// a crash during that is finished by recovery.
pub fn commit_with_receipt(
    final_dir: &Path,
    conn: &mut rusqlite::Connection,
    change: impl FnOnce(&rusqlite::Transaction<'_>) -> LauncherResult<()>,
) -> LauncherResult<()> {
    let operation = commit_receipt(final_dir, conn, change)?;
    // The instance is registered. Its cleanup failing changes nothing about that.
    if let Err(error) = clear(final_dir, conn, &operation) {
        eprintln!(
            "[new-instance] {} is registered; clearing its publication record is left for the next \
             start: {error}",
            final_dir.display()
        );
    }
    Ok(())
}

/// The commit point alone (the test seam for a crash right after it).
pub(crate) fn commit_receipt(
    final_dir: &Path,
    conn: &mut rusqlite::Connection,
    change: impl FnOnce(&rusqlite::Transaction<'_>) -> LauncherResult<()>,
) -> LauncherResult<String> {
    let journal = read_journal(final_dir)?;
    if journal.final_dir != final_dir {
        return Err(refused(format!(
            "{} is not the folder this publication made",
            final_dir.display()
        )));
    }
    let operation = journal.envelope.operation_id.clone();
    let tx = conn.transaction().map_err(database)?;
    change(&tx)?;
    tx.execute(
        "INSERT INTO staged_apply_receipts (operation_id, created_at) VALUES (?1, ?2)",
        rusqlite::params![operation, chrono::Utc::now().to_rfc3339()],
    )
    .map_err(database)?;
    tx.commit().map_err(database)?;
    Ok(operation)
}

/// Remove the publication's own files from a registered instance: journal first, then the marker
/// (a marker alone proves nothing and is harmless), then the receipt.
fn clear(final_dir: &Path, conn: &rusqlite::Connection, operation: &str) -> LauncherResult<()> {
    remove_if_there(&final_dir.join(JOURNAL))?;
    remove_if_there(&final_dir.join(MARKER))?;
    forget_receipt(conn, operation)
}

fn remove_if_there(path: &Path) -> LauncherResult<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io(path, error)),
    }
}

fn read_journal(folder: &Path) -> LauncherResult<Journal> {
    let path = folder.join(JOURNAL);
    match recovery::read::<State>(&path) {
        JournalRead::Valid(journal) => journal.payload::<Journal>().map_err(|problem| {
            refused(format!(
                "the publication record {} cannot be used: {problem:?}",
                path.display()
            ))
        }),
        JournalRead::Absent => Err(refused(format!(
            "{} has no publication record",
            folder.display()
        ))),
        other => Err(refused(format!(
            "the publication record {} cannot be used: {other:?}",
            path.display()
        ))),
    }
}

/// Whether `folder` (an entry of the instances root) holds a publication record.
pub(crate) fn has_journal(folder: &Path) -> bool {
    !matches!(
        std::fs::symlink_metadata(folder.join(JOURNAL)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    )
}

/// Settle the publication recorded in `folder`: a registered instance (its receipt is in `conn`)
/// keeps its folder and loses only the record; an unregistered one is removed, under either name.
/// Without the database it cannot be judged, and is left.
pub(crate) fn settle(
    locks: &LockManager,
    folder: &Path,
    conn: Option<&rusqlite::Connection>,
) -> StagedRecovery {
    let journal_path = folder.join(JOURNAL);
    let read = recovery::read::<State>(&journal_path);
    let journal: Journal = match &read {
        JournalRead::Valid(valid) => match valid.payload::<Journal>() {
            Ok(journal) => journal,
            Err(problem) => {
                return StagedRecovery::NeedsHand(format!(
                    "the publication record {} cannot be used: {problem:?}. Nothing was changed.",
                    journal_path.display()
                ))
            }
        },
        JournalRead::Absent => {
            return StagedRecovery::NeedsHand(format!(
                "{} disappeared while it was being read",
                journal_path.display()
            ))
        }
        other => {
            return StagedRecovery::NeedsHand(format!(
                "the publication record {} cannot be used: {other:?}. Nothing was changed.",
                journal_path.display()
            ))
        }
    };
    let operation = journal.envelope.operation_id.clone();
    // A record that names neither of the folder's names was copied or moved here: not ours to act on.
    if folder != journal.staging && folder != journal.final_dir {
        return StagedRecovery::NeedsHand(format!(
            "{} holds a publication record for {}, so Agora cannot tell it is its own. Nothing was \
             changed.",
            folder.display(),
            journal.final_dir.display()
        ));
    }
    let resource = LockResource::Instance(journal.instance_id.clone());
    let lock = match locks.acquire_with_timeout_typed(
        resource.clone(),
        "settling an interrupted new instance",
        std::time::Duration::ZERO,
        None,
    ) {
        Ok(lock) => lock,
        Err(crate::lock_manager::LockRefusal::Held(error)) => {
            return StagedRecovery::Busy(error.to_string())
        }
        Err(crate::lock_manager::LockRefusal::Unavailable(error)) => {
            return StagedRecovery::NeedsHand(error.to_string())
        }
    };
    let decision = recovery::decide_holding_reading(
        &lock,
        &resource,
        Some(&journal_path),
        || read,
        &NoContentProof,
        &process_identity::OsProcesses,
        None,
    );
    let mut capability = match decision {
        Decision::Nothing => {
            return StagedRecovery::NeedsHand(format!(
                "{} disappeared while it was being read",
                journal_path.display()
            ))
        }
        Decision::Busy(reason) => return StagedRecovery::Busy(reason),
        Decision::NeedsHand(reason) => return StagedRecovery::NeedsHand(reason),
        Decision::Act(capability) => capability,
    };
    // The folder holding the record must be one this publication provably made; the other name
    // may simply not exist.
    if let Some((path, why)) = capability
        .left_alone()
        .iter()
        .find(|(path, _)| path.as_path() == folder || path.exists())
    {
        return StagedRecovery::NeedsHand(format!(
            "{} is not provably this publication's ({why}). Nothing was changed.",
            path.display()
        ));
    }
    let registered = match conn.map(|conn| receipt_present(conn, &operation)) {
        Some(Ok(present)) => present,
        Some(Err(error)) => {
            return StagedRecovery::NeedsHand(format!(
                "whether {} was registered is recorded in the local database, which could not be \
                 read: {error}. Nothing was changed.",
                journal.final_dir.display()
            ))
        }
        None => {
            return StagedRecovery::NeedsHand(format!(
                "whether {} was registered is recorded in the local database, which this recovery \
                 was not given. Nothing was changed.",
                journal.final_dir.display()
            ))
        }
    };
    if registered {
        if folder != journal.final_dir {
            return StagedRecovery::NeedsHand(format!(
                "{} is registered but was never given its final name {}. Nothing was changed.",
                folder.display(),
                journal.final_dir.display()
            ));
        }
        if let Some(conn) = conn {
            if let Err(error) = clear(folder, conn, &operation) {
                return StagedRecovery::NeedsHand(error.to_string());
            }
        }
        return StagedRecovery::Finished { operation };
    }
    // Never registered: no row names it, so nothing has used it. Remove it, whichever name it has.
    if let Err(error) = capability.remove_dir(folder) {
        return StagedRecovery::NeedsHand(error.to_string());
    }
    if let Err(error) = capability.finish() {
        // The folder (and the record inside it) is gone; only a marker may be left elsewhere.
        eprintln!("[new-instance] {}: {error}", folder.display());
    }
    StagedRecovery::RolledBack {
        operation,
        left_at: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::Ctx;

    fn fixture() -> (tempfile::TempDir, Ctx, rusqlite::Connection) {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::for_testing(dir.path().join("app"));
        crate::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
        std::fs::create_dir_all(ctx.paths.instances_root()).unwrap();
        let conn = crate::db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
        conn.execute_batch("CREATE TABLE test_instances (id TEXT PRIMARY KEY)")
            .unwrap();
        (dir, ctx, conn)
    }

    fn names(ctx: &Ctx) -> (PathBuf, PathBuf) {
        let root = ctx.paths.instances_root();
        (root.join(".agora-import-new-x"), root.join("new"))
    }

    fn register(tx: &rusqlite::Transaction<'_>) -> LauncherResult<()> {
        tx.execute("INSERT INTO test_instances (id) VALUES ('new')", [])
            .map(|_| ())
            .map_err(database)
    }

    fn registered(conn: &rusqlite::Connection) -> bool {
        conn.query_row("SELECT COUNT(*) FROM test_instances", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap()
            == 1
    }

    fn receipts(conn: &rusqlite::Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM staged_apply_receipts", [], |row| {
            row.get(0)
        })
        .unwrap()
    }

    fn built(ctx: &Ctx) -> (PathBuf, PathBuf) {
        let (staging, final_dir) = names(ctx);
        begin(&staging, "new", &final_dir).unwrap();
        std::fs::create_dir_all(staging.join("mods")).unwrap();
        std::fs::write(staging.join("mods/a.jar"), b"a").unwrap();
        (staging, final_dir)
    }

    /// The startup pass, as the next Agora runs it: the process that wrote the record has ended.
    /// (A record whose writer is still running, this process included, is never acted on.)
    fn startup(ctx: &Ctx) -> (Vec<String>, Vec<String>) {
        for entry in std::fs::read_dir(ctx.paths.instances_root()).unwrap() {
            let journal = entry.unwrap().path().join(JOURNAL);
            if journal.exists() {
                let mut value: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&journal).unwrap()).unwrap();
                value["writer"]["pid"] = serde_json::json!(u32::MAX - 1);
                std::fs::write(&journal, serde_json::to_vec(&value).unwrap()).unwrap();
            }
        }
        super::super::recover_instances(ctx)
    }

    #[test]
    fn a_publication_whose_writer_is_still_running_is_left_alone() {
        let (_dir, ctx, _conn) = fixture();
        let (staging, final_dir) = built(&ctx);
        publish(&staging, &final_dir).unwrap();

        let (_actions, warnings) = super::super::recover_instances(&ctx);

        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            final_dir.join("mods/a.jar").exists(),
            "a live import is never removed"
        );
    }

    #[test]
    fn an_instance_is_published_then_registered_and_leaves_no_record() {
        let (_dir, ctx, mut conn) = fixture();
        let (staging, final_dir) = built(&ctx);

        publish(&staging, &final_dir).unwrap();
        commit_with_receipt(&final_dir, &mut conn, register).unwrap();

        assert_eq!(std::fs::read(final_dir.join("mods/a.jar")).unwrap(), b"a");
        assert!(!staging.exists());
        assert!(!final_dir.join(JOURNAL).exists() && !final_dir.join(MARKER).exists());
        assert!(registered(&conn));
        assert_eq!(receipts(&conn), 0);
    }

    #[test]
    fn a_crash_while_building_leaves_nothing_after_the_next_start() {
        let (_dir, ctx, conn) = fixture();
        let (staging, final_dir) = built(&ctx);

        let (actions, warnings) = startup(&ctx);

        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert!(!staging.exists() && !final_dir.exists());
        assert!(!registered(&conn));
    }

    #[test]
    fn a_crash_after_publishing_before_registering_leaves_nothing() {
        let (_dir, ctx, conn) = fixture();
        let (staging, final_dir) = built(&ctx);
        publish(&staging, &final_dir).unwrap();

        let (_actions, warnings) = startup(&ctx);

        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(
            !final_dir.exists(),
            "an unregistered instance does not hold its name"
        );
        assert!(!registered(&conn));
    }

    #[test]
    fn a_crash_right_after_registering_keeps_the_instance() {
        let (_dir, ctx, mut conn) = fixture();
        let (staging, final_dir) = built(&ctx);
        publish(&staging, &final_dir).unwrap();
        commit_receipt(&final_dir, &mut conn, register).unwrap();

        let (_actions, warnings) = startup(&ctx);

        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(std::fs::read(final_dir.join("mods/a.jar")).unwrap(), b"a");
        assert!(!final_dir.join(JOURNAL).exists() && !final_dir.join(MARKER).exists());
        assert!(registered(&conn));
        assert_eq!(receipts(&conn), 0);
    }

    #[test]
    fn without_the_database_an_interrupted_publication_is_left_alone() {
        let (_dir, ctx, _conn) = fixture();
        let (staging, final_dir) = built(&ctx);
        publish(&staging, &final_dir).unwrap();

        let outcome = settle(&ctx.lock_manager, &final_dir, None);

        assert!(
            matches!(outcome, StagedRecovery::NeedsHand(_)),
            "{outcome:?}"
        );
        assert!(final_dir.join("mods/a.jar").exists());
    }

    #[test]
    fn a_folder_whose_marker_is_not_this_publications_is_never_removed() {
        let (_dir, ctx, _conn) = fixture();
        let (staging, final_dir) = built(&ctx);
        publish(&staging, &final_dir).unwrap();
        recovery::write_marker(&final_dir.join(MARKER), "someone-else").unwrap();

        let (_actions, warnings) = startup(&ctx);

        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(final_dir.join("mods/a.jar").exists());
    }

    #[test]
    fn a_record_copied_into_another_folder_is_not_acted_on() {
        let (_dir, ctx, _conn) = fixture();
        let (staging, _final_dir) = built(&ctx);
        let other = ctx.paths.instances_root().join("someone-elses");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::copy(staging.join(JOURNAL), other.join(JOURNAL)).unwrap();
        std::fs::write(other.join("keep.txt"), b"keep").unwrap();

        let outcome = settle(&ctx.lock_manager, &other, None);

        assert!(
            matches!(outcome, StagedRecovery::NeedsHand(_)),
            "{outcome:?}"
        );
        assert!(other.join("keep.txt").exists());
    }

    #[test]
    fn publishing_over_an_existing_instance_is_refused() {
        let (_dir, ctx, _conn) = fixture();
        let (staging, final_dir) = built(&ctx);
        std::fs::create_dir_all(&final_dir).unwrap();
        std::fs::write(final_dir.join("theirs.txt"), b"theirs").unwrap();

        assert!(publish(&staging, &final_dir).is_err());

        assert_eq!(
            std::fs::read(final_dir.join("theirs.txt")).unwrap(),
            b"theirs"
        );
        assert!(staging.join("mods/a.jar").exists());
    }
}
