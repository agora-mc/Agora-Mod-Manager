//! Pinned bases for game instances (MASTER_SPEC §26.4).
//!
//! A pinned base is a private copy of one game version that every instance
//! pinned to that version runs from, so a store update cannot break an instance.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

use agora_game_api::{
    fold_path_case, FrameworkDefinition, GameDefinition, InstallId, RuntimeIdentity, VolumeInfo,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::app_paths::AppPaths;
use crate::game_discovery::volume::VolumeDetector;
use crate::game_registry::IdentifiedInstall;

pub use agora_game_api::BaseMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileIdentity {
    pub volume: u64,
    pub index: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaseFile {
    pub path: String, // relative, '/'-separated, as RelPath normalises it
    pub size: u64,
    pub sha256: String, // lowercase hex
    pub linked: bool,
    pub identity: Option<FileIdentity>, // of the base file, recorded after it is created
    pub modified_unix_ms: i64,          // of the base file
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaseManifest {
    pub manifest_version: u32, // 1
    pub base_id: String,
    pub runtime: RuntimeIdentity,
    pub mode: BaseMode,
    pub source_install: InstallId,
    #[serde(default)]
    pub source_product: Option<String>,
    pub source_location: PathBuf,
    pub location: PathBuf, // the base's own folder
    pub created_unix_ms: i64,
    pub files: Vec<BaseFile>, // sorted by path
    pub skipped: Vec<String>, // source entries not copied (symlinks, junctions), with reasons
    /// Frameworks whose marker the source install had but whose declared required files were not
    /// all there. Recorded at pin time so an incomplete pin is visible even before a check.
    #[serde(default)]
    pub incomplete_frameworks: Vec<IncompleteFramework>,
}

/// A framework installed in a source install with files it declares missing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncompleteFramework {
    pub framework: String,
    pub name: String,
    /// The declared game paths the source install did not have.
    pub missing: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BuildOutcome {
    Built {
        manifest: BaseManifest,
        linked_bytes: u64,
        copied_bytes: u64,
    },
    Existing(BaseManifest),
}

impl BuildOutcome {
    pub fn manifest(&self) -> &BaseManifest {
        match self {
            BuildOutcome::Built { manifest, .. } => manifest,
            BuildOutcome::Existing(manifest) => manifest,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildProgress {
    pub files_done: u64,
    pub files_total: u64,
    pub bytes_hashed: u64,
    pub bytes_total: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildOptions {
    pub include_excluded: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyDepth {
    Quick,
    Full,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProblemKind {
    Missing,
    SizeChanged { expected: u64, actual: u64 },
    ContentChanged,
    Unexpected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaseProblem {
    pub path: String,
    pub kind: ProblemKind,
    /// The file is hardlinked to the store install, so a change to it is a
    /// change to the store install too. Until the write layer of Phase 3
    /// exists, an undeclared write to a linked file reaches the store.
    #[serde(default)]
    pub linked_to_store: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaseVerification {
    pub checked: usize,
    pub hashed: usize,
    #[serde(default)]
    pub game_writes: Vec<String>,
    pub problems: Vec<BaseProblem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaseListing {
    pub manifest: BaseManifest,
    pub present: bool,
}

/// A base the listing found, either readable or damaged. A damaged record is listed rather than
/// dropped: the user can see it, read why it cannot be used, and remove it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BaseEntry {
    Readable(Box<BaseListing>),
    Damaged(DamagedBase),
}

impl BaseEntry {
    /// The base's id: from the manifest when it reads, otherwise from the file name.
    pub fn base_id(&self) -> &str {
        match self {
            BaseEntry::Readable(listing) => &listing.manifest.base_id,
            BaseEntry::Damaged(damaged) => &damaged.base_id,
        }
    }
}

/// A manifest file in the bases folder that cannot be read as a base record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DamagedBase {
    /// The file name without `.json`. Not checked here; removal checks it.
    pub base_id: String,
    pub manifest_path: PathBuf,
    /// Why the record cannot be used, naming the file and the cause.
    pub error: String,
}

#[derive(Debug, thiserror::Error)]
pub enum BaseError {
    #[error("install runtime is unknown or unidentified")]
    RuntimeUnknown,
    #[error("hardlinks unavailable: {reason} (copied base requires {copied_bytes} bytes)")]
    LinkUnavailable { reason: String, copied_bytes: u64 },
    #[error("base not found: {0}")]
    NotFound(String),
    #[error("framework '{0}' is not installed in this base")]
    FrameworkNotInstalled(String),
    #[error("base is in use by instance(s): {}", instances.join(", "))]
    InUse { instances: Vec<String> },
    #[error("declared write matches linked file: {path}")]
    DeclaredWriteLinked { path: String },
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}

// ---------------------------------------------------------------------------
// Base ID calculation
// ---------------------------------------------------------------------------

pub fn sanitize_base_id_part(original: &str) -> String {
    let mut kept = String::new();
    for c in original.chars() {
        let lc = c.to_ascii_lowercase();
        if lc.is_ascii_lowercase() || lc.is_ascii_digit() || lc == '.' || lc == '-' {
            kept.push(lc);
        }
    }
    let changed = kept != original;
    let is_empty = kept.is_empty();
    // Windows strips a trailing dot from a folder name, and a leading one hides
    // it on Unix; either would make the recorded location wrong.
    let edge_dot = kept.starts_with('.') || kept.ends_with('.');

    if changed || is_empty || edge_dot {
        let hash = format!("{:08x}", crate::game_registry::fnv1a_32(original));
        let kept = kept.trim_matches('.');
        if kept.is_empty() {
            format!("h{hash}")
        } else {
            format!("{kept}-{hash}")
        }
    } else {
        kept
    }
}

pub fn make_base_id(runtime: &RuntimeIdentity) -> String {
    let game = sanitize_base_id_part(runtime.game.as_str());
    let store = sanitize_base_id_part(runtime.store.as_str());
    let version = sanitize_base_id_part(&runtime.version);
    let build = match runtime.build.as_deref() {
        Some(b) if !b.is_empty() => sanitize_base_id_part(b),
        _ => "nobuild".to_string(),
    };
    format!("{game}_{store}_{version}_{build}")
}

// ---------------------------------------------------------------------------
// Volume and file identity helpers
// ---------------------------------------------------------------------------

pub fn get_file_identity(path: &Path) -> Option<FileIdentity> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        let file = std::fs::File::open(path).ok()?;
        let handle = file.as_raw_handle();
        let mut info: windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION =
            unsafe { std::mem::zeroed() };
        let ok = unsafe {
            windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle(
                handle as _,
                &mut info,
            )
        };
        if ok != 0 {
            let volume = info.dwVolumeSerialNumber as u64;
            let index = ((info.nFileIndexHigh as u64) << 32) | (info.nFileIndexLow as u64);
            Some(FileIdentity { volume, index })
        } else {
            None
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(path).ok()?;
        Some(FileIdentity {
            volume: meta.dev(),
            index: meta.ino(),
        })
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = path;
        None
    }
}

pub fn get_file_identity_from_file(file: &std::fs::File) -> Option<FileIdentity> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        let handle = file.as_raw_handle();
        let mut info: windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION =
            unsafe { std::mem::zeroed() };
        let ok = unsafe {
            windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle(
                handle as _,
                &mut info,
            )
        };
        if ok != 0 {
            let volume = info.dwVolumeSerialNumber as u64;
            let index = ((info.nFileIndexHigh as u64) << 32) | (info.nFileIndexLow as u64);
            Some(FileIdentity { volume, index })
        } else {
            None
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = file.metadata().ok()?;
        Some(FileIdentity {
            volume: meta.dev(),
            index: meta.ino(),
        })
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = file;
        None
    }
}

pub fn get_modified_unix_ms(metadata: &std::fs::Metadata) -> i64 {
    match metadata.modified() {
        Ok(t) => match t.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => d.as_millis() as i64,
            Err(e) => -(e.duration().as_millis() as i64),
        },
        Err(_) => 0,
    }
}

fn is_reparse_point_or_symlink(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        (metadata.file_attributes() & 0x0000_0400) != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

/// Pure volume decision function for checking whether hardlinking is supported.
pub fn evaluate_link_support(
    base_volume: Option<&VolumeInfo>,
    install_volume: Option<&VolumeInfo>,
) -> Result<(), String> {
    let (Some(bv), Some(iv)) = (base_volume, install_volume) else {
        return Err("volume information unavailable".to_string());
    };
    if !bv.id.eq_ignore_ascii_case(&iv.id) {
        return Err(format!(
            "base location is on volume {}, but game install is on volume {}",
            bv.id, iv.id
        ));
    }
    if !iv.supports_hardlinks || !bv.supports_hardlinks {
        return Err(format!(
            "volume filesystem '{}' does not support hard links",
            iv.filesystem
        ));
    }
    Ok(())
}

pub fn bases_root_for(
    paths: &AppPaths,
    install_location: &Path,
    install_volume_id: Option<&str>,
) -> PathBuf {
    #[cfg(windows)]
    {
        let detector = VolumeDetector::new();
        let data_vol = detector.get_volume_info(&paths.bases_dir());
        let install_vol_id = install_volume_id
            .map(|s| s.to_string())
            .or_else(|| detector.get_volume_info(install_location).map(|v| v.id));

        if let (Some(d), Some(i)) = (data_vol.map(|v| v.id), install_vol_id) {
            if d.eq_ignore_ascii_case(&i) {
                return paths.bases_dir();
            }
        }

        if let Some(mount_root) =
            crate::game_discovery::volume::get_volume_mount_root(install_location)
        {
            return mount_root.join("AgoraBases");
        }
    }
    let _ = install_location;
    let _ = install_volume_id;
    paths.bases_dir()
}

// ---------------------------------------------------------------------------
// Source walking and staging
// ---------------------------------------------------------------------------

struct SourceEntry {
    rel_path: String,
    abs_src_path: PathBuf,
    size: u64,
    is_archive: bool,
}

fn walk_source_dir(
    source_root: &Path,
    rel_dir: &Path,
    definition: &GameDefinition,
    options: BuildOptions,
    entries: &mut Vec<SourceEntry>,
    skipped: &mut Vec<String>,
) -> Result<(), std::io::Error> {
    let cur_dir = if rel_dir.as_os_str().is_empty() {
        source_root.to_path_buf()
    } else {
        source_root.join(rel_dir)
    };

    let read_dir = std::fs::read_dir(&cur_dir)?;
    for entry in read_dir {
        let entry = entry?;
        let file_name = entry.file_name();
        let rel_path = rel_dir.join(&file_name);
        let abs_path = entry.path();
        let rel_str = rel_path.to_string_lossy().replace('\\', "/");

        let meta = match std::fs::symlink_metadata(&abs_path) {
            Ok(m) => m,
            Err(e) => {
                skipped.push(format!("{rel_str}: failed to read metadata ({e})"));
                continue;
            }
        };

        if is_reparse_point_or_symlink(&meta) {
            skipped.push(format!("{rel_str}: symlink or junction"));
            continue;
        }

        if meta.is_dir() {
            walk_source_dir(
                source_root,
                &rel_path,
                definition,
                options,
                entries,
                skipped,
            )?;
        } else if meta.is_file() {
            if !options.include_excluded && definition.is_excluded(&rel_str) {
                skipped.push(format!("{rel_str}: excluded by the game definition"));
                continue;
            }
            let is_archive = definition.is_linked_archive(&rel_str);
            entries.push(SourceEntry {
                rel_path: rel_str,
                abs_src_path: abs_path,
                size: meta.len(),
                is_archive,
            });
        } else {
            skipped.push(format!("{rel_str}: non-regular file skipped"));
        }
    }
    Ok(())
}

fn hash_file(path: &Path) -> Result<String, std::io::Error> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 128 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

struct StagingGuard<'a> {
    path: &'a Path,
    active: bool,
}

impl<'a> Drop for StagingGuard<'a> {
    fn drop(&mut self) {
        if self.active && self.path.exists() {
            let _ = std::fs::remove_dir_all(self.path);
        }
    }
}

// ---------------------------------------------------------------------------
// Build base
// ---------------------------------------------------------------------------

/// How many bytes a base built from the install at `location` would hold: the sizes of the files
/// a build copies or links, left out the game's excluded paths unless `options` say otherwise.
/// This is the cost of a Copied base, which the user is told before building. Metadata only; no
/// file is read. `Err` when the folder cannot be walked, so a caller never shows 0 for "unknown".
pub fn install_size(
    location: &Path,
    definition: &GameDefinition,
    options: BuildOptions,
) -> Result<u64, BaseError> {
    let mut entries = Vec::new();
    let mut skipped = Vec::new();
    walk_source_dir(
        location,
        Path::new(""),
        definition,
        options,
        &mut entries,
        &mut skipped,
    )
    .map_err(|e| BaseError::Other(format!("cannot read '{}': {e}", location.display())))?;
    Ok(entries.iter().map(|e| e.size).sum())
}

/// The marker Agora writes into a base folder it built, so a rebuild only removes a folder it can
/// prove is its own.
const BASE_MARKER_FILE: &str = ".agora-base.json";

fn base_marker_path(dir: &Path) -> PathBuf {
    dir.join(BASE_MARKER_FILE)
}

fn write_base_marker(dir: &Path, base_id: &str) -> Result<(), std::io::Error> {
    let marker = serde_json::json!({ "base_id": base_id });
    std::fs::write(base_marker_path(dir), serde_json::to_vec(&marker)?)
}

fn base_marker_matches(dir: &Path, base_id: &str) -> bool {
    std::fs::read_to_string(base_marker_path(dir))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| {
            value
                .get("base_id")
                .and_then(|id| id.as_str())
                .map(|id| id == base_id)
        })
        .unwrap_or(false)
}

/// What the record for a base says, decided by attempting the real read. `Path::exists` is never
/// the judge here: on Linux it answers "there" for a record nobody may read, and "gone" for a
/// broken symlink, and either answer invites replacing or rewriting a base this build could not
/// understand. Only a clean `NotFound` on the read is absence.
#[derive(Debug)]
enum BaseRecord {
    /// Nothing is at the record's path.
    Missing,
    /// A record is there but is not a readable base record.
    Damaged,
    /// A healthy manifest.
    Healthy(Box<BaseManifest>),
}

fn read_base_record(base_id: &str, manifest_path: &Path) -> Result<BaseRecord, BaseError> {
    let text = match std::fs::read_to_string(manifest_path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BaseRecord::Missing)
        }
        Err(error) => {
            return Err(BaseError::Other(format!(
                "the record of base {base_id} ({}) cannot be read: {error}",
                manifest_path.display()
            )))
        }
    };
    match serde_json::from_str::<BaseManifest>(&text) {
        Ok(manifest) => Ok(BaseRecord::Healthy(Box::new(manifest))),
        Err(_) => Ok(BaseRecord::Damaged),
    }
}

/// Whether something occupies the base folder's name. Only a clean `NotFound` means the folder is
/// gone; a stat that fails any other way is "could not tell", which must not invite the staging
/// rename over it — the rename then fails and reports instead.
fn base_folder_present(dir: &Path) -> bool {
    !matches!(
        std::fs::symlink_metadata(dir),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    )
}

/// Build a base for `install`: the folder every instance of it shares.
#[allow(clippy::too_many_arguments)]
pub fn build_base(
    paths: &AppPaths,
    install: &IdentifiedInstall,
    definition: &GameDefinition,
    mode: BaseMode,
    root_override: Option<&Path>,
    options: BuildOptions,
    seams: &crate::test_seams::TestSeams,
    progress: &(dyn Fn(BuildProgress) + Send + Sync),
) -> Result<BuildOutcome, BaseError> {
    build_base_with_frameworks(
        paths,
        install,
        definition,
        &[],
        mode,
        root_override,
        options,
        seams,
        progress,
    )
}

/// Build a base, also applying the game's framework declarations.
///
/// A framework whose marker is in the source install has its declared required files copied even
/// when the game's excluded paths would leave them out. Files it declares but the install does not
/// have are recorded in the manifest as an incomplete framework.
#[allow(clippy::too_many_arguments)]
pub fn build_base_with_frameworks(
    paths: &AppPaths,
    install: &IdentifiedInstall,
    definition: &GameDefinition,
    frameworks: &[&FrameworkDefinition],
    mode: BaseMode,
    root_override: Option<&Path>,
    options: BuildOptions,
    seams: &crate::test_seams::TestSeams,
    progress: &(dyn Fn(BuildProgress) + Send + Sync),
) -> Result<BuildOutcome, BaseError> {
    // 1. Runtime must be Identified
    let runtime = match &install.runtime {
        crate::game_registry::RuntimeResolution::Identified { runtime, .. } => runtime,
        crate::game_registry::RuntimeResolution::Unidentified { .. } => {
            return Err(BaseError::RuntimeUnknown);
        }
    };

    // A Copied base stores the same files differently from a Linked one (no
    // hardlinks into the store install), and an unfiltered base holds different
    // files from the default one, so each gets its own id rather than being
    // answered by whichever was built first.
    let mut base_id = make_base_id(runtime);
    if mode == BaseMode::Copied {
        base_id.push_str("_copied");
    }
    if options.include_excluded {
        base_id.push_str("_unfiltered");
    }
    let manifest_path = paths.base_manifest_path(&base_id);

    // 2. Existing record -> return without touching anything. A record that is there but is not
    // a base record is damaged: the build replaces it (and the folder it left) once it is known
    // that no instance is pinned to it. A record that exists but cannot be read is an error,
    // because a failed read of a healthy record must not be taken as permission to replace its
    // base. Whether the record is damaged is judged again under the base lock below; the state
    // here only decides whether hashing is worth it. Nothing here asks `exists()`, which on Linux
    // says "there" for a file nobody may read: only a `NotFound` on the real read is absence.
    match read_base_record(&base_id, &manifest_path)? {
        BaseRecord::Healthy(manifest) => return Ok(BuildOutcome::Existing(*manifest)),
        BaseRecord::Damaged => {
            let pinning = instances_pinning_base(paths, &base_id)?;
            if !pinning.is_empty() {
                return Err(BaseError::Other(format!(
                    "a damaged base with this id exists ({base_id}) and instance(s) {} are pinned to it, so it was not replaced. Remove those instances first.",
                    pinning.join(", ")
                )));
            }
        }
        BaseRecord::Missing => {}
    }

    let source_dir = &install.discovered.location;
    if !source_dir.exists() {
        return Err(BaseError::Other(format!(
            "source install location does not exist: {}",
            source_dir.display()
        )));
    }

    // Walk source without following symlinks/junctions
    let mut entries = Vec::new();
    let mut skipped = Vec::new();
    walk_source_dir(
        source_dir,
        Path::new(""),
        definition,
        options,
        &mut entries,
        &mut skipped,
    )?;

    // A framework the install has is part of the base even when its files sit on an excluded
    // path. A declared file the install does not have is recorded so the pin is not a silent
    // half-install.
    let mut incomplete_frameworks = Vec::new();
    let mut present: HashSet<String> = entries
        .iter()
        .map(|entry| fold_path_case(&entry.rel_path))
        .collect();
    for framework in frameworks {
        let Some(detection) = &framework.detection else {
            continue;
        };
        if detection.required.is_empty() {
            continue;
        }
        let marker = fold_path_case(detection.marker.as_str());
        let marker_present = entries.iter().any(|entry| {
            let path = fold_path_case(&entry.rel_path);
            if marker.contains('*') {
                agora_game_api::glob_match(&marker, &path)
            } else {
                path == marker
            }
        });
        if !marker_present {
            continue;
        }
        let mut missing = Vec::new();
        for required in &detection.required {
            let required = required.as_str();
            if present.contains(&fold_path_case(required)) {
                continue;
            }
            let candidate = source_dir.join(required);
            match candidate.metadata() {
                Ok(metadata) if metadata.is_file() => {
                    present.insert(fold_path_case(required));
                    entries.push(SourceEntry {
                        rel_path: required.to_string(),
                        abs_src_path: candidate,
                        size: metadata.len(),
                        is_archive: definition.is_linked_archive(required),
                    });
                }
                _ => missing.push(required.to_string()),
            }
        }
        if !missing.is_empty() {
            incomplete_frameworks.push(IncompleteFramework {
                framework: framework.id.as_str().to_string(),
                name: framework.name.clone(),
                missing,
            });
        }
    }

    let total_source_bytes: u64 = entries.iter().map(|e| e.size).sum();

    // Determine root
    let base_root = root_override.map(|p| p.to_path_buf()).unwrap_or_else(|| {
        bases_root_for(
            paths,
            source_dir,
            install.discovered.volume.as_ref().map(|v| v.id.as_str()),
        )
    });

    #[cfg(debug_assertions)]
    seams.base_build_pause_point(
        crate::test_seams::BaseBuildPauseKind::BeforePinRecheck,
        &base_root,
    );

    // 3. Linked mode checks
    if mode == BaseMode::Linked {
        let detector = VolumeDetector::new();
        let base_volume = detector.get_volume_info(&base_root);
        let install_volume = install.discovered.volume.as_ref();
        if let Err(reason) = evaluate_link_support(base_volume.as_ref(), install_volume) {
            return Err(BaseError::LinkUnavailable {
                reason,
                copied_bytes: total_source_bytes,
            });
        }
    }

    // Refuse definition where any file it would link also matches declared_writes
    if mode == BaseMode::Linked {
        for entry in &entries {
            if entry.is_archive && definition.is_declared_write(&entry.rel_path) {
                return Err(BaseError::DeclaredWriteLinked {
                    path: entry.rel_path.clone(),
                });
            }
        }
    }

    std::fs::create_dir_all(&base_root)?;
    register_base_root(paths, &base_root)?;

    // 4. Staging folder <root>/<base_id>.partial-<unique>
    let unique = format!("{}-{}", std::process::id(), uuid::Uuid::new_v4().simple());
    let staging_dir = base_root.join(format!("{base_id}.partial-{unique}"));
    std::fs::create_dir_all(&staging_dir)?;

    let mut guard = StagingGuard {
        path: &staging_dir,
        active: true,
    };

    let mut linked_bytes = 0u64;
    let mut copied_bytes = 0u64;
    let mut staged_entries = Vec::with_capacity(entries.len());

    for entry in entries {
        let dest = staging_dir.join(&entry.rel_path);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let link_it = mode == BaseMode::Linked && entry.is_archive;
        if link_it {
            std::fs::hard_link(&entry.abs_src_path, &dest)?;
            linked_bytes += entry.size;
        } else {
            std::fs::copy(&entry.abs_src_path, &dest)?;
            copied_bytes += entry.size;
        }
        staged_entries.push((entry, link_it));
    }

    // 5. Hash every base file with scoped threads
    let pool_size = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(8);

    let total_files = staged_entries.len() as u64;
    let current_idx = AtomicUsize::new(0);
    let files_done_counter = AtomicU64::new(0);
    let bytes_hashed_counter = AtomicU64::new(0);
    let hashed_files = Mutex::new(Vec::with_capacity(staged_entries.len()));

    let hash_ctx = HashProgressContext {
        bytes_hashed_counter: &bytes_hashed_counter,
        files_done_counter: &files_done_counter,
        total_files,
        total_bytes: total_source_bytes,
        progress,
    };

    std::thread::scope(|s| {
        for _ in 0..pool_size {
            s.spawn(|| loop {
                let idx = current_idx.fetch_add(1, Ordering::Relaxed);
                if idx >= staged_entries.len() {
                    break;
                }
                let (entry, linked) = &staged_entries[idx];
                let dest = staging_dir.join(&entry.rel_path);
                let res = hash_and_record(&dest, &entry.rel_path, *linked, &hash_ctx);
                hashed_files.lock().unwrap().push(res);
            });
        }
    });

    let results = hashed_files.into_inner().unwrap();
    let mut base_files = Vec::with_capacity(results.len());
    for r in results {
        base_files.push(r?);
    }
    base_files.sort_by(|a, b| a.path.cmp(&b.path));
    skipped.sort();

    // From here the base folder is put in place and its record written. Another build of the same
    // id must not see the folder without its record and take it for an interrupted build: that
    // would delete the base this one is finishing. The lock covers the rename and the record, and
    // the re-check under it answers a build that finished while this one was hashing.
    let locks_root = paths.locks_root();
    std::fs::create_dir_all(&locks_root)?;
    let locks = crate::lock_manager::LockManager::new(locks_root);
    let _base_lock = locks
        .acquire(
            crate::lock_manager::LockResource::Base(base_id.clone()),
            "build-base",
        )
        .map_err(|e| BaseError::Other(e.to_string()))?;
    // Re-check the record under the lock, by reading it: a record that exists but cannot be
    // read (permissions, a foreign object on the name) is an error that keeps the folder —
    // `exists()` would call it present but the old suppressed read treated it as damaged, a
    // flag computed before the lock. Healthy returns Existing; still-damaged is judged from
    // the record as it is right now.
    let mut damaged_record = false;
    let mut record_missing = false;
    match read_base_record(&base_id, &manifest_path)? {
        BaseRecord::Healthy(manifest) => {
            // The staging folder goes with the guards when this returns.
            return Ok(BuildOutcome::Existing(*manifest));
        }
        BaseRecord::Damaged => damaged_record = true,
        BaseRecord::Missing => record_missing = true,
    }

    // Only the path that replaces a base folder needs the locations lock: take it after Base, and
    // acquire nothing while holding it. The early check above can be stale by now — an instance
    // may have moved into Recently deleted while the source was hashed. Under both locks the
    // answer cannot change, so a base that is still pinned is kept.
    let _locations = locks
        .try_instance_locations("build-base")
        .map_err(|error| {
            BaseError::Other(format!(
                "base {base_id} was kept because instance locations are busy: {error}. Retry once the instance move or scan finishes."
            ))
        })?;
    let pinning = instances_pinning_base(paths, &base_id)?;
    if !pinning.is_empty() {
        return Err(BaseError::Other(format!(
            "instance(s) {} still use base {base_id} (including Recently deleted), so it was not replaced. Remove or purge those instances first.",
            pinning.join(", ")
        )));
    }

    let final_base_dir = base_root.join(&base_id);

    // The marker travels with the staging folder, so the folder this build puts in place can be
    // told apart from one Agora did not build.
    write_base_marker(&staging_dir, &base_id)?;

    // A folder under our own name with no readable manifest is what an
    // interrupted build leaves (renamed into place, manifest never written).
    // Move it aside, or this base id could never be built again. It is deleted
    // only when its own marker says this build made it; anything else survives
    // under the stale name for the person to inspect.
    // A damaged record is no readable manifest either (the new manifest replaces it below).
    // The folder is judged by `symlink_metadata`: only a clean `NotFound` says gone.
    if base_folder_present(&final_base_dir) && (damaged_record || record_missing) {
        let stale = base_root.join(format!("{base_id}.stale-{unique}"));
        std::fs::rename(&final_base_dir, &stale)?;
        if base_marker_matches(&stale, &base_id) {
            std::fs::remove_dir_all(&stale)?;
        }
    }

    // 6. Rename staging folder to <root>/<base_id>/
    if let Err(e) = std::fs::rename(&staging_dir, &final_base_dir) {
        // If rename failed because another build won the race, its record is readable under
        // the base lock this build holds: adopt it, drop the staging folder, return Existing.
        // A record that cannot be read, or reads damaged, is not adopted — that is not
        // proof the winner finished, and losing the race must not rebuild over anything.
        let adoption = if base_folder_present(&final_base_dir) {
            read_base_record(&base_id, &manifest_path)
                .ok()
                .and_then(|record| match record {
                    BaseRecord::Healthy(manifest) => Some(*manifest),
                    BaseRecord::Damaged | BaseRecord::Missing => None,
                })
        } else {
            None
        };
        if let Some(manifest) = adoption {
            let _ = std::fs::remove_dir_all(&staging_dir);
            guard.active = false;
            return Ok(BuildOutcome::Existing(manifest));
        }
        return Err(BaseError::Io(e));
    }

    guard.active = false;
    #[cfg(debug_assertions)]
    seams.base_build_pause_point(
        crate::test_seams::BaseBuildPauseKind::AfterRename,
        &base_root,
    );

    let created_unix_ms = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(e) => -(e.duration().as_millis() as i64),
    };

    let manifest = BaseManifest {
        manifest_version: 1,
        base_id: base_id.clone(),
        runtime: runtime.clone(),
        mode,
        source_install: install.install_id.clone(),
        source_product: Some(install.discovered.product.clone()),
        source_location: source_dir.clone(),
        location: final_base_dir,
        created_unix_ms,
        files: base_files,
        skipped,
        incomplete_frameworks,
    };

    // Write manifest (temp file + rename)
    let manifests_dir = paths.base_manifests_dir();
    std::fs::create_dir_all(&manifests_dir)?;
    let tmp_manifest = manifests_dir.join(format!("{base_id}.json.tmp-{unique}"));
    let manifest_bytes = serde_json::to_string_pretty(&manifest)?;
    std::fs::write(&tmp_manifest, manifest_bytes)?;
    std::fs::rename(&tmp_manifest, &manifest_path)?;

    Ok(BuildOutcome::Built {
        manifest,
        linked_bytes,
        copied_bytes,
    })
}

struct HashProgressContext<'a> {
    bytes_hashed_counter: &'a AtomicU64,
    files_done_counter: &'a AtomicU64,
    total_files: u64,
    total_bytes: u64,
    progress: &'a (dyn Fn(BuildProgress) + Send + Sync),
}

fn hash_and_record(
    dest_path: &Path,
    rel_path: &str,
    linked: bool,
    ctx: &HashProgressContext<'_>,
) -> Result<BaseFile, std::io::Error> {
    use std::io::Read;

    let mut file = std::fs::File::open(dest_path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 128 * 1024];
    let mut file_size: u64 = 0;

    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        file_size += n as u64;
        let hashed = ctx
            .bytes_hashed_counter
            .fetch_add(n as u64, Ordering::Relaxed)
            + (n as u64);
        let done = ctx.files_done_counter.load(Ordering::Relaxed);
        (ctx.progress)(BuildProgress {
            files_done: done,
            files_total: ctx.total_files,
            bytes_hashed: hashed,
            bytes_total: ctx.total_bytes,
        });
    }

    let sha256 = format!("{:x}", hasher.finalize());
    let metadata = file.metadata()?;
    let identity = get_file_identity_from_file(&file);
    let modified_unix_ms = get_modified_unix_ms(&metadata);

    let done = ctx.files_done_counter.fetch_add(1, Ordering::Relaxed) + 1;
    let hashed = ctx.bytes_hashed_counter.load(Ordering::Relaxed);
    (ctx.progress)(BuildProgress {
        files_done: done,
        files_total: ctx.total_files,
        bytes_hashed: hashed,
        bytes_total: ctx.total_bytes,
    });

    Ok(BaseFile {
        path: rel_path.to_string(),
        size: file_size,
        sha256,
        linked,
        identity,
        modified_unix_ms,
    })
}

// ---------------------------------------------------------------------------
// Verify base
// ---------------------------------------------------------------------------

fn walk_base_dir(base_root: &Path, rel_dir: &Path, out: &mut Vec<String>) {
    let cur_dir = if rel_dir.as_os_str().is_empty() {
        base_root.to_path_buf()
    } else {
        base_root.join(rel_dir)
    };
    let Ok(read_dir) = std::fs::read_dir(&cur_dir) else {
        return;
    };
    for entry in read_dir.flatten() {
        let file_name = entry.file_name();
        let rel_path = rel_dir.join(&file_name);
        let abs_path = entry.path();
        let rel_str = rel_path.to_string_lossy().replace('\\', "/");

        let Ok(meta) = std::fs::symlink_metadata(&abs_path) else {
            continue;
        };

        if is_reparse_point_or_symlink(&meta) {
            out.push(rel_str);
            continue;
        }

        if meta.is_dir() {
            walk_base_dir(base_root, &rel_path, out);
        } else if meta.is_file() {
            out.push(rel_str);
        }
    }
}

pub fn verify_base(
    manifest: &BaseManifest,
    depth: VerifyDepth,
    is_declared_write: &dyn Fn(&str) -> bool,
    is_excluded: &dyn Fn(&str) -> bool,
) -> BaseVerification {
    let mut problems = Vec::new();
    let mut game_writes = Vec::new();
    let mut checked = 0;
    let mut hashed = 0;

    if !manifest.location.exists() || !manifest.location.is_dir() {
        for file in &manifest.files {
            if is_declared_write(&file.path) && !file.linked {
                game_writes.push(file.path.clone());
            } else {
                problems.push(BaseProblem {
                    path: file.path.clone(),
                    linked_to_store: file.linked,
                    kind: ProblemKind::Missing,
                });
            }
        }
        game_writes.sort();
        game_writes.dedup();
        problems.sort_by(|a, b| a.path.cmp(&b.path));
        return BaseVerification {
            checked: manifest.files.len(),
            hashed: 0,
            game_writes,
            problems,
        };
    }

    for file in &manifest.files {
        checked += 1;
        let file_path = manifest.location.join(&file.path);
        let meta = match std::fs::symlink_metadata(&file_path) {
            Ok(m) => m,
            Err(_) => {
                if is_declared_write(&file.path) && !file.linked {
                    game_writes.push(file.path.clone());
                } else {
                    problems.push(BaseProblem {
                        path: file.path.clone(),
                        linked_to_store: file.linked,
                        kind: ProblemKind::Missing,
                    });
                }
                continue;
            }
        };

        if !meta.is_file() {
            if is_declared_write(&file.path) && !file.linked {
                game_writes.push(file.path.clone());
            } else {
                problems.push(BaseProblem {
                    path: file.path.clone(),
                    linked_to_store: file.linked,
                    kind: ProblemKind::Missing,
                });
            }
            continue;
        }

        let actual_size = meta.len();
        if actual_size != file.size {
            if is_declared_write(&file.path) && !file.linked {
                game_writes.push(file.path.clone());
            } else {
                problems.push(BaseProblem {
                    path: file.path.clone(),
                    linked_to_store: file.linked,
                    kind: ProblemKind::SizeChanged {
                        expected: file.size,
                        actual: actual_size,
                    },
                });
            }
            continue;
        }

        match depth {
            VerifyDepth::Quick => {
                let cur_identity = get_file_identity(&file_path);
                let cur_modified = get_modified_unix_ms(&meta);
                let identity_differs = file.identity.is_some() && cur_identity != file.identity;
                let modified_differs = cur_modified != file.modified_unix_ms;

                if identity_differs || modified_differs {
                    hashed += 1;
                    if let Ok(actual_hash) = hash_file(&file_path) {
                        if actual_hash != file.sha256 {
                            if is_declared_write(&file.path) && !file.linked {
                                game_writes.push(file.path.clone());
                            } else {
                                problems.push(BaseProblem {
                                    path: file.path.clone(),
                                    linked_to_store: file.linked,
                                    kind: ProblemKind::ContentChanged,
                                });
                            }
                        }
                    } else if is_declared_write(&file.path) && !file.linked {
                        game_writes.push(file.path.clone());
                    } else {
                        problems.push(BaseProblem {
                            path: file.path.clone(),
                            linked_to_store: file.linked,
                            kind: ProblemKind::ContentChanged,
                        });
                    }
                }
            }
            VerifyDepth::Full => {
                hashed += 1;
                if let Ok(actual_hash) = hash_file(&file_path) {
                    if actual_hash != file.sha256 {
                        if is_declared_write(&file.path) && !file.linked {
                            game_writes.push(file.path.clone());
                        } else {
                            problems.push(BaseProblem {
                                path: file.path.clone(),
                                linked_to_store: file.linked,
                                kind: ProblemKind::ContentChanged,
                            });
                        }
                    }
                } else if is_declared_write(&file.path) && !file.linked {
                    game_writes.push(file.path.clone());
                } else {
                    problems.push(BaseProblem {
                        path: file.path.clone(),
                        linked_to_store: file.linked,
                        kind: ProblemKind::ContentChanged,
                    });
                }
            }
        }
    }

    let manifest_file_set: HashSet<&str> = manifest.files.iter().map(|f| f.path.as_str()).collect();

    let mut actual_files = Vec::new();
    walk_base_dir(&manifest.location, Path::new(""), &mut actual_files);
    for actual_rel in actual_files {
        if !manifest_file_set.contains(actual_rel.as_str()) {
            if actual_rel == BASE_MARKER_FILE {
                // Agora's own "I built this folder" marker, not a game file.
            } else if is_declared_write(&actual_rel) {
                game_writes.push(actual_rel);
            } else if is_excluded(&actual_rel) {
                // Stray excluded files appearing in the base later are ignored, not Unexpected.
            } else {
                problems.push(BaseProblem {
                    path: actual_rel,
                    linked_to_store: false,
                    kind: ProblemKind::Unexpected,
                });
            }
        }
    }

    game_writes.sort();
    game_writes.dedup();
    problems.sort_by(|a, b| a.path.cmp(&b.path));

    BaseVerification {
        checked,
        hashed,
        game_writes,
        problems,
    }
}

// ---------------------------------------------------------------------------
// Listing and removing
// ---------------------------------------------------------------------------

/// Every base record in the bases folder. A record that cannot be read is returned as
/// `BaseEntry::Damaged` with its path and the reason, never dropped. A folder that does not exist
/// yet has no bases. Any other failure to read the folder is an error: a list that skipped
/// records silently would hide bases the user needs to repair or remove.
pub fn list_bases(paths: &AppPaths) -> Result<Vec<BaseEntry>, BaseError> {
    let manifests_dir = paths.base_manifests_dir();
    let entries = match std::fs::read_dir(&manifests_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(BaseError::Io(error)),
    };
    let mut found = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension() != Some(std::ffi::OsStr::new("json")) {
            continue;
        }
        let base_id = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        match read_manifest_file(&path) {
            Ok(manifest) => {
                let present = manifest.location.exists() && manifest.location.is_dir();
                found.push(BaseEntry::Readable(Box::new(BaseListing {
                    manifest,
                    present,
                })));
            }
            Err(error) => found.push(BaseEntry::Damaged(DamagedBase {
                base_id,
                manifest_path: path,
                error,
            })),
        }
    }
    found.sort_by(|a, b| a.base_id().cmp(b.base_id()));
    Ok(found)
}

