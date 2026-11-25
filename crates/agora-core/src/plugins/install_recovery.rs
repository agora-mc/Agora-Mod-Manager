// Plugin package swaps use the local database as the commit point.

use super::*;
use crate::app_paths::AppPaths;
use crate::ctx::Ctx;
use crate::lock_manager::{LockGuard, LockManager, LockResource};
use crate::process_identity::{OsProcesses, ProcessView};
use crate::recovery::{
    self, Decision, Envelope, JournalProblem, JournalRead, NoContentProof, OwnedPath, PathIdentity,
    PathProof, RecoveryCapability,
};
use rusqlite::{params, Connection, OptionalExtension};

type Capability<'a> = RecoveryCapability<'a, PluginInstallState>;

/// The old JSON location is still checked for interrupted swaps made by earlier builds.
pub fn install_journal_path(paths: &AppPaths, id: &PluginId) -> PathBuf {
    paths.plugin_rollback_root().join(format!("{id}.json"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PluginInstallState {
    Starting,
    Stashed,
    Extracted,
    Committed,
    /// The old record was restored. Recovery can finish the file moves.
    Restoring,
    Uninstalling,
    UninstallCommitted,
}

impl recovery::JournalState for PluginInstallState {
    const KIND: recovery::OperationKind = recovery::OperationKind::PluginInstall;
}

impl PluginInstallState {
    /// This state's own name, exactly as the journal writes it.
    pub fn as_str(self) -> &'static str {
        match self {
            PluginInstallState::Starting => "starting",
            PluginInstallState::Stashed => "stashed",
            PluginInstallState::Extracted => "extracted",
            PluginInstallState::Committed => "committed",
            PluginInstallState::Restoring => "restoring",
            PluginInstallState::Uninstalling => "uninstalling",
            PluginInstallState::UninstallCommitted => "uninstall-committed",
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(in crate::plugins) struct InstallJournal {
    #[serde(flatten)]
    envelope: Envelope<PluginInstallState>,
    plugin_id: PluginId,
    previous: Option<store::InstallSnapshot>,
    old: Option<PathIdentity>,
    #[serde(flatten)]
    change: PackageChange,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
enum PackageChange {
    Install {
        new: PathIdentity,
        new_version: String,
    },
    Uninstall {
        purge_data: bool,
        version: String,
        install_dir: String,
    },
}

struct Layout {
    destination: PathBuf,
    rollback: Option<PathBuf>,
    staging: PathBuf,
    aside: PathBuf,
    marker: PathBuf,
    kept: PathBuf,
    dropping: PathBuf,
}

impl Layout {
    fn new(
        paths: &AppPaths,
        id: &PluginId,
        operation: uuid::Uuid,
        had_files: bool,
        uninstall: bool,
    ) -> Self {
        let packages = paths.plugin_packages_root();
        let rollback = paths.plugin_rollback_root();
        Self {
            destination: package_dir(&packages, id),
            rollback: had_files.then(|| {
                if uninstall {
                    rollback.join(format!(".uninstalling-{id}-{operation}"))
                } else {
                    rollback.join(id.as_str())
                }
            }),
            staging: packages.join(format!(".staging-{operation}")),
            aside: rollback.join(format!(".replaced-{id}-{operation}")),
            marker: rollback.join(format!(".owner-{operation}.json")),
            kept: rollback.join(format!(".{id}-{operation}.kept")),
            dropping: rollback.join(format!(".dropping-{id}-{operation}")),
        }
    }

    fn ownership(&self, old: Option<PathIdentity>, change: &PackageChange) -> Vec<OwnedPath> {
        let marked = |path: &Path, identities| OwnedPath {
            path: path.to_path_buf(),
            proof: PathProof::MarkedIdentity {
                at: self.marker.clone(),
                identities,
            },
        };
        match change {
            PackageChange::Install { new, .. } => {
                let mut owned = vec![
                    marked(&self.destination, old.into_iter().chain([*new]).collect()),
                    marked(&self.staging, vec![*new]),
                    marked(&self.aside, vec![*new]),
                ];
                if let (Some(path), Some(old)) = (&self.rollback, old) {
                    owned.push(marked(path, vec![old]));
                    owned.push(marked(&self.kept, vec![old]));
                    owned.push(marked(&self.dropping, vec![old]));
                }
                owned
            }
            PackageChange::Uninstall { .. } => match (&self.rollback, old) {
                (Some(path), Some(old)) => vec![
                    marked(&self.destination, vec![old]),
                    marked(path, vec![old]),
                ],
                _ => Vec::new(),
            },
        }
    }
}

impl InstallJournal {
    pub(in crate::plugins) fn begin(
        conn: &Connection,
        paths: &AppPaths,
        id: &PluginId,
        version: String,
    ) -> LauncherResult<Self> {
        conn.pragma_update(None, "synchronous", "FULL")
            .map_err(sql_error)?;
        let envelope = Envelope::new(
            &LockResource::PluginInstall,
            PluginInstallState::Starting,
            Vec::new(),
        )?;
        let destination = package_dir(&paths.plugin_packages_root(), id);
        let old = directory_identity(&destination)?;
        let layout = Layout::new(
            paths,
            id,
            operation_id(&envelope.operation_id)?,
            old.is_some(),
            false,
        );

        let tx = conn.unchecked_transaction().map_err(sql_error)?;
        store::backfill_game_packages(&tx, id)?;
        let previous = store::InstallSnapshot::capture(&tx, id)?;
        if old.is_some() {
            let record = previous.as_ref().ok_or_else(|| {
                io_error(format!("Package files for {id} have no install record"))
            })?;
            verify_package(&destination, id, &record.record.version)?;
            if Path::new(&record.record.install_dir) != destination {
                return Err(io_error(format!(
                    "The install record for {id} names a different directory"
                )));
            }
        }

        for path in [&layout.staging, &layout.marker] {
            let parent = path.parent().expect("install paths have parents");
            std::fs::create_dir_all(parent).map_err(|e| file_error(parent, e))?;
            recovery::sync_parent(parent).map_err(|e| file_error(parent, e))?;
        }
        std::fs::create_dir(&layout.staging).map_err(|e| file_error(&layout.staging, e))?;
        recovery::sync_parent(&layout.staging).map_err(|e| file_error(&layout.staging, e))?;
        let new = directory_identity(&layout.staging)?
            .ok_or_else(|| io_error("the staging folder disappeared"))?;
        let mut journal = Self {
            envelope,
            plugin_id: id.clone(),
            previous,
            old,
            change: PackageChange::Install {
                new,
                new_version: version,
            },
        };
        journal.write_new(&tx, &layout)?;
        tx.commit().map_err(sql_error)?;
        Ok(journal)
    }

    fn write_new(&mut self, conn: &Connection, layout: &Layout) -> LauncherResult<()> {
        self.envelope.owned = layout.ownership(self.old, &self.change);
        if !self.envelope.owned.is_empty() {
            let parent = layout.marker.parent().expect("marker has a parent");
            std::fs::create_dir_all(parent).map_err(|e| file_error(parent, e))?;
            recovery::sync_parent(parent).map_err(|e| file_error(parent, e))?;
            recovery::write_marker(&layout.marker, &self.envelope.operation_id)?;
        }
        conn.execute(
            "INSERT INTO plugin_install_journal (plugin_id, document) VALUES (?1, ?2)",
            params![self.plugin_id.as_str(), self.document()?],
        )
        .map_err(sql_error)?;
        Ok(())
    }

    fn layout(&self, paths: &AppPaths) -> LauncherResult<Layout> {
        Ok(Layout::new(
            paths,
            &self.plugin_id,
            operation_id(&self.envelope.operation_id)?,
            self.old.is_some(),
            matches!(self.change, PackageChange::Uninstall { .. }),
        ))
    }

    fn document(&self) -> LauncherResult<String> {
        serde_json::to_string(self).map_err(sql_error)
    }

    pub(in crate::plugins) fn advance(
        &mut self,
        conn: &Connection,
        state: PluginInstallState,
    ) -> LauncherResult<()> {
        self.envelope.state = state;
        let updated = conn
            .execute(
                "UPDATE plugin_install_journal SET document = ?2
                 WHERE plugin_id = ?1 AND json_extract(document, '$.operation_id') = ?3",
                params![
                    self.plugin_id.as_str(),
                    self.document()?,
                    self.envelope.operation_id
                ],
            )
            .map_err(sql_error)?;
        if updated != 1 {
            return Err(io_error("the plugin install journal disappeared"));
        }
        Ok(())
    }

    pub(in crate::plugins) fn swap_files(
        &mut self,
        conn: &Connection,
        ctx: &Ctx,
        lock: &LockGuard,
        bytes: &[u8],
    ) -> LauncherResult<()> {
        let PackageChange::Install { new, .. } = self.change else {
            return Err(io_error("expected an install journal"));
        };
        let layout = self.layout(&ctx.paths)?;
        let mut capability = self.authorize(conn, ctx, lock)?;
        if let Some(stash) = &layout.rollback {
            require(
                capability.verified_identity(&layout.destination)? == self.old,
                "the previous package changed before stashing",
            )?;
            capability.rename(&layout.destination, stash)?;
            crash_point(ctx, InstallCrashPoint::StashMoved);
            self.advance(conn, PluginInstallState::Stashed)?;
            crash_point(ctx, InstallCrashPoint::Stashed);
        }

        require(
            capability.verified_identity(&layout.staging)? == Some(new),
            "the staging directory disappeared",
        )?;
        extract_into(bytes, &layout.staging)?;
        crash_point(ctx, InstallCrashPoint::Unpacked);
        flush_tree(&layout.staging)?;
        crash_point(ctx, InstallCrashPoint::Flushed);
        capability.rename(&layout.staging, &layout.destination)?;
        crash_point(ctx, InstallCrashPoint::FilesPublished);
        self.advance(conn, PluginInstallState::Extracted)?;
        crash_point(ctx, InstallCrashPoint::Extracted);
        Ok(())
    }

    pub(in crate::plugins) fn finish(
        &self,
        conn: &Connection,
        ctx: &Ctx,
        lock: &LockGuard,
    ) -> LauncherResult<()> {
        apply(
            self.authorize(conn, ctx, lock)?,
            conn,
            &ctx.paths,
            InstallResolution::Automatic,
            Some(ctx),
        )
        .map(|_| ())
    }

    fn authorize<'a>(
        &self,
        conn: &Connection,
        ctx: &Ctx,
        lock: &'a LockGuard,
    ) -> LauncherResult<Capability<'a>> {
        let decision = decide_holding(
            conn,
            &ctx.paths,
            &self.plugin_id,
            lock,
            &*ctx.process_view(),
            Some(&self.envelope.operation_id),
        );
        let Decision::Act(capability) = decision else {
            return Err(io_error(format!(
                "Could not continue the package change for {}: {decision:?}",
                self.plugin_id
            )));
        };
        if let Some((path, why)) = capability.left_alone().first() {
            return Err(io_error(format!(
                "{} could not be verified: {why}",
                path.display()
            )));
        }
        Ok(*capability)
    }
}

pub(in crate::plugins) fn uninstall(
    conn: &Connection,
    ctx: &Ctx,
    lock: &LockGuard,
    installed: &store::PluginRecord,
    purge_data: bool,
) -> LauncherResult<()> {
    conn.pragma_update(None, "synchronous", "FULL")
        .map_err(sql_error)?;
    let id = installed.id();
    let envelope = Envelope::new(
        &LockResource::PluginInstall,
        PluginInstallState::Uninstalling,
        Vec::new(),
    )?;
    let destination = package_dir(&ctx.paths.plugin_packages_root(), id);
    let old =
        if matches!(installed.source, PluginSource::Package) && !store::is_tombstone(installed) {
            require(
                installed.install_dir == destination,
                "the package is outside its managed directory",
            )?;
            directory_identity(&destination)?
        } else {
            None
        };
    let tx = conn.unchecked_transaction().map_err(sql_error)?;
    store::backfill_game_packages(&tx, id)?;
    let record =
        store::install_row(&tx, id)?.ok_or_else(|| io_error("the install record disappeared"))?;
    let mut journal = InstallJournal {
        envelope,
        plugin_id: id.clone(),
        previous: None,
        old,
        change: PackageChange::Uninstall {
            purge_data,
            version: record.version,
            install_dir: record.install_dir,
        },
    };
    let layout = journal.layout(&ctx.paths)?;
    journal.write_new(&tx, &layout)?;
    tx.commit().map_err(sql_error)?;
    crash_point(ctx, InstallCrashPoint::UninstallJournalWritten);

    let result = (|| -> LauncherResult<()> {
        let mut capability = journal.authorize(conn, ctx, lock)?;
        if let Some(aside) = &layout.rollback {
            capability.rename(&layout.destination, aside)?;
        }
        crash_point(ctx, InstallCrashPoint::UninstallFilesMoved);
        let tx = conn.unchecked_transaction().map_err(sql_error)?;
        check_record(&tx, &journal, &layout)?;
        store::remove(&tx, id, purge_data)?;
        crash_point(ctx, InstallCrashPoint::UninstallRecorded);
        journal.advance(&tx, PluginInstallState::UninstallCommitted)?;
        crash_point(ctx, InstallCrashPoint::UninstallMarked);
        tx.commit().map_err(sql_error)?;
        crash_point(ctx, InstallCrashPoint::UninstallCommitted);
        Ok(())
    })();
    let settled = journal.finish(conn, ctx, lock);
    match (result, settled) {
        (Err(error), Err(recovery)) => Err(io_error(format!(
            "{error}. The uninstall was interrupted: {recovery}"
        ))),
        (result, settled) => result.and(settled),
    }
}

fn operation_id(value: &str) -> LauncherResult<uuid::Uuid> {
    uuid::Uuid::parse_str(value)
        .map_err(|error| io_error(format!("invalid install operation id: {error}")))
}

fn sql_error(error: impl std::fmt::Display) -> LauncherError {
    io_error(format!("plugin install journal: {error}"))
}

fn file_error(path: &Path, error: std::io::Error) -> LauncherError {
    io_error(format!("{}: {error}", path.display()))
}

fn directory_identity(path: &Path) -> LauncherResult<Option<PathIdentity>> {
    recovery::path_identity(path).map_err(|error| file_error(path, error))
}

fn move_directory(from: &Path, to: &Path) -> LauncherResult<()> {
    recovery::rename_directory(from, to).map_err(|error| file_error(from, error))
}

fn present(path: &Path) -> LauncherResult<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(file_error(path, error)),
    }
}

fn remove_if_present(path: &Path) -> LauncherResult<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(file_error(path, error)),
    }
}

