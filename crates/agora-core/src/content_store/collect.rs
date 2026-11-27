//! The one collection that frees unreferenced objects (MASTER_SPEC §32.8).
//!
//! The collector sweeps `content/objects` against a reference set built from current owner records.
//! The snapshot migration brings legacy objects into this store before collection can free them.
//!
//! The rule that matters more than the code: **"can't tell" always means "keep"**. A collection
//! deletes nothing at all, and says why, when any owner record, listing, pending record or object
//! time cannot be read, when an owner location is a link it cannot follow safely, or when the locks
//! it needs are busy.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::app_paths::AppPaths;
use crate::content_store::pending::PendingRecord;
use crate::content_store::ContentItem;
use crate::lock_manager::{LockGuard, LockManager, LockResource};
use crate::process_identity::{ProcessCheck, ProcessIdentity};

/// How long a newly written object is left alone whatever the records say.
///
/// The pending records catch every writer this build has. This catches anything else that can put
/// bytes in the store — an older Agora still installed beside this one, a writer that died before
/// it could register — for an hour, which is far longer than the pause between writing an object and
/// publishing the record that names it.
pub(crate) const OBJECT_GRACE: Duration = Duration::from_secs(60 * 60);

/// How long a collection waits for the content-store lock when the caller does not hold it.
const STORE_WAIT: Duration = Duration::from_secs(30);

/// What the collector does about a lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LockRequest {
    /// The caller holds it. Locks here are not reentrant, so asking again would deadlock.
    Held,
    /// Ask once: a busy lock means another operation is working, so this one deletes nothing.
    Try,
    /// Wait briefly for a passing collision, then give up.
    Wait,
}

/// What one collection is asked to do.
#[derive(Clone)]
pub(crate) struct CollectRequest {
    pub store: LockRequest,
    pub locations: LockRequest,
    /// Objects modified more recently than this are kept. `None` uses [`OBJECT_GRACE`]; a test
    /// passes `Duration::ZERO` to exercise what a collection frees.
    pub grace: Option<Duration>,
    /// The person's explicit override: an unreadable snapshot manifest is skipped and reported
    /// instead of stopping the collection. It is the same choice `--ignore-unreadable-snapshots`
    /// makes when an item is removed.
    pub ignore_unreadable_snapshots: bool,
    /// A test seam: called once the owner records have been listed and before the first delete, so
    /// a test can try to move an instance in the window the collection's locks must hold shut.
    /// Production never installs one.
    pub pause: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
}

impl std::fmt::Debug for CollectRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CollectRequest")
            .field("store", &self.store)
            .field("locations", &self.locations)
            .field("grace", &self.grace)
            .field(
                "ignore_unreadable_snapshots",
                &self.ignore_unreadable_snapshots,
            )
            .field("pause", &self.pause.as_ref().map(|_| "<hook>"))
            .finish()
    }
}

impl Default for CollectRequest {
    fn default() -> Self {
        Self {
            store: LockRequest::Wait,
            locations: LockRequest::Wait,
            grace: None,
            ignore_unreadable_snapshots: false,
            pause: None,
        }
    }
}

/// What one collection did.
#[derive(Debug, Default)]
pub struct CollectReport {
    /// The object names it removed, from either store.
    pub deleted: Vec<String>,
    /// The object names it kept only because they are younger than the grace period.
    pub kept_recent: Vec<String>,
    /// Why it deleted nothing. Non-empty means every object is still there.
    pub refusals: Vec<String>,
    /// What went wrong after it had begun deleting. The rest of the objects are still there.
    pub failures: Vec<String>,
    /// Names of pending records it found of processes that are provably gone, and removed.
    pub stale_pending: Vec<String>,
    /// Things worth telling the person that are not failures: a dead operation's record this
    /// collection cleared, an object kept because a live operation holds it.
    pub notes: Vec<String>,
}