/// Read a manifest file. The error names the file and whether it could not be read or is not a
/// base record.
fn read_manifest_file(path: &Path) -> Result<BaseManifest, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("{} cannot be read: {error}", path.display()))?;
    serde_json::from_str(&text)
        .map_err(|error| format!("{} is not a valid base record: {error}", path.display()))
}

/// Whether `base_id` can name a manifest file and a folder directly inside a bases root. Core's
/// ids are lowercase letters, digits, `.`, `-` and `_`, and start with the game id. `.` and `..`
/// pass the character test, so the first and last characters are checked too: a leading dot is a
/// relative path, and Windows drops a trailing dot from a folder name. Windows device names
/// (`nul`, `con.txt`) are not folders at all: the rule for them is the game contract's
/// [`agora_game_api::is_windows_device_name`], so a base folder and an entry Agora writes are held
/// to one rule.
pub fn is_plain_base_id(base_id: &str) -> bool {
    let chars_ok = base_id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-' | '_'));
    let starts_ok = base_id
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let ends_ok = base_id.chars().last().is_some_and(|c| c != '.');
    let stem = base_id.split('.').next().unwrap_or_default();
    base_id.len() <= 200
        && chars_ok
        && starts_ok
        && ends_ok
        && !agora_game_api::is_windows_device_name(stem)
}

