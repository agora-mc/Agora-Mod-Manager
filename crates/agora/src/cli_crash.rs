//! `agora crash analyze`: diagnose crash files, with an instance behind them or on their own.
//!
//! A thin adapter over [`agora_core::crash_engine::analyze`]. It reads the files, loads the crash
//! packs of installed plugins plus any `--plugin-dir` development folder, starts the script host a
//! script parser or inspector needs, and prints the answer. With `--instance` it also reads that
//! instance's deployed view so refs attribute to mods and an inspector can open one file; it
//! changes no game file, only Agora's own metadata caches.

use agora_core::crash_engine::attribution::{from_instance, InstanceView, Resolver};
use agora_core::crash_engine::history::{summarize, CrashHistory, HistoryOwner};
use agora_core::crash_engine::{
    analyze, owner_name, AnalyzeInput, AnalyzeOutput, CrashFile, InstanceAnalysis,
    InstanceInspector,
};
use agora_core::crash_packs::{load_plugin_packs, LoadedCrashPack};
use agora_core::game_script::instance_reads;
use agora_core::plugins::PluginService;
use agora_game_api::crash::{
    CodeEntry, CouldNotCheck, CouldNotCheckReason, GameHandle, LocalText, RefMap, RoleRule,
    SaveRiskLabel,
};
use agora_plugin_api::host::{ActivationRequest, GameReads, ScriptHost};
use agora_plugin_api::manifest::PluginId;
use agora_plugin_host::QuickJsHost;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct AnalyzeArgs {
    pub files: Vec<PathBuf>,
    /// Read text from stdin as a pasted fragment, so a pack's `snippet` parsers may read it.
    pub paste: bool,
    /// Analyze each file as its own crash, then summarize the pattern across them.
    pub each: bool,
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

    let mut crash_files = Vec::with_capacity(args.files.len() + usize::from(args.paste));
    for path in &args.files {
        let bytes = std::fs::read(path)
            .map_err(|error| anyhow::anyhow!("Could not read {}: {error}", path.display()))?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("crash file")
            .to_string();
        // A file the user named on the command line is a whole file, never a pasted fragment.
        crash_files.push(CrashFile::new(name, bytes));
    }
    if args.paste {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::io::stdin()
            .read_to_end(&mut bytes)
            .map_err(|error| anyhow::anyhow!("Could not read the pasted text: {error}"))?;
        // Only this origin may fall back to a snippet parser.
        crash_files.push(CrashFile::pasted("pasted text", bytes));
    }

    // With an instance the game is known, so only its packs take part, refs resolve to the mods
    // that provide them, and an inspector may open one file in the deployed view. Without one the
    // loose-file path is unchanged: names stand for themselves and nothing is inspected.
    let mut instance_game_id: Option<String> = None;
    let mut instance_resolver: Option<Resolver> = None;
    let mut instance_scope: Option<Arc<dyn GameReads>> = None;
    let mut instance_notes: Vec<CouldNotCheck> = Vec::new();
    let mut effective_version = args.game_version.clone();
    if let Some(instance_id) = args.instance.as_deref() {
        let (game_id, view, runtime_version, notes) = resolve_instance(ctx, instance_id)?;
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
            effective_version = runtime_version;
        }
        instance_notes = notes;
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
            could_not_check: &instance_notes,
        }),
        _ => None,
    };

    if args.each {
        let mut outputs = Vec::with_capacity(crash_files.len());
        for file in &crash_files {
            outputs.push(analyze(&AnalyzeInput {
                packs: &packs,
                files: std::slice::from_ref(file),
                runtime_version: effective_version.as_deref(),
                host: host.as_ref(),
                built_in: &built_in,
                instance,
            })?);
        }
        if args.json {
            let crashes: Vec<serde_json::Value> = outputs
                .iter()
                .map(|output| {
                    let mut value = serde_json::json!({
                        "file": output.record.artifact,
                        "answer": output.answer,
                        "files": output.files,
                    });
                    if args.facts {
                        value["parsed"] = parsed_json(output);
                    }
                    value
                })
                .collect();
            let records: Vec<_> = outputs.iter().map(|output| output.record.clone()).collect();
            let summary = summarize(&records);
            let value = serde_json::json!({
                "crashes": crashes,
                "summary": summary,
            });
            println!("{}", serde_json::to_string_pretty(&value)?);
        } else {
            print_each_text(&outputs);
        }
        if !outputs.iter().any(|output| output.read_any()) {
            anyhow::bail!("No crash file could be read.");
        }
        return Ok(());
    }

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
            value["parsed"] = parsed_json(&output);
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

