//! Turning an instance into a runnable Minecraft server.
//!
//! The export is built in two stages so the review proof test, which has no
//! network, can still exercise the local half:
//!
//! * [`export_server_environment`] copies the mods whose own metadata does not
//!   declare them client-only, plus `config/` and the chosen world. It performs
//!   no downloads and touches no loader catalog.
//! * [`export_ready_server`] runs that stage, then downloads the instance's
//!   pinned loader through the loader catalog, verifies it, and writes the start
//!   scripts, `eula.txt` and a short README.
//!
//! A server launcher whose bytes have no published hash is refused unless the
//! caller passes `allow_unverified_loader`, and the refusal names the reason.
//! Agora never accepts the Minecraft EULA for the player: `eula.txt` is written
//! with `eula=false`.

use crate::gc::{generate_args_for_java, GcProfile};
use crate::loader_manifests::{self, LoaderEntry};
use agora_core as core_api;
use core_api::app_paths::is_safe_file_name;
use core_api::artifact_hash::verify_download;
use core_api::ctx::Ctx;
use core_api::error::{LauncherError, LauncherResult};
use core_api::helpers::read_manifest;
use core_api::http_client::{self, ClientCategory, HttpClients};
use core_api::network::{NetworkCategory, NetworkPolicy};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};

/// What an export produced, for the CLI's `--json` and the desktop dialog.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportResult {
    pub total_mods: usize,
    pub server_mods: usize,
    pub removed_client_only: Vec<String>,
    /// True when a runnable server launcher jar was written (Fabric and Quilt).
    pub server_jar_downloaded: bool,
    pub start_scripts_created: bool,
    /// True when the loader artifact was pinned, downloaded and hash-checked.
    pub loader_downloaded: bool,
    /// The loader artifact written to the export folder, when there is one.
    #[serde(default)]
    pub loader_artifact: Option<String>,
    /// The world folder copied into the export, when the instance had one.
    #[serde(default)]
    pub world: Option<String>,
    /// The Java major version the start scripts and README name.
    #[serde(default)]
    pub java_major: Option<u32>,
    /// Whether `eula.txt` was written (always unaccepted).
    #[serde(default)]
    pub eula_file_written: bool,
    /// Things the player should read: unknown sides kept, unverified downloads
    /// they allowed, a missing world or mods folder.
    #[serde(default)]
    pub notes: Vec<String>,
}

/// The local, network-free half of the export.
///
/// The loader and Minecraft version are used to choose which metadata file a
/// mod's side is read from. A loader with no pinned version still exports its
/// mods; only [`export_ready_server`] needs the version.
pub fn export_server_environment(
    instance_dir: &Path,
    dest_dir: &Path,
    loader: &str,
    mc_version: &str,
) -> LauncherResult<ExportResult> {
    check_export_destination(instance_dir, dest_dir)?;
    let staging = staging_dir_for(dest_dir)?;
    std::fs::create_dir_all(&staging).map_err(|e| LauncherError::Generic {
        code: "ERR_EXPORT_MKDIR".into(),
        message: format!("Failed to create staging dir: {e}"),
    })?;
    let result = export_local(
        instance_dir,
        &staging,
        loader,
        mc_version,
        &LocalExportOptions::default(),
    );
    match result {
        Ok(value) => {
            commit_staging(&staging, dest_dir)?;
            Ok(value)
        }
        Err(error) => {
            let _ = std::fs::remove_dir_all(&staging);
            Err(error)
        }
    }
}

struct LocalExportOptions {
    java_major: u32,
    world: Option<String>,
    notes: Vec<String>,
}

impl Default for LocalExportOptions {
    fn default() -> Self {
        Self {
            java_major: 21,
            world: None,
            notes: Vec::new(),
        }
    }
}

fn export_local(
    instance_dir: &Path,
    dest_dir: &Path,
    loader: &str,
    mc_version: &str,
    options: &LocalExportOptions,
) -> LauncherResult<ExportResult> {
    let _ = mc_version;
    let dest_mods = dest_dir.join("mods");
    std::fs::create_dir_all(&dest_mods).map_err(|e| LauncherError::Generic {
        code: "ERR_EXPORT_MKDIR".into(),
        message: format!("Failed to create dest mods dir: {e}"),
    })?;

    let mut total_mods = 0;
    let mut server_mods = 0;
    let mut removed_client_only = Vec::new();
    let mut notes = options.notes.clone();

    let mods_dir = instance_dir.join("mods");
    if !mods_dir.exists() {
        notes.push("No mods folder: the export has no mods.".into());
    } else if !mods_dir.is_dir() {
        return Err(LauncherError::Generic {
            code: "ERR_EXPORT_READDIR".into(),
            message: format!("{} is not a folder.", mods_dir.display()),
        });
    } else {
        for entry in std::fs::read_dir(&mods_dir).map_err(|e| LauncherError::Generic {
            code: "ERR_EXPORT_READDIR".into(),
            message: format!("Failed to read mods dir: {e}"),
        })? {
            let entry = entry.map_err(|e| LauncherError::Generic {
                code: "ERR_EXPORT_ENTRY".into(),
                message: format!("Failed to read entry: {e}"),
            })?;
            let path = entry.path();
            if !path.is_file()
                || !path
                    .extension()
                    .and_then(|s| s.to_str())
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("jar"))
            {
                continue;
            }
            total_mods += 1;

            let side = read_mod_side(&path, loader);
            if side.is_client_only() {
                if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                    removed_client_only.push(name.to_string());
                }
                continue;
            }
            if let ModSide::Unknown(reason) = &side {
                let name = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("<unreadable file name>");
                notes.push(format!(
                    "{name}: its side could not be read ({reason}); it was kept. Check it on the server."
                ));
            }

            let fname = path.file_name().ok_or_else(|| LauncherError::Generic {
                code: "ERR_EXPORT_FILENAME".into(),
                message: "Missing file name".into(),
            })?;
            std::fs::copy(&path, dest_mods.join(fname)).map_err(|e| LauncherError::Generic {
                code: "ERR_EXPORT_COPY".into(),
                message: format!("Failed to copy mod: {e}"),
            })?;
            server_mods += 1;
        }
    }

    let config_src = instance_dir.join("config");
    if config_src.is_dir() {
        copy_dir_recursive(&config_src, &dest_dir.join("config"))?;
    }

    let world = select_world(instance_dir, options.world.as_deref())?;
    if let Some((folder, source)) = &world {
        copy_dir_recursive(source, &dest_dir.join("world"))?;
        notes.push(format!("World '{folder}' was copied to world/."));
    } else {
        notes.push("The instance has no world to copy; the server will create one.".into());
    }

    Ok(ExportResult {
        total_mods,
        server_mods,
        removed_client_only,
        server_jar_downloaded: false,
        start_scripts_created: false,
        loader_downloaded: false,
        loader_artifact: None,
        world: world.map(|(folder, _)| folder),
        java_major: Some(options.java_major),
        eula_file_written: false,
        notes,
    })
}

/// A full export: local files, then the pinned loader, scripts, EULA and README.
///
/// `allow_unverified_loader` is the explicit override for an official installer
/// whose repository publishes no hash beside it. Without it the export refuses,
/// naming why.
pub async fn export_ready_server(
    ctx: &Ctx,
    instance_id: &str,
    dest_dir: &Path,
    world: Option<&str>,
    allow_unverified_loader: bool,
) -> LauncherResult<ExportResult> {
    let id = core_api::paths::sanitize_id(instance_id);
    let instance_dir = ctx.paths.instance_dir(&id)?;
    if !instance_dir.is_dir() {
        return Err(LauncherError::Generic {
            code: "ERR_INSTANCE_NOT_FOUND".into(),
            message: format!("Instance '{instance_id}' not found."),
        });
    }
    check_export_destination(&instance_dir, dest_dir)?;
    let staging = staging_dir_for(dest_dir)?;
    std::fs::create_dir_all(&staging).map_err(|e| LauncherError::Generic {
        code: "ERR_EXPORT_MKDIR".into(),
        message: format!("Failed to create staging dir: {e}"),
    })?;
    let result =
        export_ready_server_impl(ctx, instance_id, &staging, world, allow_unverified_loader).await;
    match result {
        Ok(value) => {
            commit_staging(&staging, dest_dir)?;
            Ok(value)
        }
        Err(error) => {
            let _ = std::fs::remove_dir_all(&staging);
            Err(error)
        }
    }
}

