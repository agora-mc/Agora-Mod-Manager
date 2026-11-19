//! `agora games instance …` commands that do what the desktop's game instance screens do: rename,
//! clone and stop an instance, Recently deleted, save backups, the INI files mods ship, a tool's
//! earlier outputs and inputs, and the instance's own tools.
//!
//! Each handler is a thin adapter over the same agora-core functions the desktop commands call
//! (`desktop/src-tauri/src/games/{instances,ini_saves,tools}.rs`), so the two cannot disagree.
//! Every handler prints JSON on stdout under `--json` and plain text otherwise; a failure goes
//! through [`ini_fail`] (the message as text or as a JSON envelope on stderr, and exit 1).

use std::io::{IsTerminal, Write};
use std::path::Path;
use std::sync::Arc;

use agora_core::ctx::Ctx;
use agora_core::event_sink::CancellationToken;
use agora_core::game_instance::{self, InstanceError};
use agora_core::game_launch::SystemLauncher;
use agora_core::game_mod_ini::{self, ModIni, ModIniRead, ModIniSource};
use agora_core::game_tools::{self, CaptureMode, RunOutcome, ToolError, ToolOffer};
use agora_core::game_user_tools::{self, NewUserTool, UserTool};
use agora_core::save_backup::{self, SaveBackup};
use agora_game_api::{GameDefinition, GameId, ToolId};
use clap::Subcommand;

use crate::{
    ask_yes_no, format_size, ini_fail, merge_offers, print_table, print_tool_run, swap_cost_lines,
    switch_to_links_lines, tool_tier_name, vfs_fallback_step, vfs_next_tier, vfs_refusal_offers,
    vfs_refusal_reason, vfs_tool_envelope, VfsFallbackStep,
};

// ---------------------------------------------------------------------------
// Subcommands
// ---------------------------------------------------------------------------

#[derive(Subcommand)]
pub enum SaveBackupCmd {
    /// Copy the instance's save folder into a new backup, and say how large it is.
    Create {
        /// Instance ID.
        instance_id: String,
        /// Also record the backup for this crash-test session, as the desktop does before a test.
        #[arg(long)]
        session: Option<String>,
    },
    /// List the instance's save backups, newest first. With --session, show only the backup
    /// recorded for that crash-test session.
    List {
        /// Instance ID.
        instance_id: String,
        /// The crash-test session whose backup to show.
        #[arg(long)]
        session: Option<String>,
    },
    /// Put a save backup back. Nothing is deleted: a current save with the same name is kept
    /// beside it as `.before-restore`, and saves made after the backup stay.
    Restore {
        /// Instance ID.
        instance_id: String,
        /// Backup ID, from `save-backup list`.
        backup_id: String,
    },
}

#[derive(Subcommand)]
pub enum ModIniCmd {
    /// List the INI files the instance's mods ship, and any copies of your own.
    List {
        /// Instance ID.
        instance_id: String,
    },
    /// Show one mod INI file's keys: your copy when you have one, otherwise the mod's file.
    Get {
        /// Instance ID.
        instance_id: String,
        /// The file's path in the game folder, as `mod-ini list` shows it.
        path: String,
    },
    /// Set a key in your copy of a mod's INI file (made from the mod's file on the first edit),
    /// or remove it with --unset. The mod itself is never changed.
    Set {
        /// Instance ID.
        instance_id: String,
        /// The file's path in the game folder, as `mod-ini list` shows it.
        path: String,
        /// The section, e.g. General.
        section: String,
        /// The key.
        key: String,
        /// The value to set. Leave it out with --unset.
        value: Option<String>,
        /// Remove the key instead of setting it.
        #[arg(long, conflicts_with = "value")]
        unset: bool,
    },
    /// The game changed this file: keep the game's version as your copy.
    #[command(name = "keep-game")]
    KeepGame {
        /// Instance ID.
        instance_id: String,
        /// The file's path in the game folder.
        path: String,
    },
    /// Remove your copy of a mod's INI file, so the mod's own file is used again.
    Reset {
        /// Instance ID.
        instance_id: String,
        /// The file's path in the game folder.
        path: String,
    },
    /// The game changed this file: go back to your version and set the game's change aside.
    #[command(name = "restore-mine")]
    RestoreMine {
        /// Instance ID.
        instance_id: String,
        /// The file's path in the game folder.
        path: String,
    },
}

