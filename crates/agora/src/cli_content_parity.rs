//! Commands that give the CLI what the desktop's crash doctor, pack, mod, provider, registry and
//! instance screens can do. Each one is a thin adapter over the same core call the desktop makes.

use std::path::{Path, PathBuf};

use agora_core::ctx::Ctx;
use agora_core::providers::ProviderRegistry;
use clap::Subcommand;

use crate::{print_table, OutputFormat};

/// The Minecraft instance system, which the crash engine asks before a generic game instance.
fn minecraft_instances() -> agora_game_minecraft::installed_content::MinecraftInstanceProvider {
    agora_game_minecraft::installed_content::MinecraftInstanceProvider
}

fn print_json<T: serde::Serialize>(value: &T) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn require_instance(ctx: &Ctx, instance: &str) -> anyhow::Result<PathBuf> {
    let dir = ctx.paths.instance_dir(instance)?;
    if !dir.exists() {
        anyhow::bail!("Instance '{instance}' not found");
    }
    Ok(dir)
}

/// The instance's Minecraft version and loader, for commands that filter by them.
fn instance_version_and_loader(ctx: &Ctx, instance: &str) -> anyhow::Result<(String, String)> {
    let detail = agora_game_minecraft::instance_service::InstanceService::new(ctx.clone())
        .get(instance)?
        .ok_or_else(|| anyhow::anyhow!("Instance '{instance}' not found"))?;
    Ok((detail.row.minecraft_version, detail.row.loader))
}

/// The content providers the desktop uses: Agora's own and any enabled plugin's.
fn provider_registry(ctx: &Ctx) -> anyhow::Result<ProviderRegistry> {
    let service = crate::plugin_service(ctx);
    service.reload()?;
    let plugins = service.plugin_code_allowed().then_some(&service);
    Ok(ProviderRegistry::new(ctx, plugins))
}

// ---------------------------------------------------------------------------
// crash
// ---------------------------------------------------------------------------

#[derive(Subcommand)]
pub enum CrashParityCmd {
    /// Tell the crash doctor that turning a suspect off fixed the crash. A later crash of the same
    /// kind then counts that suspect a little more strongly.
    Fixed {
        /// The crash's bucket: `record.bucket` in `agora --json crash last <INSTANCE>`.
        bucket: String,
        /// The suspect's owner key, for example `item:...`.
        owner_key: String,
    },
    /// Say the game still crashes without a suspect, and list the next suspects to try.
    #[command(name = "still-crashing")]
    StillCrashing {
        instance: String,
        /// The owner key of the suspect that did not fix it.
        ruled_out_owner_key: String,
    },
    /// Print the installed file a crash suspect provides, so you can turn it off to test.
    #[command(name = "suspect-file")]
    SuspectFile { instance: String, owner_key: String },
    /// Build a shareable crash report (redacted) to paste into an assistant or the Discord.
    /// Agora does not send it anywhere.
    Report {
        instance: String,
        #[arg(long, help = "A crash log to include, as if pasted into Crash Doctor")]
        log: Option<PathBuf>,
        #[arg(
            long,
            short = 'o',
            help = "Write the report to this file (default: print it)"
        )]
        out: Option<PathBuf>,
    },
}