/// Where a test can interrupt an install or a manual restore.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallCrashPoint {
    JournalWritten,
    StashMoved,
    Stashed,
    Unpacked,
    Flushed,
    FilesPublished,
    Extracted,
    /// The new record was written inside the still-open transaction.
    Recorded,
    CommitMarked,
    /// The record and the journal have committed together.
    Committed,
    StashDropStarted,
    StashDropped,
    JournalDeleted,
    RestoreRecorded,
    RestoreMarked,
    RestoreCommitted,
    RestoreMovedAside,
    RestorePublished,
    RestoreJournalDeleted,
    UninstallJournalWritten,
    UninstallFilesMoved,
    UninstallRecorded,
    UninstallMarked,
    UninstallCommitted,
    UninstallDropStarted,
    UninstallFilesDropped,
    UninstallJournalDeleted,
}

pub fn crash_point(ctx: &Ctx, point: InstallCrashPoint) {
    #[cfg(debug_assertions)]
    ctx.test_seams.install_crash_point(point);
    #[cfg(not(debug_assertions))]
    let _ = (ctx, point);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallRecovery {
    None,
    Done(String),
    NeedsHand(String),
    Busy(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallResolution {
    Automatic,
    Restore,
    Discard,
    KeepAside,
}

pub fn recover_interrupted_installs(
    conn: &Connection,
    paths: &AppPaths,
) -> LauncherResult<Vec<String>> {
    recover_interrupted_installs_with(conn, paths, &OsProcesses)
}

pub fn recover_interrupted_installs_with(
    conn: &Connection,
    paths: &AppPaths,
    view: &dyn ProcessView,
) -> LauncherResult<Vec<String>> {
    let locks = LockManager::new(paths.locks_root());
    let mut warnings = Vec::new();
    for id in interrupted_installs(conn, paths)? {
        let outcome = recover_one(
            &locks,
            conn,
            paths,
            &id,
            InstallResolution::Automatic,
            view,
            None,
        );
        match outcome {
            InstallRecovery::None => {}
            InstallRecovery::Done(line) | InstallRecovery::NeedsHand(line) => warnings.push(line),
            InstallRecovery::Busy(line) => {
                warnings.push(line);
                break;
            }
        }
    }
    Ok(warnings)
}

pub fn resolve_installs(
    ctx: &Ctx,
    id: Option<&PluginId>,
    resolution: InstallResolution,
) -> LauncherResult<Vec<(PluginId, InstallRecovery)>> {
    let conn = crate::db::local_state_connection(&ctx.paths.local_state_db()).map_err(sql_error)?;
    resolve_in(
        &conn,
        &ctx.paths,
        id,
        resolution,
        &*ctx.process_view(),
        Some(ctx),
    )
}

/// Run the explicit command before startup recovery has a chance to consume its journal.
pub fn resolve_installs_at_startup(
    paths: &AppPaths,
    id: Option<&PluginId>,
    resolution: InstallResolution,
    options: &crate::db::LocalStateOpenOptions,
) -> LauncherResult<Vec<(PluginId, InstallRecovery)>> {
    paths.create_required_dirs()?;
    let db = paths.local_state_db();
    crate::db::init_local_state_db_with(&db, options).map_err(|error| {
        error
            .downcast_ref::<LauncherError>()
            .cloned()
            .unwrap_or_else(|| sql_error(error))
    })?;
    let conn = crate::db::local_state_connection(&db).map_err(sql_error)?;
    resolve_in(&conn, paths, id, resolution, &OsProcesses, None)
}

fn resolve_in(
    conn: &Connection,
    paths: &AppPaths,
    id: Option<&PluginId>,
    resolution: InstallResolution,
    view: &dyn ProcessView,
    ctx: Option<&Ctx>,
) -> LauncherResult<Vec<(PluginId, InstallRecovery)>> {
    conn.pragma_update(None, "synchronous", "FULL")
        .map_err(sql_error)?;
    let ids = match id {
        Some(id) => vec![id.clone()],
        None => interrupted_installs(conn, paths)?,
    };
    let locks = LockManager::new(paths.locks_root());
    Ok(ids
        .into_iter()
        .map(|id| {
            let outcome = recover_one(&locks, conn, paths, &id, resolution, view, ctx);
            (id, outcome)
        })
        .collect())
}

fn interrupted_installs(conn: &Connection, paths: &AppPaths) -> LauncherResult<Vec<PluginId>> {
    let mut ids = std::collections::BTreeMap::new();
    for id in store::journal_ids(conn)? {
        let parsed = PluginId::parse(&id).map_err(|error| io_error(error.message))?;
        ids.insert(id, parsed);
    }
    let rollback = paths.plugin_rollback_root();
    let entries = match std::fs::read_dir(&rollback) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ids.into_values().collect())
        }
        Err(error) => return Err(file_error(&rollback, error)),
    };
    for entry in entries {
        let name = entry
            .map_err(|error| file_error(&rollback, error))?
            .file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with('.') {
            continue;
        }
        let parse = |name: &str| PluginId::parse(name).map_err(|_| ());
        let plugin = parse(name).or_else(|()| name.strip_suffix(".json").ok_or(()).and_then(parse));
        if let Ok(id) = plugin {
            ids.insert(id.to_string(), id);
        }
    }
    Ok(ids.into_values().collect())
}