#[derive(Subcommand)]
pub enum UserToolsCmd {
    /// List the tools you added to the instance, and say which are missing now.
    List {
        /// Instance ID.
        instance_id: String,
    },
    /// Look through the instance's enabled mods for programs you could add. Adds nothing.
    Find {
        /// Instance ID.
        instance_id: String,
    },
    /// Add a program to the instance's tools. Arguments for it go after `--`.
    Add {
        /// Instance ID.
        instance_id: String,
        /// The name to show for it.
        name: String,
        /// The program: a path inside the game folder (e.g. Data/Tools/Tool.exe), as `find`
        /// shows it, or a full path to a program outside it.
        exe: String,
        /// Arguments the tool runs with.
        #[arg(last = true)]
        args: Vec<String>,
        /// The tool writes a whole mod, which is added to the instance after a successful run.
        #[arg(long)]
        output_is_mod: bool,
        /// The folder a mod-writing tool writes its mod into.
        #[arg(long, value_name = "DIR")]
        output_folder: Option<String>,
        /// The tool builds files the game reads: keep its output as a build that can be rolled back.
        #[arg(long)]
        keeps_output: bool,
    },
    /// Remove a tool from the instance's list. No files are deleted.
    Remove {
        /// Instance ID.
        instance_id: String,
        /// The tool's ID, from `user-tools list`.
        id: String,
    },
    /// Run one of your tools, the same way `tools run` runs a game's tool.
    Run {
        /// Instance ID.
        instance_id: String,
        /// The tool's ID, from `user-tools list`.
        id: String,
        /// How the run's writes are captured: auto, vfs, links or swap (see `tools run`).
        #[arg(long, default_value = "auto", value_name = "auto|vfs|links|swap")]
        capture: String,
        /// Confirm a swap or a fallback without asking, after it has been shown. Needed when
        /// there is no terminal to ask in.
        #[arg(long)]
        yes: bool,
        /// When a mod-writing tool's folder already has files, move them aside and run.
        #[arg(long)]
        move_aside: bool,
    },
    /// List a mod-writing tool's earlier outputs, newest last.
    Outputs {
        /// Instance ID.
        instance_id: String,
        /// The tool's ID, from `user-tools list`.
        id: String,
    },
    /// Use one of a tool's earlier outputs again.
    #[command(name = "use-output")]
    UseOutput {
        /// Instance ID.
        instance_id: String,
        /// The tool's ID, from `user-tools list`.
        id: String,
        /// The output's item ID, from `user-tools outputs`.
        item_id: String,
    },
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn print_json(value: &impl serde::Serialize) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// The instance's game definition. An unknown instance, or a game this build does not know, fails.
fn instance_game<'a>(ctx: &'a Ctx, json: bool, instance_id: &str) -> &'a GameDefinition {
    let record = match game_instance::get(ctx, instance_id) {
        Ok(Some(record)) => record,
        Ok(None) => ini_fail(json, format!("Instance '{instance_id}' not found.")),
        Err(e) => ini_fail(json, e.to_string()),
    };
    ctx.games.game(&record.game).unwrap_or_else(|| {
        ini_fail(
            json,
            format!(
                "Agora has no game definition for '{}', so this instance cannot be used.",
                record.game
            ),
        )
    })
}

/// Fails when the instance does not exist.
fn require_instance(ctx: &Ctx, json: bool, instance_id: &str) {
    match game_instance::get(ctx, instance_id) {
        Ok(Some(_)) => {}
        Ok(None) => ini_fail(json, format!("Instance '{instance_id}' not found.")),
        Err(e) => ini_fail(json, e.to_string()),
    }
}

/// A game named on the command line. Minecraft instances live under `agora instance`.
fn parse_game(json: bool, game: &str) -> GameId {
    if game.trim().is_empty() {
        ini_fail(json, "No game was named.".to_string());
    }
    if game == GameId::MINECRAFT {
        ini_fail(
            json,
            "Minecraft instances are under `agora instance deleted`, not here.".to_string(),
        );
    }
    GameId::new(game).unwrap_or_else(|e| ini_fail(json, format!("'{game}' is not a game: {e}")))
}

/// A core refusal, in the words the desktop uses for it.
fn instance_error(error: InstanceError) -> String {
    match error {
        InstanceError::NotFound(id) => format!("Instance '{id}' not found."),
        InstanceError::Running(id) => {
            format!("'{id}' is running. Close the game first, then try again.")
        }
        InstanceError::NothingRunning(id) => format!("Nothing is running for '{id}'."),
        InstanceError::NotStartedByAgora(id) => format!(
            "'{id}' is running, but Agora did not start that process, so it will not stop it. Close the game yourself."
        ),
        InstanceError::RestoreIdTaken(id) => format!(
            "An instance called '{id}' exists again. Choose a new name with --name to restore it."
        ),
        InstanceError::AlreadyExists(id) => format!("An instance called '{id}' already exists."),
        InstanceError::MinecraftInstance(id) => {
            format!("'{id}' is a Minecraft instance. Manage it with `agora instance`.")
        }
        other => other.to_string(),
    }
}

/// The OS-trash adapter core calls when a game instance leaves Recently deleted. Core does not
/// depend on the `trash` crate; the desktop has the same adapter.
fn game_trash_adapter() -> game_instance::TrashFn {
    Arc::new(|path: &Path| {
        ::trash::delete(path)
            .map_err(|error| format!("Could not move it to the system trash: {error}"))
    })
}

fn tool_id(json: bool, tool: &str) -> ToolId {
    ToolId::new(tool).unwrap_or_else(|e| ini_fail(json, e.to_string()))
}

fn unix_ms_text(ms: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
        .map(|at| at.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "an unknown time".to_string())
}

// ---------------------------------------------------------------------------
// Rename, clone, stop
// ---------------------------------------------------------------------------

/// `games instance rename`: a new display name. The id and folder stay.
pub fn rename(ctx: &Ctx, json: bool, instance_id: &str, name: &str) -> anyhow::Result<()> {
    let name = name.trim();
    if name.is_empty() {
        ini_fail(json, "Give the instance a name.".to_string());
    }
    require_instance(ctx, json, instance_id);
    game_instance::rename(ctx, instance_id, name)
        .unwrap_or_else(|e| ini_fail(json, instance_error(e)));
    if json {
        print_json(&serde_json::json!({
            "status": "renamed",
            "instance_id": instance_id,
            "name": name,
        }))?;
    } else {
        println!("Renamed instance '{instance_id}' to '{name}'.");
    }
    Ok(())
}