pub fn run_crash(
    action: CrashParityCmd,
    ctx: &Ctx,
    output_fmt: OutputFormat,
) -> anyhow::Result<()> {
    let json = output_fmt.is_json_output();
    match action {
        CrashParityCmd::Fixed { bucket, owner_key } => {
            let conn = agora_core::db::local_state_connection(&ctx.paths.local_state_db())?;
            agora_core::db::increment_confirmation(&conn, &bucket, &owner_key)?;
            if json {
                print_json(&serde_json::json!({
                    "status": "confirmed", "bucket": bucket, "ownerKey": owner_key
                }))?;
            } else {
                println!(
                    "Thanks. Agora will remember that turning off {owner_key} fixed this crash."
                );
            }
        }
        CrashParityCmd::StillCrashing {
            instance,
            ruled_out_owner_key,
        } => {
            let instances = minecraft_instances();
            let result = agora_core::crash_live::report_still_crashing(
                ctx,
                &instance,
                &ruled_out_owner_key,
                Some(&instances),
            )?;
            if json {
                return print_json(&result);
            }
            println!("Ruled out {ruled_out_owner_key}.");
            if result.suspects.is_empty() {
                println!("There are no more suspects to try for '{instance}'.");
                return Ok(());
            }
            println!("Next suspects, most likely first:");
            let rows: Vec<Vec<String>> = result
                .suspects
                .iter()
                .map(|suspect| {
                    vec![
                        suspect.filename.clone(),
                        suspect.mod_id.clone(),
                        format!("{}", suspect.total_score),
                    ]
                })
                .collect();
            print_table(&["File", "Owner key", "Score"], &rows);
            println!(
                "Turn the first one off with: agora mod disable {instance} {}",
                result.suspects[0].filename
            );
        }
        CrashParityCmd::SuspectFile {
            instance,
            owner_key,
        } => {
            let instances = minecraft_instances();
            let file = agora_core::crash_live::resolve_suspect_file(
                ctx,
                &instance,
                &owner_key,
                Some(&instances),
            )?;
            if json {
                print_json(&serde_json::json!({ "instanceId": instance, "file": file }))?;
            } else {
                println!("{file}");
            }
        }
        CrashParityCmd::Report { instance, log, out } => {
            let crash_log = match &log {
                Some(path) => Some(std::fs::read_to_string(path).map_err(|error| {
                    anyhow::anyhow!("Could not read crash log '{}': {error}", path.display())
                })?),
                None => None,
            };
            let answer = agora_core::crash_live::latest_analyzed_answer(ctx, &instance)?
                .map(|answer| answer_text(&answer));
            let context = agora_core::crash_export::CrashReportContext {
                instance_id: Some(instance.clone()),
                crash_log,
                answer,
                suspects: None,
            };
            let report =
                agora_core::crash_export::build_instance_crash_report(&ctx.paths, &context);
            match &out {
                Some(path) => {
                    std::fs::write(path, &report).map_err(|error| {
                        anyhow::anyhow!("Could not write '{}': {error}", path.display())
                    })?;
                    if json {
                        print_json(&serde_json::json!({
                            "instanceId": instance, "out": path.display().to_string()
                        }))?;
                    } else {
                        println!("Wrote the crash report to {}.", path.display());
                    }
                }
                None if json => print_json(&serde_json::json!({
                    "instanceId": instance, "report": report
                }))?,
                None => println!("{report}"),
            }
        }
    }
    Ok(())
}

/// The crash answer as plain text, the way the desktop puts it into a shared report.
fn answer_text(answer: &agora_game_api::crash::CrashAnswer) -> String {
    let mut lines = vec![
        crate::cli_crash::english(&answer.headline).to_string(),
        format!("How sure: {}", answer.level.label()),
    ];
    if let Some(cause) = answer.cause {
        lines.push(format!("Likely cause: {}", cause.label()));
    }
    if !answer.candidates.is_empty() {
        lines.push("Suspects:".to_string());
        for candidate in &answer.candidates {
            lines.push(format!("- {}", crate::cli_crash::suspect_line(candidate)));
        }
    }
    for note in &answer.trust_notes {
        lines.push(format!("Note: {}", crate::cli_crash::english(&note.text)));
    }
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// snapshots
// ---------------------------------------------------------------------------

#[derive(Subcommand)]
pub enum SnapshotsParityCmd {
    /// Show what changed in an instance since a snapshot was taken. Changes nothing.
    Drift {
        instance: String,
        snapshot_id: String,
    },
}

pub fn run_snapshots(
    action: SnapshotsParityCmd,
    ctx: &Ctx,
    output_fmt: OutputFormat,
) -> anyhow::Result<()> {
    let json = output_fmt.is_json_output();
    match action {
        SnapshotsParityCmd::Drift {
            instance,
            snapshot_id,
        } => {
            let dir = require_instance(ctx, &instance)?;
            let reference = agora_core::snapshot::snapshot_file_index(&dir, &snapshot_id)
                .map_err(|error| anyhow::anyhow!(error))?;
            let scope = agora_core::snapshot::restore_scope(&dir, &snapshot_id)
                .map_err(|error| anyhow::anyhow!(error))?;
            let scope_refs: Vec<&str> = scope.iter().map(String::as_str).collect();
            let current = agora_core::snapshot::live_file_index(&dir, &scope_refs)
                .map_err(|error| anyhow::anyhow!(error))?;
            let diff = agora_core::lkg::compute_diff(&reference, &current, Some(snapshot_id), None);
            if json {
                return print_json(&diff);
            }
            if diff.added.is_empty() && diff.removed.is_empty() && diff.modified.is_empty() {
                println!(
                    "Nothing changed since that snapshot ({} files).",
                    diff.unchanged_count
                );
                return Ok(());
            }
            for (title, entries) in [
                ("Added", &diff.added),
                ("Removed", &diff.removed),
                ("Changed", &diff.modified),
            ] {
                if !entries.is_empty() {
                    println!("{title}:");
                    for entry in entries {
                        println!("  {}", entry.path);
                    }
                }
            }
            println!("{} file(s) unchanged.", diff.unchanged_count);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// pack
// ---------------------------------------------------------------------------

#[derive(Subcommand)]
pub enum PackParityCmd {
    /// Throw away the copy `pack review` staged, when you decide not to import it.
    Discard {
        /// The review id `agora pack review` printed.
        review_id: String,
    },
    /// Try an Agora pack's mods again on the instance its import made. Mods already installed
    /// are skipped.
    Retry {
        instance: String,
        /// The .agora-pack.json file the instance was imported from.
        pack: PathBuf,
        #[arg(
            long = "entry",
            value_name = "FILENAME",
            help = "Only this mod (repeatable; default: every mod in the pack)"
        )]
        entries: Vec<String>,
        #[arg(
            long,
            help = "Let just these mods come from Modrinth, without turning Modrinth on"
        )]
        allow_modrinth_once: bool,
        #[arg(
            long,
            help = "Install mods that differ from the hash the pack lists, after checking them yourself"
        )]
        install_anyway: bool,
    },
    /// List the mods in a catalog pack.
    Mods {
        /// Catalog pack ID.
        pack: String,
    },
    /// Extract a pack's overrides zip into an instance, with the same safety checks an import
    /// uses (allowed folders only, size limits, no path tricks).
    #[command(name = "extract-overrides")]
    ExtractOverrides {
        instance: String,
        zip: PathBuf,
        /// Extract the entries that can be written and list the ones left out, instead of refusing
        /// the whole archive. An entry whose name a disk cannot hold (a Windows reserved name, a
        /// trailing dot or space, a case collision) stops the extraction by default, and the refusal
        /// names every offending entry and why.
        #[arg(long)]
        without_unwritable_names: bool,
    },
    /// Install a catalog entry that pins one version of a content provider's modpack, as a new
    /// instance.
    #[command(name = "install-catalog")]
    InstallCatalog {
        /// Catalog item ID.
        item: String,
        #[arg(long, help = "Name for the new instance")]
        name: Option<String>,
        #[arg(
            long,
            help = "Install although the provider's pack no longer matches what the catalog pinned"
        )]
        accept_changed: bool,
        /// Install provider files that publish no integrity information, for this command only.
        #[arg(long)]
        allow_unverified: bool,
    },
}