fn journal_document(conn: &Connection, id: &PluginId) -> LauncherResult<Option<String>> {
    conn.query_row(
        "SELECT document FROM plugin_install_journal WHERE plugin_id = ?1",
        params![id.as_str()],
        |row| row.get(0),
    )
    .optional()
    .map_err(sql_error)
}

fn legacy_present(paths: &AppPaths, id: &PluginId) -> LauncherResult<bool> {
    Ok(present(&install_journal_path(paths, id))?
        || present(&paths.plugin_rollback_root().join(id.as_str()))?)
}

fn read_locked(
    conn: &Connection,
    paths: &AppPaths,
    id: &PluginId,
) -> JournalRead<PluginInstallState> {
    read_snapshot(conn, paths, id)
        .unwrap_or_else(|error| JournalRead::Unusable(JournalProblem::CantTell(error.to_string())))
}

/// Both reads use one SQLite snapshot. The envelope is validated before its paths are used.
fn read_snapshot(
    conn: &Connection,
    paths: &AppPaths,
    id: &PluginId,
) -> LauncherResult<JournalRead<PluginInstallState>> {
    let tx = conn.unchecked_transaction().map_err(sql_error)?;
    let Some(document) = journal_document(&tx, id)? else {
        return if legacy_present(paths, id)? {
            Ok(JournalRead::Unusable(JournalProblem::Malformed(
                "legacy journal or stash without a SQLite journal".into(),
            )))
        } else {
            Ok(JournalRead::Absent)
        };
    };
    let read = recovery::read_document(document.as_bytes(), "the SQLite install journal");
    if let JournalRead::Valid(journal) = &read {
        let journal: InstallJournal = journal.payload().map_err(sql_error)?;
        let layout = journal.layout(paths)?;
        if journal.plugin_id != *id
            || journal.envelope.owned != layout.ownership(journal.old, &journal.change)
        {
            return Err(io_error(
                "the journal's paths do not match this plugin install",
            ));
        }
        check_record(&tx, &journal, &layout)?;
    }
    tx.commit().map_err(sql_error)?;
    Ok(read)
}