/// What one collection did, as an adapter hands it back to the person who asked: one line for a
/// person, and the two facts a front end words its own line from (MASTER_SPEC §32.8).
///
/// A removal, a snapshot delete and a retention round all report through this, so the CLI and the
/// desktop say the same thing about the same collection. Nothing here is a failure of the
/// operation that asked for the collection: the item or snapshot is gone either way.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectOutcome {
    /// One line: what was reclaimed, or that nothing was and why. Begins in lower case so a caller
    /// can put it after a colon, and reads as "nothing was reclaimed: <why>" when it refused.
    pub summary: String,
    /// How many stored objects it freed.
    pub reclaimed: usize,
    /// Whether it refused to delete anything at all. `summary` says why.
    pub refused: bool,
}

impl CollectReport {
    /// Whether the collection refused to delete anything.
    pub fn refused(&self) -> bool {
        !self.refusals.is_empty()
    }

    /// Why it deleted nothing, as one line; empty when it deleted what it could.
    pub fn refusal_reasons(&self) -> String {
        self.refusals.join("; ")
    }

    /// What a caller shows the person who asked, or writes to the support log.
    pub fn outcome(&self) -> CollectOutcome {
        CollectOutcome {
            summary: self.summary(),
            reclaimed: self.deleted.len(),
            refused: self.refused(),
        }
    }

    /// One line for a person: what was reclaimed, or what was not and why.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.refused() {
            // Nothing was deleted at all, so the reasons are the report: a caller puts this after
            // "nothing was reclaimed:".
            parts.push(format!("nothing was reclaimed: {}", self.refusal_reasons()));
        } else {
            if !self.deleted.is_empty() {
                parts.push(format!("reclaimed {} object(s)", self.deleted.len()));
            }
            if !self.kept_recent.is_empty() {
                parts.push(format!(
                    "kept {} recent object(s) inside the grace period",
                    self.kept_recent.len()
                ));
            }
            if !self.stale_pending.is_empty() {
                parts.push(format!(
                    "cleared {} pending record(s) of operations that are no longer running",
                    self.stale_pending.len()
                ));
            }
            if !self.failures.is_empty() {
                parts.push(format!("could not finish: {}", self.failures.join("; ")));
            }
        }
        parts.extend(self.notes.iter().cloned());
        if parts.is_empty() {
            return "nothing to reclaim".into();
        }
        parts.join("; ")
    }
}

/// Collect every object in the content store that nothing uses.
///
/// `identity` is the one liveness check (32.13) as a function, so a context's seam reaches it and a
/// caller without a context still asks the real process table.
fn collect(
    paths: &AppPaths,
    identity: &dyn Fn(&ProcessIdentity) -> ProcessCheck,
    request: &CollectRequest,
    after_live_listing: Option<&(dyn Fn() + Send + Sync)>,
) -> Result<CollectReport, String> {
    #[cfg(test)]
    COLLECTION_COUNT.set(COLLECTION_COUNT.get() + 1);
    let manager = LockManager::new(paths.locks_root());
    // Store lock first, then instance locations: the order every caller uses, so two collectors
    // and an instance move cannot deadlock.
    let _store = acquire(
        &manager,
        LockResource::ContentStore,
        request.store,
        STORE_WAIT,
    )?;
    let _locations = match request.locations {
        LockRequest::Held => None,
        LockRequest::Try => Some(
            manager
                .try_instance_locations("content-collect")
                .map_err(|error| format!("instance locations are busy: {error}"))?,
        ),
        LockRequest::Wait => Some(
            manager
                .wait_instance_locations("content-collect")
                .map_err(|error| format!("instance locations are busy: {error}"))?,
        ),
    };

    let mut report = CollectReport::default();
    let mut keep: HashSet<String> = HashSet::new();
    let mut owners = Owners::default();

    // Every pending record, owner record and listing is read before anything is deleted. A read
    // that fails is a refusal, not a half-finished collection: the report says which path stopped
    // it, and every object stays where it is.
    //
    // The pending records are read first on purpose. A writer publishes its owner record and drops
    // its pending record under one hold of the store lock this collection is holding, so reading
    // the records before the owners means the collection sees one or the other — never a writer
    // whose record is already gone and whose owner record it read a moment too early.
    let stale = match read_pending(paths, identity, &mut report) {
        Ok((live_hashes, stale)) => {
            keep.extend(live_hashes);
            stale
        }
        Err(error) => {
            report.refusals.push(error);
            Vec::new()
        }
    };
    if let Err(error) = owners.read_items(paths, &mut keep, &mut report) {
        report.refusals.push(error);
    }
    if let Err(error) =
        owners.read_instances(paths, request, &mut keep, &mut report, after_live_listing)
    {
        report.refusals.push(error);
    }

    // Read every object time in the content store before the first delete. An unreadable time
    // keeps everything. The migration owns the legacy store.
    let mut candidates = Vec::new();
    let mut recent = Vec::new();
    let grace = request.grace.unwrap_or(OBJECT_GRACE);
    let now = SystemTime::now();
    if let Err(error) = read_object_root(
        &paths.content_objects_dir(),
        &keep,
        grace,
        now,
        &mut candidates,
        &mut recent,
    ) {
        report.refusals.push(error);
    }
    if report.refused() {
        // Something could not be read: the listing is incomplete and nothing is deleted.
        return Ok(report);
    }
    report.kept_recent = recent;
    if let Some(pause) = &request.pause {
        pause();
    }

    for (hash, path) in candidates {
        match remove_object(&path) {
            Ok(()) => report.deleted.push(hash),
            Err(error) => report.failures.push(error),
        }
    }

    // A dead operation's record is cleared under the store lock this collection holds, after the
    // objects it names have been judged. It was not read as "in use", so its presence is reported.
    for path in stale {
        match std::fs::remove_file(&path) {
            Ok(()) => report.stale_pending.push(path.display().to_string()),
            Err(error) => report
                .failures
                .push(format!("cannot remove {}: {error}", path.display())),
        }
    }
    Ok(report)
}