async fn export_ready_server_impl(
    ctx: &Ctx,
    instance_id: &str,
    dest_dir: &Path,
    world: Option<&str>,
    allow_unverified_loader: bool,
) -> LauncherResult<ExportResult> {
    let id = core_api::paths::sanitize_id(instance_id);
    let instance_dir = ctx.paths.instance_dir(&id)?;
    if !instance_dir.is_dir() {
        return Err(LauncherError::Generic {
            code: "ERR_INSTANCE_NOT_FOUND".into(),
            message: format!("Instance '{instance_id}' not found."),
        });
    }
    let manifest = read_manifest(&ctx.paths.instance_manifest(&id)?)?;

    let mut notes = Vec::new();
    let (heap_mb, gc_profile, loader_version) = instance_launch_settings(ctx, &id, &manifest);

    let mut policy = NetworkPolicy::all_disabled();
    if let Ok(conn) = core_api::db::local_state_connection(&ctx.paths.local_state_db()) {
        policy = NetworkPolicy::from_db(&conn);
    }
    preflight_loader(
        &manifest.loader,
        &manifest.minecraft_version,
        &loader_version,
    )?;
    let java_major =
        required_java_major(ctx, &manifest.minecraft_version, &policy, &mut notes).await;

    // Fetch and verify the loader before writing anything, so a refused
    // download never leaves a half-built server folder.
    let loader = prepare_loader(
        ctx,
        &policy,
        &manifest.loader,
        &manifest.minecraft_version,
        &loader_version,
        allow_unverified_loader,
    )
    .await?;

    let local = export_local(
        &instance_dir,
        dest_dir,
        &manifest.loader,
        &manifest.minecraft_version,
        &LocalExportOptions {
            java_major,
            world: world.map(str::to_string),
            notes,
        },
    )?;

    if let Some(bytes) = &loader.bytes {
        std::fs::write(dest_dir.join(&loader.artifact_name), bytes).map_err(|e| {
            LauncherError::Generic {
                code: "ERR_EXPORT_WRITE".into(),
                message: format!("Failed to write {}: {e}", loader.artifact_name),
            }
        })?;
    }

    // A folder imported without a Minecraft version has no runnable server to
    // prepare; the mods/config/world export still succeeds and says what is
    // missing, rather than pretending to be ready.
    let runnable = !loader.artifact_name.is_empty();
    if runnable {
        let scripts = start_scripts(
            &loader.plan,
            heap_mb,
            gc_profile,
            java_major,
            &manifest.loader,
        );
        write_start_scripts(dest_dir, &scripts)?;
        if matches!(
            &loader.plan,
            LauncherPlan::InstallerThenRun {
                runner: InstallerRunner::RunScript,
                ..
            }
        ) {
            let jvm_args = generate_args_for_java(gc_profile, heap_mb, "", java_major);
            write_jvm_args(dest_dir, &jvm_args)?;
        }
    }
    write_eula(dest_dir)?;
    let all_notes = [local.notes.clone(), loader.notes.clone()].concat();
    write_readme(
        dest_dir,
        &ReadmeInfo {
            mc_version: &manifest.minecraft_version,
            loader: &manifest.loader,
            loader_version: &loader.loader_version,
            java_major,
            world: local.world.as_deref(),
            plan: &loader.plan,
            runnable,
            notes: &all_notes,
        },
    )?;

    let mut result = local;
    result.notes.extend(loader.notes);
    result.start_scripts_created = runnable;
    result.loader_downloaded = runnable;
    result.loader_artifact = runnable.then_some(loader.artifact_name);
    result.server_jar_downloaded =
        runnable && matches!(loader.plan, LauncherPlan::LauncherJar { .. });
    result.eula_file_written = true;
    result.java_major = Some(java_major);
    Ok(result)
}

/// The instance's memory, GC choice and pinned loader version, with the manifest
/// as a fallback when local state cannot be read.
fn instance_launch_settings(
    ctx: &Ctx,
    instance_id: &str,
    manifest: &core_api::models::InstanceManifest,
) -> (i64, GcProfile, String) {
    let mut heap_mb = 4096;
    let mut gc_profile = GcProfile::HighEfficiency;
    let mut loader_version = manifest.loader_version.clone();
    if let Ok(conn) = core_api::db::local_state_connection(&ctx.paths.local_state_db()) {
        if let Ok(Some(row)) = core_api::db::get_instance(&conn, instance_id) {
            heap_mb = row.jvm_memory_mb;
            gc_profile = crate::gc::GcSelection::from_persisted(&row.jvm_gc)
                .resolve()
                .unwrap_or(GcProfile::HighEfficiency);
            if !row.loader_version.is_empty() {
                loader_version = row.loader_version;
            }
        }
    }
    (heap_mb, gc_profile, loader_version)
}

/// The Java major named in the README and used for GC flags. Read from the
/// cached or fetched base version metadata; a failure is a note, never a guess
/// presented as fact.
async fn required_java_major(
    ctx: &Ctx,
    mc_version: &str,
    policy: &NetworkPolicy,
    notes: &mut Vec<String>,
) -> u32 {
    match crate::minecraft_metadata::ensure_base_version_metadata(
        &ctx.paths.minecraft_runtime_root(),
        mc_version,
        policy,
    )
    .await
    {
        Ok(info) => info
            .java_version
            .as_ref()
            .map(|java| java.major_version.max(1) as u32)
            .unwrap_or_else(|| {
                notes.push(format!(
                    "Minecraft {mc_version} does not name a required Java version; the scripts use Java 21."
                ));
                21
            }),
        Err(error) => {
            notes.push(format!(
                "Could not read the Java version Minecraft {mc_version} needs ({error}); install the Java version Mojang lists for it. The scripts use Java 21."
            ));
            21
        }
    }
}

// ---------------------------------------------------------------------------
// Mod side detection
// ---------------------------------------------------------------------------

/// Which sides a mod says it runs on. `Unknown` is kept, never dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ModSide {
    Both,
    ClientOnly,
    ServerOnly,
    Unknown(String),
}

impl ModSide {
    fn is_client_only(&self) -> bool {
        matches!(self, ModSide::ClientOnly)
    }
}

fn read_mod_side(jar_path: &Path, loader: &str) -> ModSide {
    let file = match std::fs::File::open(jar_path) {
        Ok(file) => file,
        Err(error) => return ModSide::Unknown(error.to_string()),
    };
    let mut archive = match zip::ZipArchive::new(file) {
        Ok(archive) => archive,
        Err(_) => return ModSide::Unknown("not a readable jar".into()),
    };

    for name in metadata_candidates(loader) {
        let Some(content) = read_entry_utf8(&mut archive, name) else {
            continue;
        };
        return side_from_metadata(name, &content);
    }
    ModSide::Unknown("no loader metadata was found".into())
}

/// Metadata files to read for a loader, most specific first. Quilt can load
/// Fabric mods, so a Quilt instance reads `fabric.mod.json` when there is no
/// `quilt.mod.json`.
fn metadata_candidates(loader: &str) -> &'static [&'static str] {
    match loader.to_ascii_lowercase().as_str() {
        "fabric" => &["fabric.mod.json"],
        "quilt" => &["quilt.mod.json", "fabric.mod.json"],
        "forge" => &["META-INF/mods.toml", "META-INF/neoforge.mods.toml"],
        "neoforge" => &["META-INF/neoforge.mods.toml", "META-INF/mods.toml"],
        _ => &[
            "fabric.mod.json",
            "quilt.mod.json",
            "META-INF/neoforge.mods.toml",
            "META-INF/mods.toml",
        ],
    }
}

fn side_from_metadata(file_name: &str, content: &str) -> ModSide {
    match file_name {
        "fabric.mod.json" => side_from_json(content, &["/environment"], true),
        "quilt.mod.json" => side_from_json(
            content,
            &[
                "/minecraft/environment",
                "/quilt_loader/environment",
                "/environment",
            ],
            true,
        ),
        _ => side_from_toml(content),
    }
}

/// Read an environment value from JSON. An absent value is both sides for
/// Fabric and Quilt; an unparseable file or an unrecognised value is unknown.
fn side_from_json(content: &str, pointers: &[&str], absent_is_both: bool) -> ModSide {
    let value: serde_json::Value = match serde_json::from_str(content) {
        Ok(value) => value,
        Err(error) => return ModSide::Unknown(format!("metadata is not valid JSON: {error}")),
    };
    let raw = pointers
        .iter()
        .find_map(|pointer| value.pointer(pointer).and_then(|v| v.as_str()));
    match raw {
        None if absent_is_both => ModSide::Both,
        None => ModSide::Unknown("no environment was declared".into()),
        Some(value) => side_from_environment(value),
    }
}

fn side_from_environment(value: &str) -> ModSide {
    match value.trim().to_ascii_lowercase().as_str() {
        "client" => ModSide::ClientOnly,
        "server" => ModSide::ServerOnly,
        "*" | "both" => ModSide::Both,
        other => ModSide::Unknown(format!("unrecognised side '{other}'")),
    }
}

/// Forge and NeoForge: only a `side` on the mod's own `[[mods]]` entry (or a
/// top-level `side`) speaks for the mod. A dependency's `side = "CLIENT"` says
/// where that dependency is needed, not that this mod is client-only.
fn side_from_toml(content: &str) -> ModSide {
    let value: toml::Value = match toml::from_str(content) {
        Ok(value) => value,
        Err(error) => return ModSide::Unknown(format!("metadata is not valid TOML: {error}")),
    };
    let mut declared = Vec::new();
    if let Some(side) = value.get("side").and_then(toml::Value::as_str) {
        declared.push(side.to_string());
    }
    if let Some(mods) = value.get("mods").and_then(toml::Value::as_array) {
        for entry in mods {
            if let Some(side) = entry.get("side").and_then(toml::Value::as_str) {
                declared.push(side.to_string());
            }
        }
    }
    let sides: Vec<ModSide> = declared
        .iter()
        .map(|side| side_from_environment(side))
        .collect();
    if sides.is_empty() {
        return ModSide::Unknown("the mod declares no side".into());
    }
    if sides.iter().any(|side| matches!(side, ModSide::Both)) {
        return ModSide::Both;
    }
    if sides.iter().any(|side| matches!(side, ModSide::ServerOnly)) {
        return ModSide::ServerOnly;
    }
    if sides.iter().all(|side| matches!(side, ModSide::ClientOnly)) {
        return ModSide::ClientOnly;
    }
    ModSide::Unknown("the mod declares conflicting sides".into())
}

