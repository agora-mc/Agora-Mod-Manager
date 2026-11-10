//! `agora crash analyze`: diagnose crash files, with an instance behind them or on their own.
//!
//! A thin adapter over [`agora_core::crash_engine::analyze`]. It reads the files, loads the crash
//! packs of installed plugins plus any `--plugin-dir` development folder, starts the script host a
//! script parser or inspector needs, and prints the answer. With `--instance` it also reads that
//! instance's deployed view so refs attribute to mods and an inspector can open one file; it
//! changes nothing.

use agora_core::crash_engine::attribution::{from_instance, Resolver};
use agora_core::crash_engine::{
    analyze, owner_name, AnalyzeInput, AnalyzeOutput, CrashFile, InstanceAnalysis,
    InstanceInspector,
};
use agora_core::crash_packs::{load_plugin_packs, LoadedCrashPack};
use agora_core::game_script::instance_reads;
use agora_core::plugins::PluginService;
use agora_game_api::crash::{
    CodeEntry, CouldNotCheckReason, GameHandle, LocalText, RefMap, RoleRule,
};
use agora_plugin_api::host::{ActivationRequest, GameReads, ScriptHost};
use agora_plugin_api::manifest::PluginId;
use agora_plugin_host::QuickJsHost;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct AnalyzeArgs {
    pub files: Vec<PathBuf>,
    pub instance: Option<String>,
    pub plugin_dirs: Vec<PathBuf>,
    pub game_version: Option<String>,
    pub allow_code: bool,
    /// Add what each parser found to the JSON output.
    pub facts: bool,
    pub json: bool,
}

pub async fn run(ctx: &agora_core::ctx::Ctx, args: AnalyzeArgs) -> anyhow::Result<()> {
    let host: Arc<dyn ScriptHost> = Arc::new(QuickJsHost::new());
    let service = PluginService::headless(ctx.clone(), host.clone());
    service.reload()?;

    let installed = service.crash_packs();
    for problem in &installed.problems {
        eprintln!("Warning: {problem}");
    }
    let mut packs: Vec<LoadedCrashPack> = installed.packs;
    // The packs compiled into Agora load through the same linter as a plugin's.
    let built_in = agora_game_minecraft::crash::built_in_packs();
    let built_in_report = agora_core::crash_packs::load_built_in_packs(&built_in);
    for problem in &built_in_report.problems {
        eprintln!("Warning: {problem}");
    }
    packs.extend(built_in_report.packs);
    let installed_script = installed_script_plugins(&packs);
    for id in installed_script {
        if let Ok(plugin_id) = PluginId::parse(&id) {
            // A plugin whose code the user did not allow still loads its packs; its script parsers
            // then fail, and every failure is shown as "could not check" rather than hidden.
            let _ = service.activate(&plugin_id);
        }
    }

    for folder in &args.plugin_dirs {
        let (folder_packs, script_ids) = load_development_folder(folder)?;
        if !script_ids.is_empty() && !args.allow_code {
            anyhow::bail!(
                "{} has a crash parser or inspector written in a script, which runs code. Pass --allow-code to run it.",
                folder.display()
            );
        }
        packs.extend(folder_packs);
        for id in script_ids {
            activate_folder_parser(&host, ctx, folder, &id)?;
        }
    }

    if packs.is_empty() {
        anyhow::bail!("No crash pack loaded, so Agora cannot read crash files.");
    }

    let mut crash_files = Vec::with_capacity(args.files.len());
    for path in &args.files {
        let bytes = std::fs::read(path)
            .map_err(|error| anyhow::anyhow!("Could not read {}: {error}", path.display()))?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("crash file")
            .to_string();
        crash_files.push(CrashFile { name, bytes });
    }

    // With an instance the game is known, so only its packs take part, refs resolve to the mods
    // that provide them, and an inspector may open one file in the deployed view. Without one the
    // loose-file path is unchanged: names stand for themselves and nothing is inspected.
    let mut instance_game_id: Option<String> = None;
    let mut instance_resolver: Option<Resolver> = None;
    let mut instance_scope: Option<Arc<dyn GameReads>> = None;
    let mut effective_version = args.game_version.clone();
    if let Some(instance_id) = args.instance.as_deref() {
        let manifest = agora_core::game_instance::get_manifest(ctx, instance_id)
            .map_err(|error| anyhow::anyhow!("Unknown instance '{instance_id}': {error}"))?;
        let game_id = manifest.game.as_str().to_string();
        let view = from_instance(ctx, instance_id)
            .map_err(|error| anyhow::anyhow!("Could not read instance '{instance_id}': {error}"))?;
        // Only the instance's game and the packs for every game may say how a ref is attributed.
        let game_packs: Vec<&LoadedCrashPack> = packs
            .iter()
            .filter(|pack| {
                pack.pack.games.is_empty() || pack.pack.games.iter().any(|game| game == &game_id)
            })
            .collect();
        let ref_maps: Vec<RefMap> = game_packs
            .iter()
            .flat_map(|pack| pack.pack.ref_maps.iter().cloned())
            .collect();
        let roles: Vec<RoleRule> = game_packs
            .iter()
            .flat_map(|pack| pack.pack.roles.iter().cloned())
            .collect();
        instance_resolver = Some(Resolver::new(view, ref_maps, roles));
        instance_scope = Some(Arc::new(instance_reads(ctx, instance_id).map_err(
            |error| anyhow::anyhow!("Could not read instance '{instance_id}': {error}"),
        )?));
        if effective_version.is_none() {
            effective_version = manifest
                .runtime_identity
                .as_ref()
                .map(|runtime| runtime.version.clone());
        }
        instance_game_id = Some(game_id);
    }
    let instance_inspector = match (&instance_resolver, &instance_scope) {
        (Some(resolver), Some(scope)) => Some(InstanceInspector::new(
            host.as_ref(),
            &packs,
            resolver,
            scope.clone(),
            GameHandle {
                id: instance_game_id.clone().unwrap_or_default(),
                runtime_version: effective_version.clone(),
            },
        )),
        _ => None,
    };
    let instance = match (
        instance_game_id.as_deref(),
        instance_resolver.as_ref(),
        instance_inspector.as_ref(),
    ) {
        (Some(game_id), Some(resolver), Some(inspector)) => Some(InstanceAnalysis {
            game_id,
            resolver,
            inspector,
        }),
        _ => None,
    };

    let output = analyze(&AnalyzeInput {
        packs: &packs,
        files: &crash_files,
        runtime_version: effective_version.as_deref(),
        host: host.as_ref(),
        built_in: &built_in,
        instance,
    })?;

    if args.json {
        let mut value = serde_json::json!({
            "answer": output.answer,
            "files": output.files,
        });
        if args.facts {
            let parsed: Vec<serde_json::Value> = output
                .parsed
                .iter()
                .map(|parsed| {
                    serde_json::json!({
                        "file": parsed.artifact,
                        "plugin_id": parsed.plugin.id,
                        "parser_id": parsed.parser_id,
                        "output": parsed.output,
                    })
                })
                .collect();
            value["parsed"] = serde_json::Value::Array(parsed);
        }
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        print_text(&output);
    }

    if !output.read_any() {
        anyhow::bail!("No crash file could be read.");
    }
    Ok(())
}

