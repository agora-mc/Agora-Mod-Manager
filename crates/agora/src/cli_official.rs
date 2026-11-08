//! `agora plugin official list|install|update|rollback`.
//!
//! An adapter over `PluginService`. Which plugins are official, what a download
//! must hash to, and whether an installed copy still counts are decided by core
//! against the signed registry; nothing here re-derives any of it.

use agora_core::plugins::{
    InstallPreview, OfficialInstallState, OfficialPluginInfo, OfficialStatus, PluginService,
    UpdateOutcome,
};
use clap::Subcommand;

#[derive(Subcommand)]
pub enum OfficialCmd {
    /// List the official plugins the signed catalog offers and where each stands.
    List,
    /// Install an official plugin that is not installed yet.
    Install {
        /// Plugin id, for example `acme.fixture`.
        id: String,
        #[arg(
            long,
            help = "Accept the capabilities the package asks for, and let it run its code if it has any"
        )]
        yes: bool,
        #[arg(long, help = "Install without allowing the plugin's code to run")]
        no_code: bool,
    },
    /// Move an installed plugin to the catalog's current official version.
    Update {
        id: String,
        #[arg(
            long,
            help = "Accept capabilities the installed version was not granted"
        )]
        yes: bool,
    },
    /// Return an official plugin to the previous version the catalog keeps.
    Rollback {
        id: String,
        #[arg(
            long,
            help = "Accept capabilities the installed version was not granted"
        )]
        yes: bool,
    },
}

fn state_text(info: &OfficialPluginInfo) -> String {
    match info.state {
        OfficialInstallState::NotInstalled => "not installed".into(),
        OfficialInstallState::UpToDate => "installed".into(),
        OfficialInstallState::UpdateAvailable => "update available".into(),
        OfficialInstallState::NotOfficial => match &info.installed_status {
            Some(OfficialStatus::Unverified { reason }) => {
                format!("installed, blocked ({reason})")
            }
            Some(
                OfficialStatus::CatalogUnavailable { reason }
                | OfficialStatus::NotListed { reason },
            ) => format!("installed, loaded with a warning ({reason})"),
            _ => "installed, but not the official package".into(),
        },
    }
}

pub async fn run(service: &PluginService, action: OfficialCmd, json: bool) -> anyhow::Result<()> {
    match action {
        OfficialCmd::List => {
            let list = service.official_plugins()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&list)?);
                return Ok(());
            }
            if let Some(reason) = &list.unavailable_reason {
                println!("{reason}");
                return Ok(());
            }
            let rows: Vec<Vec<String>> = list
                .plugins
                .iter()
                .map(|info| {
                    vec![
                        info.id.clone(),
                        info.name.clone(),
                        info.version.clone(),
                        info.installed_version.clone().unwrap_or_else(|| "-".into()),
                        info.game_ids.join(", "),
                        match &info.problem {
                            Some(problem) => format!("unavailable: {problem}"),
                            None => state_text(info),
                        },
                    ]
                })
                .collect();
            crate::print_table(
                &["ID", "Name", "Latest", "Installed", "Games", "Status"],
                &rows,
            );
        }
        OfficialCmd::Install { id, yes, no_code } => {
            let plugin_id = crate::parse_plugin_id(&id)?;
            // `--yes` is the answer to both questions; `--no-code` withholds the
            // second. Core only applies the code answer to a first install.
            let consent = agora_core::plugins::Consent {
                capabilities: yes,
                scripts: yes && !no_code,
            };
            let outcome = service
                .install_official(plugin_id.as_str(), consent, &service.http_fetcher())
                .await?;
            report(&id, outcome, "Installed", json, no_code)?;
        }
        OfficialCmd::Update { id, yes } => {
            let plugin_id = crate::parse_plugin_id(&id)?;
            let outcome = service
                .update_official(plugin_id.as_str(), yes, &service.http_fetcher())
                .await?;
            report(&id, outcome, "Updated", json, false)?;
        }
        OfficialCmd::Rollback { id, yes } => {
            let plugin_id = crate::parse_plugin_id(&id)?;
            let outcome = service
                .rollback_official(plugin_id.as_str(), yes, &service.http_fetcher())
                .await?;
            report(&id, outcome, "Rolled back", json, false)?;
        }
    }
    Ok(())
}

fn report(
    id: &str,
    outcome: UpdateOutcome,
    verb: &str,
    json: bool,
    no_code_available: bool,
) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&outcome)?);
        return Ok(());
    }
    match outcome {
        UpdateOutcome::Installed { plugin } => {
            println!("{verb} official plugin {} v{}.", plugin.id, plugin.version);
        }
        // Not applied and not an error: the package is fine, it just wants
        // something the person has not agreed to yet.
        UpdateOutcome::NeedsConsent { preview } => {
            println!(
                "Not installed. Version {} of {id} asks for:",
                preview.manifest.version
            );
            print_code_question(&preview);
            for capability in &preview.added_capabilities {
                println!("  - {capability}");
            }
            for host in &preview.added_hosts {
                println!("  - may contact {host}");
            }
            for extra in &preview.also_installs {
                println!();
                println!(
                    "It also installs {} v{} (needed by {}):",
                    extra.manifest.name, extra.manifest.version, preview.manifest.name
                );
                print_code_question(extra);
                for capability in &extra.added_capabilities {
                    println!("  - {capability}");
                }
                for host in &extra.added_hosts {
                    println!("  - may contact {host}");
                }
                if extra.added_capabilities.is_empty() && extra.added_hosts.is_empty() {
                    println!("  - nothing beyond what it does already");
                }
            }
            println!();
            let has_code =
                preview.runs_code || preview.also_installs.iter().any(|extra| extra.runs_code);
            if no_code_available && has_code {
                println!(
                    "Re-run with --yes to accept and allow code, or --yes --no-code to accept \
                     without running it, once the person using this computer has agreed."
                );
            } else {
                println!(
                    "Re-run with --yes to accept, once the person using this computer has agreed."
                );
            }
        }
    }
    Ok(())
}

/// The same three lines the app shows for each package that runs code.
fn print_code_question(preview: &InstallPreview) {
    if !preview.runs_code {
        return;
    }
    let manifest = &preview.manifest;
    let name = &manifest.name;
    println!("{name} runs code.");
    if preview.official {
        println!("  This is an official Agora plugin. Allowing its code is strongly recommended.");
    }
    match (preview.official, manifest.without_code.as_deref()) {
        (true, Some(text)) => {
            println!("  Without its code, {name} will be missing features: {text}");
        }
        (false, Some(text)) => {
            println!(
                "  Without its code, {name} will be missing features. The plugin says: \u{201c}{text}\u{201d}"
            );
        }
        (_, None) => {
            println!("  Without its code, {name} will be missing the features its code provides.");
        }
    }
    println!(
        "  You can allow it later with `agora plugin scripts {} allow`.",
        manifest.id
    );
}