fn read_entry_utf8(archive: &mut zip::ZipArchive<std::fs::File>, name: &str) -> Option<String> {
    let mut file = archive.by_name(name).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

// ---------------------------------------------------------------------------
// World choice
// ---------------------------------------------------------------------------

/// The chosen world folder and its source. `None` means the instance has no
/// world at all. An explicitly requested world that does not exist is an error.
fn select_world(
    instance_dir: &Path,
    requested: Option<&str>,
) -> LauncherResult<Option<(String, PathBuf)>> {
    let saves = instance_dir.join("saves");
    let mut candidates: Vec<(String, PathBuf, i64)> = Vec::new();

    let root_world = instance_dir.join("world");
    if root_world.is_dir() {
        candidates.push((
            "world".to_string(),
            root_world.clone(),
            world_recency(&root_world),
        ));
    }
    if saves.is_dir() {
        for entry in std::fs::read_dir(&saves).map_err(|e| LauncherError::Generic {
            code: "ERR_EXPORT_READDIR".into(),
            message: format!("Failed to read saves: {e}"),
        })? {
            let entry = entry.map_err(|e| LauncherError::Generic {
                code: "ERR_EXPORT_ENTRY".into(),
                message: format!("Failed to read saves entry: {e}"),
            })?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(folder) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !is_safe_file_name(&folder) {
                continue;
            }
            candidates.push((folder, path.clone(), world_recency(&path)));
        }
    }

    if let Some(folder) = requested {
        if !is_safe_file_name(folder) {
            return Err(LauncherError::Generic {
                code: "ERR_EXPORT_WORLD_NAME".into(),
                message: format!("'{folder}' is not a valid world folder name."),
            });
        }
        let found = candidates
            .into_iter()
            .find(|(candidate, _, _)| candidate == folder);
        return found
            .map(|(folder, path, _)| Some((folder, path)))
            .ok_or_else(|| LauncherError::Generic {
                code: "ERR_EXPORT_WORLD_NOT_FOUND".into(),
                message: format!(
                    "The instance has no world named '{folder}'. Worlds: {}.",
                    available_worlds(instance_dir)
                ),
            });
    }

    candidates.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    Ok(candidates
        .into_iter()
        .next()
        .map(|(folder, path, _)| (folder, path)))
}

fn available_worlds(instance_dir: &Path) -> String {
    let saves = instance_dir.join("saves");
    let mut names = Vec::new();
    if instance_dir.join("world").is_dir() {
        names.push("world".to_string());
    }
    if let Ok(entries) = std::fs::read_dir(saves) {
        for entry in entries.filter_map(Result::ok) {
            if entry.path().is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    names.push(name.to_string());
                }
            }
        }
    }
    if names.is_empty() {
        "none".into()
    } else {
        names.join(", ")
    }
}

/// A world's `LastPlayed`, or the level.dat modification time when the game
/// never recorded one. Never a made-up zero that would beat a real world.
fn world_recency(world_dir: &Path) -> i64 {
    let folder = world_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if let Some((_, played)) = crate::worlds::world_summary(world_dir, folder) {
        return played;
    }
    std::fs::metadata(world_dir.join("level.dat"))
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Loader download and verification
// ---------------------------------------------------------------------------

/// The official installer repositories. The installer version comes from each
/// repository's `maven-metadata.xml` `<release>` (then `<latest>`, then the last
/// `<version>`); Agora's loader catalog pins loader versions, not installer
/// versions.
const FABRIC_INSTALLER_BASE: &str = "https://maven.fabricmc.net/net/fabricmc/fabric-installer";
const QUILT_INSTALLER_BASE: &str =
    "https://maven.quiltmc.org/repository/release/org/quiltmc/quilt-installer";

/// How a verified installer builds the server on first start.
#[derive(Debug, Clone, PartialEq, Eq)]
enum InstallerRunner {
    /// Forge/NeoForge's generated run scripts, which read user_jvm_args.txt.
    RunScript,
    /// A launcher jar written by the installer (Fabric, Quilt).
    LauncherJar { file: String },
}

/// How the start script reaches a running server.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LauncherPlan {
    /// A runnable server launcher jar (vanilla).
    LauncherJar { file: String },
    /// A verified installer that builds the server on first start.
    InstallerThenRun {
        installer: String,
        args: Vec<String>,
        runner: InstallerRunner,
    },
}

struct PreparedLoader {
    loader_version: String,
    plan: LauncherPlan,
    artifact_name: String,
    bytes: Option<Vec<u8>>,
    notes: Vec<String>,
}

/// How an instance's loader is exported, resolved before anything is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoaderKind {
    Vanilla,
    FabricLike,
    Installer,
}

fn plan_loader(
    loader: &str,
    mc_version: &str,
    loader_version: &str,
) -> LauncherResult<(LoaderKind, Option<LoaderEntry>)> {
    let loader_key = loader.to_ascii_lowercase();
    if matches!(loader_key.as_str(), "" | "vanilla" | "none") {
        return Ok((LoaderKind::Vanilla, None));
    }
    if matches!(loader_key.as_str(), "fabric" | "quilt") {
        return Ok((
            LoaderKind::FabricLike,
            Some(pinned_entry(&loader_key, mc_version, loader_version)?),
        ));
    }
    if matches!(loader_key.as_str(), "forge" | "neoforge") {
        return Ok((
            LoaderKind::Installer,
            Some(pinned_entry(&loader_key, mc_version, loader_version)?),
        ));
    }
    Err(LauncherError::Generic {
        code: "ERR_EXPORT_UNSUPPORTED_LOADER".into(),
        message: format!(
            "The instance's loader '{loader}' has no server export. Export a Fabric, Quilt, Forge, NeoForge or vanilla instance."
        ),
    })
}

/// Resolve the loader before any local file is written, so an unsupported
/// loader or a missing pin is refused without leaving a half-built folder.
fn preflight_loader(loader: &str, mc_version: &str, loader_version: &str) -> LauncherResult<()> {
    plan_loader(loader, mc_version, loader_version).map(|_| ())
}

async fn prepare_loader(
    ctx: &Ctx,
    policy: &NetworkPolicy,
    loader: &str,
    mc_version: &str,
    loader_version: &str,
    allow_unverified: bool,
) -> LauncherResult<PreparedLoader> {
    let clients = &ctx.http_clients;
    let (kind, entry) = plan_loader(loader, mc_version, loader_version)?;
    let loader_key = loader.to_ascii_lowercase();
    let entry = match (kind, entry) {
        (LoaderKind::Vanilla, _) if mc_version.trim().is_empty() => {
            return Ok(PreparedLoader {
                loader_version: String::new(),
                plan: LauncherPlan::LauncherJar {
                    file: "server.jar".into(),
                },
                artifact_name: String::new(),
                bytes: None,
                notes: vec![
                    "The instance has no Minecraft version, so no server jar or start script was prepared."
                        .into(),
                ],
            });
        }
        (LoaderKind::Vanilla, _) => return download_vanilla_server(ctx, policy, mc_version).await,
        (_, Some(entry)) => entry,
        (_, None) => unreachable!("a modded loader has a pinned entry"),
    };
    let mut notes = Vec::new();

    if kind == LoaderKind::FabricLike {
        if entry.file_type != "profile_json" {
            return Err(LauncherError::Generic {
                code: "ERR_EXPORT_UNSUPPORTED_LOADER".into(),
                message: format!(
                    "The pinned {loader} catalog entry is {}, not a loader profile.",
                    entry.file_type
                ),
            });
        }
        let profile =
            fetch_loader_bytes(clients, policy, &entry.source_url, &entry.file_name).await?;
        verify_loader_artifact(&loader_key, &entry, &profile)?;

        let (artifact_name, bytes, installer_version, hash_verified) =
            prepare_official_installer(clients, policy, &loader_key, allow_unverified).await?;
        if !hash_verified {
            notes.push(format!(
                "No published .sha512 was found beside the {loader} installer {installer_version}; it was downloaded because unverified loader downloads were allowed."
            ));
        }
        let (args, runner_file) = match loader_key.as_str() {
            "quilt" => (
                quilt_installer_server_args(mc_version, &entry.loader_version),
                "quilt-server-launch.jar".to_string(),
            ),
            _ => (
                fabric_installer_server_args(mc_version, &entry.loader_version),
                "fabric-server-launch.jar".to_string(),
            ),
        };
        notes.push(format!(
            "The first start runs the verified {loader} installer {installer_version} in server mode and writes {runner_file}; it downloads the loader's libraries and the Minecraft server jar."
        ));
        return Ok(PreparedLoader {
            loader_version: entry.loader_version,
            plan: LauncherPlan::InstallerThenRun {
                installer: artifact_name.clone(),
                args,
                runner: InstallerRunner::LauncherJar { file: runner_file },
            },
            artifact_name,
            bytes: Some(bytes),
            notes,
        });
    }

    let bytes = fetch_loader_bytes(clients, policy, &entry.source_url, &entry.file_name).await?;
    verify_loader_artifact(&loader_key, &entry, &bytes)?;
    let artifact_name = format!("{loader_key}-installer.jar");
    notes.push(format!(
        "The first start runs the verified {loader} installer to build the server; it needs Java and downloads the loader's libraries."
    ));
    Ok(PreparedLoader {
        loader_version: entry.loader_version,
        plan: LauncherPlan::InstallerThenRun {
            installer: artifact_name.clone(),
            args: vec!["--installServer".into()],
            runner: InstallerRunner::RunScript,
        },
        artifact_name,
        bytes: Some(bytes),
        notes,
    })
}

/// Fabric's installer server mode. Source: `ServerHandler.java` lines 69-94
/// (`installCli` and `cliHelp`) and `Main.java` lines 86-89 (the handler name
/// is matched case-insensitively) in
/// https://github.com/FabricMC/fabric-installer. It writes
/// `fabric-server-launch.jar` (`ServerInstaller.java` line 61).
fn fabric_installer_server_args(mc_version: &str, loader_version: &str) -> Vec<String> {
    vec![
        "server".into(),
        "-dir".into(),
        ".".into(),
        "-mcversion".into(),
        mc_version.into(),
        "-loader".into(),
        loader_version.into(),
        "-downloadMinecraft".into(),
    ]
}