fn check_record(
    conn: &Connection,
    journal: &InstallJournal,
    layout: &Layout,
) -> LauncherResult<()> {
    let record = store::install_row(conn, &journal.plugin_id)?;
    let names = |row: &store::InstallRow| (row.version.clone(), row.install_dir.clone());
    use PluginInstallState::*;
    let expected = match (&journal.change, journal.envelope.state) {
        (PackageChange::Install { new_version, .. }, Committed) => Some((
            new_version.clone(),
            layout.destination.to_string_lossy().into_owned(),
        )),
        (PackageChange::Install { .. }, Starting | Stashed | Extracted | Restoring) => journal
            .previous
            .as_ref()
            .map(|snapshot| names(&snapshot.record)),
        (
            PackageChange::Uninstall {
                version,
                install_dir,
                ..
            },
            Uninstalling,
        ) => Some((version.clone(), install_dir.clone())),
        (
            PackageChange::Uninstall {
                purge_data: true, ..
            },
            UninstallCommitted,
        ) => None,
        (
            PackageChange::Uninstall {
                version,
                purge_data: false,
                ..
            },
            UninstallCommitted,
        ) => Some((version.clone(), String::new())),
        _ => {
            return Err(io_error(
                "the plugin journal has an invalid operation state",
            ))
        }
    };
    if record.as_ref().map(names) != expected {
        return Err(io_error(format!(
            "the install record for {} disagrees with its journal",
            journal.plugin_id
        )));
    }
    Ok(())
}

fn decide_holding<'a>(
    conn: &Connection,
    paths: &AppPaths,
    id: &PluginId,
    lock: &'a LockGuard,
    view: &dyn ProcessView,
    finished: Option<&str>,
) -> Decision<'a, PluginInstallState> {
    recovery::decide_holding_reading(
        lock,
        &LockResource::PluginInstall,
        None,
        || read_locked(conn, paths, id),
        &NoContentProof,
        view,
        finished,
    )
}

pub(in crate::plugins) fn settle_before_install(
    conn: &Connection,
    paths: &AppPaths,
    id: &PluginId,
    lock: &LockGuard,
    view: &dyn ProcessView,
) -> LauncherResult<Option<String>> {
    match decide_holding(conn, paths, id, lock, view, None) {
        Decision::Nothing => Ok(None),
        Decision::Busy(why) | Decision::NeedsHand(why) => Err(io_error(needs_hand(id, &why))),
        Decision::Act(capability) => {
            apply(*capability, conn, paths, InstallResolution::Automatic, None)
                .map(Some)
                .map_err(|error| io_error(needs_hand(id, &error.to_string())))
        }
    }
}

pub(in crate::plugins) fn check_for_interrupted_install(
    conn: &Connection,
    paths: &AppPaths,
    id: &PluginId,
    lock: &LockGuard,
    view: &dyn ProcessView,
) -> LauncherResult<()> {
    match decide_holding(conn, paths, id, lock, view, None) {
        Decision::Nothing => Ok(()),
        Decision::Busy(why) | Decision::NeedsHand(why) => Err(io_error(needs_hand(id, &why))),
        Decision::Act(_) => Err(io_error(needs_hand(
            id,
            "Resolve the interrupted install before changing the plugin.",
        ))),
    }
}

fn needs_hand(id: &PluginId, why: &str) -> String {
    format!("Plugin package change for {id} needs a hand: {why} Run `agora plugin recover {id}` to resolve it. Legacy stashes can be preserved with `--keep-aside --yes`.")
}

fn recover_one(
    locks: &LockManager,
    conn: &Connection,
    paths: &AppPaths,
    id: &PluginId,
    resolution: InstallResolution,
    view: &dyn ProcessView,
    ctx: Option<&Ctx>,
) -> InstallRecovery {
    let run = |capability| apply(capability, conn, paths, resolution, ctx);
    let outcome = if resolution == InstallResolution::KeepAside {
        // The explicit legacy escape hatch has no journal identity to prove. It only
        // renames the stash and leaves the package and plugin record alone.
        match locks.acquire_with_timeout_typed(
            LockResource::PluginInstall,
            "plugin-recover",
            std::time::Duration::ZERO,
            None,
        ) {
            Ok(lock) => {
                let legacy = journal_document(conn, id).and_then(|row| {
                    if row.is_none() && legacy_present(paths, id)? {
                        return keep_legacy_aside(paths, id).map(Some);
                    }
                    Ok(None)
                });
                match legacy {
                    Ok(Some(message)) => Ok(Some(message)),
                    Err(error) => Err(error),
                    Ok(None) => match decide_holding(conn, paths, id, &lock, view, None) {
                        Decision::Nothing => Ok(None),
                        Decision::Act(capability) => {
                            apply(*capability, conn, paths, resolution, ctx).map(Some)
                        }
                        Decision::Busy(why) | Decision::NeedsHand(why) => Err(io_error(why)),
                    },
                }
            }
            Err(crate::lock_manager::LockRefusal::Held(error)) => {
                return InstallRecovery::Busy(error.to_string())
            }
            Err(crate::lock_manager::LockRefusal::Unavailable(error)) => Err(error),
        }
    } else {
        match recovery::decide_reading(
            locks,
            &LockResource::PluginInstall,
            None,
            || read_locked(conn, paths, id),
            &NoContentProof,
            view,
        ) {
            Decision::Nothing => Ok(None),
            Decision::Busy(why) => return InstallRecovery::Busy(why),
            Decision::NeedsHand(why) => Err(io_error(why)),
            Decision::Act(capability) => run(*capability).map(Some),
        }
    };
    match outcome {
        Ok(None) => InstallRecovery::None,
        Ok(Some(message)) => InstallRecovery::Done(message),
        Err(error) => InstallRecovery::NeedsHand(needs_hand(id, &error.to_string())),
    }
}

fn keep_legacy_aside(paths: &AppPaths, id: &PluginId) -> LauncherResult<String> {
    let root = paths.plugin_rollback_root();
    let stash = root.join(id.as_str());
    let legacy = install_journal_path(paths, id);
    let mut kept = Vec::new();
    for path in [&stash, &legacy] {
        if present(path)? {
            let name = path
                .file_name()
                .expect("legacy paths have names")
                .to_string_lossy();
            let aside = root.join(format!(".{name}-{}.kept", uuid::Uuid::new_v4()));
            move_directory(path, &aside)?;
            kept.push(aside.display().to_string());
        }
    }
    Ok(format!(
        "Kept the legacy files for plugin {id} at {}.",
        kept.join(", ")
    ))
}

fn verify_package(path: &Path, id: &PluginId, version: &str) -> LauncherResult<()> {
    let manifest = read_folder_manifest(path)?;
    if manifest.id != *id || manifest.version.to_string() != version {
        return Err(io_error(format!(
            "{} does not contain plugin {id} version {version}",
            path.display()
        )));
    }
    Ok(())
}

fn require(condition: bool, message: &str) -> LauncherResult<()> {
    if condition {
        Ok(())
    } else {
        Err(io_error(message))
    }
}