pub async fn run_pack(
    action: PackParityCmd,
    ctx: &Ctx,
    output_fmt: OutputFormat,
) -> anyhow::Result<()> {
    let json = output_fmt.is_json_output();
    match action {
        PackParityCmd::Discard { review_id } => {
            let review_id = review_id.trim().to_ascii_lowercase();
            agora_game_minecraft::import_service::ImportService::new(ctx.clone())
                .discard_reviewed_copy(&review_id);
            if json {
                print_json(&serde_json::json!({ "status": "discarded", "reviewId": review_id }))?;
            } else {
                println!("Discarded the staged pack {review_id}.");
            }
        }
        PackParityCmd::Retry {
            instance,
            pack,
            entries,
            allow_modrinth_once,
            install_anyway,
        } => {
            require_instance(ctx, &instance)?;
            let chosen = pack_entries(&pack, &entries)?;
            let report = agora_game_minecraft::agora_pack_import::retry_agora_pack_entries(
                ctx,
                &instance,
                chosen,
                allow_modrinth_once,
                install_anyway,
            )
            .await?;
            if json {
                return print_json(&report);
            }
            println!(
                "{} of {} mod(s) installed in '{instance}'.",
                report.installed, report.total
            );
            for outcome in &report.outcomes {
                use agora_game_minecraft::agora_pack_import::PackEntryStatus;
                let reason = outcome.reason.as_deref().unwrap_or("");
                match outcome.status {
                    PackEntryStatus::Installed => {}
                    PackEntryStatus::Failed => {
                        println!("  Not installed: {}: {reason}", outcome.name)
                    }
                    PackEntryStatus::NeedsAction => {
                        println!("  Needs action, not installed: {}: {reason}", outcome.name)
                    }
                }
            }
            for warning in &report.warnings {
                println!("  Note: {warning}");
            }
        }
        PackParityCmd::Mods { pack } => {
            let rows = agora_core::registry::RegistryService::new(ctx.clone())
                .pack_mods_for_pack(&pack)?;
            if json {
                return print_json(&rows);
            }
            if rows.is_empty() {
                println!("Catalog pack '{pack}' lists no mods.");
                return Ok(());
            }
            let table: Vec<Vec<String>> = rows
                .iter()
                .map(|row| {
                    vec![
                        row.mod_id.clone(),
                        row.version.clone().unwrap_or_default(),
                        row.source.clone(),
                        row.status.clone(),
                    ]
                })
                .collect();
            print_table(&["Mod", "Version", "Source", "Status"], &table);
        }
        PackParityCmd::ExtractOverrides {
            instance,
            zip,
            without_unwritable_names,
        } => {
            let dest = require_instance(ctx, &instance)?;
            let names = if without_unwritable_names {
                agora_game_api::NamePolicy::WithoutThem
            } else {
                agora_game_api::NamePolicy::Refuse
            };
            let result = agora_game_minecraft::override_sanitizer::extract_overrides_with_inputs(
                &zip,
                &dest,
                agora_game_minecraft::override_sanitizer::OverridePolicy::from_settings(ctx),
                names,
            )?;
            if json {
                return print_json(&result);
            }
            println!(
                "Extracted {} file(s) into '{instance}' ({} bytes).",
                result.extracted.len(),
                result.total_bytes_written
            );
            if !result.left_out.is_empty() {
                println!("Left out:");
                for entry in &result.left_out {
                    println!("  {} - {}", entry.path, entry.reason);
                }
            }
        }
        PackParityCmd::InstallCatalog {
            item,
            name,
            accept_changed,
            allow_unverified,
        } => {
            let registry = provider_registry(ctx)?;
            let result = agora_game_minecraft::providers::install::install_catalog_pack(
                ctx,
                &registry,
                &item,
                accept_changed,
                name.as_deref(),
                allow_unverified,
            )
            .await
            .map_err(|error| {
                if error.code() == "ERR_PROVIDER_PACK_CHANGED" {
                    anyhow::anyhow!(
                        "{error} Re-run with --accept-changed to install it as it is now."
                    )
                } else {
                    crate::cli_govniche::low_security_hint(error)
                }
            })?;
            crate::cli_govniche::print_import_result(
                &result,
                json,
                &format!("catalog pack '{item}'"),
            )?;
        }
    }
    Ok(())
}