/// Quilt's installer server mode. Source: `CliInstaller.java` lines 193-275
/// (`install server <minecraftVersion> [<loaderVersion>] [--install-dir=]
/// [--download-server]`) and `InstallServer.java` lines 67-70, 109 and 126-129
/// in https://github.com/QuiltMC/quilt-installer. It writes
/// `quilt-server-launch.jar`.
fn quilt_installer_server_args(mc_version: &str, loader_version: &str) -> Vec<String> {
    vec![
        "install".into(),
        "server".into(),
        mc_version.into(),
        loader_version.into(),
        "--install-dir=.".into(),
        "--download-server".into(),
    ]
}

/// Download a Fabric/Quilt installer from its official Maven repository and
/// check it against the strongest hash the repository publishes beside it
/// (`.sha512`; both repositories publish it). A missing hash file is the only
/// case `allow_unverified` may override.
async fn prepare_official_installer(
    clients: &HttpClients,
    policy: &NetworkPolicy,
    loader: &str,
    allow_unverified: bool,
) -> LauncherResult<(String, Vec<u8>, String, bool)> {
    let base = match loader {
        "fabric" => FABRIC_INSTALLER_BASE,
        "quilt" => QUILT_INSTALLER_BASE,
        _ => {
            return Err(LauncherError::Generic {
                code: "ERR_EXPORT_UNSUPPORTED_LOADER".into(),
                message: format!("No official installer repository is known for {loader}."),
            })
        }
    };
    let metadata_url = format!("{base}/maven-metadata.xml");
    let metadata = fetch_loader_bytes(clients, policy, &metadata_url, "maven-metadata.xml").await?;
    let installer_version = parse_maven_metadata_latest(&String::from_utf8_lossy(&metadata))?;
    let jar_name = format!("{loader}-installer-{installer_version}.jar");
    let jar_url = format!("{base}/{installer_version}/{jar_name}");
    let bytes = fetch_loader_bytes(clients, policy, &jar_url, &jar_name).await?;
    let hash_url = format!("{jar_url}.sha512");
    let published =
        match fetch_loader_bytes(clients, policy, &hash_url, &format!("{jar_name}.sha512")).await {
            Ok(text) => Some(String::from_utf8_lossy(&text).into_owned()),
            Err(error) if is_http_not_found(&error) => None,
            Err(error) => return Err(error),
        };
    let verified =
        verify_published_hash(&bytes, published.as_deref(), &jar_name, allow_unverified)?;
    Ok((
        format!("{loader}-installer.jar"),
        bytes,
        installer_version,
        verified,
    ))
}

fn is_http_not_found(error: &LauncherError) -> bool {
    error.to_string().contains("HTTP 404")
}

/// `<release>` first, then `<latest>`, then the last `<version>` in a Maven
/// `maven-metadata.xml`.
fn parse_maven_metadata_latest(xml: &str) -> LauncherResult<String> {
    for tag in ["release", "latest"] {
        if let Some(value) = xml_tag(xml, tag) {
            if !value.is_empty() {
                return Ok(value.to_string());
            }
        }
    }
    let mut last = None;
    let mut rest = xml;
    while let Some(start) = rest.find("<version>") {
        let after = &rest[start + "<version>".len()..];
        let Some(end) = after.find("</version>") else {
            break;
        };
        last = Some(after[..end].trim().to_string());
        rest = &after[end + "</version>".len()..];
    }
    last.filter(|value| !value.is_empty())
        .ok_or_else(|| LauncherError::Generic {
            code: "ERR_EXPORT_INSTALLER_METADATA".into(),
            message: "The installer repository's maven-metadata.xml names no version.".into(),
        })
}

fn xml_tag<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].trim())
}

/// The hash in a Maven `.sha512` file: a bare digest, or `sha512:`/`sha512-`
/// prefixed, optionally followed by the file name.
fn parse_published_hash(text: &str) -> LauncherResult<String> {
    let raw = text
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let raw = raw.strip_prefix("sha512:").unwrap_or(&raw);
    let raw = raw.strip_prefix("sha512-").unwrap_or(raw);
    let raw = raw.strip_prefix("sha512=").unwrap_or(raw);
    if raw.len() != 128 || !raw.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(LauncherError::Generic {
            code: "ERR_EXPORT_INSTALLER_HASH".into(),
            message: "The installer's published hash is not a SHA-512 digest.".into(),
        });
    }
    Ok(raw.to_string())
}

/// Check installer bytes against the published hash. A missing hash file is
/// only allowed with the explicit override, and is reported as unverified.
fn verify_published_hash(
    bytes: &[u8],
    published: Option<&str>,
    file: &str,
    allow_unverified: bool,
) -> LauncherResult<bool> {
    match published {
        Some(text) => {
            let expected = parse_published_hash(text)?;
            let actual = core_api::download::sha512_hex(bytes);
            if actual.eq_ignore_ascii_case(&expected) {
                Ok(true)
            } else {
                Err(LauncherError::HashMismatch)
            }
        }
        None if allow_unverified => Ok(false),
        None => Err(LauncherError::Generic {
            code: "ERR_EXPORT_UNVERIFIED_LOADER".into(),
            message: format!(
                "No .sha512 hash is published beside {file}, so it cannot be verified. Pass --allow-unverified-loader to download it anyway."
            ),
        }),
    }
}

/// Vanilla has no loader catalog entry: the server jar is Mojang's own, with
/// the SHA-1 the version JSON publishes.
async fn download_vanilla_server(
    ctx: &Ctx,
    policy: &NetworkPolicy,
    mc_version: &str,
) -> LauncherResult<PreparedLoader> {
    let info = crate::minecraft_metadata::ensure_base_version_metadata(
        &ctx.paths.minecraft_runtime_root(),
        mc_version,
        policy,
    )
    .await?;
    let server = info
        .downloads
        .and_then(|downloads| downloads.server)
        .ok_or_else(|| LauncherError::Generic {
            code: "ERR_EXPORT_NO_SERVER_JAR".into(),
            message: format!("Minecraft {mc_version} publishes no server jar."),
        })?;
    policy.check(NetworkCategory::MojangContent)?;
    let bytes = http_client::checked_get_bytes(
        &ctx.http_clients,
        ClientCategory::MojangContent,
        &server.url,
    )
    .await?;
    let check = verify_download(
        &bytes,
        server.sha1.as_deref(),
        None,
        &[],
        false,
        "server.jar",
        Some(mc_version),
    )?;
    if !check.verified {
        return Err(LauncherError::Generic {
            code: "ERR_EXPORT_UNVERIFIED_LOADER".into(),
            message: format!(
                "Minecraft {mc_version} publishes no hash for its server jar, so it cannot be verified."
            ),
        });
    }
    Ok(PreparedLoader {
        loader_version: "vanilla".into(),
        plan: LauncherPlan::LauncherJar {
            file: "server.jar".into(),
        },
        artifact_name: "server.jar".into(),
        bytes: Some(bytes),
        notes: Vec::new(),
    })
}

fn pinned_entry(
    loader: &str,
    mc_version: &str,
    loader_version: &str,
) -> LauncherResult<LoaderEntry> {
    if !loader_version.is_empty() {
        return loader_manifests::find_entry(loader, mc_version, loader_version).ok_or_else(|| {
            LauncherError::Generic {
                code: "ERR_EXPORT_NO_PINNED".into(),
                message: format!(
                    "{loader} {loader_version} is not in the pinned loader catalog for Minecraft {mc_version}."
                ),
            }
        });
    }
    loader_manifests::newest_verified(loader, mc_version).ok_or_else(|| LauncherError::Generic {
        code: "ERR_EXPORT_NO_PINNED".into(),
        message: format!("No pinned {loader} version is available for Minecraft {mc_version}."),
    })
}

async fn fetch_loader_bytes(
    clients: &HttpClients,
    policy: &NetworkPolicy,
    url: &str,
    file_name: &str,
) -> LauncherResult<Vec<u8>> {
    policy.check(NetworkCategory::LoaderMetadataAndContent)?;
    loader_manifests::ensure_allowed_domain(url).inspect_err(|_| {
        eprintln!(
            "[server-export] rejected loader host file={file_name} url={}",
            core_api::network::sanitized_url_for_log(url)
        );
    })?;
    http_client::checked_get_bytes(clients, ClientCategory::Loader, url).await
}

/// Check downloaded loader bytes against the catalog's pin. Fabric and Quilt
/// profile JSONs use the stable normalised hash, installers the raw SHA-256.
fn verify_loader_artifact(loader: &str, entry: &LoaderEntry, bytes: &[u8]) -> LauncherResult<()> {
    let expected = loader_manifests::strip_sha_prefix(&entry.sha256);
    if expected.is_empty() {
        return Err(LauncherError::Generic {
            code: "ERR_EXPORT_UNVERIFIED_LOADER".into(),
            message: format!(
                "The pinned {} {} entry has no published hash, so the download cannot be verified.",
                entry.loader_version, entry.file_name
            ),
        });
    }
    let actual = crate::loader_download::compute_loader_hash(
        loader,
        &entry.file_name,
        &entry.file_type,
        bytes,
    );
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(LauncherError::HashMismatch)
    }
}

// ---------------------------------------------------------------------------
// Start scripts, EULA and README
// ---------------------------------------------------------------------------

fn start_scripts(
    plan: &LauncherPlan,
    heap_mb: i64,
    gc_profile: GcProfile,
    java_major: u32,
    loader: &str,
) -> (String, String) {
    let gc_args = generate_args_for_java(gc_profile, heap_mb, "", java_major);
    let sh = format!(
        "#!/usr/bin/env sh\n# Agora server export for {loader}. Java {java_major} or newer is required.\n# Accept the Minecraft EULA in eula.txt before the first start.\ncd \"$(dirname \"$0\")\"\nJAVA_BIN=\"${{JAVA:-java}}\"\n{}\n",
        shell_body(plan, &gc_args)
    );
    let bat = format!(
        "@echo off\r\nrem Agora server export for {loader}. Java {java_major} or newer is required.\r\nrem Accept the Minecraft EULA in eula.txt before the first start.\r\ncd /d \"%~dp0\"\r\nif \"%JAVA%\"==\"\" set \"JAVA=java\"\r\n{}\r\n",
        batch_body(plan, &gc_args)
    );
    (sh, bat)
}

