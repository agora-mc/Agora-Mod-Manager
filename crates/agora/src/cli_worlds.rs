//! `agora world …` and `agora server …`: an instance's worlds and its in-game
//! server list. Thin adapters over `agora_game_minecraft::worlds::WorldService`, which
//! the desktop Worlds & Servers tab uses too.

use std::path::PathBuf;

use agora_core::ctx::Ctx;
use agora_game_minecraft::worlds::WorldService;
use clap::Subcommand;

use crate::{print_table, OutputFormat};

#[derive(Subcommand)]
pub enum WorldCmd {
    /// List an instance's worlds, most recently played first.
    List { instance: String },
    /// Change the name a world shows in game (its folder is kept).
    Rename {
        instance: String,
        /// The world's folder name under saves/.
        world: String,
        name: String,
        #[arg(
            long,
            help = "Change the world even while the game is running (the running game may overwrite it when it saves)"
        )]
        while_running: bool,
    },
    /// Copy a world next to itself.
    Copy {
        instance: String,
        world: String,
        #[arg(long, help = "Change the world even while the game is running")]
        while_running: bool,
    },
    /// Move a world to the system recycle bin.
    Delete {
        instance: String,
        world: String,
        #[arg(long, help = "Do not ask for confirmation")]
        yes: bool,
        #[arg(long, help = "Change the world even while the game is running")]
        while_running: bool,
    },
    /// Save a world as a zip archive.
    Export {
        instance: String,
        world: String,
        #[arg(long, help = "Where to write the .zip")]
        out: PathBuf,
        #[arg(long, help = "Read the world even while the game is running")]
        while_running: bool,
    },
    /// Add a world from a zip archive.
    Import {
        instance: String,
        archive: PathBuf,
        #[arg(long, help = "Change the instance even while the game is running")]
        while_running: bool,
    },
}

#[derive(Subcommand)]
pub enum ServerCmd {
    /// List the instance's multiplayer servers. Positions start at 1.
    List {
        instance: String,
        #[arg(long, help = "Also ask each server whether it is online")]
        ping: bool,
    },
    /// Add a server to the end of the list.
    Add {
        instance: String,
        /// host or host:port
        address: String,
        #[arg(long, help = "Name shown in the multiplayer list")]
        name: Option<String>,
        #[arg(long, help = "Change the list even while the game is running")]
        while_running: bool,
    },
    /// Remove the server at a position shown by `server list`.
    Remove {
        instance: String,
        position: usize,
        #[arg(long, help = "Change the list even while the game is running")]
        while_running: bool,
    },
    /// Rename a server or change its address.
    Edit {
        instance: String,
        /// Its position from `server list`, or its name.
        server: String,
        #[arg(long, help = "New name shown in the multiplayer list")]
        name: Option<String>,
        #[arg(long, help = "New address (host or host:port)")]
        address: Option<String>,
        #[arg(long, help = "Change the list even while the game is running")]
        while_running: bool,
    },
    /// Move a server one place up or down the list.
    Move {
        instance: String,
        /// Its position from `server list`, or its name.
        server: String,
        #[arg(value_enum)]
        direction: crate::cli_misc_parity::MoveDirection,
        #[arg(long, help = "Change the list even while the game is running")]
        while_running: bool,
    },
    /// Ask any server whether it is online, without adding it.
    Ping {
        /// host or host:port
        address: String,
    },
}

fn forge(service: &WorldService, while_running: bool) -> WorldService {
    if while_running {
        service.clone().while_running()
    } else {
        service.clone()
    }
}