/// The entries of an Agora pack file to retry: the named ones, or all of them.
fn pack_entries(
    pack: &Path,
    names: &[String],
) -> anyhow::Result<Vec<agora_game_minecraft::agora_pack_import::AgoraPackEntry>> {
    use agora_game_minecraft::agora_pack_import::{
        parse_agora_pack, PackEntryParse, MAX_PACK_FILE_BYTES,
    };
    let size = std::fs::metadata(pack)
        .map_err(|error| anyhow::anyhow!("Could not read '{}': {error}", pack.display()))?
        .len();
    if size > MAX_PACK_FILE_BYTES {
        return Err(agora_game_minecraft::agora_pack_import::pack_too_big_error().into());
    }
    let text = std::fs::read_to_string(pack)
        .map_err(|error| anyhow::anyhow!("Could not read '{}': {error}", pack.display()))?;
    let parsed = parse_agora_pack(&text)?;
    let all: Vec<_> = parsed
        .mods
        .into_iter()
        .filter_map(|entry| match entry {
            PackEntryParse::Entry(entry) => Some(entry),
            PackEntryParse::Invalid { .. } => None,
        })
        .collect();
    if names.is_empty() {
        return Ok(all);
    }
    let missing: Vec<&String> = names
        .iter()
        .filter(|name| !all.iter().any(|entry| &entry.filename == *name))
        .collect();
    if !missing.is_empty() {
        anyhow::bail!(
            "The pack has no mod named: {}",
            missing
                .iter()
                .map(|name| name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(all
        .into_iter()
        .filter(|entry| names.contains(&entry.filename))
        .collect())
}

// ---------------------------------------------------------------------------
// mod
// ---------------------------------------------------------------------------

#[derive(Subcommand)]
pub enum ModsParityCmd {
    /// Add an item that is already in the content store to an instance. Needs no network.
    #[command(name = "add-stored")]
    AddStored {
        instance: String,
        /// The stored item's ID, as `agora games content list` shows it.
        item: String,
    },
    /// List the versions of a catalog item (or, with --modrinth, of a Modrinth project).
    Versions {
        /// Catalog item ID, or a Modrinth project ID with --modrinth.
        item: String,
        #[arg(
            long,
            help = "Only versions that fit this instance's Minecraft version and loader"
        )]
        instance: Option<String>,
        #[arg(
            long,
            help = "Read ITEM as a Modrinth project ID and ask Modrinth directly"
        )]
        modrinth: bool,
        #[arg(
            long,
            requires = "modrinth",
            help = "Modrinth project type (mod, resourcepack, shader, datapack)"
        )]
        project_type: Option<String>,
        #[arg(long, default_value_t = 25, help = "How many versions to show")]
        limit: usize,
        #[arg(long, default_value_t = 0, help = "Skip this many versions first")]
        offset: usize,
    },
}