fn shell_body(plan: &LauncherPlan, gc_args: &str) -> String {
    match plan {
        LauncherPlan::LauncherJar { file } => {
            format!("exec \"$JAVA_BIN\" {gc_args} -jar ./{file} nogui")
        }
        LauncherPlan::InstallerThenRun {
            installer,
            args,
            runner,
        } => {
            let install = format!("\"$JAVA_BIN\" -jar ./{installer} {}", args.join(" "));
            match runner {
                InstallerRunner::RunScript => format!(
                    "if [ ! -f ./run.sh ]; then\n  {install} || exit 1\nfi\nexec bash ./run.sh nogui"
                ),
                InstallerRunner::LauncherJar { file } => format!(
                    "if [ ! -f ./{file} ]; then\n  {install} || exit 1\nfi\nexec \"$JAVA_BIN\" {gc_args} -jar ./{file} nogui"
                ),
            }
        }
    }
}

fn batch_body(plan: &LauncherPlan, gc_args: &str) -> String {
    match plan {
        LauncherPlan::LauncherJar { file } => {
            format!("\"%JAVA%\" {gc_args} -jar \"%~dp0{file}\" nogui")
        }
        LauncherPlan::InstallerThenRun {
            installer,
            args,
            runner,
        } => {
            let install = format!("\"%JAVA%\" -jar \"%~dp0{installer}\" {}", args.join(" "));
            match runner {
                InstallerRunner::RunScript => format!(
                    "if not exist \"%~dp0run.bat\" (\r\n  {install}\r\n  if errorlevel 1 exit /b 1\r\n)\r\ncall \"%~dp0run.bat\" nogui"
                ),
                InstallerRunner::LauncherJar { file } => format!(
                    "if not exist \"%~dp0{file}\" (\r\n  {install}\r\n  if errorlevel 1 exit /b 1\r\n)\r\n\"%JAVA%\" {gc_args} -jar \"%~dp0{file}\" nogui"
                ),
            }
        }
    }
}

fn write_start_scripts(dest_dir: &Path, scripts: &(String, String)) -> LauncherResult<()> {
    let sh_path = dest_dir.join("start.sh");
    std::fs::write(&sh_path, &scripts.0).map_err(|e| LauncherError::Generic {
        code: "ERR_EXPORT_WRITE".into(),
        message: format!("Failed to write start.sh: {e}"),
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&sh_path, std::fs::Permissions::from_mode(0o755));
    }
    std::fs::write(dest_dir.join("start.bat"), &scripts.1).map_err(|e| LauncherError::Generic {
        code: "ERR_EXPORT_WRITE".into(),
        message: format!("Failed to write start.bat: {e}"),
    })?;
    Ok(())
}

/// Forge and NeoForge's generated `run.sh`/`run.bat` read `user_jvm_args.txt`,
/// so the instance's memory and GC flags go there, one flag per line.
fn write_jvm_args(dest_dir: &Path, gc_args: &str) -> LauncherResult<()> {
    let text = gc_args.split_whitespace().collect::<Vec<_>>().join("\n") + "\n";
    std::fs::write(dest_dir.join("user_jvm_args.txt"), text).map_err(|e| LauncherError::Generic {
        code: "ERR_EXPORT_WRITE".into(),
        message: format!("Failed to write user_jvm_args.txt: {e}"),
    })
}

fn write_eula(dest_dir: &Path) -> LauncherResult<()> {
    let path = dest_dir.join("eula.txt");
    if path.exists() {
        return Ok(());
    }
    std::fs::write(
        &path,
        "# By changing eula=false to eula=true you are agreeing to the Minecraft EULA:\n# https://aka.ms/MinecraftEULA\neula=false\n",
    )
    .map_err(|e| LauncherError::Generic {
        code: "ERR_EXPORT_WRITE".into(),
        message: format!("Failed to write eula.txt: {e}"),
    })
}

struct ReadmeInfo<'a> {
    mc_version: &'a str,
    loader: &'a str,
    loader_version: &'a str,
    java_major: u32,
    world: Option<&'a str>,
    plan: &'a LauncherPlan,
    runnable: bool,
    notes: &'a [String],
}

fn write_readme(dest_dir: &Path, info: &ReadmeInfo<'_>) -> LauncherResult<()> {
    let start = "Windows: double-click start.bat. Linux/macOS: run ./start.sh.";
    let mut text = format!(
        "Agora server export\n===================\n\nMinecraft {} with {} {}.\nRequired Java: {} or newer. Install a matching Java runtime and put it on PATH as `java`, or set the JAVA environment variable to its path.\n\nStart the server\n----------------\n{start}\n\nAccept the EULA\n---------------\neula.txt is written with eula=false. Agora does not accept the Minecraft EULA for you; read https://aka.ms/MinecraftEULA and change the value to true only if you agree.\n\nWorld\n-----\n{}\n\nWhat is in this folder\n----------------------\n- mods/ (mods whose own metadata does not make them client-only)\n- config/\n- world/ (the world it will host, unless you export one with no world)\n- eula.txt (unaccepted)\n{}",
        info.mc_version,
        info.loader,
        info.loader_version,
        info.java_major,
        info.world
            .map(|name| format!("'{name}' was copied to world/."))
            .unwrap_or_else(|| "No world was copied; the server will create one.".into()),
        if info.runnable {
            "- start.sh and start.bat\n"
        } else {
            ""
        }
    );
    if !info.runnable {
        text.push_str(
            "\nThis folder has no server launcher and no start script. The instance has no Minecraft version, so there was nothing to download; set the instance's game version and export again.\n",
        );
    } else if let LauncherPlan::InstallerThenRun {
        installer, runner, ..
    } = info.plan
    {
        match runner {
            InstallerRunner::RunScript => text.push_str(&format!(
                "\nThe first start runs the verified installer {installer} (--installServer). It needs Java and downloads the loader's libraries; later starts reuse them. Memory and GC flags are in user_jvm_args.txt.\n"
            )),
            InstallerRunner::LauncherJar { file } => text.push_str(&format!(
                "\nThe first start runs the verified installer {installer} in server mode and writes {file}; it downloads the loader's libraries and the Minecraft server jar. Later starts reuse them. Memory and GC flags are in the start script.\n"
            )),
        }
    }
    if !info.notes.is_empty() {
        text.push_str("\nNotes\n-----\n");
        for note in info.notes {
            text.push_str("- ");
            text.push_str(note);
            text.push('\n');
        }
    }
    std::fs::write(dest_dir.join("README.txt"), text).map_err(|e| LauncherError::Generic {
        code: "ERR_EXPORT_WRITE".into(),
        message: format!("Failed to write README.txt: {e}"),
    })
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> LauncherResult<()> {
    std::fs::create_dir_all(dst).map_err(|e| LauncherError::Generic {
        code: "ERR_EXPORT_MKDIR".into(),
        message: format!("mkdir {dst:?}: {e}"),
    })?;
    for entry in std::fs::read_dir(src).map_err(|e| LauncherError::Generic {
        code: "ERR_EXPORT_READDIR".into(),
        message: format!("readdir {src:?}: {e}"),
    })? {
        let entry = entry.map_err(|e| LauncherError::Generic {
            code: "ERR_EXPORT_ENTRY".into(),
            message: format!("entry: {e}"),
        })?;
        let ty = entry.file_type().map_err(|e| LauncherError::Generic {
            code: "ERR_EXPORT_FTYPE".into(),
            message: format!("ftype: {e}"),
        })?;
        if ty.is_dir() {
            copy_dir_recursive(&entry.path(), &dst.join(entry.file_name()))?;
        } else {
            std::fs::copy(entry.path(), dst.join(entry.file_name())).map_err(|e| {
                LauncherError::Generic {
                    code: "ERR_EXPORT_COPY".into(),
                    message: format!("copy {}: {e}", entry.path().display()),
                }
            })?;
        }
    }
    Ok(())
}

fn canonicalize_path(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }
    if let Some(parent) = path.parent() {
        if let Ok(canonical_parent) = std::fs::canonicalize(parent) {
            return canonical_parent.join(path.file_name().unwrap_or_default());
        }
    }
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    }
}

fn check_export_destination(instance_dir: &Path, dest_dir: &Path) -> LauncherResult<()> {
    let instance = canonicalize_path(instance_dir);
    let dest = canonicalize_path(dest_dir);
    if instance == dest || dest.starts_with(&instance) || instance.starts_with(&dest) {
        return Err(LauncherError::Generic {
            code: "ERR_EXPORT_DEST_OVERLAPS_INSTANCE".into(),
            message: format!(
                "The export destination '{}' overlaps the instance '{}'; refusing to avoid damaging the source.",
                dest_dir.display(),
                instance_dir.display()
            ),
        });
    }
    Ok(())
}

fn staging_dir_for(dest_dir: &Path) -> LauncherResult<PathBuf> {
    let parent = dest_dir.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|e| LauncherError::Generic {
        code: "ERR_EXPORT_MKDIR".into(),
        message: format!("Failed to create destination parent: {e}"),
    })?;
    let base_name = dest_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("server");
    let mut staging = parent.join(format!(".{base_name}.staging-{}", std::process::id()));
    let mut counter = 0u32;
    while staging.exists() {
        counter += 1;
        staging = parent.join(format!(
            ".{base_name}.staging-{}-{counter}",
            std::process::id()
        ));
    }
    Ok(staging)
}