fn apply(
    mut capability: Capability<'_>,
    conn: &Connection,
    paths: &AppPaths,
    resolution: InstallResolution,
    ctx: Option<&Ctx>,
) -> LauncherResult<String> {
    if let Some((path, why)) = capability.left_alone().first() {
        return Err(io_error(format!(
            "{} could not be verified: {why}",
            path.display()
        )));
    }
    let mut journal: InstallJournal = capability.payload()?;
    let id = journal.plugin_id.clone();
    let layout = journal.layout(paths)?;
    let PackageChange::Install { new, new_version } = &journal.change else {
        return apply_uninstall(capability, conn, &journal, &layout, resolution, ctx);
    };
    let new = *new;
    let installed = capability.verified_identity(&layout.destination)?;
    let stashed = match &layout.rollback {
        Some(path) => capability.verified_identity(path)?,
        None => None,
    };
    let staged = capability.verified_identity(&layout.staging)?;
    let aside = capability.verified_identity(&layout.aside)?;
    let cut = |point| {
        if let Some(ctx) = ctx {
            crash_point(ctx, point)
        }
    };
    let mut state = *capability.state();

    if state == PluginInstallState::Committed {
        require(
            installed == Some(new),
            "the committed package directory is missing",
        )?;
        verify_package(&layout.destination, &id, new_version)?;
        let drop_started =
            layout.rollback.is_some() && capability.verified_identity(&layout.dropping)?.is_some();
        require(
            !drop_started || stashed.is_none(),
            "the stash and its deletion folder are both occupied",
        )?;
        if resolution == InstallResolution::Restore {
            require(
                !drop_started,
                "The previous version was already being removed. The new version stays installed. Run without --restore to finish cleanup.",
            )?;
            let stash = layout
                .rollback
                .as_ref()
                .filter(|_| stashed.is_some())
                .ok_or_else(|| io_error("there is no restorable stash"))?;
            let previous = journal
                .previous
                .as_ref()
                .ok_or_else(|| io_error("there is no previous install record"))?;
            verify_package(stash, &id, &previous.record.version)?;
            require(
                aside.is_none(),
                "the destination for the kept copy is occupied",
            )?;
            let tx = conn.unchecked_transaction().map_err(sql_error)?;
            check_record(&tx, &journal, &layout)?;
            // Recheck both directories immediately before committing the rollback decision.
            capability.verified_identity(stash)?;
            capability.verified_identity(&layout.destination)?;
            previous.restore(&tx, &id)?;
            cut(InstallCrashPoint::RestoreRecorded);
            journal.advance(&tx, PluginInstallState::Restoring)?;
            cut(InstallCrashPoint::RestoreMarked);
            tx.commit().map_err(sql_error)?;
            cut(InstallCrashPoint::RestoreCommitted);
            state = PluginInstallState::Restoring;
        } else {
            if let Some(stash) = &layout.rollback {
                if stashed.is_some() {
                    if resolution == InstallResolution::KeepAside {
                        // Preserve the previous version in a user-visible archive.
                        capability.rename(stash, &layout.kept)?;
                    } else {
                        capability.rename(stash, &layout.dropping)?;
                    }
                }
                if capability.verified_identity(&layout.dropping)?.is_some() {
                    require(resolution != InstallResolution::KeepAside, "the stash is already being dropped; run automatic recovery to finish cleanup")?;
                    cut(InstallCrashPoint::StashDropStarted);
                    capability.remove_dir(&layout.dropping)?;
                }
                recovery::sync_parent(stash).map_err(|e| file_error(stash, e))?;
                cut(InstallCrashPoint::StashDropped);
            }
            delete_journal(conn, &journal)?;
            cut(InstallCrashPoint::JournalDeleted);
            remove_if_present(&layout.marker)?;
            let kept = if resolution == InstallResolution::KeepAside && present(&layout.kept)? {
                format!(" The previous version is at {}.", layout.kept.display())
            } else {
                String::new()
            };
            return Ok(format!(
                "Finished the interrupted install of plugin {id} {new_version}.{kept}"
            ));
        }
    }

    require(
        resolution != InstallResolution::Discard,
        "only a committed install can discard its stash",
    )?;
    if let (Some(old), Some(stash)) = (journal.old, &layout.rollback) {
        require(
            installed == Some(old) || stashed == Some(old),
            "the previous package is missing",
        )?;
        let previous = journal
            .previous
            .as_ref()
            .ok_or_else(|| io_error("there is no previous install record"))?;
        let old_dir = if stashed.is_some() {
            stash
        } else {
            &layout.destination
        };
        verify_package(old_dir, &id, &previous.record.version)?;
    }
    if installed == Some(new) {
        capability.rename(&layout.destination, &layout.aside)?;
        if state == PluginInstallState::Restoring {
            cut(InstallCrashPoint::RestoreMovedAside);
        }
    } else if staged.is_some() {
        // Extraction may have completed before the publish rename. Preserve its bytes as well.
        capability.rename(&layout.staging, &layout.aside)?;
    }
    if let Some(stash) = &layout.rollback {
        if stashed.is_some() {
            capability.rename(stash, &layout.destination)?;
            if state == PluginInstallState::Restoring {
                cut(InstallCrashPoint::RestorePublished);
            }
        }
        recovery::sync_parent(stash).map_err(|e| file_error(stash, e))?;
        let previous = journal
            .previous
            .as_ref()
            .ok_or_else(|| io_error("there is no previous install record"))?;
        verify_package(&layout.destination, &id, &previous.record.version)?;
    }
    recovery::sync_parent(&layout.aside).map_err(|e| file_error(&layout.aside, e))?;
    recovery::sync_parent(&layout.destination).map_err(|e| file_error(&layout.destination, e))?;
    delete_journal(conn, &journal)?;
    if state == PluginInstallState::Restoring {
        cut(InstallCrashPoint::RestoreJournalDeleted);
    }
    // The marker is needed across all file moves. A leftover marker is inert.
    remove_if_present(&layout.marker)?;
    let kept = if present(&layout.aside)? {
        format!(" The replacement is at {}.", layout.aside.display())
    } else {
        String::new()
    };
    Ok(format!(
        "Recovered plugin {id} from an interrupted install.{kept}"
    ))
}

fn apply_uninstall(
    mut capability: Capability<'_>,
    conn: &Connection,
    journal: &InstallJournal,
    layout: &Layout,
    resolution: InstallResolution,
    ctx: Option<&Ctx>,
) -> LauncherResult<String> {
    let committed = journal.envelope.state == PluginInstallState::UninstallCommitted;
    if committed {
        require(
            matches!(
                resolution,
                InstallResolution::Automatic | InstallResolution::Discard
            ),
            "the uninstall already committed; run automatic recovery to finish removing its files",
        )?;
    } else {
        require(
            resolution != InstallResolution::Discard,
            "the uninstall has not committed; recover it to put the package back",
        )?;
    }
    let cut = |point| {
        if let Some(ctx) = ctx {
            crash_point(ctx, point);
        }
    };
    if let Some(aside) = &layout.rollback {
        let installed = capability.verified_identity(&layout.destination)?;
        let moved = capability.verified_identity(aside)?;
        if committed {
            require(
                installed.is_none(),
                "the uninstalled package still occupies the install directory",
            )?;
            cut(InstallCrashPoint::UninstallDropStarted);
            capability.remove_dir(aside)?;
            cut(InstallCrashPoint::UninstallFilesDropped);
        } else if moved.is_some() {
            capability.rename(aside, &layout.destination)?;
        } else {
            require(
                installed == journal.old,
                "the package to restore is missing",
            )?;
        }
        recovery::sync_parent(aside).map_err(|e| file_error(aside, e))?;
        recovery::sync_parent(&layout.destination)
            .map_err(|e| file_error(&layout.destination, e))?;
    }
    delete_journal(conn, journal)?;
    cut(InstallCrashPoint::UninstallJournalDeleted);
    remove_if_present(&layout.marker)?;
    Ok(if committed {
        format!("Finished uninstalling plugin {}.", journal.plugin_id)
    } else {
        format!(
            "Recovered plugin {} from an interrupted uninstall.",
            journal.plugin_id
        )
    })
}