/// Read one development folder the way `agora plugin validate` does, and name the plugin ids that
/// contribute script code so the caller can decide about running it.
fn load_development_folder(folder: &Path) -> anyhow::Result<(Vec<LoadedCrashPack>, Vec<String>)> {
    let manifest = agora_core::plugins::install::read_folder_manifest(folder)?;
    let report = load_plugin_packs(manifest.id.as_str(), folder, &manifest);
    for problem in &report.problems {
        eprintln!("Warning: {problem}");
    }
    let script_ids = report
        .packs
        .iter()
        .filter(|pack| has_script_code(pack))
        .map(|pack| pack.plugin_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    Ok((report.packs, script_ids))
}

/// Plugin ids whose packs contain script code, deduplicated in a stable order.
fn installed_script_plugins(packs: &[LoadedCrashPack]) -> Vec<String> {
    packs
        .iter()
        .filter(|pack| has_script_code(pack))
        .map(|pack| pack.plugin_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn has_script_code(pack: &LoadedCrashPack) -> bool {
    pack.pack
        .parsers
        .iter()
        .any(|parser| matches!(parser.entry, CodeEntry::Script { .. }))
        || pack
            .pack
            .inspectors
            .iter()
            .any(|inspector| matches!(inspector.entry, CodeEntry::Script { .. }))
}

/// Start a development folder's script so its parser exports exist. `--allow-code` was checked by
/// the caller; this is the only place a loose crash file's adapter runs code the user allows.
fn activate_folder_parser(
    host: &Arc<dyn ScriptHost>,
    ctx: &agora_core::ctx::Ctx,
    folder: &Path,
    plugin_id: &str,
) -> anyhow::Result<()> {
    let manifest = agora_core::plugins::install::read_folder_manifest(folder)?;
    let Some(entrypoint) = manifest.entrypoint.clone() else {
        anyhow::bail!(
            "{} contributes script crash code but its manifest names no entrypoint",
            folder.display()
        );
    };
    let id = PluginId::parse(plugin_id)?;
    // A folder may reuse the id of an installed plugin. The folder's code is the one the user
    // asked to run, so it wins for this process.
    let _ = host.deactivate(&id);
    host.activate(
        ActivationRequest {
            plugin_id: id,
            package_root: folder.to_path_buf(),
            data_root: ctx.paths.root().to_path_buf(),
            entrypoint,
            memory_limit_bytes: ActivationRequest::DEFAULT_MEMORY_LIMIT,
            stack_limit_bytes: ActivationRequest::DEFAULT_STACK_LIMIT,
            api_version: agora_plugin_api::host_api_version_string(),
            settings: serde_json::json!({}),
        },
        Arc::new(NoopBridge),
    )?;
    Ok(())
}

/// A parser only needs the scoped reads the crash contract gives it. Any other host call is
/// answered with nothing, so a development folder cannot reach an instance from here.
struct NoopBridge;

impl agora_plugin_api::host::HostBridge for NoopBridge {
    fn call(
        &self,
        _plugin_id: &PluginId,
        request: agora_plugin_api::protocol::HostRequest,
    ) -> agora_plugin_api::protocol::HostResponse {
        agora_plugin_api::protocol::HostResponse::Ok {
            request_id: request.request_id,
            value: serde_json::Value::Null,
        }
    }

    fn log(
        &self,
        _plugin_id: &PluginId,
        _level: agora_plugin_api::protocol::LogLevel,
        _message: &str,
    ) {
    }
}

fn print_text(output: &AnalyzeOutput) {
    let answer = &output.answer;
    println!("{}", english(&answer.headline));
    println!("How sure: {}", answer.level.label());

    if !answer.candidates.is_empty() {
        println!("Suspects:");
        for candidate in &answer.candidates {
            println!(
                "  - {}: {}",
                owner_name(&candidate.owner),
                candidate.level.label()
            );
        }
    }

    if !answer.findings.is_empty() {
        println!("Findings:");
        for finding in &answer.findings {
            println!(
                "  - {} (from {})",
                english(&finding.headline),
                finding.source.plugin_name
            );
        }
    }

    if !answer.inspections.is_empty() {
        println!("Inspections:");
        for inspection in &answer.inspections {
            let detail = inspection
                .detail
                .as_ref()
                .map(|detail| format!(": {detail}"))
                .unwrap_or_default();
            println!(
                "  - {} [{}]{detail} (from {})",
                inspection.target,
                inspect_words(inspection.result),
                inspection.source.plugin_name
            );
        }
    }

    if !answer.could_not_check.is_empty() {
        println!("Could not check:");
        for check in &answer.could_not_check {
            let plugin = check
                .source
                .as_ref()
                .map(|source| source.plugin_name.as_str())
                .unwrap_or("Agora");
            let detail = check
                .detail
                .as_ref()
                .map(|detail| format!(": {detail}"))
                .unwrap_or_default();
            println!(
                "  - {}: {}{detail} (from {plugin})",
                check.subject,
                reason_words(check.reason)
            );
        }
    }

    if !output.files.is_empty() {
        println!("Files:");
        for file in &output.files {
            match &file.read_by {
                Some(reader) => println!("  {}: read by {}", file.name, reader.plugin_name),
                None => println!(
                    "  {}: not recognised by any installed crash pack",
                    file.name
                ),
            }
        }
    }
}

fn english(text: &LocalText) -> &str {
    text.get("en").map(String::as_str).unwrap_or("")
}

fn inspect_words(result: agora_game_api::crash::InspectResult) -> &'static str {
    use agora_game_api::crash::InspectResult;
    match result {
        InspectResult::Pass => "pass",
        InspectResult::Fail => "fail",
        InspectResult::Unknown => "unknown",
    }
}

fn reason_words(reason: CouldNotCheckReason) -> &'static str {
    match reason {
        CouldNotCheckReason::NotCovered => "no parser looked for it",
        CouldNotCheckReason::AbsentFromArtifact => "the file does not contain it",
        CouldNotCheckReason::VersionUnknown => "the version could not be judged",
        CouldNotCheckReason::CodeFailed => "a parser failed",
        CouldNotCheckReason::InspectorMissing => "no inspector is installed",
        CouldNotCheckReason::NotRetained => "Agora did not keep these bytes",
        CouldNotCheckReason::TooManyMatches => "the rule matched too many times",
    }
}