pub async fn run_mods(
    action: ModsParityCmd,
    ctx: &Ctx,
    output_fmt: OutputFormat,
) -> anyhow::Result<()> {
    let json = output_fmt.is_json_output();
    match action {
        ModsParityCmd::AddStored { instance, item } => {
            let installed = agora_game_minecraft::install_service::InstallService::new(ctx.clone())
                .install_stored_item(&instance, &item)?;
            if json {
                return print_json(&installed);
            }
            println!("Added {} to '{instance}'.", installed.filename);
        }
        ModsParityCmd::Versions {
            item,
            instance,
            modrinth: true,
            project_type,
            limit,
            offset,
        } => {
            let versions = agora_game_minecraft::modrinth::ModrinthService::new(ctx.clone())
                .list_raw_modrinth_versions(instance.as_deref(), &item, project_type.as_deref())
                .await?;
            let total = versions.len();
            let page: Vec<_> = versions.into_iter().skip(offset).take(limit).collect();
            if json {
                return print_json(&serde_json::json!({
                    "items": page, "total": total, "hasMore": offset + limit < total
                }));
            }
            let rows: Vec<Vec<String>> = page
                .iter()
                .map(|version| {
                    vec![
                        version.version.clone(),
                        version.version_id.clone(),
                        version.mc_versions.join(", "),
                        version.loaders.join(", "),
                        version.release_date.clone().unwrap_or_default(),
                    ]
                })
                .collect();
            print_versions_footer(
                &["Version", "Version ID", "Minecraft", "Loaders", "Released"],
                &rows,
                offset,
                total,
            );
        }
        ModsParityCmd::Versions {
            item,
            instance,
            modrinth: false,
            limit,
            offset,
            ..
        } => {
            let (mc_version, loader) = match &instance {
                Some(id) => instance_version_and_loader(ctx, id)?,
                None => (String::new(), String::new()),
            };
            let registry_item = agora_core::registry::RegistryService::new(ctx.clone())
                .get_item_by_id(&item)?
                .ok_or_else(|| anyhow::anyhow!("Catalog item '{item}' not found"))?;
            let versions = agora_game_minecraft::resolver::Resolver::new(ctx.clone())
                .list_curated_versions(&registry_item, &mc_version, &loader)
                .await?;
            let total = versions.len();
            let page: Vec<_> = versions.into_iter().skip(offset).take(limit).collect();
            if json {
                return print_json(&serde_json::json!({
                    "items": page, "total": total, "hasMore": offset + limit < total
                }));
            }
            let rows: Vec<Vec<String>> = page
                .iter()
                .map(|version| {
                    vec![
                        version.version.clone(),
                        version.mc_version.clone().unwrap_or_default(),
                        version.loader.clone().unwrap_or_default(),
                        if version.is_compatible { "yes" } else { "no" }.to_string(),
                        version.release_date.clone().unwrap_or_default(),
                    ]
                })
                .collect();
            print_versions_footer(
                &["Version", "Minecraft", "Loader", "Fits", "Released"],
                &rows,
                offset,
                total,
            );
        }
    }
    Ok(())
}

fn print_versions_footer(columns: &[&str], rows: &[Vec<String>], offset: usize, total: usize) {
    if rows.is_empty() {
        println!("No versions to show ({total} in all).");
        return;
    }
    print_table(columns, rows);
    let shown_to = offset + rows.len();
    if shown_to < total {
        println!(
            "Showing {}-{shown_to} of {total}. Use --offset {shown_to} for more.",
            offset + 1
        );
    }
}

// ---------------------------------------------------------------------------
// loadout
// ---------------------------------------------------------------------------

#[derive(Subcommand)]
pub enum LoadoutParityCmd {
    /// List the mods that are on but that a loadout does not mention. `loadout apply --others`
    /// decides what happens to them.
    Unlisted { instance: String, name: String },
}