/// Collect through a context, so its liveness seam answers for pending records and its object-grace
/// seam decides how long a new object is spared.
///
/// The content-store lock must already be held by the caller (`request.store` is `Held`): an
/// operation that publishes an owner record holds it for its whole body, and taking it again would
/// deadlock on a lock that is not reentrant.
pub(crate) fn collect_with_ctx(ctx: &crate::ctx::Ctx, request: &CollectRequest) -> CollectReport {
    let mut request = request.clone();
    if request.grace.is_none() {
        request.grace = ctx.test_seams.object_grace();
    }
    match collect(
        &ctx.paths,
        &|identity| ctx.process_check(identity),
        &request,
        ctx.test_seams.object_collection_after_live().as_deref(),
    ) {
        Ok(report) => report,
        Err(error) => CollectReport {
            refusals: vec![error],
            ..CollectReport::default()
        },
    }
}

/// Collect for a caller that has an instance directory but no context: the real process table is
/// the one liveness check, and there is no seam to shorten the grace period.
pub(crate) fn collect_at_instance(
    instance_dir: &Path,
    request: &CollectRequest,
) -> Result<CollectReport, String> {
    let paths = crate::snapshot::snapshot_store_paths(instance_dir);
    collect(
        &paths,
        &|identity| crate::process_identity::check(identity),
        request,
        None,
    )
}

/// One collection the person asked for by running `agora games content gc` (MASTER_SPEC §32.8).
///
/// Nothing is removed first: this is the collector alone, over both object stores, against the
/// owner records as they are now. It waits briefly for the content store and instance-locations
/// locks, the way a user-requested delete does, and reports what it freed — or that it deleted
/// nothing and why. The one choice it takes is the person's override for a snapshot manifest that
/// cannot be read, which keeps that snapshot's objects and names it in `failures`, with the risk.
pub fn request_collection(
    ctx: &crate::ctx::Ctx,
    ignore_unreadable_snapshots: bool,
) -> CollectReport {
    collect_with_ctx(
        ctx,
        &CollectRequest {
            store: LockRequest::Wait,
            locations: LockRequest::Wait,
            grace: None,
            pause: None,
            ignore_unreadable_snapshots,
        },
    )
}