/// `games instance clone`: a new instance with the same content, plugin order, INIs and settings.
pub fn clone(
    ctx: &Ctx,
    json: bool,
    instance_id: &str,
    name: &str,
    copy_saves: bool,
) -> anyhow::Result<()> {
    let definition = instance_game(ctx, json, instance_id);
    let cloned = game_instance::clone_instance(
        ctx,
        instance_id,
        name,
        definition,
        game_instance::CloneOptions { copy_saves },
    )
    .unwrap_or_else(|e| ini_fail(json, instance_error(e)));
    if json {
        print_json(&serde_json::json!({
            "status": "cloned",
            "source_instance_id": instance_id,
            "instance_id": cloned.instance_id,
            "name": cloned.name,
            "copied_saves": copy_saves,
        }))?;
    } else {
        println!(
            "Cloned '{instance_id}' as '{}' ({}).{}",
            cloned.name,
            cloned.instance_id,
            if copy_saves {
                " Its saves were copied too."
            } else {
                " It starts with no saves of its own."
            }
        );
    }
    Ok(())
}

/// `games instance stop`: end the game Agora started for the instance, then close the session.
pub fn stop(ctx: &Ctx, json: bool, instance_id: &str) -> anyhow::Result<()> {
    let definition = instance_game(ctx, json, instance_id);
    let stopped = game_instance::stop(ctx, instance_id, definition)
        .unwrap_or_else(|e| ini_fail(json, instance_error(e)));
    if json {
        print_json(&serde_json::json!({
            "status": "stopped",
            "instance_id": stopped.instance_id,
            "processes_ended": stopped.processes_ended,
            "play_time_ms": stopped.play_time_ms,
        }))?;
    } else {
        println!(
            "Stopped '{}': ended {} process(es). This session lasted {} minute(s).",
            stopped.instance_id,
            stopped.processes_ended,
            stopped.play_time_ms / 60_000
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Recently deleted
// ---------------------------------------------------------------------------

/// `games instance deleted`: a game's instances in Recently deleted, newest first. Old entries
/// past the retention window are cleaned up first, as the desktop does when the list is opened.
pub fn deleted(ctx: &Ctx, json: bool, game: &str) -> anyhow::Result<()> {
    let game = parse_game(json, game);
    // The retention pass runs when someone looks, as on the desktop, so old entries are cleaned
    // up even for someone who never deletes anything.
    let _ = game_instance::trash::cleanup(
        ctx,
        chrono::DateTime::<chrono::Utc>::from(ctx.clock.now()),
        &game_trash_adapter(),
    );
    let entries = game_instance::trash::list(ctx, game.as_str());
    let days = game_instance::trash::retention_days(ctx);
    if json {
        print_json(&serde_json::json!({
            "game": game.as_str(),
            "retention_days": days,
            "entries": entries,
        }))?;
    } else if entries.is_empty() {
        println!(
            "Recently deleted has no {} instances. A deleted instance stays there for {days} day(s).",
            game.as_str()
        );
    } else {
        let rows: Vec<Vec<String>> = entries
            .iter()
            .map(|e| {
                vec![
                    e.entry_id.clone(),
                    e.name.clone(),
                    e.deleted_at.clone(),
                    e.expires_at.clone(),
                    format_size(e.size_bytes),
                ]
            })
            .collect();
        print_table(&["ID", "Name", "Deleted", "Kept until", "Size"], &rows);
    }
    Ok(())
}

/// `games instance restore`: bring a deleted instance back, under its old id, or under a new name.
pub fn restore(ctx: &Ctx, json: bool, entry_id: &str, name: Option<&str>) -> anyhow::Result<()> {
    let restored = game_instance::trash::restore(ctx, entry_id, name)
        .unwrap_or_else(|e| ini_fail(json, instance_error(e)));
    if json {
        print_json(&serde_json::json!({
            "status": "restored",
            "entry_id": entry_id,
            "instance_id": restored.instance_id,
            "original_instance_id": restored.original_instance_id,
            "renamed": restored.renamed,
        }))?;
    } else if restored.renamed {
        println!(
            "Restored '{}' as '{}'.",
            restored.original_instance_id, restored.instance_id
        );
    } else {
        println!("Restored instance '{}'.", restored.instance_id);
    }
    Ok(())
}

/// `games instance purge`: send one deleted entry, or every deleted entry of a game, to the OS
/// trash. Asks first in a terminal; without one it needs `--yes`, like `agora instance purge`.
pub fn purge(
    ctx: &Ctx,
    json: bool,
    game: &str,
    entry_id: Option<&str>,
    yes: bool,
) -> anyhow::Result<()> {
    let game = parse_game(json, game);
    let entries = game_instance::trash::list(ctx, game.as_str());
    let what = match entry_id {
        Some(entry_id) => match entries.iter().find(|e| e.entry_id == entry_id) {
            Some(entry) => format!("'{}'", entry.name),
            None => ini_fail(
                json,
                format!(
                    "No deleted {} instance with ID '{entry_id}'. Run `agora games instance deleted {}` to see them.",
                    game.as_str(),
                    game.as_str()
                ),
            ),
        },
        None if entries.is_empty() => {
            if json {
                print_json(&serde_json::json!({
                    "status": "purged", "purged": 0, "failed": 0, "kept": 0,
                }))?;
            } else {
                println!("Recently deleted has no {} instances.", game.as_str());
            }
            return Ok(());
        }
        None => format!(
            "all {} deleted {} instance(s)",
            entries.len(),
            game.as_str()
        ),
    };
    if !yes && !confirm_purge(&what)? {
        if json {
            print_json(&serde_json::json!({"status": "kept"}))?;
        } else {
            println!("Left as it is.");
        }
        return Ok(());
    }
    let summary =
        game_instance::trash::purge_for_game(ctx, game.as_str(), entry_id, &game_trash_adapter())
            .unwrap_or_else(|e| ini_fail(json, instance_error(e)));
    if json {
        print_json(&serde_json::json!({
            "status": "purged",
            "purged": summary.purged,
            "failed": summary.failed,
            "kept": summary.kept,
        }))?;
    } else {
        println!(
            "Sent {} to the OS trash. Agora cannot restore it now.",
            if entry_id.is_some() {
                what
            } else {
                format!("{} instance(s)", summary.purged)
            }
        );
        if summary.failed > 0 {
            println!(
                "{} could not be moved and are still in Recently deleted.",
                summary.failed
            );
        }
    }
    if summary.failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

/// Ask before a purge, which cannot be undone. Without a terminal it refuses, so a script has to
/// say `--yes` on purpose.
fn confirm_purge(what: &str) -> anyhow::Result<bool> {
    if !std::io::stdin().is_terminal() {
        anyhow::bail!(
            "Purging {what} needs confirmation. Run it in an interactive terminal, or pass --yes once the person using this computer has agreed."
        );
    }
    eprint!("Send {what} to the OS trash now? Agora cannot restore it afterwards. [y/N]: ");
    std::io::stderr().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    Ok(matches!(
        input.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

// ---------------------------------------------------------------------------
// Save backups
// ---------------------------------------------------------------------------

fn backup_line(backup: &SaveBackup) -> String {
    format!(
        "{}  {}  {} file(s), {}  from {}",
        backup.id,
        backup.created_at,
        backup.file_count,
        format_size(backup.bytes),
        backup.source.display()
    )
}

/// `games instance save-backup create|list|restore`.
pub fn save_backup_command(ctx: &Ctx, json: bool, action: SaveBackupCmd) -> anyhow::Result<()> {
    match action {
        SaveBackupCmd::Create {
            instance_id,
            session,
        } => {
            let definition = instance_game(ctx, json, &instance_id);
            let backup = save_backup::backup(ctx, &instance_id, definition)
                .unwrap_or_else(|e| ini_fail(json, e.to_string()));
            if let Some(session) = session.as_deref() {
                let conn = agora_core::db::local_state_connection(&ctx.paths.local_state_db())
                    .unwrap_or_else(|e| ini_fail(json, e.to_string()));
                save_backup::set_session_backup(&conn, &instance_id, session, &backup.id)
                    .unwrap_or_else(|e| ini_fail(json, e.to_string()));
            }
            if json {
                print_json(&serde_json::json!({
                    "instance_id": instance_id,
                    "session": session,
                    "backup": backup,
                }))?;
            } else {
                println!(
                    "Backed up {} save file(s) ({}) from {}.",
                    backup.file_count,
                    format_size(backup.bytes),
                    backup.source.display()
                );
                println!(
                    "To put them back: agora games instance save-backup restore {instance_id} {}",
                    backup.id
                );
            }
        }
        SaveBackupCmd::List {
            instance_id,
            session: Some(session),
        } => {
            require_instance(ctx, json, &instance_id);
            let conn = agora_core::db::local_state_connection(&ctx.paths.local_state_db())
                .unwrap_or_else(|e| ini_fail(json, e.to_string()));
            let backup_id = save_backup::session_backup(&conn, &instance_id, &session)
                .unwrap_or_else(|e| ini_fail(json, e.to_string()));
            if json {
                print_json(&serde_json::json!({
                    "instance_id": instance_id,
                    "session": session,
                    "backup_id": backup_id,
                }))?;
            } else {
                match backup_id {
                    Some(id) => println!("Session '{session}' was backed up as {id}."),
                    None => println!("No save backup was recorded for session '{session}'."),
                }
            }
        }
        SaveBackupCmd::List {
            instance_id,
            session: None,
        } => {
            require_instance(ctx, json, &instance_id);
            let backups = save_backup::list(ctx, &instance_id)
                .unwrap_or_else(|e| ini_fail(json, e.to_string()));
            if json {
                print_json(&serde_json::json!({
                    "instance_id": instance_id,
                    "backups": backups,
                }))?;
            } else if backups.is_empty() {
                println!("Instance '{instance_id}' has no save backups.");
            } else {
                for backup in &backups {
                    println!("{}", backup_line(backup));
                }
            }
        }
        SaveBackupCmd::Restore {
            instance_id,
            backup_id,
        } => {
            let definition = instance_game(ctx, json, &instance_id);
            let restored = save_backup::restore(ctx, &instance_id, definition, &backup_id)
                .unwrap_or_else(|e| ini_fail(json, e.to_string()));
            if json {
                print_json(&restored)?;
            } else {
                println!(
                    "Put back {} save file(s) from backup {} into {}.",
                    restored.restored,
                    restored.backup_id,
                    restored.folder.display()
                );
                if !restored.kept_current.is_empty() {
                    println!(
                        "Your current copies of {} file(s) were kept beside them as .before-restore:",
                        restored.kept_current.len()
                    );
                    for name in &restored.kept_current {
                        println!("  {name}");
                    }
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// INI files that mods ship
// ---------------------------------------------------------------------------

fn mod_ini_source(source: ModIniSource) -> &'static str {
    match source {
        ModIniSource::Writable => "writable",
        ModIniSource::Override => "override",
        ModIniSource::Mod => "mod",
    }
}

fn mod_ini_contents(read: &ModIniRead) -> serde_json::Value {
    serde_json::json!({
        "path": read.path,
        "source": mod_ini_source(read.source),
        "entries": read.document.entries(),
    })
}

fn print_mod_ini(read: &ModIniRead) {
    let from = match read.source {
        ModIniSource::Writable => "the game's own change",
        ModIniSource::Override => "your copy",
        ModIniSource::Mod => "the mod's file",
    };
    println!("{} (from {from}):", read.path);
    let entries = read.document.entries();
    if entries.is_empty() {
        println!("  It sets no keys.");
    }
    let mut section: Option<&str> = None;
    for entry in &entries {
        if section != Some(entry.section.as_str()) {
            println!("[{}]", entry.section);
            section = Some(entry.section.as_str());
        }
        println!("  {} = {}", entry.key, entry.value);
    }
}

fn print_mod_inis(rows: &[ModIni]) {
    if rows.is_empty() {
        println!("The instance's enabled mods ship no INI files.");
        return;
    }
    for row in rows {
        let from = if row.mod_name.is_empty() {
            "no mod ships it any more".to_string()
        } else {
            format!("from {}", row.mod_name)
        };
        let mut notes = Vec::new();
        if row.overridden {
            notes.push("you have your own copy".to_string());
        }
        if !row.mod_enabled {
            notes.push("its mod is turned off".to_string());
        }
        if let Some(by) = &row.shadowed_by {
            notes.push(format!("{by} ships it too"));
        }
        if row.game_changed {
            notes.push("the game changed it: use keep-game or restore-mine".to_string());
        }
        let notes = if notes.is_empty() {
            String::new()
        } else {
            format!(" ({})", notes.join("; "))
        };
        println!("{}  {from}{notes}", row.path);
    }
}

fn list_mod_inis_or_fail(ctx: &Ctx, json: bool, instance_id: &str) -> Vec<ModIni> {
    game_mod_ini::list_mod_inis(ctx, instance_id).unwrap_or_else(|e| ini_fail(json, e.to_string()))
}

/// `games instance mod-ini …`.
pub fn mod_ini_command(ctx: &Ctx, json: bool, action: ModIniCmd) -> anyhow::Result<()> {
    match action {
        ModIniCmd::List { instance_id } => {
            require_instance(ctx, json, &instance_id);
            let rows = list_mod_inis_or_fail(ctx, json, &instance_id);
            if json {
                print_json(&serde_json::json!({
                    "instance_id": instance_id,
                    "files": rows,
                }))?;
            } else {
                print_mod_inis(&rows);
            }
        }
        ModIniCmd::Get { instance_id, path } => {
            require_instance(ctx, json, &instance_id);
            let read = game_mod_ini::read_mod_ini(ctx, &instance_id, &path)
                .unwrap_or_else(|e| ini_fail(json, e.to_string()));
            if json {
                print_json(&mod_ini_contents(&read))?;
            } else {
                print_mod_ini(&read);
            }
        }
        ModIniCmd::Set {
            instance_id,
            path,
            section,
            key,
            value,
            unset,
        } => {
            require_instance(ctx, json, &instance_id);
            let changed = match (&value, unset) {
                (Some(value), false) => {
                    game_mod_ini::set_mod_ini_value(ctx, &instance_id, &path, &section, &key, value)
                }
                (None, true) => {
                    game_mod_ini::unset_mod_ini_value(ctx, &instance_id, &path, &section, &key)
                }
                _ => ini_fail(
                    json,
                    "Give a value to set, or --unset to remove the key.".to_string(),
                ),
            }
            .unwrap_or_else(|e| ini_fail(json, e.to_string()));
            let read = game_mod_ini::read_mod_ini(ctx, &instance_id, &path)
                .unwrap_or_else(|e| ini_fail(json, e.to_string()));
            if json {
                print_json(&serde_json::json!({
                    "changed": changed,
                    "file": mod_ini_contents(&read),
                }))?;
            } else {
                match (&value, changed) {
                    (_, false) => println!("Nothing changed: the file already says that."),
                    (Some(value), true) => println!(
                        "Set [{section}] {key} = {value} in your copy of {path}. The mod's file is unchanged."
                    ),
                    (None, true) => println!(
                        "Removed [{section}] {key} from your copy of {path}. The mod's file is unchanged."
                    ),
                }
            }
        }
        ModIniCmd::KeepGame { instance_id, path } => {
            require_instance(ctx, json, &instance_id);
            game_mod_ini::keep_game_version(ctx, &instance_id, &path)
                .unwrap_or_else(|e| ini_fail(json, e.to_string()));
            mod_ini_done(
                ctx,
                json,
                &instance_id,
                &format!("Kept the game's version of {path} as your copy."),
            )?;
        }
        ModIniCmd::Reset { instance_id, path } => {
            require_instance(ctx, json, &instance_id);
            game_mod_ini::reset_mod_ini(ctx, &instance_id, &path)
                .unwrap_or_else(|e| ini_fail(json, e.to_string()));
            mod_ini_done(
                ctx,
                json,
                &instance_id,
                &format!("Removed your copy of {path}. The mod's own file is used again."),
            )?;
        }
        ModIniCmd::RestoreMine { instance_id, path } => {
            require_instance(ctx, json, &instance_id);
            game_mod_ini::restore_mine(ctx, &instance_id, &path)
                .unwrap_or_else(|e| ini_fail(json, e.to_string()));
            mod_ini_done(
                ctx,
                json,
                &instance_id,
                &format!("Went back to your version of {path}. The game's change was set aside."),
            )?;
        }
    }
    Ok(())
}

/// After a change to one mod INI: the files as they are now.
fn mod_ini_done(ctx: &Ctx, json: bool, instance_id: &str, message: &str) -> anyhow::Result<()> {
    let rows = list_mod_inis_or_fail(ctx, json, instance_id);
    if json {
        print_json(&serde_json::json!({
            "instance_id": instance_id,
            "files": rows,
        }))
    } else {
        println!("{message}");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// A tool's earlier outputs and its inputs (`games instance tools …`)
// ---------------------------------------------------------------------------

fn print_outputs(
    json: bool,
    instance_id: &str,
    tool: &str,
    records: &[game_tools::ToolOutputRecord],
) -> anyhow::Result<()> {
    if json {
        return print_json(&serde_json::json!({
            "instance_id": instance_id,
            "tool": tool,
            "outputs": records,
        }));
    }
    if records.is_empty() {
        println!("{tool} has no earlier outputs in this instance.");
    }
    for record in records {
        println!(
            "{}  {}  added {}",
            record.item_id,
            record.name,
            unix_ms_text(record.added_at_unix_ms)
        );
    }
    Ok(())
}

/// `games instance tools outputs`: a mod-writing tool's earlier outputs, newest last.
pub fn tool_outputs(ctx: &Ctx, json: bool, instance_id: &str, tool: &str) -> anyhow::Result<()> {
    let tool = tool_id(json, tool);
    let records = game_tools::list_tool_outputs(ctx, instance_id, &tool)
        .unwrap_or_else(|e| ini_fail(json, e.to_string()));
    print_outputs(json, instance_id, tool.as_str(), &records)
}

/// `games instance tools use-output`: point the tool's layer back at an earlier output.
pub fn tool_use_output(
    ctx: &Ctx,
    json: bool,
    instance_id: &str,
    definition: &GameDefinition,
    tool: &str,
    item_id: &str,
) -> anyhow::Result<()> {
    let tool = tool_id(json, tool);
    let records = game_tools::use_tool_output(ctx, instance_id, definition, &tool, item_id)
        .unwrap_or_else(|e| ini_fail(json, e.to_string()));
    if !json {
        println!("{tool} uses output {item_id} again. The game reads it on its next launch.");
    }
    print_outputs(json, instance_id, tool.as_str(), &records)
}

/// `games instance tools set-input`: save a value for one of a tool's inputs. A blank or missing
/// value clears it, so the input's default applies again.
pub fn tool_set_input(
    ctx: &Ctx,
    json: bool,
    instance_id: &str,
    definition: &GameDefinition,
    tool: &str,
    input_id: &str,
    value: Option<&str>,
) -> anyhow::Result<()> {
    let tool = tool_id(json, tool);
    let value = value.unwrap_or("");
    game_tools::save_tool_input(ctx, instance_id, definition, &tool, input_id, value)
        .unwrap_or_else(|e| ini_fail(json, e.to_string()));
    let cleared = value.trim().is_empty();
    if json {
        print_json(&serde_json::json!({
            "instance_id": instance_id,
            "tool": tool.as_str(),
            "input": input_id,
            "value": if cleared { None } else { Some(value) },
        }))?;
    } else if cleared {
        println!("Cleared {tool}'s '{input_id}'. Its default applies again.");
    } else {
        println!("Saved {tool}'s '{input_id}' as '{value}'.");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The instance's own tools (`games instance user-tools …`)
// ---------------------------------------------------------------------------

fn user_tool_json(tool: &UserTool, present: bool) -> serde_json::Value {
    serde_json::json!({
        "id": tool.id,
        "tool_id": game_tools::user_tool_id(&tool.id).ok().map(|id| id.to_string()),
        "name": tool.name,
        "exe": tool.exe,
        "args": tool.args,
        "known_tool_id": tool.known_tool_id,
        "output_is_mod": tool.output_is_mod,
        "output_folder": tool.output_folder,
        "keeps_output": tool.keeps_output,
        "missing": !present,
    })
}

fn print_user_tools(
    json: bool,
    instance_id: &str,
    tools: &[(UserTool, bool)],
) -> anyhow::Result<()> {
    if json {
        let rows: Vec<_> = tools
            .iter()
            .map(|(tool, present)| user_tool_json(tool, *present))
            .collect();
        return print_json(&serde_json::json!({
            "instance_id": instance_id,
            "tools": rows,
        }));
    }
    if tools.is_empty() {
        println!(
            "Instance '{instance_id}' has no tools of your own. `user-tools find` suggests some."
        );
    }
    for (tool, present) in tools {
        let args = if tool.args.is_empty() {
            String::new()
        } else {
            format!(" {}", tool.args.join(" "))
        };
        let missing = if *present {
            ""
        } else {
            " (missing: its mod is off or removed, or the file moved)"
        };
        println!("{}  {}  {}{args}{missing}", tool.id, tool.name, tool.exe);
    }
    Ok(())
}

/// `games instance user-tools …`.
pub fn user_tools_command(ctx: &Ctx, json: bool, action: UserToolsCmd) -> anyhow::Result<()> {
    match action {
        UserToolsCmd::List { instance_id } => {
            let definition = instance_game(ctx, json, &instance_id);
            let tools = game_user_tools::list_with_presence(ctx, &instance_id, definition)
                .unwrap_or_else(|e| ini_fail(json, e.to_string()));
            print_user_tools(json, &instance_id, &tools)?;
        }
        UserToolsCmd::Find { instance_id } => {
            let definition = instance_game(ctx, json, &instance_id);
            let candidates = game_user_tools::find_candidates(ctx, &instance_id, definition)
                .unwrap_or_else(|e| ini_fail(json, e.to_string()));
            if json {
                print_json(&serde_json::json!({
                    "instance_id": instance_id,
                    "candidates": candidates,
                }))?;
            } else if candidates.is_empty() {
                println!("No programs were found in the instance's enabled mods.");
            } else {
                for candidate in &candidates {
                    let from = if candidate.mod_name.is_empty() {
                        String::new()
                    } else {
                        format!(" from {}", candidate.mod_name)
                    };
                    let added = if candidate.already_added {
                        " (already added)"
                    } else {
                        ""
                    };
                    println!(
                        "{}{from}: suggested name '{}'{added}",
                        candidate.exe, candidate.suggested_name
                    );
                }
                println!(
                    "To add one: agora games instance user-tools add {instance_id} <NAME> <EXE>"
                );
            }
        }
        UserToolsCmd::Add {
            instance_id,
            name,
            exe,
            args,
            output_is_mod,
            output_folder,
            keeps_output,
        } => {
            require_instance(ctx, json, &instance_id);
            let tool = game_user_tools::add(
                ctx,
                &instance_id,
                NewUserTool {
                    name,
                    exe,
                    args,
                    output_is_mod,
                    output_folder,
                    keeps_output,
                },
            )
            .unwrap_or_else(|e| ini_fail(json, e.to_string()));
            if json {
                // `add` accepts only a program that is there, so a new tool is never missing.
                print_json(&user_tool_json(&tool, true))?;
            } else {
                println!(
                    "Added '{}' as {}. Run it with: agora games instance user-tools run {instance_id} {}",
                    tool.name, tool.id, tool.id
                );
            }
        }
        UserToolsCmd::Remove { instance_id, id } => {
            let definition = instance_game(ctx, json, &instance_id);
            game_user_tools::remove(ctx, &instance_id, &id)
                .unwrap_or_else(|e| ini_fail(json, e.to_string()));
            if json {
                let tools = game_user_tools::list_with_presence(ctx, &instance_id, definition)
                    .unwrap_or_else(|e| ini_fail(json, e.to_string()));
                print_user_tools(json, &instance_id, &tools)?;
            } else {
                println!("Removed {id} from the instance's tools. No files were deleted.");
            }
        }
        UserToolsCmd::Run {
            instance_id,
            id,
            capture,
            yes,
            move_aside,
        } => {
            let definition = instance_game(ctx, json, &instance_id);
            run_user_tool(
                ctx,
                json,
                &instance_id,
                definition,
                &id,
                &capture,
                yes,
                move_aside,
            )?;
        }
        UserToolsCmd::Outputs { instance_id, id } => {
            let definition = instance_game(ctx, json, &instance_id);
            let tool = game_tools::user_tool_output_id(ctx, &instance_id, definition, &id)
                .unwrap_or_else(|e| ini_fail(json, e.to_string()));
            let records = game_tools::list_tool_outputs(ctx, &instance_id, &tool)
                .unwrap_or_else(|e| ini_fail(json, e.to_string()));
            print_outputs(json, &instance_id, &id, &records)?;
        }
        UserToolsCmd::UseOutput {
            instance_id,
            id,
            item_id,
        } => {
            let definition = instance_game(ctx, json, &instance_id);
            let tool = game_tools::user_tool_output_id(ctx, &instance_id, definition, &id)
                .unwrap_or_else(|e| ini_fail(json, e.to_string()));
            let records =
                game_tools::use_tool_output(ctx, &instance_id, definition, &tool, &item_id)
                    .unwrap_or_else(|e| ini_fail(json, e.to_string()));
            if !json {
                println!("{id} uses output {item_id} again. The game reads it on its next launch.");
            }
            print_outputs(json, &instance_id, &id, &records)?;
        }
    }
    Ok(())
}

/// The command that runs a user tool by a capture, as the CLI prints it. A swap is printed with
/// `--yes`, because a person runs it after its costs have been shown.
fn user_tool_run_command(instance_id: &str, id: &str, capture: CaptureMode) -> String {
    let yes = if capture == CaptureMode::Swap {
        " --yes"
    } else {
        ""
    };
    format!(
        "agora games instance user-tools run {instance_id} {id} --capture {}{yes}",
        capture.as_str()
    )
}

/// The swap's costs, shown before a user tool runs by swap, as `tools run` shows them (see
/// `confirm_swap` in main.rs). `Ok(true)` runs the swap.
fn confirm_user_tool_swap(
    ctx: &Ctx,
    json: bool,
    instance_id: &str,
    definition: &GameDefinition,
    id: &str,
    name: &str,
    yes: bool,
) -> anyhow::Result<bool> {
    let command = user_tool_run_command(instance_id, id, CaptureMode::Swap);
    let costs = agora_core::game_tool_copy::swap_costs(ctx, instance_id, definition)
        .unwrap_or_else(|e| ini_fail(json, e.to_string()));
    let lines = swap_cost_lines(name, &costs);
    if !json {
        for line in &lines {
            eprintln!("{line}");
        }
    }
    if yes {
        return Ok(true);
    }
    if json {
        eprintln!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "status": "error",
                "error": lines.join(" "),
                "exitCode": 1,
                "retry": [command],
            }))?
        );
        std::process::exit(1);
    }
    if std::io::stdin().is_terminal() && ask_yes_no("Run it by swap? [y/N]: ")? {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        eprintln!(
            "There is no terminal to ask in, so the swap was not run. Confirm it with --yes:"
        );
    }
    eprintln!("To run it by swap:");
    eprintln!("  {command}");
    Ok(false)
}

/// The virtual file system could not start for a user tool, and nothing ran. An explicit `vfs`
/// refuses; `auto` says what happened and asks before it runs from `next` (`--yes` confirms it).
/// `Ok(())` means the run goes on from `next`.
#[allow(clippy::too_many_arguments)]
fn user_tool_vfs_fallback(
    ctx: &Ctx,
    json: bool,
    instance_id: &str,
    definition: &GameDefinition,
    id: &str,
    name: &str,
    error: &ToolError,
    next: CaptureMode,
    explicit_vfs: bool,
    yes: bool,
) -> anyhow::Result<()> {
    let step = if explicit_vfs {
        VfsFallbackStep::Stop
    } else {
        vfs_fallback_step(json, yes, std::io::stdin().is_terminal())
    };
    if json && step == VfsFallbackStep::Run {
        return Ok(());
    }
    let command = user_tool_run_command(instance_id, id, next);
    let offers = vfs_refusal_offers(error);
    let cost_lines = match next {
        CaptureMode::Swap => {
            let costs = agora_core::game_tool_copy::swap_costs(ctx, instance_id, definition)
                .unwrap_or_else(|e| ini_fail(json, e.to_string()));
            swap_cost_lines(name, &costs)
        }
        _ => Vec::new(),
    };
    if json {
        let out = vfs_tool_envelope(&error.to_string(), next, &command, &cost_lines, &offers);
        eprintln!("{}", serde_json::to_string_pretty(&out)?);
        std::process::exit(1);
    }
    eprintln!("Error: {error}");
    for line in &cost_lines {
        eprintln!("{line}");
    }
    for line in offer_lines(instance_id, id, name, &offers, next == CaptureMode::Swap) {
        eprintln!("{line}");
    }
    match step {
        VfsFallbackStep::Run => return Ok(()),
        VfsFallbackStep::Ask => {
            if ask_yes_no(&format!("Try {} now? [y/N]: ", tool_tier_name(next)))? {
                return Ok(());
            }
        }
        VfsFallbackStep::Stop => {}
    }
    eprintln!("To run {name} by {} instead:", tool_tier_name(next));
    eprintln!("  {command}");
    std::process::exit(1);
}

/// What a user tool that keeps failing can try next, as lines to print. Nothing is applied here.
fn offer_lines(
    instance_id: &str,
    id: &str,
    name: &str,
    offers: &[ToolOffer],
    costs_shown: bool,
) -> Vec<String> {
    let mut lines = Vec::new();
    for offer in offers {
        match offer {
            ToolOffer::CopySwap { costs } => {
                if !costs_shown {
                    lines.extend(swap_cost_lines(name, costs));
                }
                lines.push(format!(
                    "To try the full-copy swap: {}",
                    user_tool_run_command(instance_id, id, CaptureMode::Swap)
                ));
            }
            ToolOffer::SwitchToLinks => lines.extend(switch_to_links_lines(instance_id)),
        }
    }
    lines
}

/// `games instance user-tools run`: the same pipeline and questions as `tools run`. A run that does
/// not promote its output exits 1.
#[allow(clippy::too_many_arguments)]
fn run_user_tool(
    ctx: &Ctx,
    json: bool,
    instance_id: &str,
    definition: &GameDefinition,
    id: &str,
    capture: &str,
    yes: bool,
    move_aside: bool,
) -> anyhow::Result<()> {
    let capture = CaptureMode::parse(capture).unwrap_or_else(|| {
        ini_fail(
            json,
            format!("--capture must be auto, vfs, links or swap, not '{capture}'"),
        )
    });
    let name = game_user_tools::list(ctx, instance_id)
        .unwrap_or_else(|e| ini_fail(json, e.to_string()))
        .into_iter()
        .find(|tool| tool.id == id)
        .map(|tool| tool.name)
        .unwrap_or_else(|| {
            ini_fail(
                json,
                format!(
                    "Instance '{instance_id}' has no tool '{id}'. Run `agora games instance user-tools list {instance_id}` to see them."
                ),
            )
        });
    let cancel = CancellationToken::new();
    let launcher = &SystemLauncher;
    // `confirmed` is passed true only on the paths below that showed the swap's costs and got a yes.
    let run_as = |capture: CaptureMode, confirmed: bool| {
        game_tools::run_user_tool(
            ctx,
            instance_id,
            definition,
            id,
            capture,
            confirmed,
            move_aside,
            launcher,
            &cancel,
        )
    };
    let confirm = || confirm_user_tool_swap(ctx, json, instance_id, definition, id, &name, yes);
    let result: Result<RunOutcome, ToolError> = if capture == CaptureMode::Swap {
        if !confirm()? {
            std::process::exit(1);
        }
        run_as(capture, true)
    } else {
        match run_as(capture, false) {
            Err(ToolError::InstallPathNeedsSwap { .. }) => {
                if !confirm()? {
                    std::process::exit(1);
                }
                run_as(CaptureMode::Swap, true)
            }
            Err(e) => match vfs_next_tier(&e) {
                Some(next) => {
                    user_tool_vfs_fallback(
                        ctx,
                        json,
                        instance_id,
                        definition,
                        id,
                        &name,
                        &e,
                        next,
                        capture != CaptureMode::Auto,
                        yes,
                    )?;
                    // The next tier runs, and its outcome says the virtual file system failed first.
                    game_tools::run_user_tool_after_vfs_refusal(
                        ctx,
                        instance_id,
                        definition,
                        id,
                        next,
                        // The fallback above showed the swap's costs and got a yes.
                        true,
                        &vfs_refusal_reason(&e),
                        move_aside,
                        launcher,
                        &cancel,
                    )
                    .map(|mut out| {
                        if json {
                            merge_offers(&mut out.offers, vfs_refusal_offers(&e));
                        }
                        out
                    })
                }
                None => Err(e),
            },
            ok => ok,
        }
    };
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(ToolError::OutputFolderNotEmpty { folder, aside, .. }) => ini_fail(
            json,
            format!(
                "{name} would write into '{folder}', but that folder already has files. Nothing ran. To move them aside to '{aside}' and run, add --move-aside."
            ),
        ),
        Err(e) => ini_fail(json, e.to_string()),
    };
    if json {
        print_json(&outcome)?;
    } else {
        print_tool_run(&outcome);
        for line in offer_lines(instance_id, id, &name, &outcome.offers, false) {
            println!("{line}");
        }
    }
    if !outcome.promoted {
        std::process::exit(1);
    }
    Ok(())
}