/// The bases roots outside the data folder that builds here created, from `bases/roots.json`. No
/// file means none. A file that cannot be read is an error: removal must not treat it as "no
/// roots", and must not guess one.
/// The folders Agora has put bases in: the data folder's own, plus every root a build registered.
/// An unreadable list of roots is an error: a caller that cannot see every root must not decide
/// that a folder elsewhere does not exist.
pub fn known_base_roots(paths: &AppPaths) -> Result<Vec<PathBuf>, BaseError> {
    let mut roots = vec![paths.bases_dir()];
    roots.extend(read_base_roots(paths)?);
    roots.sort();
    roots.dedup();
    Ok(roots)
}

fn read_base_roots(paths: &AppPaths) -> Result<Vec<PathBuf>, BaseError> {
    let file = paths.base_roots_file();
    let text = match std::fs::read_to_string(&file) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(BaseError::Other(format!(
                "{} cannot be read, so Agora cannot tell which folders its bases live in: {error}",
                file.display()
            )))
        }
    };
    serde_json::from_str::<Vec<PathBuf>>(&text).map_err(|error| {
        BaseError::Other(format!(
            "{} is not a list of folders, so Agora cannot tell which folders its bases live in: {error}",
            file.display()
        ))
    })
}

/// Serialises the read-modify-write of `roots.json` between builds in this process.
static BASE_ROOTS_LOCK: Mutex<()> = Mutex::new(());