pub fn run_loadout(
    action: LoadoutParityCmd,
    ctx: &Ctx,
    output_fmt: OutputFormat,
) -> anyhow::Result<()> {
    let json = output_fmt.is_json_output();
    match action {
        LoadoutParityCmd::Unlisted { instance, name } => {
            require_instance(ctx, &instance)?;
            // A game profile records every content layer, so it leaves nothing unlisted.
            let unlisted = if agora_core::game_instance::get(ctx, &instance)?.is_some() {
                Vec::new()
            } else {
                agora_game_minecraft::loadout_service::LoadoutService::new(ctx.clone())
                    .unlisted_mods(&instance, &name)?
            };
            if json {
                return print_json(&unlisted);
            }
            if unlisted.is_empty() {
                println!("Loadout '{name}' mentions every mod that is on.");
            } else {
                println!("On, but not in loadout '{name}':");
                for file in &unlisted {
                    println!("  {file}");
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// instance
// ---------------------------------------------------------------------------

#[derive(Subcommand)]
pub enum InstanceParityCmd {
    /// Move an instance to another Minecraft version, swapping each mod for a build that fits.
    /// Takes a snapshot first and rolls back if anything fails.
    Migrate {
        instance: String,
        /// The Minecraft version to move to.
        mc_version: String,
        #[arg(long, help = "Show the plan without changing anything")]
        dry_run: bool,
        #[arg(
            long,
            help = "Move even though the plan leaves some mods at their current version"
        )]
        accept_blockers: bool,
    },
    /// Show an instance's recent launches and how they went.
    History { instance: String },
    /// Stop the game an Agora launch is running for this instance.
    Stop { instance: String },
    /// List Minecraft snapshot and pre-release versions you can make a vanilla instance on.
    #[command(name = "snapshot-versions")]
    SnapshotVersions,
}

pub async fn run_instance(
    action: InstanceParityCmd,
    ctx: &Ctx,
    output_fmt: OutputFormat,
) -> anyhow::Result<()> {
    let json = output_fmt.is_json_output();
    match action {
        InstanceParityCmd::Migrate {
            instance,
            mc_version,
            dry_run,
            accept_blockers,
        } => migrate(ctx, &instance, &mc_version, dry_run, accept_blockers, json).await?,
        InstanceParityCmd::History { instance } => {
            require_instance(ctx, &instance)?;
            let conn = agora_core::db::local_state_connection(&ctx.paths.local_state_db())?;
            let records = agora_core::launch_history::list_history(
                &conn,
                &agora_core::paths::sanitize_id(&instance),
                agora_core::launch_history::HISTORY_LIMIT,
            )?;
            let stats = agora_core::launch_history::summarize(&records);
            if json {
                return print_json(&serde_json::json!({ "records": records, "stats": stats }));
            }
            if records.is_empty() {
                println!("'{instance}' has not been launched yet.");
                return Ok(());
            }
            println!("{} run(s), {} crash(es).", stats.runs, stats.crashes);
            let rows: Vec<Vec<String>> = records
                .iter()
                .map(|record| {
                    use agora_core::launch_history::LaunchResult;
                    vec![
                        record.started_at.clone(),
                        match record.outcome {
                            None => "running",
                            Some(LaunchResult::Ok) => "ok",
                            Some(LaunchResult::Crashed) => "crashed",
                            Some(LaunchResult::Stopped) => "stopped",
                            Some(LaunchResult::Unknown) => "unknown",
                        }
                        .to_string(),
                        record
                            .duration_ms
                            .map(|ms| format!("{} min", ms / 60_000))
                            .unwrap_or_default(),
                        record.enabled_mod_count.to_string(),
                        format!("{} {}", record.minecraft_version, record.loader),
                    ]
                })
                .collect();
            print_table(&["Started", "Outcome", "Played", "Mods", "Version"], &rows);
        }
        InstanceParityCmd::Stop { instance } => {
            let pid = agora_core::instance_runtime::stop_game(ctx, &instance)?;
            if json {
                print_json(&serde_json::json!({
                    "status": "stopped", "instanceId": instance, "pid": pid
                }))?;
            } else {
                println!("Stopped the game for '{instance}' (process {pid}).");
            }
        }
        InstanceParityCmd::SnapshotVersions => {
            let policy = agora_core::network::NetworkPolicy::from_ctx(ctx)?;
            let versions = agora_game_minecraft::minecraft_metadata::list_snapshot_versions(
                &ctx.paths.minecraft_runtime_root(),
                &policy,
                &*ctx.network_gate(),
            )
            .await?;
            if json {
                return print_json(&versions);
            }
            for version in versions {
                println!("{version}");
            }
        }
    }
    Ok(())
}

async fn migrate(
    ctx: &Ctx,
    instance: &str,
    mc_version: &str,
    dry_run: bool,
    accept_blockers: bool,
    json: bool,
) -> anyhow::Result<()> {
    use agora_game_minecraft::version_migration::{MigrationOutcome, VersionMigrationService};
    let instance = agora_core::paths::sanitize_id(instance);
    let service = VersionMigrationService::new(ctx.clone());
    let plan = service
        .plan(&instance, mc_version)
        .await
        .map_err(|rejection| {
            anyhow::anyhow!(
                "Agora cannot move '{instance}' to Minecraft {mc_version}:\n  - {}",
                rejection
                    .reasons
                    .iter()
                    .map(|reason| reason.message.clone())
                    .collect::<Vec<_>>()
                    .join("\n  - ")
            )
        })?;
    if dry_run {
        if json {
            return print_json(&plan);
        }
        println!(
            "Moving '{instance}' from Minecraft {} to {}:",
            plan.source_version, plan.target_version
        );
        if let Some(loader) = &plan.target_loader_version {
            println!(
                "  {} {} becomes {loader}",
                plan.loader, plan.source_loader_version
            );
        }
        for swap in &plan.swaps {
            println!("  {} -> {}", swap.old_filename, swap.new_filename);
        }
        print_blockers(&plan.blockers);
        for warning in &plan.warnings {
            println!("  Note: {warning}");
        }
        println!("Nothing was changed (--dry-run).");
        return Ok(());
    }
    if !plan.blockers.is_empty() && !accept_blockers {
        if !json {
            print_blockers(&plan.blockers);
        }
        anyhow::bail!(
            "{} item(s) would stay at their current version. Rerun with --accept-blockers to move anyway, or --dry-run to see the whole plan.",
            plan.blockers.len()
        );
    }
    let outcome = service
        .migrate(&instance, mc_version, accept_blockers)
        .await;
    if json {
        print_json(&outcome)?;
    }
    match outcome {
        MigrationOutcome::Migrated {
            from_version,
            to_version,
            replaced,
            snapshot_id,
            warnings,
            ..
        } => {
            if !json {
                println!(
                    "Moved '{instance}' from Minecraft {from_version} to {to_version}; {} item(s) replaced. Undo point: snapshot {snapshot_id}.",
                    replaced.len()
                );
                for warning in warnings {
                    println!("  Note: {warning}");
                }
            }
            Ok(())
        }
        MigrationOutcome::Blocked { reasons, .. } => anyhow::bail!(
            "The move was refused and nothing changed:\n  - {}",
            reasons
                .iter()
                .map(|reason| reason.message.clone())
                .collect::<Vec<_>>()
                .join("\n  - ")
        ),
        MigrationOutcome::RolledBack { phase, error, .. } => {
            anyhow::bail!("The move failed while {phase} and was rolled back: {error}")
        }
        MigrationOutcome::Failed {
            phase,
            error,
            snapshot_id,
            ..
        } => anyhow::bail!(
            "The move failed while {phase}: {error}{}",
            snapshot_id
                .map(|id| format!(" Restore snapshot {id} to undo it."))
                .unwrap_or_default()
        ),
    }
}

fn print_blockers(blockers: &[agora_game_minecraft::version_migration::RejectionReason]) {
    if blockers.is_empty() {
        return;
    }
    println!("These stay at their current version:");
    for blocker in blockers {
        match &blocker.filename {
            Some(file) => println!("  {file}: {}", blocker.message),
            None => println!("  {}", blocker.message),
        }
    }
}

// ---------------------------------------------------------------------------
// provider
// ---------------------------------------------------------------------------

#[derive(Subcommand, Clone)]
pub enum ProviderParityCmd {
    /// Show a provider project's details. ITEM is `<provider-id>:<project-id>`.
    Show { item: String },
    /// List a provider project's versions. ITEM is `<provider-id>:<project-id>`.
    Versions {
        item: String,
        #[arg(
            long,
            help = "Only versions that fit this instance's Minecraft version and loader"
        )]
        instance: Option<String>,
    },
    /// Show what installing a provider project would download, before anything is downloaded.
    Preview {
        item: String,
        #[arg(long, help = "Version ID (default: the provider's choice)")]
        version: Option<String>,
        #[arg(
            long,
            help = "The instance it would go into, for its Minecraft version and loader"
        )]
        instance: Option<String>,
    },
}

