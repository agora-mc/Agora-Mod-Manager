//! Commands that give the CLI what the desktop app already does: new-download
//! notices, item votes, GitHub sign-in, Microsoft token refresh, the saved
//! skin library, editing and reordering servers, running plugin commands,
//! diagnostics and repairs, and editing templates. Thin adapters over the
//! same core functions the desktop calls.

use std::path::{Path, PathBuf};

use agora_core::ctx::Ctx;
use agora_core::download_watch::{self, AddDownloadOutcome, DownloadRow};
use agora_core::error::LauncherError;
use agora_core::governance_vote::{ItemVote, ItemVoteState};
use agora_core::plugins::PluginService;
use agora_game_minecraft::gc::GcSelection;
use agora_game_minecraft::template_service::{self, TemplateJvm};
use agora_game_minecraft::worlds::WorldService;
use clap::{Subcommand, ValueEnum};

use crate::{print_table, OutputFormat};

fn print_json<T: serde::Serialize>(value: &T) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

// ---------------------------------------------------------------------------
// downloads list / add / ignore
// ---------------------------------------------------------------------------

/// Show the downloads Agora noticed, with files that no longer exist dropped.
pub fn run_downloads_list(ctx: &Ctx, output_fmt: OutputFormat) -> anyhow::Result<()> {
    let rows = download_watch::list(ctx).map_err(LauncherError::from)?;
    if output_fmt.is_json_output() {
        return print_json(&rows);
    }
    if rows.is_empty() {
        println!("No new downloads.");
        return Ok(());
    }
    let table: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            vec![
                row.sha256.chars().take(12).collect(),
                row.file_name.clone(),
                row.game_id.clone(),
                row.kind.clone(),
                crate::format_size(row.size),
                if row.already_in_library {
                    "already in your library".to_string()
                } else {
                    "ready to add".to_string()
                },
            ]
        })
        .collect();
    print_table(&["ID", "File", "Game", "Kind", "Size", "Status"], &table);
    println!(
        "Add one with `agora downloads add <ID>`, or hide it with `agora downloads ignore <ID>`."
    );
    Ok(())
}

/// The full hash of the download `wanted` names: its hash, a unique start of it, or its file name.
fn resolve_download(rows: &[DownloadRow], wanted: &str) -> anyhow::Result<String> {
    let lowered = wanted.trim().to_ascii_lowercase();
    if let Some(row) = rows.iter().find(|row| row.file_name == wanted.trim()) {
        return Ok(row.sha256.clone());
    }
    let matches: Vec<&DownloadRow> = rows
        .iter()
        .filter(|row| !lowered.is_empty() && row.sha256.starts_with(&lowered))
        .collect();
    match matches.as_slice() {
        [row] => Ok(row.sha256.clone()),
        [] => Ok(lowered),
        _ => anyhow::bail!("'{wanted}' matches more than one download; give more of its ID."),
    }
}

/// Add a noticed download to the library through its game's own flow. The file is only read.
pub fn run_downloads_add(
    ctx: &Ctx,
    download: &str,
    output_fmt: OutputFormat,
) -> anyhow::Result<()> {
    let rows = download_watch::list(ctx).map_err(LauncherError::from)?;
    let sha256 = resolve_download(&rows, download)?;
    let outcome = download_watch::add(ctx, &sha256).map_err(LauncherError::from)?;
    if output_fmt.is_json_output() {
        return print_json(&outcome);
    }
    match outcome {
        AddDownloadOutcome::Added { item_id } => {
            println!("Added to your library as {item_id}.");
        }
        AddDownloadOutcome::NeedsInstaller { item_id, title } => {
            println!("'{title}' has an installer with choices; it is stored as {item_id}.");
            println!("Run `agora games content fomod show {item_id}` to see its options.");
        }
    }
    Ok(())
}