/// Remember a bases root a build is about to use, so removal can later tell a folder Agora made
/// from one a hand-edited record names. The data folder's own `bases` folder needs no entry.
fn register_base_root(paths: &AppPaths, root: &Path) -> Result<(), BaseError> {
    if proven_one_folder(root, &paths.bases_dir()) {
        return Ok(());
    }
    let _guard = BASE_ROOTS_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut roots = read_base_roots(paths)?;
    if roots.iter().any(|known| proven_one_folder(known, root)) {
        return Ok(());
    }
    roots.push(root.to_path_buf());
    let file = paths.base_roots_file();
    std::fs::create_dir_all(paths.bases_dir())?;
    let tmp = file.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&roots)?)?;
    std::fs::rename(&tmp, &file)?;
    Ok(())
}

/// Remove staging folders a build or deploy left behind, when the process that made them is no
/// longer running. A folder whose owner cannot be parsed, or whose process is alive, is left alone:
/// a live operation must never lose its staging. Called at startup.
pub fn sweep_stale_staging(paths: &AppPaths) -> Result<usize, BaseError> {
    let live = live_process_ids();
    let mut removed = 0usize;
    for root in known_base_roots(paths)? {
        removed += sweep_staging_in(&root, ".partial-", false, &live)?;
        // A `.stale-` folder is one a build moved aside. It is removed only when its own marker
        // says Agora built it: an unmarked one was kept on purpose, for the person to inspect.
        removed += sweep_staging_in(&root, ".stale-", true, &live)?;
        let deployments = root.join("deployments");
        let read = match std::fs::read_dir(&deployments) {
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(BaseError::Other(format!(
                    "cannot read {} to sweep stale staging: {error}",
                    deployments.display()
                )))
            }
        };
        for entry in read.flatten() {
            let instance_dir = entry.path();
            if !instance_dir.is_dir() {
                continue;
            }
            removed += sweep_staging_in(&instance_dir, "game.partial-", false, &live)?;
        }
    }
    Ok(removed)
}