/// The view behind `--instance`: a game instance as before, or a Minecraft instance when the id
/// belongs to that separate instance system. The game package builds the Minecraft view, so no
/// game's names or file layout reach this adapter or core.
fn resolve_instance(
    ctx: &agora_core::ctx::Ctx,
    instance_id: &str,
) -> anyhow::Result<(String, InstanceView, Option<String>, Vec<CouldNotCheck>)> {
    match agora_core::game_instance::get_manifest(ctx, instance_id) {
        Ok(manifest) => {
            let view = from_instance(ctx, instance_id).map_err(|error| {
                anyhow::anyhow!("Could not read instance '{instance_id}': {error}")
            })?;
            Ok((
                manifest.game.as_str().to_string(),
                view,
                manifest
                    .runtime_identity
                    .as_ref()
                    .map(|runtime| runtime.version.clone()),
                Vec::new(),
            ))
        }
        Err(generic_error) => {
            match agora_game_minecraft::installed_content::crash_instance_view(ctx, instance_id) {
                Ok(Some(found)) => Ok((
                    found.game_id,
                    found.view,
                    found.runtime_version,
                    found.could_not_check,
                )),
                Ok(None) => Err(anyhow::anyhow!(
                    "Unknown instance '{instance_id}': {generic_error}"
                )),
                Err(error) => Err(anyhow::anyhow!(
                    "Could not read instance '{instance_id}': {error}"
                )),
            }
        }
    }
}

/// What each parser found, for `--json --facts`.
fn parsed_json(output: &AnalyzeOutput) -> serde_json::Value {
    serde_json::Value::Array(
        output
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
            .collect(),
    )
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
    if !output.read_any() {
        // Nothing was read, so there is no diagnosis to state: say what happened to each file.
        println!("Agora could not read these files, so it has nothing to diagnose.");
        print_files(output);
        return;
    }
    let answer = &output.answer;
    println!("{}", english(&answer.headline));
    println!("How sure: {}", answer.level.label());
    if let Some(cause) = answer.cause {
        println!("Likely cause: {}", cause.label());
    }

    if !answer.trust_notes.is_empty() {
        println!("Notes:");
        for note in &answer.trust_notes {
            println!("  - {}", english(&note.text));
        }
    }

    if !answer.candidates.is_empty() {
        println!("Suspects:");
        for candidate in &answer.candidates {
            println!("  - {}", suspect_line(candidate));
        }
    }

    if !answer.findings.is_empty() {
        println!("Findings:");
        for finding in &answer.findings {
            println!(
                "  - {} {}: {} (from {})",
                level_word(finding.level),
                cause_words(finding.cause),
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
        // Several rules often miss the same thing; say it once, with how many.
        let mut lines: Vec<(String, usize)> = Vec::new();
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
            let line = format!(
                "{}: {}{detail} (from {plugin})",
                check.subject,
                reason_words(check.reason)
            );
            match lines.iter_mut().find(|(seen, _)| *seen == line) {
                Some((_, count)) => *count += 1,
                None => lines.push((line, 1)),
            }
        }
        for (line, count) in lines {
            if count > 1 {
                println!("  - {line} ({count} rules)");
            } else {
                println!("  - {line}");
            }
        }
    }

    print_files(output);
}

fn print_each_text(outputs: &[AnalyzeOutput]) {
    for output in outputs {
        println!("{}", output.record.artifact);
        if !output.read_any() {
            println!("  Agora could not read this crash.");
            println!();
            continue;
        }
        let answer = &output.answer;
        println!("  {}", english(&answer.headline));
        for note in &answer.trust_notes {
            println!("  Note: {}", english(&note.text));
        }
        if !answer.candidates.is_empty() {
            println!("  Suspects:");
            for candidate in answer.candidates.iter().take(3) {
                println!("    - {}", suspect_line(candidate));
            }
        }
        println!();
    }
    let records: Vec<_> = outputs.iter().map(|output| output.record.clone()).collect();
    print_history(&summarize(&records));
}

fn print_history(summary: &CrashHistory) {
    println!(
        "Across {} crashes. This is a count of how often each suspect came up, not a verdict.",
        summary.crashes
    );
    if summary.suspects.is_empty() {
        println!("Suspects: none was named.");
    } else {
        println!("Suspects:");
        for owner in &summary.suspects {
            print_history_owner(owner);
        }
    }
    if !summary.always_running.is_empty() {
        println!("Always running (framework or hook framework; appears in many crashes for that reason):");
        for owner in &summary.always_running {
            print_history_owner(owner);
        }
    }
    if summary.unreadable > 0 {
        println!(
            "Could not read: {} {}.",
            summary.unreadable,
            crashes_word(summary.unreadable)
        );
    }
    println!(
        "Named nobody: {} {}.",
        summary.named_nobody,
        crashes_word(summary.named_nobody)
    );
}

fn print_history_owner(owner: &HistoryOwner) {
    println!(
        "  - {}: named in {} {} ({} as the top suspect), {} distinct {}",
        owner_name(&owner.owner),
        owner.crashes_named,
        crashes_word(owner.crashes_named),
        owner.crashes_top,
        owner.distinct_places,
        places_word(owner.distinct_places)
    );
}

fn crashes_word(count: usize) -> &'static str {
    if count == 1 {
        "crash"
    } else {
        "crashes"
    }
}