#[cfg(test)]
thread_local! {
    static COLLECTION_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn take_collection_count() -> usize {
    COLLECTION_COUNT.replace(0)
}

/// Take one lock for a collection.
fn acquire(
    manager: &LockManager,
    resource: LockResource,
    request: LockRequest,
    wait: Duration,
) -> Result<Option<LockGuard>, String> {
    let guard = match request {
        LockRequest::Held => return Ok(None),
        LockRequest::Try => {
            manager.acquire_with_timeout(resource.clone(), "content-collect", Duration::ZERO, None)
        }
        LockRequest::Wait => {
            manager.acquire_with_timeout(resource.clone(), "content-collect", wait, None)
        }
    };
    match guard {
        Ok(guard) => Ok(Some(guard)),
        Err(error) => Err(format!(
            "cannot take the {} lock: {error}",
            resource.lock_name().unwrap_or_default()
        )),
    }
}

/// The object hashes the item records name: files, a saved icon, and a hash a source published.
#[derive(Default)]
struct Owners {
    /// The hashes each item record names, so an instance that uses an item keeps its objects.
    item_hashes: HashMap<String, HashSet<String>>,
}

impl Owners {
    fn read_items(
        &mut self,
        paths: &AppPaths,
        keep: &mut HashSet<String>,
        report: &mut CollectReport,
    ) -> Result<(), String> {
        let dir = paths.content_items_dir();
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(format!(
                    "cannot list the item records in {}: {error}",
                    dir.display()
                ))
            }
        };
        for entry in entries {
            let entry = entry
                .map_err(|error| format!("cannot read an entry in {}: {error}", dir.display()))?;
            let path = entry.path();
            if path
                .extension()
                .map(|extension| extension != "json")
                .unwrap_or(true)
            {
                continue;
            }
            let item: ContentItem = match std::fs::read_to_string(&path)
                .map_err(|error| error.to_string())
                .and_then(|text| serde_json::from_str(&text).map_err(|error| error.to_string()))
            {
                Ok(item) => item,
                Err(error) => {
                    report.refusals.push(format!(
                        "the item record {} cannot be read, so it may name any object: {error}",
                        path.display()
                    ));
                    continue;
                }
            };
            let mut hashes = HashSet::new();
            // Object names are lowercase hex: a record that wrote a digest in upper case still
            // names the same object, and comparing it as written would free it.
            for file in &item.files {
                hashes.insert(file.sha256.to_ascii_lowercase());
            }
            if let Some(icon) = &item.icon_sha256 {
                hashes.insert(icon.to_ascii_lowercase());
            }
            for source in &item.sources {
                match source {
                    crate::content_store::ContentSource::Archive { sha256, .. }
                    | crate::content_store::ContentSource::Catalog { sha256, .. } => {
                        hashes.insert(sha256.to_ascii_lowercase());
                    }
                    _ => {}
                }
            }
            keep.extend(hashes.iter().cloned());
            self.item_hashes.insert(item.item_id.clone(), hashes);
        }
        Ok(())
    }

    /// Every live instance manifest, every snapshot manifest inside one, and both Recently deleted
    /// roots. An owner location that is a link is named and stops the collection: following it would
    /// judge objects by a tree this collection was not asked about.
    fn read_instances(
        &mut self,
        paths: &AppPaths,
        request: &CollectRequest,
        keep: &mut HashSet<String>,
        report: &mut CollectReport,
        after_live_listing: Option<&(dyn Fn() + Send + Sync)>,
    ) -> Result<(), String> {
        let mut instances: Vec<PathBuf> = Vec::new();
        let mut collect_folder = |path: &Path, report: &mut CollectReport| -> Result<(), String> {
            match std::fs::symlink_metadata(path) {
                Ok(metadata) if metadata.is_dir() => instances.push(path.to_path_buf()),
                // A link where an instance folder should be is named, not followed: the manifests
                // behind it belong to a tree this collection was not asked about, so judging
                // objects by it would be a guess about what is in use.
                Ok(_) => report.refusals.push(format!(
                    "the instance folder {} is a link or not a folder, so what it names cannot be \
                     read; nothing is deleted",
                    path.display()
                )),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => report.refusals.push(format!(
                    "cannot inspect the instance folder {}: {error}",
                    path.display()
                )),
            }
            Ok(())
        };
        self.folder_entries(&paths.instances_root(), report, &mut collect_folder)?;
        if let Some(hook) = after_live_listing {
            hook();
        }

        for root in [
            paths.recently_deleted_root(),
            paths.recently_deleted_games_root(),
        ] {
            let mut collect_trashed =
                |path: &Path, report: &mut CollectReport| -> Result<(), String> {
                    let moved = path.join("instance");
                    match std::fs::symlink_metadata(&moved) {
                        Ok(metadata) if metadata.is_dir() => instances.push(moved),
                        // The same rule for a deleted instance: a link names a tree this
                        // collection cannot read, so it refuses and keeps every object.
                        Ok(_) => report.refusals.push(format!(
                            "the deleted instance {} is a link or not a folder, so what it names \
                             cannot be read; nothing is deleted",
                            moved.display()
                        )),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => report.refusals.push(format!(
                            "cannot inspect the deleted instance {}: {error}",
                            moved.display()
                        )),
                    }
                    Ok(())
                };
            self.folder_entries(&root, report, &mut collect_trashed)?;
        }

        // A launcher import moves an instance tree — snapshots included — under `staging/` before
        // it rolls back or promotes it. A collection running in that window, or one that starts
        // after a crash before the rollback, has to read the manifests that moved there: they name
        // objects the instance will need back, and nothing else names them meanwhile
        // (MASTER_SPEC §32.8). `folder_entries` has already refused a link or a folder it cannot
        // list, so a name that matches is read as an instance tree below.
        let mut collect_moved = |path: &Path, _report: &mut CollectReport| -> Result<(), String> {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                return Ok(());
            };
            if name.starts_with(".agora-quarantine-") || name.starts_with(".agora-import-") {
                instances.push(path.to_path_buf());
            }
            Ok(())
        };
        self.folder_entries(&paths.staging_root(), report, &mut collect_moved)?;

        for instance in &instances {
            self.read_instance_manifest(instance, keep, report);
            self.read_snapshot_folder(instance, request, keep, report);
        }
        Ok(())
    }

    /// List one folder of owner locations, refusing a link where a folder should be.
    fn folder_entries(
        &self,
        root: &Path,
        report: &mut CollectReport,
        mut visit: impl FnMut(&Path, &mut CollectReport) -> Result<(), String>,
    ) -> Result<(), String> {
        match std::fs::symlink_metadata(root) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                report.refusals.push(format!(
                    "{} is a link, so the objects its records name cannot be judged",
                    root.display()
                ));
                return Ok(());
            }
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                report.refusals.push(format!(
                    "{} is not a folder, so the objects its records name cannot be judged",
                    root.display()
                ));
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(format!("cannot inspect {}: {error}", root.display()));
            }
        }
        let entries = std::fs::read_dir(root)
            .map_err(|error| format!("cannot list {}: {error}", root.display()))?;
        for entry in entries {
            let entry = entry
                .map_err(|error| format!("cannot read an entry in {}: {error}", root.display()))?;
            let path = entry.path();
            if std::fs::symlink_metadata(&path)
                .map(|metadata| metadata.file_type().is_symlink())
                .unwrap_or(false)
            {
                report.refusals.push(format!(
                    "{} is a link, so the objects its records name cannot be judged",
                    path.display()
                ));
                continue;
            }
            visit(&path, report)?;
        }
        Ok(())
    }

    /// The object hashes and item references one instance manifest names.
    fn read_instance_manifest(
        &mut self,
        instance_dir: &Path,
        keep: &mut HashSet<String>,
        report: &mut CollectReport,
    ) {
        let path = instance_dir.join("instance_manifest.json");
        // Asked without following a link: a dangling link reads as "no manifest" through
        // `metadata`, which would let a collection judge an instance by nothing at all.
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => {
                report.refusals.push(format!(
                    "{} is not a readable file, so the objects this instance uses cannot be told",
                    path.display()
                ));
                return;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => {
                report
                    .refusals
                    .push(format!("cannot inspect {}: {error}", path.display()));
                return;
            }
        }
        let manifest = match crate::helpers::read_manifest(&path) {
            Ok(manifest) => manifest,
            Err(error) => {
                report.refusals.push(format!(
                    "the instance manifest {} cannot be read, so it may name any object: {error}",
                    path.display()
                ));
                return;
            }
        };
        let (item_ids, hashes) = crate::library::instance_references(&manifest);
        keep.extend(hashes);
        for item_id in item_ids {
            if let Some(item_hashes) = self.item_hashes.get(&item_id) {
                keep.extend(item_hashes.iter().cloned());
            }
        }
    }

    /// Every snapshot manifest inside one instance's `.agora_snapshots` folder.
    fn read_snapshot_folder(
        &self,
        instance_dir: &Path,
        request: &CollectRequest,
        keep: &mut HashSet<String>,
        report: &mut CollectReport,
    ) {
        let dir = crate::snapshot::snapshots_dir(instance_dir);
        match std::fs::symlink_metadata(&dir) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                report.refusals.push(format!(
                    "{} is a link, so the snapshots it holds cannot be judged",
                    dir.display()
                ));
                return;
            }
            Ok(metadata) if metadata.is_dir() => {}
            // A file where the snapshots folder should be is not "no snapshots": what the
            // instance holds cannot be told.
            Ok(_) => {
                report.refusals.push(format!(
                    "{} is not a folder, so the snapshots it holds cannot be judged",
                    dir.display()
                ));
                return;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => {
                report
                    .refusals
                    .push(format!("cannot inspect {}: {error}", dir.display()));
                return;
            }
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) => {
                report
                    .refusals
                    .push(format!("cannot list {}: {error}", dir.display()));
                return;
            }
        };
        for entry in entries {
            let path = match entry {
                Ok(entry) => entry.path(),
                Err(error) => {
                    report.refusals.push(format!(
                        "cannot read an entry in {}: {error}",
                        dir.display()
                    ));
                    continue;
                }
            };
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !crate::snapshot::looks_like_snapshot_manifest(name) {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            match crate::snapshot::snapshot_manifest_references(&path, id) {
                Ok((hashes, content_refs)) => {
                    keep.extend(hashes);
                    for item_id in content_refs {
                        if let Some(item_hashes) = self.item_hashes.get(&item_id) {
                            keep.extend(item_hashes.iter().cloned());
                        }
                    }
                }
                Err(error) if request.ignore_unreadable_snapshots => {
                    report.failures.push(format!(
                        "kept the objects of a snapshot that cannot be read ({}) because \
                         --ignore-unreadable-snapshots was given, and a snapshot that cannot be \
                         read may lose files it needs to restore: {error}",
                        path.display()
                    ));
                }
                Err(error) => {
                    report.refusals.push(format!(
                        "the snapshot manifest {} cannot be read, so it may name any object: {error}",
                        path.display()
                    ));
                }
            }
        }
    }
}