/// Stop offering a download. Remembered by its contents, so a re-download stays hidden too.
pub fn run_downloads_ignore(
    ctx: &Ctx,
    download: &str,
    output_fmt: OutputFormat,
) -> anyhow::Result<()> {
    let rows = download_watch::list(ctx).map_err(LauncherError::from)?;
    let sha256 = resolve_download(&rows, download)?;
    download_watch::ignore(ctx, &sha256).map_err(LauncherError::from)?;
    if output_fmt.is_json_output() {
        return print_json(&serde_json::json!({ "status": "ignored", "sha256": sha256 }));
    }
    println!("Agora will not offer that download again.");
    Ok(())
}

// ---------------------------------------------------------------------------
// registry vote
// ---------------------------------------------------------------------------

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoteChoice {
    /// Vote to keep the item.
    Up,
    /// Vote against the item.
    Down,
    /// Take your vote back.
    Clear,
}

fn print_vote(item: &str, state: &ItemVoteState, output_fmt: OutputFormat) -> anyhow::Result<()> {
    if output_fmt.is_json_output() {
        return print_json(&serde_json::json!({
            "itemId": item,
            "vote": state.vote,
            "conflicted": state.conflicted,
        }));
    }
    match state.vote {
        Some(ItemVote::Upvote) => println!("Your vote on {item}: up."),
        Some(ItemVote::Downvote) => println!("Your vote on {item}: down."),
        None if state.conflicted => println!(
            "You have reacted both up and down on {item}'s voting issue, so neither counts. Pick one with `agora registry vote {item} up` or `down`."
        ),
        None => println!("You have not voted on {item}."),
    }
    Ok(())
}

/// Show, cast, switch or retract your vote on a curated item. Needs `agora auth github login`.
pub async fn run_registry_vote(
    ctx: &Ctx,
    item: &str,
    vote: Option<VoteChoice>,
    output_fmt: OutputFormat,
) -> anyhow::Result<()> {
    use agora_core::governance_vote::{get_item_vote, set_item_vote};
    let state = match vote {
        None => get_item_vote(ctx, item).await?,
        Some(VoteChoice::Up) => set_item_vote(ctx, item, Some(ItemVote::Upvote)).await?,
        Some(VoteChoice::Down) => set_item_vote(ctx, item, Some(ItemVote::Downvote)).await?,
        Some(VoteChoice::Clear) => set_item_vote(ctx, item, None).await?,
    };
    print_vote(item, &state, output_fmt)
}

// ---------------------------------------------------------------------------
// auth github / auth refresh
// ---------------------------------------------------------------------------

#[derive(Subcommand)]
pub enum GithubAuthCmd {
    /// Sign in to GitHub (used for voting and community reports). Prints a code to enter on
    /// github.com and waits until you have.
    Login {
        /// Print the sign-in link instead of opening a browser.
        #[arg(long)]
        no_browser: bool,
    },
    /// Say whether a GitHub sign-in is stored. Does not go online.
    Status,
    /// Forget the stored GitHub sign-in.
    Logout,
}