/// `registry` is the one `agora provider` already built for this command.
pub async fn run_provider(
    action: ProviderParityCmd,
    ctx: &Ctx,
    registry: &ProviderRegistry,
    output_fmt: OutputFormat,
) -> anyhow::Result<()> {
    let json = output_fmt.is_json_output();
    let project_of = |item: &str| -> anyhow::Result<(String, String)> {
        agora_core::providers::parse_item_id(item)
            .map(|(provider, project)| (provider.to_string(), project.to_string()))
            .ok_or_else(|| anyhow::anyhow!("`{item}` is not a content-provider item."))
    };
    let fit = |instance: &Option<String>| -> anyhow::Result<(Option<String>, Option<String>)> {
        Ok(match instance {
            Some(id) => {
                let (mc, loader) = instance_version_and_loader(ctx, id)?;
                (Some(mc), Some(loader))
            }
            None => (None, None),
        })
    };
    match action {
        ProviderParityCmd::Show { item } => {
            let (provider_id, project_id) = project_of(&item)?;
            let detail = registry.usable(&provider_id)?.project(&project_id).await?;
            if json {
                return print_json(&detail);
            }
            let project = &detail.project;
            println!("{} ({})", project.title, project.content_type);
            if let Some(author) = &project.author {
                println!("By: {author}");
            }
            if let Some(description) = &project.description {
                println!("{description}");
            }
            if !project.minecraft_versions.is_empty() {
                println!("Minecraft: {}", project.minecraft_versions.join(", "));
            }
            if !project.loaders.is_empty() {
                println!("Loaders: {}", project.loaders.join(", "));
            }
            if let Some(license) = &detail.license {
                println!("License: {license}");
            }
            if let Some(url) = &project.page_url {
                println!("Page: {url}");
            }
        }
        ProviderParityCmd::Versions { item, instance } => {
            let (provider_id, project_id) = project_of(&item)?;
            let (minecraft_version, loader) = fit(&instance)?;
            let response = registry
                .usable(&provider_id)?
                .versions(agora_core::providers::VersionsRequest {
                    project_id,
                    minecraft_version,
                    loader,
                })
                .await?;
            if json {
                return print_json(&response);
            }
            if response.versions.is_empty() {
                println!("No versions to show.");
                return Ok(());
            }
            let rows: Vec<Vec<String>> = response
                .versions
                .iter()
                .map(|version| {
                    vec![
                        version.version_number.clone(),
                        version.id.clone(),
                        version.minecraft_versions.join(", "),
                        version.loaders.join(", "),
                        version.published.clone().unwrap_or_default(),
                    ]
                })
                .collect();
            print_table(
                &["Version", "Version ID", "Minecraft", "Loaders", "Published"],
                &rows,
            );
        }
        ProviderParityCmd::Preview {
            item,
            version,
            instance,
        } => {
            let (minecraft_version, loader) = fit(&instance)?;
            let preview = agora_game_minecraft::providers::install::preview(
                ctx,
                registry,
                &item,
                version.as_deref(),
                minecraft_version.as_deref().unwrap_or_default(),
                loader.as_deref().unwrap_or_default(),
            )
            .await?;
            if json {
                return print_json(&preview);
            }
            println!(
                "{} {} from {}: a {} with {} file(s).",
                preview.name,
                preview.version,
                preview.provider_title,
                preview.kind,
                preview.file_count
            );
            for (host, count) in &preview.hosts {
                println!("  {count} file(s) from {host}");
            }
            for note in &preview.warnings {
                println!("  Warning: {} ({})", note.reason, note.url_host);
            }
            for note in &preview.low_security {
                println!(
                    "  No integrity information: {} ({})",
                    note.reason, note.url_host
                );
            }
            if !preview.low_security.is_empty() && !preview.low_security_allowed {
                println!("  Installing it needs --allow-unverified, or the allow_unverified_packs setting.");
            }
            for path in &preview.outside_content_folders {
                println!("  Outside the usual content folders: {path}");
            }
            println!("Nothing was downloaded.");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// registry
// ---------------------------------------------------------------------------

#[derive(Subcommand)]
pub enum RegistryParityCmd {
    /// Show one catalog item.
    Item { item: String },
}

pub fn run_registry(
    action: RegistryParityCmd,
    ctx: &Ctx,
    output_fmt: OutputFormat,
) -> anyhow::Result<()> {
    let json = output_fmt.is_json_output();
    match action {
        RegistryParityCmd::Item { item } => {
            let found = agora_core::registry::RegistryService::new(ctx.clone())
                .get_item_by_id(&item)?
                .ok_or_else(|| anyhow::anyhow!("Catalog item '{item}' not found"))?;
            if json {
                return print_json(&found);
            }
            println!("{} ({})", found.name, found.content_type);
            println!("ID:      {}", found.id);
            println!("Status:  {}", found.status);
            println!(
                "Source:  {} {}",
                found.download_strategy, found.source_identifier
            );
            println!("Votes:   +{} / -{}", found.upvotes, found.downvotes);
            if let Some(description) = &found.description {
                println!("{description}");
            }
            if let Some(url) = &found.page_url {
                println!("Page:    {url}");
            }
        }
    }
    Ok(())
}