pub fn run_world_command(
    action: WorldCmd,
    ctx: &Ctx,
    trash: &agora_game_minecraft::instance_service::TrashFn,
    output_fmt: OutputFormat,
) -> anyhow::Result<()> {
    let json = output_fmt.is_json_output();
    let service = WorldService::new(ctx.clone());
    let done = |message: String, value: serde_json::Value| -> anyhow::Result<()> {
        if json {
            println!("{}", serde_json::to_string_pretty(&value)?);
        } else {
            println!("{message}");
        }
        Ok(())
    };
    match action {
        WorldCmd::List { instance } => {
            let worlds = service.list(&instance)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&worlds)?);
            } else if worlds.is_empty() {
                println!("No worlds.");
            } else {
                let rows: Vec<Vec<String>> = worlds
                    .iter()
                    .map(|world| {
                        vec![
                            world.folder.clone(),
                            world.name.clone(),
                            world.game_mode.clone().unwrap_or_default(),
                            world.version_name.clone().unwrap_or_default(),
                            format!("{:.1} MB", world.size_bytes as f64 / 1_048_576.0),
                        ]
                    })
                    .collect();
                print_table(&["Folder", "Name", "Mode", "Version", "Size"], &rows);
            }
        }
        WorldCmd::Rename {
            instance,
            world,
            name,
            while_running,
        } => {
            let service = forge(&service, while_running);
            service.rename(&instance, &world, &name)?;
            done(
                format!("Renamed '{world}' to \"{}\".", name.trim()),
                serde_json::json!({ "world": world, "name": name.trim() }),
            )?;
        }
        WorldCmd::Copy {
            instance,
            world,
            while_running,
        } => {
            let service = forge(&service, while_running);
            let folder = service.copy(&instance, &world)?;
            done(
                format!("Copied '{world}' to '{folder}'."),
                serde_json::json!({ "world": world, "copy": folder }),
            )?;
        }
        WorldCmd::Delete {
            instance,
            world,
            yes,
            while_running,
        } => {
            if !yes
                && !crate::cli_tools::confirm(
                    &format!("Move world '{world}' to the recycle bin? [y/N] "),
                    "Deleting a world needs confirmation; pass --yes.",
                )?
            {
                anyhow::bail!("Cancelled.");
            }
            forge(&service, while_running).delete(&instance, &world, trash)?;
            done(
                format!("Moved '{world}' to the recycle bin."),
                serde_json::json!({ "world": world, "deleted": true }),
            )?;
        }
        WorldCmd::Export {
            instance,
            world,
            out,
            while_running,
        } => {
            let service = forge(&service, while_running);
            service.export(&instance, &world, &out)?;
            done(
                format!("Saved '{world}' to {}.", out.display()),
                serde_json::json!({ "world": world, "path": out }),
            )?;
        }
        WorldCmd::Import {
            instance,
            archive,
            while_running,
        } => {
            let service = forge(&service, while_running);
            let folder = service.import(&instance, &archive)?;
            done(
                format!("Imported the world as '{folder}'."),
                serde_json::json!({ "world": folder }),
            )?;
        }
    }
    Ok(())
}