/// Which way recovery would take a journal in `state`, for `agora doctor` (read-only).
///
/// The direction is the *feature's* knowledge, not the decision's: it is read from this kind's own
/// state enum, and it must stay in step with [`apply`], which rolls forward from
/// [`PluginInstallState::Committed`] and [`PluginInstallState::UninstallCommitted`] and puts the
/// previous version back from every other state (a restore the writer had already recorded, or an
/// install or uninstall that never reached its commit point).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallDirection {
    /// Recovery finishes what the operation had already committed.
    Finish,
    /// Recovery puts back what the operation replaced.
    Undo,
}

pub fn direction(state: PluginInstallState) -> InstallDirection {
    match state {
        PluginInstallState::Committed | PluginInstallState::UninstallCommitted => {
            InstallDirection::Finish
        }
        PluginInstallState::Starting
        | PluginInstallState::Stashed
        | PluginInstallState::Extracted
        | PluginInstallState::Restoring
        | PluginInstallState::Uninstalling => InstallDirection::Undo,
    }
}

/// One plugin-install journal as the read-only report sees it (MASTER_SPEC §32.11).
#[derive(Debug, Clone)]
pub struct InstallJournalReport {
    /// The plugin the journal covers.
    pub plugin_id: PluginId,
    /// Where the journal is: the database row, or the file an older build left.
    pub path: PathBuf,
    /// This kind's own state, when the journal was readable.
    pub state: Option<PluginInstallState>,
    /// What recovery would do, and why.
    pub outcome: recovery::RecoveryOutcome,
    pub reason: Option<String>,
}

/// Every plugin-install journal this data root has, decided without touching anything.
///
/// The same journals the startup pass recovers from ([`interrupted_installs`]), each put through
/// [`recovery::decide_readonly`]: nothing is locked, written, renamed or deleted, and a journal
/// whose lock is held or whose writer may still be running is reported as such rather than acted on.
pub fn journal_report(
    conn: &Connection,
    paths: &AppPaths,
    view: &dyn ProcessView,
) -> LauncherResult<Vec<InstallJournalReport>> {
    let locks = LockManager::new(paths.locks_root());
    let mut out = Vec::new();
    for id in interrupted_installs(conn, paths)? {
        let legacy = install_journal_path(paths, &id);
        // One read of the journal: the state it carries, and the document the decision is made
        // from, are the same bytes rather than two chances for the row to change in between.
        let read = read_locked(conn, paths, &id);
        let state = match &read {
            JournalRead::Valid(journal) => Some(journal.envelope().state),
            _ => None,
        };
        let decision = recovery::decide_readonly(
            &locks,
            &LockResource::PluginInstall,
            || read.clone(),
            &NoContentProof,
            view,
        );
        let path = if journal_document(conn, &id)?.is_some() {
            paths.local_state_db()
        } else {
            legacy
        };
        let (outcome, reason) = match decision {
            recovery::ReadOnlyDecision::Nothing => (
                recovery::RecoveryOutcome::NothingToDo,
                "The journal is already settled: recovery has nothing to do.".to_string(),
            ),
            recovery::ReadOnlyDecision::Busy(why) => (recovery::RecoveryOutcome::Busy, why),
            recovery::ReadOnlyDecision::NeedsHand(why) => {
                (recovery::RecoveryOutcome::NeedsHand, why)
            }
            recovery::ReadOnlyDecision::WouldAct(_) => match state.map(direction) {
                Some(InstallDirection::Finish) => (
                    recovery::RecoveryOutcome::WouldFinish,
                    format!(
                        "The operation had committed (state \"{}\"), so recovery would finish its \
                         cleanup and keep the version that is installed.",
                        state.map(PluginInstallState::as_str).unwrap_or("committed")
                    ),
                ),
                _ => (
                    recovery::RecoveryOutcome::WouldUndo,
                    format!(
                        "The operation had not committed (state \"{}\"), so recovery would put the \
                         previous package back.",
                        state.map(PluginInstallState::as_str).unwrap_or("unknown")
                    ),
                ),
            },
        };
        out.push(InstallJournalReport {
            plugin_id: id,
            path,
            state,
            outcome,
            reason: Some(reason),
        });
    }
    Ok(out)
}