fn sweep_staging_in(
    folder: &Path,
    marker: &str,
    require_base_marker: bool,
    live: &std::collections::HashSet<u32>,
) -> Result<usize, BaseError> {
    let read = match std::fs::read_dir(folder) {
        Ok(read) => read,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(BaseError::Other(format!(
                "cannot read {} to sweep stale staging: {error}",
                folder.display()
            )))
        }
    };
    let mut removed = 0usize;
    for entry in read.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        // Names are `<prefix><marker><pid>-<uuid>`, as the builders write them.
        let Some((prefix, rest)) = name.rsplit_once(marker) else {
            continue;
        };
        let Some(owner) = rest
            .split('-')
            .next()
            .and_then(|pid| pid.parse::<u32>().ok())
        else {
            continue;
        };
        if require_base_marker && !base_marker_matches(&path, prefix) {
            continue;
        }
        if live.contains(&owner) {
            continue;
        }
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            continue;
        }
        std::fs::remove_dir_all(&path)?;
        removed += 1;
    }
    Ok(removed)
}

fn live_process_ids() -> std::collections::HashSet<u32> {
    let mut system = sysinfo::System::new();
    system.refresh_processes();
    system.processes().keys().map(|pid| pid.as_u32()).collect()
}

/// Whether two paths are provably one folder. A pair that cannot be resolved is **not** the same:
/// a caller that would otherwise copy a file onto itself, or delete a folder it cannot identify,
/// refuses instead.
fn proven_one_folder(one: &Path, other: &Path) -> bool {
    matches!(
        agora_game_api::compare_folder(one, other),
        agora_game_api::PathRelation::Same
    )
}