pub async fn run_github_auth(
    action: GithubAuthCmd,
    output_fmt: OutputFormat,
) -> anyhow::Result<()> {
    let json = output_fmt.is_json_output();
    match action {
        GithubAuthCmd::Login { no_browser } => {
            let flow = agora_core::auth::start_device_flow().await?;
            // In --json mode stdout must stay machine-readable, so the prompt goes to stderr.
            let prompt = format!(
                "To sign in to GitHub, open {} and enter the code: {}",
                flow.verification_uri, flow.user_code
            );
            if json {
                eprintln!("{prompt}");
            } else {
                println!("{prompt}");
            }
            if !no_browser {
                if let Err(error) = crate::open_url_in_browser(&flow.verification_uri) {
                    eprintln!("Could not open a browser ({error}). Open the link above yourself.");
                }
            }
            eprintln!("Waiting for you to finish signing in… (Ctrl-C to cancel)");
            let bundle = agora_core::auth::poll_device_flow(flow.device_code, flow.interval)
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "GitHub sign-in did not finish: the code expired or was declined. Run `agora auth github login` again."
                    )
                })?;
            agora_core::auth::store_token_bundle(&bundle)?;
            if json {
                print_json(&serde_json::json!({ "status": "signed_in" }))?;
            } else {
                println!("Signed in to GitHub.");
            }
        }
        GithubAuthCmd::Status => {
            let signed_in = agora_core::auth::is_authenticated();
            if json {
                print_json(&serde_json::json!({
                    "signedIn": signed_in,
                    "storage": agora_core::auth::github_credential_backend(),
                }))?;
            } else if signed_in {
                println!("Signed in to GitHub.");
            } else {
                println!("Not signed in to GitHub. Run `agora auth github login`.");
            }
        }
        GithubAuthCmd::Logout => {
            agora_core::auth::clear_token().map_err(|message| LauncherError::Generic {
                code: "ERR_AUTH_LOGOUT".into(),
                message,
            })?;
            if json {
                print_json(&serde_json::json!({ "status": "signed_out" }))?;
            } else {
                println!("Signed out of GitHub.");
            }
        }
    }
    Ok(())
}