/// The hashes the pending records of live or unjudgeable operations hold, and the records of
/// provably dead processes that this collection will clear.
fn read_pending(
    paths: &AppPaths,
    identity: &dyn Fn(&ProcessIdentity) -> ProcessCheck,
    report: &mut CollectReport,
) -> Result<(HashSet<String>, Vec<PathBuf>), String> {
    let mut keep = HashSet::new();
    let mut stale = Vec::new();
    let dir = paths.content_pending_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((keep, stale)),
        Err(error) => return Err(format!("cannot list {}: {error}", dir.display())),
    };
    for entry in entries {
        let path = entry
            .map_err(|error| format!("cannot read an entry in {}: {error}", dir.display()))?
            .path();
        if path
            .extension()
            .map(|extension| extension != "json")
            .unwrap_or(true)
        {
            continue;
        }
        let record = match PendingRecord::read(&path) {
            Ok(record) => record,
            Err(error) => {
                report.refusals.push(format!(
                    "{error}; the objects it holds are kept because they cannot be told"
                ));
                continue;
            }
        };
        match identity(&record.process) {
            ProcessCheck::Gone => {
                // A record left by an operation that is provably gone is a note, not a failure:
                // the collection cleared it, and nothing was kept on account of it.
                report.notes.push(format!(
                    "the pending record of '{}' (process {}) was left by an operation that is no \
                     longer running; its objects are no longer held by it",
                    record.operation, record.process.pid
                ));
                stale.push(path);
            }
            // The hashes a record names are object names, which are lowercase hex: a record that
            // wrote one in upper case still holds that object.
            ProcessCheck::Alive => {
                keep.extend(record.hashes.iter().map(|hash| hash.to_ascii_lowercase()))
            }
            ProcessCheck::CantTell(reason) => {
                report.failures.push(format!(
                    "cannot tell whether '{}' (process {}) is still running ({reason}), so the \
                     objects it holds are kept",
                    record.operation, record.process.pid
                ));
                keep.extend(record.hashes.iter().map(|hash| hash.to_ascii_lowercase()));
            }
        }
    }
    Ok((keep, stale))
}