fn commit_staging(staging: &Path, dest: &Path) -> LauncherResult<()> {
    if dest.exists() {
        if dest.is_dir() {
            let is_empty = std::fs::read_dir(dest)
                .map_err(|e| LauncherError::Generic {
                    code: "ERR_EXPORT_READDIR".into(),
                    message: format!("Failed to read destination: {e}"),
                })?
                .next()
                .is_none();
            if !is_empty {
                return Err(LauncherError::Generic {
                    code: "ERR_EXPORT_DEST_NOT_EMPTY".into(),
                    message: format!(
                        "The export destination '{}' already contains files. Choose an empty or new folder.",
                        dest.display()
                    ),
                });
            }
            // remove_dir, not remove_dir_all: if anything appeared since the emptiness check,
            // this fails instead of deleting it.
            std::fs::remove_dir(dest).map_err(|e| LauncherError::Generic {
                code: "ERR_EXPORT_DEST_NOT_EMPTY".into(),
                message: format!("Failed to replace the empty destination: {e}"),
            })?;
        } else {
            return Err(LauncherError::Generic {
                code: "ERR_EXPORT_DEST_NOT_EMPTY".into(),
                message: format!(
                    "The export destination '{}' already exists. Choose an empty or new folder.",
                    dest.display()
                ),
            });
        }
    }
    std::fs::rename(staging, dest).map_err(|e| LauncherError::Generic {
        code: "ERR_EXPORT_WRITE".into(),
        message: format!("Failed to finalize export: {e}"),
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn zip_with(path: &Path, entries: &[(&str, &[u8])]) {
        let file = std::fs::File::create(path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        for (name, bytes) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
    }

    fn level_dat(name: &str, last_played: i64) -> Vec<u8> {
        let mut data = std::collections::HashMap::new();
        data.insert("LevelName".to_string(), fastnbt::Value::String(name.into()));
        data.insert("LastPlayed".to_string(), fastnbt::Value::Long(last_played));
        let mut root = std::collections::HashMap::new();
        root.insert("Data".to_string(), fastnbt::Value::Compound(data));
        let bytes = fastnbt::to_bytes(&fastnbt::Value::Compound(root)).unwrap();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, &bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn export(instance: &Path, dest: &Path) -> ExportResult {
        export_server_environment(instance, dest, "fabric", "1.21").unwrap()
    }

    #[test]
    fn export_with_empty_instance_is_explicit_about_nothing_to_export() {
        let tmp = TempDir::new().unwrap();
        let instance = tmp.path().join("instance");
        let dest = tmp.path().join("server");
        std::fs::create_dir_all(&instance).unwrap();

        let result = export(&instance, &dest);
        assert_eq!(result.total_mods, 0);
        assert_eq!(result.server_mods, 0);
        assert!(result.removed_client_only.is_empty());
        assert!(result
            .notes
            .iter()
            .any(|note| note.contains("No mods folder")));
        assert!(result.world.is_none());
        assert!(result.notes.iter().any(|note| note.contains("no world")));
    }

    #[test]
    fn export_with_fake_jars_keeps_them_and_says_their_side_is_unknown() {
        let tmp = TempDir::new().unwrap();
        let instance = tmp.path().join("instance");
        let dest = tmp.path().join("server");
        std::fs::create_dir_all(instance.join("mods")).unwrap();

        std::fs::write(instance.join("mods").join("example.jar"), b"not a real zip").unwrap();
        std::fs::write(instance.join("mods").join("other.jar"), b"also fake").unwrap();

        let result = export(&instance, &dest);
        assert_eq!(result.total_mods, 2);
        assert_eq!(result.server_mods, 2);
        assert!(result.removed_client_only.is_empty());
        assert_eq!(
            result
                .notes
                .iter()
                .filter(|note| note.contains("side could not be read"))
                .count(),
            2
        );
        assert!(dest.join("mods").join("example.jar").exists());
        assert!(dest.join("mods").join("other.jar").exists());
    }

    #[test]
    fn a_fabric_client_mod_is_left_out_and_a_server_mod_is_kept() {
        let tmp = TempDir::new().unwrap();
        let instance = tmp.path().join("instance");
        let dest = tmp.path().join("server");
        std::fs::create_dir_all(instance.join("mods")).unwrap();
        zip_with(
            &instance.join("mods/client.jar"),
            &[(
                "fabric.mod.json",
                br#"{"id":"clientmod","environment":"client"}"#,
            )],
        );
        zip_with(
            &instance.join("mods/server.jar"),
            &[(
                "fabric.mod.json",
                br#"{"id":"servermod","environment":"server"}"#,
            )],
        );
        zip_with(
            &instance.join("mods/both.jar"),
            &[("fabric.mod.json", br#"{"id":"bothmod"}"#)],
        );

        let result = export(&instance, &dest);
        assert_eq!(result.total_mods, 3);
        assert_eq!(result.removed_client_only, vec!["client.jar"]);
        assert_eq!(result.server_mods, 2);
        assert!(!dest.join("mods/client.jar").exists());
        assert!(dest.join("mods/server.jar").is_file());
        assert!(dest.join("mods/both.jar").is_file());
    }

    #[test]
    fn a_quilt_mod_reads_its_minecraft_environment() {
        let tmp = TempDir::new().unwrap();
        let instance = tmp.path().join("instance");
        let dest = tmp.path().join("server");
        std::fs::create_dir_all(instance.join("mods")).unwrap();
        zip_with(
            &instance.join("mods/qui.jar"),
            &[(
                "quilt.mod.json",
                br#"{"schema_version":1,"minecraft":{"environment":"client"}}"#,
            )],
        );
        zip_with(
            &instance.join("mods/fabric-style.jar"),
            &[("fabric.mod.json", br#"{"environment":"server"}"#)],
        );

        let result = export_server_environment(&instance, &dest, "quilt", "1.21").unwrap();
        assert_eq!(result.removed_client_only, vec!["qui.jar"]);
        assert!(dest.join("mods/fabric-style.jar").is_file());
    }

    #[test]
    fn a_forge_mods_dependency_side_does_not_make_the_mod_client_only() {
        let tmp = TempDir::new().unwrap();
        let instance = tmp.path().join("instance");
        let dest = tmp.path().join("server");
        std::fs::create_dir_all(instance.join("mods")).unwrap();
        let mods_toml = br#"
modLoader = "javafml"
[[mods]]
modId = "worldgen"
version = "1.0.0"
[[dependencies.worldgen]]
modId = "jei"
side = "CLIENT"
"#;
        zip_with(
            &instance.join("mods/worldgen.jar"),
            &[("META-INF/mods.toml", mods_toml)],
        );

        let result = export_server_environment(&instance, &dest, "forge", "1.20.1").unwrap();
        assert!(result.removed_client_only.is_empty());
        assert!(dest.join("mods/worldgen.jar").is_file());
    }

    #[test]
    fn a_forge_mods_own_side_is_read_and_its_dependency_side_is_ignored() {
        let tmp = TempDir::new().unwrap();
        let instance = tmp.path().join("instance");
        let dest = tmp.path().join("server");
        std::fs::create_dir_all(instance.join("mods")).unwrap();
        let client = br#"
[[mods]]
modId = "hud"
side = "CLIENT"
[[dependencies.hud]]
modId = "minecraft"
side = "BOTH"
"#;
        zip_with(
            &instance.join("mods/hud.jar"),
            &[("META-INF/mods.toml", client)],
        );
        let server = br#"
[[mods]]
modId = "worldgen"
side = "SERVER"
[[dependencies.worldgen]]
modId = "jei"
side = "CLIENT"
"#;
        zip_with(
            &instance.join("mods/worldgen.jar"),
            &[("META-INF/mods.toml", server)],
        );

        let result = export_server_environment(&instance, &dest, "forge", "1.20.1").unwrap();
        assert_eq!(result.removed_client_only, vec!["hud.jar"]);
        assert!(dest.join("mods/worldgen.jar").is_file());
    }

    #[test]
    fn a_neoforge_mods_toml_is_read() {
        let tmp = TempDir::new().unwrap();
        let instance = tmp.path().join("instance");
        let dest = tmp.path().join("server");
        std::fs::create_dir_all(instance.join("mods")).unwrap();
        let neo = br#"
[[mods]]
modId = "hud"
side = "CLIENT"
"#;
        zip_with(
            &instance.join("mods/neo-client.jar"),
            &[("META-INF/neoforge.mods.toml", neo)],
        );
        let dep_only = br#"
[[mods]]
modId = "worldgen"
[[dependencies.worldgen]]
modId = "jei"
side = "CLIENT"
"#;
        zip_with(
            &instance.join("mods/neo-world.jar"),
            &[("META-INF/neoforge.mods.toml", dep_only)],
        );

        let result = export_server_environment(&instance, &dest, "neoforge", "1.21").unwrap();
        assert_eq!(result.removed_client_only, vec!["neo-client.jar"]);
        assert!(dest.join("mods/neo-world.jar").is_file());
    }

    #[test]
    fn the_most_recently_played_world_is_the_default() {
        let tmp = TempDir::new().unwrap();
        let instance = tmp.path().join("instance");
        let dest = tmp.path().join("server");
        for (folder, name, played) in [("older", "Older", 100), ("newer", "Newer", 900)] {
            let world = instance.join("saves").join(folder);
            std::fs::create_dir_all(&world).unwrap();
            std::fs::write(world.join("level.dat"), level_dat(name, played)).unwrap();
            std::fs::write(world.join("region.mca"), folder.as_bytes()).unwrap();
        }

        let result = export(&instance, &dest);
        assert_eq!(result.world.as_deref(), Some("newer"));
        assert_eq!(
            std::fs::read(dest.join("world/region.mca")).unwrap(),
            b"newer"
        );
    }

    #[test]
    fn a_named_world_is_exported_and_an_unknown_one_is_an_error() {
        let tmp = TempDir::new().unwrap();
        let instance = tmp.path().join("instance");
        let dest = tmp.path().join("server");
        for folder in ["alpha", "beta"] {
            let world = instance.join("saves").join(folder);
            std::fs::create_dir_all(&world).unwrap();
            std::fs::write(world.join("level.dat"), level_dat(folder, 1)).unwrap();
        }
        let options = LocalExportOptions {
            world: Some("alpha".into()),
            ..LocalExportOptions::default()
        };
        let result = export_local(&instance, &dest, "fabric", "1.21", &options).unwrap();
        assert_eq!(result.world.as_deref(), Some("alpha"));
        assert!(dest.join("world/level.dat").is_file());

        let options = LocalExportOptions {
            world: Some("nope".into()),
            ..LocalExportOptions::default()
        };
        let error = export_local(&instance, &dest, "fabric", "1.21", &options).unwrap_err();
        assert!(
            error.to_string().contains("no world named 'nope'"),
            "{error}"
        );
    }

    #[test]
    fn start_scripts_name_the_memory_java_and_launcher() {
        let (sh, bat) = start_scripts(
            &LauncherPlan::LauncherJar {
                file: "fabric-server-launch.jar".into(),
            },
            4096,
            GcProfile::HighEfficiency,
            21,
            "fabric",
        );
        assert!(sh.contains("-Xmx4096M"), "{sh}");
        assert!(sh.contains("-Xms4096M"), "{sh}");
        assert!(sh.contains("-XX:"), "{sh}");
        assert!(sh.contains("fabric-server-launch.jar"), "{sh}");
        assert!(sh.contains("Java 21"), "{sh}");
        assert!(sh.contains("nogui"), "{sh}");
        assert!(bat.contains("-Xmx4096M"), "{bat}");
        assert!(bat.contains("fabric-server-launch.jar"), "{bat}");
        assert!(bat.contains("Java 21"), "{bat}");
        assert!(sh.starts_with("#!/usr/bin/env sh"), "{sh}");
        assert!(bat.starts_with("@echo off"), "{bat}");
    }

    #[test]
    fn a_forge_start_script_installs_once_then_runs_the_generated_script() {
        let (sh, bat) = start_scripts(
            &LauncherPlan::InstallerThenRun {
                installer: "forge-installer.jar".into(),
                args: vec!["--installServer".into()],
                runner: InstallerRunner::RunScript,
            },
            2048,
            GcProfile::HighEfficiency,
            17,
            "forge",
        );
        assert!(sh.contains("--installServer"), "{sh}");
        assert!(sh.contains("run.sh"), "{sh}");
        assert!(bat.contains("--installServer"), "{bat}");
        assert!(bat.contains("run.bat"), "{bat}");

        let tmp = TempDir::new().unwrap();
        let gc_args = generate_args_for_java(GcProfile::HighEfficiency, 2048, "", 17);
        write_jvm_args(tmp.path(), &gc_args).unwrap();
        let text = std::fs::read_to_string(tmp.path().join("user_jvm_args.txt")).unwrap();
        assert!(text.lines().any(|line| line == "-Xmx2048M"), "{text}");
        assert!(text.lines().any(|line| line == "-Xms2048M"), "{text}");
    }

    #[test]
    fn start_scripts_reference_local_scripts_and_jars_by_explicit_path() {
        let (sh, bat) = start_scripts(
            &LauncherPlan::InstallerThenRun {
                installer: "forge-installer.jar".into(),
                args: vec!["--installServer".into()],
                runner: InstallerRunner::RunScript,
            },
            2048,
            GcProfile::HighEfficiency,
            17,
            "forge",
        );
        assert!(
            bat.contains("call \"%~dp0run.bat\" nogui"),
            "run.bat must be called by explicit path: {bat}"
        );
        assert!(
            bat.contains("if not exist \"%~dp0run.bat\""),
            "the run.bat existence check must use an explicit path: {bat}"
        );
        assert!(
            bat.contains("-jar \"%~dp0forge-installer.jar\""),
            "the installer jar must be run by explicit path: {bat}"
        );
        assert!(
            sh.contains("cd \"$(dirname \"$0\")\""),
            "start.sh must cd to its own folder: {sh}"
        );
        assert!(
            sh.contains("exec bash ./run.sh nogui"),
            "run.sh must be called by explicit path: {sh}"
        );
        assert!(
            sh.contains("if [ ! -f ./run.sh ]"),
            "the run.sh existence check must use an explicit path: {sh}"
        );

        let (sh, bat) = start_scripts(
            &LauncherPlan::LauncherJar {
                file: "fabric-server-launch.jar".into(),
            },
            4096,
            GcProfile::HighEfficiency,
            21,
            "fabric",
        );
        assert!(
            bat.contains("-jar \"%~dp0fabric-server-launch.jar\""),
            "the launcher jar must be run by explicit path: {bat}"
        );
        assert!(
            sh.contains("-jar ./fabric-server-launch.jar"),
            "the launcher jar must be run by explicit path: {sh}"
        );
    }

    #[test]
    fn the_eula_is_written_unaccepted_and_never_overwritten() {
        let tmp = TempDir::new().unwrap();
        write_eula(tmp.path()).unwrap();
        let text = std::fs::read_to_string(tmp.path().join("eula.txt")).unwrap();
        assert!(text.lines().any(|line| line == "eula=false"));
        assert!(!text.lines().any(|line| line == "eula=true"));

        std::fs::write(tmp.path().join("eula.txt"), "eula=true\n").unwrap();
        write_eula(tmp.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("eula.txt")).unwrap(),
            "eula=true\n"
        );
    }

    #[test]
    fn the_official_installer_commands_match_the_documented_flags() {
        assert_eq!(
            fabric_installer_server_args("1.21", "0.16.0"),
            vec![
                "server",
                "-dir",
                ".",
                "-mcversion",
                "1.21",
                "-loader",
                "0.16.0",
                "-downloadMinecraft",
            ]
        );
        assert_eq!(
            quilt_installer_server_args("1.21", "0.27.0"),
            vec![
                "install",
                "server",
                "1.21",
                "0.27.0",
                "--install-dir=.",
                "--download-server",
            ]
        );
    }

    #[test]
    fn a_fabric_installer_script_installs_once_then_runs_the_launcher_jar() {
        let (sh, bat) = start_scripts(
            &LauncherPlan::InstallerThenRun {
                installer: "fabric-installer.jar".into(),
                args: fabric_installer_server_args("1.21", "0.16.0"),
                runner: InstallerRunner::LauncherJar {
                    file: "fabric-server-launch.jar".into(),
                },
            },
            4096,
            GcProfile::HighEfficiency,
            21,
            "fabric",
        );
        assert!(
            sh.contains("server -dir . -mcversion 1.21 -loader 0.16.0 -downloadMinecraft"),
            "{sh}"
        );
        assert!(sh.contains("fabric-server-launch.jar"), "{sh}");
        assert!(sh.contains("-Xmx4096M"), "{sh}");
        assert!(sh.contains("if [ ! -f ./fabric-server-launch.jar ]"), "{sh}");
        assert!(bat.contains("-mcversion 1.21"), "{bat}");
        assert!(bat.contains("fabric-server-launch.jar"), "{bat}");
        assert!(!sh.contains("user_jvm_args.txt"), "{sh}");
    }

    #[test]
    fn maven_metadata_names_the_release_then_latest_then_last_version() {
        let xml = "<metadata><versioning><latest>9.9</latest><release>1.1.2</release>\
                   <versions><version>1.0.0</version><version>1.1.2</version></versions>\
                   </versioning></metadata>";
        assert_eq!(parse_maven_metadata_latest(xml).unwrap(), "1.1.2");

        let xml = "<metadata><versioning><latest>0.15.1</latest>\
                   <versions><version>0.14.1</version><version>0.15.1</version></versions>\
                   </versioning></metadata>";
        assert_eq!(parse_maven_metadata_latest(xml).unwrap(), "0.15.1");

        let xml = "<metadata><versioning><versions><version>0.14.1</version>\
                   <version>0.15.1</version></versions></versioning></metadata>";
        assert_eq!(parse_maven_metadata_latest(xml).unwrap(), "0.15.1");

        assert!(parse_maven_metadata_latest("<metadata/>").is_err());
    }

    #[test]
    fn the_published_sha512_is_parsed_and_checked() {
        let digest = core_api::download::sha512_hex(b"installer bytes");
        assert_eq!(
            parse_published_hash(&format!("{digest}  fabric-installer.jar\n")).unwrap(),
            digest
        );
        assert_eq!(
            parse_published_hash(&format!("sha512:{digest}")).unwrap(),
            digest
        );

        let verified = verify_published_hash(
            b"installer bytes",
            Some(&digest),
            "fabric-installer.jar",
            false,
        )
        .unwrap();
        assert!(verified);

        assert!(matches!(
            verify_published_hash(b"other bytes", Some(&digest), "fabric-installer.jar", false),
            Err(LauncherError::HashMismatch)
        ));
    }

    #[test]
    fn a_missing_hash_file_refuses_unless_unverified_downloads_are_allowed() {
        let error = verify_published_hash(b"installer bytes", None, "fabric-installer.jar", false)
            .unwrap_err();
        assert!(error.to_string().contains("No .sha512"), "{error}");
        assert!(
            !verify_published_hash(b"installer bytes", None, "fabric-installer.jar", true).unwrap()
        );
    }

    fn loader_entry(file_type: &str, source_url: &str, sha256: &str) -> LoaderEntry {
        LoaderEntry {
            mc_version: "1.21".into(),
            loader_version: "0.16.0".into(),
            source_url: source_url.into(),
            sha256: sha256.into(),
            file_name: "fabric-loader-0.16.0-1.21.json".into(),
            file_type: file_type.into(),
            version_json_sha256: None,
            installer_spec: None,
            provided_versions: Default::default(),
            release_channel: Default::default(),
            recommendation_rank: None,
        }
    }

    #[test]
    fn an_unknown_loader_is_refused_and_a_missing_pin_is_named() {
        let error = preflight_loader("sand", "1.21", "").unwrap_err();
        assert!(error.to_string().contains("no server export"), "{error}");

        let error = preflight_loader("forge", "1.21", "0.0.0").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("not in the pinned loader catalog"),
            "{error}"
        );
    }

    #[test]
    fn a_pinned_loader_artifact_is_checked_against_its_hash() {
        let bytes = br#"{"id":"fabric-loader-0.16.0-1.21","libraries":[]}"#;
        let entry = loader_entry(
            "profile_json",
            "https://meta.example.test/profile/json",
            "ignored",
        );
        let expected = crate::loader_download::compute_loader_hash(
            "fabric",
            &entry.file_name,
            &entry.file_type,
            bytes,
        );
        let entry = loader_entry(
            "profile_json",
            &entry.source_url,
            &format!("sha256:{expected}"),
        );
        verify_loader_artifact("fabric", &entry, bytes).unwrap();

        let tampered = br#"{"id":"fabric-loader-9.9.9-9.99","libraries":[]}"#;
        assert!(matches!(
            verify_loader_artifact("fabric", &entry, tampered),
            Err(LauncherError::HashMismatch)
        ));
    }

    #[test]
    fn a_loader_entry_without_a_hash_cannot_be_verified() {
        let entry = loader_entry(
            "installer_jar",
            "https://example.test/forge-installer.jar",
            "",
        );
        let error = verify_loader_artifact("forge", &entry, b"bytes").unwrap_err();
        assert!(error.to_string().contains("no published hash"), "{error}");
    }

    #[test]
    fn export_refuses_when_destination_is_the_instance() {
        let tmp = TempDir::new().unwrap();
        let instance = tmp.path().join("instance");
        std::fs::create_dir_all(instance.join("mods")).unwrap();
        std::fs::write(instance.join("mods").join("mod.jar"), b"mod-bytes").unwrap();

        let err = export_server_environment(&instance, &instance, "fabric", "1.21").unwrap_err();
        assert!(err.to_string().contains("overlaps"), "{err}");
        assert_eq!(
            std::fs::read(instance.join("mods").join("mod.jar")).unwrap(),
            b"mod-bytes"
        );
    }

    #[test]
    fn export_refuses_destination_inside_config_and_inside_world() {
        let tmp = TempDir::new().unwrap();
        let instance = tmp.path().join("instance");
        std::fs::create_dir_all(instance.join("config")).unwrap();
        std::fs::create_dir_all(instance.join("world")).unwrap();

        let dest = instance.join("config").join("out");
        let err = export_server_environment(&instance, &dest, "fabric", "1.21").unwrap_err();
        assert!(err.to_string().contains("overlaps"), "{err}");

        let dest = instance.join("world").join("out");
        let err = export_server_environment(&instance, &dest, "fabric", "1.21").unwrap_err();
        assert!(err.to_string().contains("overlaps"), "{err}");
    }

    #[test]
    fn export_refuses_destination_that_is_a_parent_of_the_instance() {
        let tmp = TempDir::new().unwrap();
        let instance = tmp.path().join("instance");
        std::fs::create_dir_all(&instance).unwrap();

        let err = export_server_environment(&instance, tmp.path(), "fabric", "1.21").unwrap_err();
        assert!(err.to_string().contains("overlaps"), "{err}");
    }

    #[cfg(windows)]
    #[test]
    fn export_refuses_a_junction_pointing_into_the_instance() {
        let tmp = TempDir::new().unwrap();
        let instance = tmp.path().join("instance");
        std::fs::create_dir_all(&instance).unwrap();
        let link = tmp.path().join("junction");
        junction::create(&instance, &link).unwrap();

        let err = export_server_environment(&instance, &link, "fabric", "1.21").unwrap_err();
        assert!(err.to_string().contains("overlaps"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn export_refuses_a_symlink_pointing_into_the_instance() {
        let tmp = TempDir::new().unwrap();
        let instance = tmp.path().join("instance");
        std::fs::create_dir_all(&instance).unwrap();
        let link = tmp.path().join("symlink");
        std::os::unix::fs::symlink(&instance, &link).unwrap();

        let err = export_server_environment(&instance, &link, "fabric", "1.21").unwrap_err();
        assert!(err.to_string().contains("overlaps"), "{err}");
    }

    #[test]
    fn export_refuses_a_non_empty_destination() {
        let tmp = TempDir::new().unwrap();
        let instance = tmp.path().join("instance");
        std::fs::create_dir_all(&instance).unwrap();
        let dest = tmp.path().join("server");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("keep.txt"), b"untouched").unwrap();

        let err = export_server_environment(&instance, &dest, "fabric", "1.21").unwrap_err();
        assert!(err.to_string().contains("already contains"), "{err}");
        assert_eq!(std::fs::read(dest.join("keep.txt")).unwrap(), b"untouched");
    }

    #[test]
    fn export_fails_without_leaving_a_destination_or_staging() {
        let tmp = TempDir::new().unwrap();
        let instance = tmp.path().join("instance");
        std::fs::create_dir_all(&instance).unwrap();
        // `mods` is a file, not a directory: export fails after staging is created.
        std::fs::write(instance.join("mods"), b"not a dir").unwrap();

        let dest = tmp.path().join("server");
        let err = export_server_environment(&instance, &dest, "fabric", "1.21").unwrap_err();
        let _ = err;
        assert!(
            !dest.exists(),
            "a failed export must not leave a destination"
        );
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("staging"))
            .collect();
        assert!(leftovers.is_empty(), "staging left behind: {leftovers:?}");
    }

    #[tokio::test]
    async fn export_denied_when_loader_switch_is_off() {
        let tmp = TempDir::new().unwrap();
        let ctx = agora_core::ctx::Ctx::for_testing(tmp.path().to_path_buf());
        let instance = ctx.paths.instance_dir("instance").unwrap();
        std::fs::create_dir_all(instance.join("mods")).unwrap();
        let manifest = serde_json::json!({
            "manifest_version": 3,
            "game_data": {},
            "pack_origin": null,
            "instance_id": "instance",
            "name": "Test",
            "created_from_pack": null,
            "minecraft_version": "1.21",
            "loader": "fabric",
            "loader_version": "",
            "is_locked": false,
            "mods": [],
            "resourcepacks": [],
            "shaders": [],
            "datapacks": [],
            "worlds": [],
            "user_preferences": {}
        });
        std::fs::write(
            ctx.paths.instance_manifest("instance").unwrap(),
            manifest.to_string(),
        )
        .unwrap();
        agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
        {
            let conn = agora_core::db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
            agora_core::db::set_setting(&conn, "network_loader_enabled", &serde_json::json!(false))
                .unwrap();
        }
        agora_core::network_gate::install(std::sync::Arc::new(
            agora_core::network_gate::SettingsGate::new(ctx.paths.local_state_db()),
        ));

        let dest = tmp.path().join("server");
        let err = export_ready_server(&ctx, "instance", &dest, None, false)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("loader") || err.to_string().contains("Loader"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn export_denied_when_mojang_switch_is_off() {
        let tmp = TempDir::new().unwrap();
        let ctx = agora_core::ctx::Ctx::for_testing(tmp.path().to_path_buf());
        let instance = ctx.paths.instance_dir("instance").unwrap();
        std::fs::create_dir_all(&instance).unwrap();
        let manifest = serde_json::json!({
            "manifest_version": 3,
            "game_data": {},
            "pack_origin": null,
            "instance_id": "instance",
            "name": "Test",
            "created_from_pack": null,
            "minecraft_version": "1.21",
            "loader": "vanilla",
            "loader_version": "",
            "is_locked": false,
            "mods": [],
            "resourcepacks": [],
            "shaders": [],
            "datapacks": [],
            "worlds": [],
            "user_preferences": {}
        });
        std::fs::write(
            ctx.paths.instance_manifest("instance").unwrap(),
            manifest.to_string(),
        )
        .unwrap();
        agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
        {
            let conn = agora_core::db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
            agora_core::db::set_setting(
                &conn,
                "network_mojang_metadata_enabled",
                &serde_json::json!(false),
            )
            .unwrap();
        }
        agora_core::network_gate::install(std::sync::Arc::new(
            agora_core::network_gate::SettingsGate::new(ctx.paths.local_state_db()),
        ));

        let dest = tmp.path().join("server");
        let err = export_ready_server(&ctx, "instance", &dest, None, false)
            .await
            .unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("mojang")
                || err.to_string().to_lowercase().contains("metadata"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn export_denied_when_database_unreadable() {
        let tmp = TempDir::new().unwrap();
        let ctx = agora_core::ctx::Ctx::for_testing(tmp.path().to_path_buf());
        let instance = ctx.paths.instance_dir("instance").unwrap();
        std::fs::create_dir_all(&instance).unwrap();
        let manifest = serde_json::json!({
            "manifest_version": 3,
            "game_data": {},
            "pack_origin": null,
            "instance_id": "instance",
            "name": "Test",
            "created_from_pack": null,
            "minecraft_version": "1.21",
            "loader": "fabric",
            "loader_version": "",
            "is_locked": false,
            "mods": [],
            "resourcepacks": [],
            "shaders": [],
            "datapacks": [],
            "worlds": [],
            "user_preferences": {}
        });
        std::fs::write(
            ctx.paths.instance_manifest("instance").unwrap(),
            manifest.to_string(),
        )
        .unwrap();
        // Make the db path a directory so opening it fails.
        if let Some(parent) = ctx.paths.local_state_db().parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::create_dir_all(ctx.paths.local_state_db()).unwrap();

        let dest = tmp.path().join("server");
        let err = export_ready_server(&ctx, "instance", &dest, None, false)
            .await
            .unwrap_err();
        assert!(!err.to_string().is_empty());
    }
}