pub async fn run_server_command(
    action: ServerCmd,
    ctx: &Ctx,
    output_fmt: OutputFormat,
) -> anyhow::Result<()> {
    let json = output_fmt.is_json_output();
    let service = WorldService::new(ctx.clone());
    match action {
        ServerCmd::List { instance, ping } => {
            let servers = service.list_servers(&instance)?;
            let mut statuses = Vec::new();
            if ping {
                for server in &servers {
                    statuses.push(Some(
                        agora_game_minecraft::server_ping::ping(&server.address).await?,
                    ));
                }
            } else {
                statuses.resize(servers.len(), None);
            }
            if json {
                let rows: Vec<_> = servers
                    .iter()
                    .zip(&statuses)
                    .map(|(server, status)| {
                        serde_json::json!({
                            "name": server.name,
                            "address": server.address,
                            "status": status,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else if servers.is_empty() {
                println!("No servers.");
            } else {
                let rows: Vec<Vec<String>> = servers
                    .iter()
                    .zip(&statuses)
                    .enumerate()
                    .map(|(index, (server, status))| {
                        let mut row = vec![
                            (index + 1).to_string(),
                            server.name.clone(),
                            server.address.clone(),
                        ];
                        if let Some(status) = status {
                            row.push(
                                match (status.online, status.players_online, status.players_max) {
                                    (true, Some(on), Some(max)) => format!(
                                        "online {on}/{max}{}",
                                        status
                                            .latency_ms
                                            .map(|ms| format!(", {ms} ms"))
                                            .unwrap_or_default()
                                    ),
                                    (true, _, _) => "online".into(),
                                    (false, _, _) => "offline".into(),
                                },
                            );
                        }
                        row
                    })
                    .collect();
                if ping {
                    print_table(&["#", "Name", "Address", "Status"], &rows);
                } else {
                    print_table(&["#", "Name", "Address"], &rows);
                }
            }
        }
        ServerCmd::Add {
            instance,
            address,
            name,
            while_running,
        } => {
            forge(&service, while_running).add_server(
                &instance,
                name.as_deref().unwrap_or(""),
                &address,
            )?;
            if !json {
                println!("Added {address}.");
            }
        }
        ServerCmd::Edit {
            instance,
            server,
            name,
            address,
            while_running,
        } => crate::cli_misc_parity::run_server_edit(
            ctx,
            &instance,
            &server,
            name,
            address,
            while_running,
            output_fmt,
        )?,
        ServerCmd::Move {
            instance,
            server,
            direction,
            while_running,
        } => crate::cli_misc_parity::run_server_move(
            ctx,
            &instance,
            &server,
            direction,
            while_running,
            output_fmt,
        )?,
        ServerCmd::Ping { address } => {
            crate::cli_misc_parity::run_server_ping(&address, output_fmt).await?
        }
        ServerCmd::Remove {
            instance,
            position,
            while_running,
        } => {
            let index = position
                .checked_sub(1)
                .ok_or_else(|| anyhow::anyhow!("Positions start at 1."))?;
            forge(&service, while_running).remove_server(&instance, index)?;
            if !json {
                println!("Removed server {position}.");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agora_core::ctx::Ctx;

    fn ctx_with_world() -> (tempfile::TempDir, Ctx, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let ctx = Ctx::for_testing(root.path().to_path_buf());
        agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
        let world = ctx.paths.instance_dir("inst").unwrap().join("saves/Alpha");
        std::fs::create_dir_all(&world).unwrap();
        std::fs::write(world.join("level.dat"), b"level").unwrap();
        (root, ctx, world)
    }

    fn trash_dir() -> agora_game_minecraft::instance_service::TrashFn {
        std::sync::Arc::new(|path: &std::path::Path| {
            std::fs::remove_dir_all(path).map_err(|error| {
                agora_core::error::LauncherError::Generic {
                    code: "ERR_TRASH".into(),
                    message: error.to_string(),
                }
            })
        })
    }

    /// The CLI world path goes through the core guard: a delegated session
    /// blocks it, and `--while-running` is the explicit override.
    #[test]
    fn a_lease_blocks_the_cli_world_path_and_the_flag_overrides_it() {
        let (_root, ctx, world) = ctx_with_world();
        agora_core::instance_runtime::publish_starting(&ctx.paths, "inst").unwrap();
        let trash = trash_dir();

        let refused = run_world_command(
            WorldCmd::Delete {
                instance: "inst".into(),
                world: "Alpha".into(),
                yes: true,
                while_running: false,
            },
            &ctx,
            &trash,
            OutputFormat::Human,
        );
        assert!(refused.is_err(), "the CLI world path ignored the lease");
        assert!(world.join("level.dat").is_file());

        run_world_command(
            WorldCmd::Delete {
                instance: "inst".into(),
                world: "Alpha".into(),
                yes: true,
                while_running: true,
            },
            &ctx,
            &trash,
            OutputFormat::Human,
        )
        .unwrap();
        assert!(!world.exists());
        agora_core::instance_runtime::clear(&ctx.paths, "inst", None).unwrap();
    }
}