/// Renew the active Microsoft account's tokens now, as the desktop's refresh does.
pub async fn run_msa_refresh(
    ctx: &Ctx,
    data_dir: &Path,
    output_fmt: OutputFormat,
) -> anyhow::Result<()> {
    let creds =
        agora_game_minecraft::msa::load_credentials()?.ok_or_else(|| LauncherError::Generic {
            code: "ERR_MSA_NOT_AUTHENTICATED".into(),
            message: "Not signed in. Sign in with your Microsoft account first.".into(),
        })?;
    let refreshed = agora_game_minecraft::msa::refresh_credentials(
        &ctx.http_clients,
        &creds,
        &data_dir.join("local_state.db"),
    )
    .await?;
    if output_fmt.is_json_output() {
        print_json(&serde_json::json!({
            "username": refreshed.username,
            "uuid": refreshed.uuid,
            "expires": refreshed.expires.to_rfc3339(),
            "needsReauth": refreshed.needs_reauth(),
        }))
    } else {
        println!(
            "Refreshed {}; good until {}.",
            refreshed.username, refreshed.expires
        );
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// skin library
// ---------------------------------------------------------------------------

#[derive(Subcommand)]
pub enum SkinLibraryCmd {
    /// List the skins saved on this computer (the default).
    List,
    /// Save a 64×64 skin PNG to the library. Nothing is uploaded.
    Add {
        file: PathBuf,
        #[arg(long, help = "Name to show (defaults to the file name)")]
        name: Option<String>,
        #[arg(long, help = "Slim (three-pixel) arms")]
        slim: bool,
    },
    /// Remove a saved skin by its ID (see `skin library list`).
    Remove { id: String },
}

pub fn run_skin_library(
    action: Option<SkinLibraryCmd>,
    ctx: &Ctx,
    output_fmt: OutputFormat,
) -> anyhow::Result<()> {
    use agora_game_minecraft::skins::{self, SkinVariant};
    let json = output_fmt.is_json_output();
    match action.unwrap_or(SkinLibraryCmd::List) {
        SkinLibraryCmd::List => {
            let saved = skins::library(ctx);
            if json {
                print_json(&saved)?;
            } else if saved.is_empty() {
                println!("No saved skins. Add one with `agora skin library add <FILE>`.");
            } else {
                let rows: Vec<Vec<String>> = saved
                    .iter()
                    .map(|skin| {
                        vec![
                            skin.id.clone(),
                            skin.name.clone(),
                            format!("{:?}", skin.variant).to_lowercase(),
                        ]
                    })
                    .collect();
                print_table(&["ID", "Name", "Arms"], &rows);
            }
        }
        SkinLibraryCmd::Add { file, name, slim } => {
            let bytes = std::fs::read(&file).map_err(|e| LauncherError::Generic {
                code: "ERR_SKIN_FILE".into(),
                message: format!("Could not read the file: {e}"),
            })?;
            let name = match name.filter(|name| !name.trim().is_empty()) {
                Some(name) => name,
                None => file
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or("Skin")
                    .to_string(),
            };
            let variant = if slim {
                SkinVariant::Slim
            } else {
                SkinVariant::Classic
            };
            let saved = skins::save_to_library(ctx, &name, variant, &bytes)?;
            if json {
                print_json(&saved)?;
            } else {
                println!(
                    "Saved '{}' to your skin library ({}).",
                    saved.name, saved.id
                );
            }
        }
        SkinLibraryCmd::Remove { id } => {
            skins::remove_from_library(ctx, &id)?;
            if json {
                print_json(&serde_json::json!({ "status": "removed", "id": id }))?;
            } else {
                println!("Removed it from your skin library.");
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// server edit / move / ping
// ---------------------------------------------------------------------------

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoveDirection {
    Up,
    Down,
}

/// The zero-based index of the server `wanted` names: a position from `server list`, or a name.
fn resolve_server(service: &WorldService, instance: &str, wanted: &str) -> anyhow::Result<usize> {
    let servers = service.list_servers(instance)?;
    if let Ok(position) = wanted.trim().parse::<usize>() {
        if position == 0 || position > servers.len() {
            anyhow::bail!(
                "There is no server at position {position}; `agora server list {instance}` shows them."
            );
        }
        return Ok(position - 1);
    }
    let matches: Vec<usize> = servers
        .iter()
        .enumerate()
        .filter(|(_, server)| server.name.eq_ignore_ascii_case(wanted.trim()))
        .map(|(index, _)| index)
        .collect();
    match matches.as_slice() {
        [index] => Ok(*index),
        [] => anyhow::bail!("No server called '{wanted}' in this instance."),
        _ => anyhow::bail!("More than one server is called '{wanted}'; use its position instead."),
    }
}

/// Rename a server or change its address. Whatever is not given stays as it is.
pub fn run_server_edit(
    ctx: &Ctx,
    instance: &str,
    server: &str,
    name: Option<String>,
    address: Option<String>,
    while_running: bool,
    output_fmt: OutputFormat,
) -> anyhow::Result<()> {
    if name.is_none() && address.is_none() {
        anyhow::bail!("Nothing to change. Pass --name and/or --address.");
    }
    let service = if while_running {
        WorldService::new(ctx.clone()).while_running()
    } else {
        WorldService::new(ctx.clone())
    };
    let index = resolve_server(&service, instance, server)?;
    let current = service
        .list_servers(instance)?
        .into_iter()
        .nth(index)
        .ok_or_else(|| anyhow::anyhow!("That server is no longer in the list."))?;
    let name = name.unwrap_or(current.name);
    let address = address.unwrap_or(current.address);
    service.update_server(instance, index, &name, &address)?;
    if output_fmt.is_json_output() {
        print_json(&serde_json::json!({
            "position": index + 1,
            "name": name,
            "address": address,
        }))
    } else {
        println!("Server {} is now '{name}' at {address}.", index + 1);
        Ok(())
    }
}

/// Swap a server with the one above or below it.
pub fn run_server_move(
    ctx: &Ctx,
    instance: &str,
    server: &str,
    direction: MoveDirection,
    while_running: bool,
    output_fmt: OutputFormat,
) -> anyhow::Result<()> {
    let service = if while_running {
        WorldService::new(ctx.clone()).while_running()
    } else {
        WorldService::new(ctx.clone())
    };
    let index = resolve_server(&service, instance, server)?;
    let offset = match direction {
        MoveDirection::Up => -1,
        MoveDirection::Down => 1,
    };
    service
        .move_server(instance, index, offset)
        .map_err(|error| {
            if index == 0 && offset < 0 {
                anyhow::anyhow!("That server is already at the top.")
            } else {
                anyhow::Error::from(error)
            }
        })?;
    let position = (index as i64 + i64::from(offset) + 1) as usize;
    if output_fmt.is_json_output() {
        print_json(&serde_json::json!({ "position": position }))
    } else {
        println!("Moved it to position {position}.");
        Ok(())
    }
}

/// Ask any server for its status. Uses the same network setting as the desktop.
pub async fn run_server_ping(address: &str, output_fmt: OutputFormat) -> anyhow::Result<()> {
    let status = agora_game_minecraft::server_ping::ping(address).await?;
    if output_fmt.is_json_output() {
        return print_json(&status);
    }
    if !status.online {
        println!("{address} is offline.");
        return Ok(());
    }
    let mut line = format!("{address} is online");
    if let (Some(on), Some(max)) = (status.players_online, status.players_max) {
        line.push_str(&format!(", {on}/{max} players"));
    }
    if let Some(ms) = status.latency_ms {
        line.push_str(&format!(", {ms} ms"));
    }
    println!("{line}.");
    Ok(())
}

// ---------------------------------------------------------------------------
// plugin run / diagnose / repair / launch-checks
// ---------------------------------------------------------------------------

fn from_plugin_error(error: agora_plugin_api::PluginError) -> anyhow::Error {
    LauncherError::Generic {
        code: format!("ERR_PLUGIN_{:?}", error.code).to_uppercase(),
        message: error.message,
    }
    .into()
}

fn parse_args(args: Option<String>) -> anyhow::Result<serde_json::Value> {
    match args {
        None => Ok(serde_json::Value::Null),
        Some(text) => serde_json::from_str(&text)
            .map_err(|error| anyhow::anyhow!("--args must be JSON: {error}")),
    }
}

/// Run a command a plugin contributes. Every enable, permission and code check the desktop
/// applies is applied by core here too.
pub fn run_plugin_run(
    service: &PluginService,
    id: &str,
    export: &str,
    args: Option<String>,
    output_fmt: OutputFormat,
) -> anyhow::Result<()> {
    service.reload()?;
    let plugin_id = crate::parse_plugin_id(id)?;
    let result = service
        .run_command(&plugin_id, export, parse_args(args)?)
        .map_err(from_plugin_error)?;
    if output_fmt.is_json_output() || !result.is_string() {
        print_json(&result)
    } else {
        println!("{}", result.as_str().unwrap_or_default());
        Ok(())
    }
}

fn run_diagnostic(
    service: &PluginService,
    id: &str,
    export: &str,
    instance: &str,
) -> anyhow::Result<agora_plugin_api::diagnostics::DiagnosticReport> {
    service.reload()?;
    let plugin_id = crate::parse_plugin_id(id)?;
    service
        .run_diagnostic(
            &plugin_id,
            export,
            serde_json::json!({ "instanceId": instance }),
        )
        .map_err(from_plugin_error)
}

fn print_report(report: &agora_plugin_api::diagnostics::DiagnosticReport) {
    if report.findings.is_empty() {
        println!("Nothing found.");
    }
    for finding in &report.findings {
        println!("[{:?}] {}", finding.severity, finding.title);
        if let Some(summary) = &finding.summary {
            println!("    {summary}");
        }
        for repair in &finding.repairs {
            println!("    Fix '{}': {}", repair.id, repair.title);
            for action in &repair.actions {
                println!("      - {}", action.describe());
            }
        }
    }
    if let Some(reason) = &report.incomplete_reason {
        println!("The check did not finish: {reason}");
    }
}

/// Run one of a plugin's diagnostics against an instance and show what it found.
pub fn run_plugin_diagnose(
    service: &PluginService,
    id: &str,
    export: &str,
    instance: &str,
    output_fmt: OutputFormat,
) -> anyhow::Result<()> {
    let report = run_diagnostic(service, id, export, instance)?;
    if output_fmt.is_json_output() {
        return print_json(&report);
    }
    print_report(&report);
    let has_repairs = report
        .findings
        .iter()
        .any(|finding| !finding.repairs.is_empty());
    if has_repairs {
        println!(
            "Apply a fix with `agora plugin repair {id} {export} {instance} <FIX>`; you will be asked first."
        );
    }
    Ok(())
}

/// Re-run a diagnostic, show the chosen fix, ask, then apply it. Core re-checks every action
/// against the instance as it is now before changing anything.
pub fn run_plugin_repair(
    service: &PluginService,
    id: &str,
    export: &str,
    instance: &str,
    repair: &str,
    yes: bool,
    output_fmt: OutputFormat,
) -> anyhow::Result<()> {
    let report = run_diagnostic(service, id, export, instance)?;
    let proposal = report
        .findings
        .iter()
        .flat_map(|finding| finding.repairs.iter())
        .find(|proposal| proposal.id == repair)
        .cloned()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "The diagnostic no longer offers a fix called '{repair}'. Run `agora plugin diagnose {id} {export} {instance}` to see what it offers now."
            )
        })?;
    if !yes {
        let actions: Vec<String> = proposal
            .actions
            .iter()
            .map(|action| format!("  - {}", action.describe()))
            .collect();
        let question = format!(
            "{} ({id}) wants to:\n{}\nApply this fix? [y/N] ",
            proposal.title,
            actions.join("\n")
        );
        if !crate::cli_tools::confirm(
            &question,
            "Applying a plugin's fix needs confirmation; pass --yes once you have read it with `plugin diagnose`.",
        )? {
            anyhow::bail!("Cancelled.");
        }
    }
    let plugin_id = crate::parse_plugin_id(id)?;
    let outcome = service.apply_repair(&plugin_id, &proposal)?;
    if output_fmt.is_json_output() {
        print_json(&outcome)?;
    } else {
        for action in &outcome.applied {
            println!("Done: {action}");
        }
        for action in &outcome.stale {
            println!("Skipped, because things changed since the check: {action}");
        }
        for failure in &outcome.failed {
            println!("Failed: {} ({})", failure.action, failure.message);
        }
    }
    if !outcome.is_complete() {
        anyhow::bail!("The fix was only partly applied.");
    }
    Ok(())
}

/// Run every pre-launch check plugins contribute for an instance, without launching.
pub fn run_plugin_launch_checks(
    service: &PluginService,
    instance: &str,
    output_fmt: OutputFormat,
) -> anyhow::Result<()> {
    service.reload()?;
    let outcome = service.run_launch_checks(instance);
    if output_fmt.is_json_output() {
        return print_json(&outcome);
    }
    if outcome.results.is_empty() && outcome.errors.is_empty() {
        println!("No plugin has a launch check for this instance.");
    }
    for result in &outcome.results {
        println!("{} ({}):", result.title, result.plugin_id);
        print_report(&result.report);
    }
    for error in &outcome.errors {
        eprintln!("A check could not run: {error}");
    }
    if outcome.should_prompt() {
        println!("A launch would stop and ask you about the warnings above first.");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// template update
// ---------------------------------------------------------------------------

/// Change a template's name, description or JVM settings. JVM flags change only the settings
/// they name; `--clear-jvm` removes them all first.
#[derive(clap::Args)]
pub struct TemplateUpdateArgs {
    /// Template ID, from `agora template list`.
    pub template: String,
    #[arg(long, help = "New display name")]
    pub name: Option<String>,
    #[arg(long, conflicts_with = "clear_description", help = "New description")]
    pub description: Option<String>,
    #[arg(long, help = "Remove the description")]
    pub clear_description: bool,
    #[arg(long, help = "Remove every JVM setting before applying the ones given")]
    pub clear_jvm: bool,
    #[arg(long = "java-path", help = "Java executable the template sets")]
    pub java_path: Option<String>,
    #[arg(long, help = "Heap size in MB the template sets")]
    pub memory: Option<i64>,
    #[arg(
        long,
        help = "GC the template sets: auto, low_latency, high_efficiency, or manual"
    )]
    pub gc: Option<String>,
    #[arg(
        long = "jvm-args",
        allow_hyphen_values = true,
        help = "Extra JVM arguments the template sets"
    )]
    pub jvm_args: Option<String>,
}

pub fn run_template_update(
    ctx: &Ctx,
    args: TemplateUpdateArgs,
    output_fmt: OutputFormat,
) -> anyhow::Result<()> {
    let template_error = |message: String| -> anyhow::Error {
        LauncherError::Generic {
            code: "ERR_TEMPLATE".into(),
            message,
        }
        .into()
    };
    let root = ctx.paths.templates_root();
    let jvm_given = args.clear_jvm
        || args.java_path.is_some()
        || args.memory.is_some()
        || args.gc.is_some()
        || args.jvm_args.is_some();
    let description = if args.clear_description {
        Some(None)
    } else {
        args.description.map(Some)
    };
    if args.name.is_none() && description.is_none() && !jvm_given {
        anyhow::bail!(
            "Nothing to change. Pass --name, --description, or a JVM setting such as --memory."
        );
    }
    let jvm = if jvm_given {
        let mut jvm = if args.clear_jvm {
            TemplateJvm::default()
        } else {
            template_service::get_template(&root, &args.template)
                .map_err(template_error)?
                .jvm
        };
        if let Some(java_path) = args.java_path {
            jvm.java_path = Some(java_path);
        }
        if let Some(memory) = args.memory {
            jvm.jvm_memory_mb = Some(memory);
            jvm.jvm_memory_mode = Some("manual".to_string());
        }
        if let Some(gc) = args.gc {
            let selection = GcSelection::from_input(&gc).ok_or_else(|| {
                anyhow::anyhow!(
                    "Unsupported GC '{}'. Expected auto, low_latency, high_efficiency, or manual.",
                    gc.trim()
                )
            })?;
            jvm.jvm_gc = Some(selection.as_str().to_string());
        }
        if let Some(jvm_args) = args.jvm_args {
            jvm.jvm_custom_args = Some(jvm_args);
        }
        Some(jvm)
    } else {
        None
    };
    let now = chrono::Utc::now().to_rfc3339();
    let template = template_service::update_template(
        &root,
        &args.template,
        args.name.as_deref(),
        description,
        jvm,
        &now,
    )
    .map_err(template_error)?;
    if output_fmt.is_json_output() {
        print_json(&template)
    } else {
        println!(
            "Updated template '{}' ({}): JVM settings {}.",
            template.name,
            template.id,
            if template.jvm.is_empty() {
                "none"
            } else {
                "included"
            }
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(sha256: &str, file_name: &str) -> DownloadRow {
        DownloadRow {
            path: format!("/downloads/{file_name}"),
            file_name: file_name.to_string(),
            size: 1,
            sha256: sha256.to_string(),
            game_id: "skyrimse".to_string(),
            kind: "mod".to_string(),
            reason: String::new(),
            nexus_mod_id: None,
            catalog_item_id: None,
            catalog_item_name: None,
            already_in_library: false,
        }
    }

    #[test]
    fn a_download_is_found_by_file_name_or_a_unique_start_of_its_hash() {
        let rows = vec![row("abc123", "a.zip"), row("abd456", "b.zip")];
        assert_eq!(resolve_download(&rows, "b.zip").unwrap(), "abd456");
        assert_eq!(resolve_download(&rows, "ABC").unwrap(), "abc123");
        assert!(resolve_download(&rows, "ab").is_err());
        assert_eq!(resolve_download(&rows, "ffff").unwrap(), "ffff");
    }
}