fn places_word(count: usize) -> &'static str {
    if count == 1 {
        "crash place"
    } else {
        "crash places"
    }
}

fn suspect_line(candidate: &agora_game_api::crash::Candidate) -> String {
    let risk = candidate
        .save_risk
        .map(save_risk_words)
        .map(|words| format!(" - {words}"))
        .unwrap_or_default();
    format!(
        "{}: {}{risk}",
        owner_name(&candidate.owner),
        candidate.level.label()
    )
}

fn save_risk_words(label: SaveRiskLabel) -> &'static str {
    match label {
        SaveRiskLabel::Safe => "safe to test on your save",
        SaveRiskLabel::Risky => "risky: test on a new game or a backup of your save",
        SaveRiskLabel::Unknown => "not sure: back up your save first",
    }
}

fn print_files(output: &AnalyzeOutput) {
    if output.files.is_empty() {
        return;
    }
    println!("Files:");
    for file in &output.files {
        match (&file.read_by, file.read_by_extra.first()) {
            (Some(reader), _) => {
                let how = if file.snippet { "a pasted snippet" } else { "" };
                if how.is_empty() {
                    println!("  {}: read by {}", file.name, reader.plugin_name);
                } else {
                    println!("  {}: read by {} as {how}", file.name, reader.plugin_name);
                }
            }
            (None, Some(reader)) => {
                println!(
                    "  {}: no primary reader; read alongside by {}",
                    file.name, reader.plugin_name
                );
            }
            (None, None) => println!(
                "  {}: {}",
                file.name,
                file.reason
                    .as_deref()
                    .unwrap_or("not recognised by any installed crash pack")
            ),
        }
        for reader in &file.also_recognised_by {
            println!(
                "    also recognised by {}/{} (not run; one primary reads a file)",
                reader.plugin_name, reader.parser_id
            );
        }
    }
}

fn level_word(level: agora_game_api::crash::Level) -> &'static str {
    match level {
        agora_game_api::crash::Level::NotSure => "Not sure",
        agora_game_api::crash::Level::Possible => "Possible",
        agora_game_api::crash::Level::Likely => "Likely",
        agora_game_api::crash::Level::VeryLikely => "Very likely",
    }
}

/// How a finding's cause reads in a sentence: "Likely a setting: ...".
fn cause_words(cause: agora_game_api::crash::Cause) -> &'static str {
    use agora_game_api::crash::Cause;
    match cause {
        Cause::Mod => "a mod",
        Cause::VersionMismatch => "a version mismatch",
        Cause::MissingRequirement => "a missing requirement",
        Cause::File => "a file",
        Cause::Setting => "a setting",
        Cause::Save => "a save",
        Cause::Memory => "a memory problem",
        Cause::Driver => "a driver problem",
        Cause::GameInstall => "a game install problem",
        Cause::Other => "another cause",
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
        CouldNotCheckReason::InstanceIncomplete => "the instance could not supply it",
    }
}