/// Whether two paths are provably two different folders. "Could not tell" is not one of them, so a
/// caller about to copy one onto the other refuses.
fn proven_two_folders(one: &Path, other: &Path) -> bool {
    matches!(
        agora_game_api::compare_folder(one, other),
        agora_game_api::PathRelation::Different
    )
}

/// The folder a record names, or a refusal: it has to be a base folder Agora made, and it may not
/// be the install it was built from.
///
/// The record matches by **spelling**, as it always has: a record whose `location` is some other
/// path that happens to resolve to the base folder — an alias, a link, a junction — is not a record
/// Agora wrote, and deleting through it is exactly what this refuses. The overlap with the source
/// install is then checked on **resolved** values, because a source install that is a link to the
/// base folder overlaps it even though no spelling of it looks like the base's.
pub(crate) fn trusted_base_folder(
    paths: &AppPaths,
    manifest: &BaseManifest,
    base_id: &str,
) -> Result<PathBuf, BaseError> {
    let refuse = |why: &str| {
        BaseError::Other(format!(
            "refusing to remove {}: the record for base {base_id} {why}",
            manifest.location.display()
        ))
    };
    if manifest.base_id != base_id {
        return Err(refuse("names a different base id"));
    }
    let mut roots = vec![paths.bases_dir()];
    roots.extend(read_base_roots(paths)?);
    let folder = roots
        .iter()
        .map(|root| root.join(base_id))
        .find(|candidate| agora_game_api::same_spelling(candidate, &manifest.location))
        .ok_or_else(|| refuse("does not point at a base folder Agora made"))?;
    let folder_real = resolved_for_removal(&folder).map_err(|why| {
        refuse(&format!(
            "names a base folder that could not be resolved ({why}), so it was not removed"
        ))
    })?;
    let source_real = resolved_for_removal(&manifest.source_location).map_err(|why| {
        refuse(&format!(
            "names a source install that could not be resolved ({why}), so whether the base is \
             inside it could not be told"
        ))
    })?;
    let overlaps_source =
        folder_real.starts_with(&source_real) || source_real.starts_with(&folder_real);
    if overlaps_source {
        return Err(refuse("points at the install it was built from"));
    }
    Ok(folder)
}

/// A path resolved once, for a decision about deleting it: the part that exists is canonicalized and
/// the part that does not is appended as written. An error means it cannot be done, which a caller
/// about to delete must treat as "do not", and the reason is reported rather than dropped.
fn resolved_for_removal(path: &Path) -> Result<PathBuf, String> {
    agora_game_api::resolve_destination(path).map_err(|error| error.to_string())
}

/// The game instances pinned to `base_id`. Fails closed: if the instance table
/// cannot be read, or a row's base reference cannot be parsed, the answer is an
/// error rather than "none", because the caller may be about to delete the base.
pub fn instances_pinning_base(paths: &AppPaths, base_id: &str) -> Result<Vec<String>, BaseError> {
    instances_pinning_base_with(paths, base_id, |path| std::fs::metadata(path), || {})
}