/// Read one store: every object's name and time, and whether it may go.
fn read_object_root(
    root: &Path,
    keep: &HashSet<String>,
    grace: Duration,
    now: SystemTime,
    candidates: &mut Vec<(String, PathBuf)>,
    recent: &mut Vec<String>,
) -> Result<(), String> {
    match std::fs::symlink_metadata(root) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(format!(
                "{} is a link or not a folder, so the objects in it cannot be listed; nothing is deleted",
                root.display()
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("cannot inspect {}: {error}", root.display())),
    }
    let shards = std::fs::read_dir(root)
        .map_err(|error| format!("cannot list {}: {error}", root.display()))?;
    for shard in shards {
        let shard = shard
            .map_err(|error| format!("cannot read an entry in {}: {error}", root.display()))?
            .path();
        match std::fs::symlink_metadata(&shard) {
            Ok(metadata) if metadata.is_dir() => {}
            // A link where a shard of the store should be names objects this collection cannot
            // see: listing it would judge an incomplete set, so nothing is deleted.
            Ok(_) => {
                return Err(format!(
                    "{} is a link or not a folder, so the objects in it cannot be listed; nothing \
                     is deleted",
                    shard.display()
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!("cannot inspect {}: {error}", shard.display()));
            }
        }
        let objects = std::fs::read_dir(&shard)
            .map_err(|error| format!("cannot list {}: {error}", shard.display()))?;
        for object in objects {
            let path = object
                .map(|entry| entry.path())
                .map_err(|error| format!("cannot read an entry in {}: {error}", shard.display()))?;
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if crate::snapshot::validate_blob_hash(name).is_err() {
                // A receipt beside an object, a temp file on its way in: not an object.
                continue;
            }
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_file() => metadata,
                // Anything else under an object name — a link included — is something this
                // collection cannot judge, so it stops rather than reads past it.
                Ok(_) => {
                    return Err(format!(
                        "{} is a link or not a file, so what it holds cannot be told; nothing is \
                         deleted",
                        path.display()
                    ))
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(format!("cannot inspect {}: {error}", path.display()));
                }
            };
            if keep.contains(name) {
                continue;
            }
            let modified = match metadata.modified() {
                Ok(modified) => modified,
                Err(error) => {
                    return Err(format!(
                        "cannot read the time of {}, so nothing is deleted: {error}",
                        path.display()
                    ))
                }
            };
            let age = now.duration_since(modified).unwrap_or_default();
            if age < grace {
                recent.push(name.to_string());
                continue;
            }
            candidates.push((name.to_string(), path));
        }
    }
    Ok(())
}

/// Remove one object, with the store's own protection cleared first, and the receipt beside it.
fn remove_object(path: &Path) -> Result<(), String> {
    crate::content_store::protect::unprotect(path)
        .map_err(|error| format!("cannot clear the protection on {}: {error}", path.display()))?;
    std::fs::remove_file(path)
        .map_err(|error| format!("cannot remove {}: {error}", path.display()))?;
    if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
        let _ = std::fs::remove_file(path.with_file_name(format!("{name}.agora-verified.json")));
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir(parent);
    }
    Ok(())
}