fn delete_journal(conn: &Connection, journal: &InstallJournal) -> LauncherResult<()> {
    conn.pragma_update(None, "synchronous", "FULL")
        .map_err(sql_error)?;
    let deleted = conn
        .execute(
            "DELETE FROM plugin_install_journal
         WHERE plugin_id = ?1 AND json_extract(document, '$.operation_id') = ?2",
            params![journal.plugin_id.as_str(), journal.envelope.operation_id],
        )
        .map_err(sql_error)?;
    require(deleted == 1, "the plugin install journal disappeared")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::service::PluginService;
    use std::io::Write;

    const ID: &str = "acme.one";

    fn package(version: &str, script: &str) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file(MANIFEST_FILENAME, options).unwrap();
        zip.write_all(
            serde_json::json!({
                "manifest": 1, "id": ID, "name": "Test", "version": version,
                "license": "MIT", "apiRange": ">=0.1, <0.2", "entrypoint": "main.js"
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();
        zip.start_file("main.js", options).unwrap();
        zip.write_all(script.as_bytes()).unwrap();
        zip.finish().unwrap().into_inner()
    }

    struct Fixture {
        _root: tempfile::TempDir,
        ctx: Ctx,
        service: PluginService,
        id: PluginId,
        conn: Connection,
    }

    impl Fixture {
        fn new(installed: bool) -> Self {
            let root = tempfile::tempdir().unwrap();
            let ctx = Ctx::for_testing(root.path().to_path_buf());
            ctx.paths.create_required_dirs().unwrap();
            crate::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
            let conn = crate::db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
            let service = PluginService::headless(
                ctx.clone(),
                std::sync::Arc::new(agora_plugin_host::QuickJsHost::new()),
            );
            if installed {
                service
                    .install_package_bytes(&package("1.0.0", "// old package"), true)
                    .unwrap();
            }
            Self {
                _root: root,
                ctx,
                service,
                conn,
                id: PluginId::parse(ID).unwrap(),
            }
        }

        fn journal(&self) -> Option<InstallJournal> {
            journal_document(&self.conn, &self.id)
                .unwrap()
                .map(|text| serde_json::from_str(&text).unwrap())
        }

        fn destination(&self) -> PathBuf {
            package_dir(&self.ctx.paths.plugin_packages_root(), &self.id)
        }

        fn stash(&self) -> PathBuf {
            self.ctx.paths.plugin_rollback_root().join(ID)
        }

        fn crash_at(&self, point: InstallCrashPoint, operation: impl FnOnce()) {
            self.ctx.test_seams.set_install_crash(Some(point));
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation));
            self.ctx.test_seams.set_install_crash(None);
            assert!(
                result.is_err_and(|panic| panic
                    .downcast_ref::<String>()
                    .is_some_and(|message| message
                        .contains(&format!("cut this plugin install short at {point:?}")))),
                "the seam did not cut the operation at {point:?}"
            );
            if let Some(mut journal) = self.journal() {
                journal.envelope.writer = Some(crate::process_identity::ProcessIdentity {
                    pid: u32::MAX - 2,
                    start_time: 1,
                    expected_exe: None,
                    job: None,
                });
                self.conn
                    .execute(
                        "UPDATE plugin_install_journal SET document=?2 WHERE plugin_id=?1",
                        params![ID, journal.document().unwrap()],
                    )
                    .unwrap();
            }
        }

        fn interrupt_install(&self, point: InstallCrashPoint, version: &str) {
            self.crash_at(point, || {
                self.service
                    .install_package_bytes(&package(version, "// new package"), true)
                    .unwrap();
            });
        }

        fn restart(&self) -> Vec<String> {
            let conn = crate::db::local_state_connection(&self.ctx.paths.local_state_db()).unwrap();
            recover_interrupted_installs(&conn, &self.ctx.paths).unwrap()
        }

        fn assert_agreement(&self, version: &str, script: &str) {
            let record = store::get(&self.conn, &self.id).unwrap().unwrap();
            assert_eq!(record.manifest.version.to_string(), version);
            assert_eq!(record.install_dir, self.destination());
            verify_package(&self.destination(), &self.id, version).unwrap();
            assert_eq!(
                std::fs::read_to_string(self.destination().join("main.js")).unwrap(),
                script
            );
            assert!(self.journal().is_none());
            assert!(!self.stash().is_dir());
        }
    }

    #[test]
    fn plugin_recovery_agrees_at_every_install_boundary() {
        use InstallCrashPoint::*;
        for point in [
            JournalWritten,
            StashMoved,
            Stashed,
            Unpacked,
            Flushed,
            FilesPublished,
            Extracted,
            Recorded,
            CommitMarked,
            Committed,
            StashDropped,
            JournalDeleted,
        ] {
            let f = Fixture::new(true);
            f.interrupt_install(point, "2.0.0");
            assert!(
                f.restart()
                    .iter()
                    .all(|line| !line.contains("needs a hand")),
                "{point:?}"
            );
            if matches!(point, Committed | StashDropped | JournalDeleted) {
                f.assert_agreement("2.0.0", "// new package");
            } else {
                f.assert_agreement("1.0.0", "// old package");
            }
            assert!(f.restart().is_empty());
        }
    }

    #[test]
    fn plugin_recovery_agrees_at_every_explicit_restore_boundary() {
        use InstallCrashPoint::*;
        for point in [
            RestoreRecorded,
            RestoreMarked,
            RestoreCommitted,
            RestoreMovedAside,
            RestorePublished,
            RestoreJournalDeleted,
        ] {
            let f = Fixture::new(true);
            store::set_script_consent(
                &f.conn,
                &f.id,
                false,
                Some("old consent digest"),
                "original",
            )
            .unwrap();
            f.interrupt_install(Committed, "2.0.0");
            let aside = f.journal().unwrap().layout(&f.ctx.paths).unwrap().aside;
            f.crash_at(point, || {
                f.service
                    .recover_interrupted_installs(Some(&f.id), InstallResolution::Restore)
                    .unwrap();
            });
            assert!(
                f.restart()
                    .iter()
                    .all(|line| !line.contains("needs a hand")),
                "{point:?}"
            );
            if matches!(point, RestoreRecorded | RestoreMarked) {
                f.assert_agreement("2.0.0", "// new package");
            } else {
                f.assert_agreement("1.0.0", "// old package");
                verify_package(&aside, &f.id, "2.0.0").unwrap();
                assert_eq!(
                    std::fs::read_to_string(aside.join("main.js")).unwrap(),
                    "// new package"
                );
                assert_eq!(store::script_consent(&f.conn, &f.id).unwrap(), Some(false));
                assert_eq!(
                    store::script_consent_code_hash(&f.conn, &f.id)
                        .unwrap()
                        .as_deref(),
                    Some("old consent digest")
                );
            }
            assert!(f.restart().is_empty());
        }
    }

    #[test]
    fn plugin_recovery_uninstall_refuses_an_interrupted_install_in_an_open_launcher() {
        let f = Fixture::new(true);
        f.interrupt_install(InstallCrashPoint::Extracted, "2.0.0");
        let error = f.service.uninstall(&f.id, false).unwrap_err();
        assert!(error.to_string().contains("plugin recover"));
        verify_package(&f.destination(), &f.id, "2.0.0").unwrap();
        verify_package(&f.stash(), &f.id, "1.0.0").unwrap();
        assert!(f.journal().is_some());
        f.restart();
        f.assert_agreement("1.0.0", "// old package");
    }

    #[test]
    fn plugin_recovery_refuses_a_replaced_stash_with_the_original_marker() {
        let f = Fixture::new(true);
        f.interrupt_install(InstallCrashPoint::Committed, "2.0.0");
        let marker = f.journal().unwrap().layout(&f.ctx.paths).unwrap().marker;
        let copied = std::fs::read(&marker).unwrap();
        let original = f.stash().with_file_name(".original");
        std::fs::rename(f.stash(), &original).unwrap();
        extract_package(&package("1.0.0", "// replacement"), &f.stash()).unwrap();
        std::fs::write(marker, copied).unwrap();
        let results = f
            .service
            .recover_interrupted_installs(Some(&f.id), InstallResolution::Restore)
            .unwrap();
        assert!(
            matches!(results.as_slice(), [(_, InstallRecovery::NeedsHand(why))] if why.contains("identity"))
        );
        let warnings = f.restart();
        assert!(
            warnings.iter().any(|line| line.contains("identity")),
            "{warnings:?}"
        );
        assert!(f.stash().is_dir() && original.is_dir() && f.journal().is_some());
        verify_package(&f.destination(), &f.id, "2.0.0").unwrap();
    }

    #[test]
    fn round_two_uninstall_crash_keeps_record_files_and_data_in_agreement() {
        use InstallCrashPoint::*;
        for purge in [false, true] {
            for point in [
                None,
                Some(UninstallJournalWritten),
                Some(UninstallFilesMoved),
                Some(UninstallRecorded),
                Some(UninstallMarked),
                Some(UninstallCommitted),
                Some(UninstallDropStarted),
                Some(UninstallFilesDropped),
                Some(UninstallJournalDeleted),
            ] {
                let f = Fixture::new(true);
                let kind = store::StorageKind::Data;
                let value = serde_json::json!("data from v1");
                store::storage_set(&f.conn, &f.id, kind, None, "saved", &value).unwrap();
                let operation = || f.service.uninstall(&f.id, purge).unwrap();
                if let Some(point) = point {
                    f.crash_at(point, operation);
                    if point == UninstallDropStarted {
                        let aside = f
                            .journal()
                            .unwrap()
                            .layout(&f.ctx.paths)
                            .unwrap()
                            .rollback
                            .unwrap();
                        assert!(!f.destination().is_dir());
                        std::fs::remove_file(aside.join("main.js")).unwrap();
                    }
                } else {
                    operation();
                }
                let warnings = f.restart();
                assert!(
                    !warnings.iter().any(|line| line.contains("needs a hand")),
                    "{point:?}: {warnings:?}"
                );
                let committed = matches!(
                    point,
                    None | Some(
                        UninstallCommitted
                            | UninstallDropStarted
                            | UninstallFilesDropped
                            | UninstallJournalDeleted
                    )
                );
                if committed {
                    let record = store::get(&f.conn, &f.id).unwrap();
                    if purge {
                        assert!(record.is_none());
                    } else {
                        assert!(record.as_ref().is_some_and(store::is_tombstone));
                    }
                    assert!(!f.destination().is_dir());
                } else {
                    f.assert_agreement("1.0.0", "// old package");
                }
                let stored = store::storage_get(&f.conn, &f.id, kind, None, "saved").unwrap();
                assert_eq!(stored, (!(committed && purge)).then_some(value));
                assert!(f.journal().is_none());
                assert!(std::fs::read_dir(f.ctx.paths.plugin_rollback_root())
                    .unwrap()
                    .all(|entry| !entry.unwrap().path().is_dir()));
                assert!(f.restart().is_empty());
            }
        }
    }

    #[test]
    fn round_two_uninstall_keeps_the_development_source_folder() {
        let f = Fixture::new(false);
        let source = f._root.path().join("source");
        extract_package(&package("1.0.0", "// development package"), &source).unwrap();
        f.service.add_development_folder(&source, true).unwrap();
        f.service.uninstall(&f.id, true).unwrap();
        assert_eq!(
            std::fs::read_to_string(source.join("main.js")).unwrap(),
            "// development package"
        );
        assert!(store::get(&f.conn, &f.id).unwrap().is_none());
        assert!(f.journal().is_none());
    }

    #[test]
    fn round_three_restore_refusal_leaves_a_partly_dropped_stash_and_journal_unchanged() {
        let restore = |f: &Fixture| {
            f.service
                .recover_interrupted_installs(Some(&f.id), InstallResolution::Restore)
                .unwrap()
        };
        let control = Fixture::new(true);
        control.interrupt_install(InstallCrashPoint::Committed, "2.0.0");
        assert!(matches!(
            restore(&control).as_slice(),
            [(_, InstallRecovery::Done(_))]
        ));
        control.assert_agreement("1.0.0", "// old package");

        for explicit_restore in [false, true] {
            let f = Fixture::new(true);
            // Simulate a recursive delete that removed an auxiliary file while leaving the
            // directory identity, manifest and entrypoint intact.
            std::fs::write(f.destination().join("auxiliary.txt"), "required content").unwrap();
            f.interrupt_install(InstallCrashPoint::StashDropStarted, "2.0.0");
            let partially_dropped = f.journal().unwrap().layout(&f.ctx.paths).unwrap().dropping;
            assert!(!f.stash().is_dir());
            std::fs::remove_file(partially_dropped.join("auxiliary.txt")).unwrap();
            if explicit_restore {
                let journal = journal_document(&f.conn, &f.id).unwrap().unwrap();
                let payload = std::fs::read(partially_dropped.join("main.js")).unwrap();
                let refused = restore(&f);
                assert_eq!(
                    journal_document(&f.conn, &f.id).unwrap().as_deref(),
                    Some(journal.as_str())
                );
                assert_eq!(
                    std::fs::read(partially_dropped.join("main.js")).unwrap(),
                    payload
                );
                assert_eq!(
                    store::get(&f.conn, &f.id)
                        .unwrap()
                        .unwrap()
                        .manifest
                        .version
                        .to_string(),
                    "2.0.0"
                );
                assert!(
                    matches!(refused.as_slice(), [(_, InstallRecovery::NeedsHand(why))] if why.contains("already being removed") && why.contains("Run without --restore"))
                );
            }
            assert!(f
                .restart()
                .iter()
                .any(|line| line.contains("Finished the interrupted install")));
            f.assert_agreement("2.0.0", "// new package");
            assert!(!partially_dropped.is_dir());
            assert!(f.restart().is_empty());
        }
    }

    #[test]
    fn plugin_recovery_retry_preserves_an_unjournaled_stash_and_can_keep_it_aside() {
        let f = Fixture::new(true);
        extract_package(&package("0.9.0", "// legacy package"), &f.stash()).unwrap();
        assert!(f
            .service
            .install_package_bytes(&package("2.0.0", "// new package"), true)
            .is_err());
        verify_package(&f.stash(), &f.id, "0.9.0").unwrap();
        let outcomes = f
            .service
            .recover_interrupted_installs(Some(&f.id), InstallResolution::KeepAside)
            .unwrap();
        assert!(matches!(
            outcomes.as_slice(),
            [(_, InstallRecovery::Done(_))]
        ));
        let kept = std::fs::read_dir(f.ctx.paths.plugin_rollback_root())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        verify_package(&kept, &f.id, "0.9.0").unwrap();
        assert_eq!(
            std::fs::read_to_string(kept.join("main.js")).unwrap(),
            "// legacy package"
        );
        assert!(f.restart().is_empty());
        f.assert_agreement("1.0.0", "// old package");
    }

    #[test]
    fn plugin_recovery_discovers_legacy_stashes_with_suffix_like_ids() {
        let f = Fixture::new(false);
        let rollback = f.ctx.paths.plugin_rollback_root();
        for name in [
            "acme.json",
            "acme.kept",
            "acme.owner.json",
            "acme.json.owner.json",
        ] {
            std::fs::create_dir_all(rollback.join(name)).unwrap();
        }
        let ids = interrupted_installs(&f.conn, &f.ctx.paths).unwrap();
        let names: Vec<_> = ids.iter().map(PluginId::as_str).collect();
        assert_eq!(names, ["acme.json", "acme.kept", "acme.owner"]);
    }

    #[test]
    fn plugin_recovery_first_install_never_claims_a_stash() {
        for point in [
            InstallCrashPoint::JournalWritten,
            InstallCrashPoint::Extracted,
            InstallCrashPoint::Committed,
        ] {
            let f = Fixture::new(false);
            f.interrupt_install(point, "2.0.0");
            let journal = f.journal().unwrap();
            assert!(
                journal.old.is_none() && journal.layout(&f.ctx.paths).unwrap().rollback.is_none()
            );
            assert!(journal
                .envelope
                .owned
                .iter()
                .all(|owned| owned.path != f.stash()));
            assert!(f
                .restart()
                .iter()
                .all(|line| !line.contains("needs a hand")));
            if point == InstallCrashPoint::Committed {
                f.assert_agreement("2.0.0", "// new package");
            } else {
                assert!(store::get(&f.conn, &f.id).unwrap().is_none());
                assert!(!f.destination().is_dir());
                assert!(f.journal().is_none());
            }
        }
    }

    #[test]
    fn plugin_recovery_same_version_swap_uses_the_commit_state() {
        let f = Fixture::new(true);
        f.interrupt_install(InstallCrashPoint::Extracted, "1.0.0");
        f.restart();
        f.assert_agreement("1.0.0", "// old package");
        f.interrupt_install(InstallCrashPoint::Committed, "1.0.0");
        f.restart();
        f.assert_agreement("1.0.0", "// new package");
    }

    #[test]
    fn plugin_recovery_retry_settles_a_sqlite_journal_but_preserves_a_legacy_one() {
        let f = Fixture::new(true);
        f.interrupt_install(InstallCrashPoint::Extracted, "2.0.0");
        f.service
            .install_package_bytes(&package("3.0.0", "// next package"), true)
            .unwrap();
        f.assert_agreement("3.0.0", "// next package");
        let legacy = install_journal_path(&f.ctx.paths, &f.id);
        std::fs::write(&legacy, "legacy json").unwrap();
        assert!(f
            .service
            .install_package_bytes(&package("4.0.0", "// later package"), true)
            .is_err());
        assert_eq!(std::fs::read_to_string(legacy).unwrap(), "legacy json");
    }

    #[test]
    fn plugin_recovery_waits_for_the_install_lock() {
        let f = Fixture::new(true);
        f.interrupt_install(InstallCrashPoint::Stashed, "2.0.0");
        let locks = LockManager::new(f.ctx.paths.locks_root());
        let held = locks
            .acquire(LockResource::PluginInstall, "other install")
            .unwrap();
        assert!(!f.restart().is_empty());
        assert!(f.stash().is_dir() && !f.destination().is_dir());
        drop(held);
        f.restart();
        f.assert_agreement("1.0.0", "// old package");
    }
}