/// The same read with its state-database metadata lookup and its between-reads hook injected, so
/// an unreadable database and a move between the two reads can be exercised without depending on
/// the OS or the thread scheduler. Production passes the real filesystem and a no-op.
fn instances_pinning_base_with(
    paths: &AppPaths,
    base_id: &str,
    metadata: impl Fn(&Path) -> std::io::Result<std::fs::Metadata>,
    between_reads: impl Fn(),
) -> Result<Vec<String>, BaseError> {
    let unreadable = |e: &dyn std::fmt::Display| {
        BaseError::Other(format!(
            "cannot tell which instances use base {base_id}, so it is kept: {e}"
        ))
    };
    // No live database means no live pins ever were created here, but that is not
    // "nothing pins this base": a Recently-deleted entry is an independent source
    // and is scanned below either way.
    let db_path = paths.local_state_db();
    let mut instances = Vec::new();
    match metadata(&db_path) {
        Ok(_) => {
            let conn = rusqlite::Connection::open(&db_path).map_err(|e| unreadable(&e))?;
            let mut stmt = conn
                .prepare("SELECT instance_id, base_json FROM game_instances")
                .map_err(|e| unreadable(&e))?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|e| unreadable(&e))?;
            for row in rows {
                let (instance_id, base_json) = row.map_err(|e| unreadable(&e))?;
                let base_ref: agora_game_api::BaseReference = serde_json::from_str(&base_json)
                    .map_err(|e| unreadable(&format!("instance {instance_id}: {e}")))?;
                if matches!(&base_ref, agora_game_api::BaseReference::Pinned { id, .. } if id == base_id)
                {
                    instances.push(instance_id);
                }
            }
        }
        // A failed lookup other than absence is not an empty database. Without this,
        // an error would be read as "no instances", the base would be deleted, and a
        // later restore would lose its files.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(unreadable(&format!(
                "cannot inspect {}: {error}",
                db_path.display()
            )));
        }
    }
    // The caller holds InstanceLocations across both reads through the deletion: an instance
    // cannot move between live and Recently deleted while these two answers disagree.
    between_reads();
    // An instance in Recently deleted is still restorable, so its base must stay.
    for instance_id in trash_instances_pinning_base(paths, base_id)? {
        if !instances.contains(&instance_id) {
            instances.push(instance_id);
        }
    }
    instances.sort();
    Ok(instances)
}

/// The instances in Recently deleted whose entry is still restorable and whose base record names
/// `base_id`. An entry that cannot be read is an error: a caller about to delete a base must not
/// take "I could not tell" as "nothing pins it".
fn trash_instances_pinning_base(paths: &AppPaths, base_id: &str) -> Result<Vec<String>, BaseError> {
    let root = paths.recently_deleted_games_root();
    let read = match std::fs::read_dir(&root) {
        Ok(read) => read,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(BaseError::Other(format!(
                "cannot read {}, so Agora cannot tell which deleted instances use base {base_id}: {error}",
                root.display()
            )))
        }
    };
    let now = chrono::Utc::now();
    let mut instances = Vec::new();
    for entry in read {
        let entry = entry.map_err(|error| {
            BaseError::Other(format!(
                "cannot read {}, so Agora cannot tell which deleted instances use base {base_id}: {error}",
                root.display()
            ))
        })?;
        let entry_path = entry.path();
        let record = entry_path.join("entry.json");
        let text = match std::fs::read_to_string(&record) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(BaseError::Other(format!(
                    "cannot read the deleted entry {}, so Agora cannot tell whether base {base_id} is in use: {error}",
                    record.display()
                )))
            }
        };
        let stored: serde_json::Value = serde_json::from_str(&text).map_err(|error| {
            BaseError::Other(format!(
                "cannot read the deleted entry {}, so Agora cannot tell whether base {base_id} is in use: {error}",
                record.display()
            ))
        })?;
        let instance_id = stored
            .get("row")
            .and_then(|row| row.get("instance_id"))
            .and_then(|id| id.as_str())
            .ok_or_else(|| {
                BaseError::Other(format!(
                    "the deleted entry {} names no instance, so Agora cannot tell whether base {base_id} is in use",
                    record.display()
                ))
            })?;
        let base_json = stored
            .get("row")
            .and_then(|row| row.get("base_json"))
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                BaseError::Other(format!(
                    "the deleted entry {} names no base, so Agora cannot tell whether base {base_id} is in use",
                    record.display()
                ))
            })?;
        let base_ref: agora_game_api::BaseReference =
            serde_json::from_str(base_json).map_err(|error| {
                BaseError::Other(format!(
                    "the deleted entry {} has an unreadable base record, so Agora cannot tell whether base {base_id} is in use: {error}",
                    record.display()
                ))
            })?;
        if !matches!(&base_ref, agora_game_api::BaseReference::Pinned { id, .. } if id == base_id) {
            continue;
        }
        // An entry whose expiry cannot be read is counted, so a damaged entry keeps the base.
        let expired = stored
            .get("expires_at")
            .and_then(|value| value.as_str())
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
            .is_some_and(|at| at <= now);
        if !expired {
            instances.push(instance_id.to_string());
        }
    }
    Ok(instances)
}

/// The damaged record the list shows under `base_id`, found by its real file name. The list names
/// a record by its file name without `.json`, which for a damaged file can be anything (`..`,
/// `x.`), so those ids can only be matched against the folder, never joined into a path.
fn damaged_record_listed_as(paths: &AppPaths, base_id: &str) -> Result<Option<PathBuf>, BaseError> {
    let entries = match std::fs::read_dir(paths.base_manifests_dir()) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(BaseError::Io(error)),
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension() != Some(std::ffi::OsStr::new("json")) {
            continue;
        }
        let listed_as = path.file_stem().map(|stem| stem.to_string_lossy());
        if listed_as.as_deref() == Some(base_id) && read_manifest_file(&path).is_err() {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

pub fn remove_base(paths: &AppPaths, base_id: &str) -> Result<(), BaseError> {
    remove_base_with(paths, base_id, || {}, |path| std::fs::metadata(path))
}

/// Removal with the pinning read's hooks injected. See [`instances_pinning_base_with`].
/// Production calls this through [`remove_base`]; the closure runs inside the removal's locks.
pub(crate) fn remove_base_with(
    paths: &AppPaths,
    base_id: &str,
    between_reads: impl Fn(),
    metadata: impl Fn(&Path) -> std::io::Result<std::fs::Metadata>,
) -> Result<(), BaseError> {
    let locks = crate::lock_manager::LockManager::new(paths.locks_root());
    // A plain id can also be named by a build or a framework repair, so take its lock first. A
    // damaged record is listed under a file name that need not be a valid lock component; it only
    // removes that one record, but still takes the locations lock below.
    let plain_id = is_plain_base_id(base_id);
    let _base_lock = plain_id
        .then(|| {
            locks.acquire(
                crate::lock_manager::LockResource::Base(base_id.to_string()),
                "remove-base",
            )
        })
        .transpose()
        .map_err(|error| BaseError::Other(error.to_string()))?;
    // Hold InstanceLocations from the first pinning read through the deletion. A move between live
    // and Recently deleted can otherwise hide an instance from both reads, letting us delete a
    // base it still pins. Nothing else is locked while this guard is alive; on a busy lock, keep
    // the base and tell the caller to retry.
    let _locations = locks.try_instance_locations("remove-base").map_err(|error| {
        BaseError::Other(format!(
            "base {base_id} was kept because instance locations are busy: {error}. Retry once the instance move or scan finishes."
        ))
    })?;
    if !plain_id {
        // Not a name that can reach a folder or a manifest path, but the list can still show a
        // damaged record under it. That record file (and nothing else) is removed.
        let Some(record) = damaged_record_listed_as(paths, base_id)? else {
            return Err(BaseError::Other(format!(
                "'{base_id}' is not a base id, so nothing was removed."
            )));
        };
        let pinning = instances_pinning_base_with(paths, base_id, &metadata, &between_reads)?;
        if !pinning.is_empty() {
            return Err(BaseError::InUse { instances: pinning });
        }
        let record_metadata = std::fs::symlink_metadata(&record)?;
        if !record_metadata.is_file()
            || record.parent() != Some(paths.base_manifests_dir().as_path())
        {
            return Err(BaseError::Other(format!(
                "refusing to remove {}: it is not a plain record file",
                record.display()
            )));
        }
        std::fs::remove_file(&record)?;
        return Ok(());
    }
    let pinning = instances_pinning_base_with(paths, base_id, &metadata, &between_reads)?;
    if !pinning.is_empty() {
        return Err(BaseError::InUse { instances: pinning });
    }

    let manifest_path = paths.base_manifest_path(base_id);
    if !manifest_path.exists() {
        return Err(BaseError::NotFound(base_id.to_string()));
    }
    // A record that cannot be read at all is an error, not a damaged record: a failed read of a
    // healthy record must never be taken as permission to delete a folder elsewhere.
    let text = std::fs::read_to_string(&manifest_path)?;
    match serde_json::from_str::<BaseManifest>(&text) {
        Ok(manifest) => {
            let folder = trusted_base_folder(paths, &manifest, base_id)?;
            if folder.exists() {
                std::fs::remove_dir_all(&folder)?;
            }
            let _ = std::fs::remove_file(&manifest_path);
            Ok(())
        }
        // The record reads but is not a base record, so there is no recorded location to check.
        // Only the folder the id names in the data folder's bases folder is removed, and only if
        // it is a plain folder.
        Err(_) => remove_damaged_base(paths, base_id, &manifest_path),
    }
}

/// What repairing one framework in a pinned base copied and what it could not find.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameworkRepair {
    pub framework: String,
    pub name: String,
    /// How many files the framework declares as required.
    pub required_total: usize,
    /// Required files copied from the source install into the base in this repair.
    pub copied: Vec<String>,
    /// Required files the base already had (any case).
    pub already_present: Vec<String>,
    /// Required files the source install does not have either.
    pub missing_from_source: Vec<String>,
}

impl FrameworkRepair {
    /// Whether the base now has every required file.
    pub fn is_complete(&self) -> bool {
        self.missing_from_source.is_empty()
    }
}

/// Re-capture a framework's declared required files from the source install into an existing
/// base. This repairs a base pinned before the framework's `required` list existed, or one whose
/// source install lacked the scripts. Files the source lacks are reported, not invented; the
/// caller can then tell the user to install the framework as content.
pub fn repair_framework(
    paths: &AppPaths,
    base_id: &str,
    framework: &FrameworkDefinition,
) -> Result<FrameworkRepair, BaseError> {
    if !is_plain_base_id(base_id) {
        return Err(BaseError::NotFound(base_id.to_string()));
    }
    let manifest_path = paths.base_manifest_path(base_id);
    if !manifest_path.exists() {
        return Err(BaseError::NotFound(base_id.to_string()));
    }
    let read_manifest = || -> Result<BaseManifest, BaseError> {
        Ok(serde_json::from_str::<BaseManifest>(
            &std::fs::read_to_string(&manifest_path)?,
        )?)
    };
    let mut manifest = read_manifest()?;
    let Some(detection) = &framework.detection else {
        return Err(BaseError::Other(format!(
            "{} declares no detection, so there is nothing to repair",
            framework.name
        )));
    };
    if detection.required.is_empty() {
        return Err(BaseError::Other(format!(
            "{} declares no required files, so there is nothing to repair",
            framework.name
        )));
    }
    let marker_matches = |path: &str| {
        let marker = fold_path_case(detection.marker.as_str());
        let path = fold_path_case(path);
        if marker.contains('*') {
            agora_game_api::glob_match(&marker, &path)
        } else {
            marker == path
        }
    };
    if !manifest.files.iter().any(|file| marker_matches(&file.path)) {
        return Err(BaseError::FrameworkNotInstalled(framework.name.clone()));
    }
    if !manifest.location.is_dir() {
        return Err(BaseError::Other(format!(
            "the base folder {} is missing",
            manifest.location.display()
        )));
    }
    if !manifest.source_location.is_dir() {
        return Err(BaseError::Other(format!(
            "the source install {} is missing",
            manifest.source_location.display()
        )));
    }
    // Copying a file onto itself zeroes it. A base whose source install is its own folder cannot
    // be repaired by copying, so nothing is written — and a pair that cannot be resolved is refused
    // for the same reason: it might be one folder.
    if !proven_two_folders(&manifest.location, &manifest.source_location) {
        return Err(BaseError::Other(format!(
            "refusing to repair {}: the base folder and its source install are the same folder, or \
             could not be told apart ({})",
            framework.name,
            manifest.location.display()
        )));
    }

    let _lock = crate::lock_manager::LockManager::new(paths.locks_root())
        .acquire(
            crate::lock_manager::LockResource::Base(base_id.to_string()),
            "repair-framework",
        )
        .map_err(|e| BaseError::Other(e.to_string()))?;
    // A build may have finished while this waited; repair the record that is on disk now.
    manifest = read_manifest()?;

    let mut copied = Vec::new();
    let mut already_present = Vec::new();
    let mut missing_from_source = Vec::new();
    for required in &detection.required {
        let required = required.as_str();
        let recorded = manifest
            .files
            .iter()
            .position(|file| fold_path_case(&file.path) == fold_path_case(required));
        let destination = match recorded {
            Some(index) => manifest.location.join(&manifest.files[index].path),
            None => manifest.location.join(required),
        };
        let source = manifest.source_location.join(required);
        if !proven_two_folders(&source, &destination) {
            return Err(BaseError::Other(format!(
                "refusing to repair {required}: its source and destination are the same file or \
                 could not be told apart ({})",
                source.display()
            )));
        }
        // The manifest's word is not enough: the file has to be on disk with the size and digest
        // the record states, or it is replaced.
        let present_and_correct = recorded.is_some_and(|index| {
            let file = &manifest.files[index];
            std::fs::metadata(&destination).is_ok_and(|metadata| {
                metadata.is_file()
                    && metadata.len() == file.size
                    && hash_file(&destination).is_ok_and(|sha256| sha256 == file.sha256)
            })
        });
        if present_and_correct {
            already_present.push(required.to_string());
            continue;
        }
        if !source.is_file() {
            missing_from_source.push(required.to_string());
            continue;
        }
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&source, &destination)?;
        let sha256 = hash_file(&destination)?;
        let metadata = std::fs::metadata(&destination)?;
        let source_sha256 = hash_file(&source)?;
        let source_size = std::fs::metadata(&source)?.len();
        if sha256 != source_sha256 || metadata.len() != source_size {
            return Err(BaseError::Other(format!(
                "the copy of {required} into {} does not match its source {}",
                destination.display(),
                source.display()
            )));
        }
        let identity = get_file_identity(&destination);
        let modified_unix_ms = get_modified_unix_ms(&metadata);
        match recorded {
            Some(index) => {
                let file = &mut manifest.files[index];
                file.size = metadata.len();
                file.sha256 = sha256;
                file.linked = false;
                file.identity = identity;
                file.modified_unix_ms = modified_unix_ms;
            }
            None => manifest.files.push(BaseFile {
                path: required.to_string(),
                size: metadata.len(),
                sha256,
                linked: false,
                identity,
                modified_unix_ms,
            }),
        }
        copied.push(required.to_string());
    }
    manifest
        .files
        .sort_by(|left, right| left.path.cmp(&right.path));
    manifest
        .incomplete_frameworks
        .retain(|entry| entry.framework != framework.id.as_str());
    if !missing_from_source.is_empty() {
        manifest.incomplete_frameworks.push(IncompleteFramework {
            framework: framework.id.as_str().to_string(),
            name: framework.name.clone(),
            missing: missing_from_source.clone(),
        });
    }

    let unique = format!("{}-{}", std::process::id(), uuid::Uuid::new_v4().simple());
    let manifests_dir = paths.base_manifests_dir();
    std::fs::create_dir_all(&manifests_dir)?;
    let tmp_manifest = manifests_dir.join(format!("{base_id}.json.tmp-{unique}"));
    std::fs::write(&tmp_manifest, serde_json::to_string_pretty(&manifest)?)?;
    std::fs::rename(&tmp_manifest, &manifest_path)?;

    Ok(FrameworkRepair {
        framework: framework.id.as_str().to_string(),
        name: framework.name.clone(),
        required_total: detection.required.len(),
        copied,
        already_present,
        missing_from_source,
    })
}

/// Remove a base whose record cannot be read: the folder the id names in the data folder's bases
/// folder (if there is one), then the record. A base on another drive keeps its folder under that
/// drive's `AgoraBases` folder, because a damaged record does not say where that is.
fn remove_damaged_base(
    paths: &AppPaths,
    base_id: &str,
    manifest_path: &Path,
) -> Result<(), BaseError> {
    let folder = paths.bases_dir().join(base_id);
    match std::fs::symlink_metadata(&folder) {
        Ok(metadata) => {
            if is_reparse_point_or_symlink(&metadata) || !metadata.is_dir() {
                return Err(BaseError::Other(format!(
                    "refusing to remove {}: it is not a plain folder",
                    folder.display()
                )));
            }
            std::fs::remove_dir_all(&folder)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(BaseError::Io(error)),
    }
    std::fs::remove_file(manifest_path)?;
    Ok(())
}
