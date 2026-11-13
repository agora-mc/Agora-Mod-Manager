//! Diagnosing crash files a person handed Agora, with or without an instance behind them
//! (MASTER_SPEC §30.4, §30.5, §30.11).
//!
//! This is the one adapter the CLI and the desktop share: it builds the instance-backed analysis
//! when an instance is named and the loose-file analysis when it is not. It holds no game knowledge;
//! packs supply every format, source, rule and inspector.
//!
//! An empty or unreadable input is an explicit "nothing to read", never a clean bill of health.

use std::sync::Arc;

use agora_game_api::crash::{BuiltInPack, CouldNotCheck, GameHandle, RefMap, RoleRule};
use agora_plugin_api::host::ScriptHost;
use serde::{Deserialize, Serialize};

use crate::crash_engine::attribution::Resolver;
use crate::crash_engine::instance_view::resolve_crash_instance;
use crate::crash_engine::{
    analyze, AnalyzeInput, AnalyzeOutput, AnalyzedFile, CrashFile, InstanceAnalysis,
    InstanceInspector,
};
use crate::crash_packs::LoadedCrashPack;
use crate::ctx::Ctx;
use crate::error::LauncherResult;

/// The answer for a supplied file. A file nothing could read says so; it is not an empty answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SuppliedAnswer {
    /// The one analyze pipeline ran.
    Analyzed {
        answer: Box<agora_game_api::crash::CrashAnswer>,
        files: Vec<AnalyzedFile>,
    },
    /// Nothing was readable in what the user supplied. That is not a clean bill of health.
    NothingToRead { why: String },
}

/// Everything one supplied-file analysis needs.
pub struct SuppliedAnalysis<'a> {
    pub packs: &'a [LoadedCrashPack],
    pub built_in: &'a [BuiltInPack],
    pub host: &'a dyn ScriptHost,
    pub files: &'a [CrashFile],
    /// The instance the crash came from, when the user named one.
    pub instance_id: Option<&'a str>,
    /// The game version the user says it came from, when they know it.
    pub runtime_version: Option<&'a str>,
}

/// Run the same pipeline a live session runs, over files the user supplied.
///
/// With an instance the game is known, so only its packs and the packs for every game take part,
/// refs resolve to the items that provide them, and an inspector may open one file in the deployed
/// view. Without one the loose-file path is unchanged: names stand for themselves and nothing is
/// inspected.
pub fn analyze_supplied(ctx: &Ctx, input: SuppliedAnalysis<'_>) -> LauncherResult<SuppliedAnswer> {
    if input.files.is_empty() {
        return Ok(SuppliedAnswer::NothingToRead {
            why: "No crash file was given.".to_string(),
        });
    }
    if input
        .files
        .iter()
        .all(|file| file.bytes.iter().all(u8::is_ascii_whitespace))
    {
        return Ok(SuppliedAnswer::NothingToRead {
            why: "The crash log is empty; there is nothing to read.".to_string(),
        });
    }

    let mut instance_game_id: Option<String> = None;
    let mut instance_resolver: Option<Resolver> = None;
    let mut instance_scope = None;
    let mut instance_notes: Vec<CouldNotCheck> = Vec::new();
    let mut effective_version = input.runtime_version.map(str::to_string);
    if let Some(instance_id) = input.instance_id {
        let resolved = resolve_crash_instance(ctx, instance_id, None)?;
        let game_id = resolved.game_id.clone();
        let game_packs: Vec<&LoadedCrashPack> = input
            .packs
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
        instance_resolver = Some(Resolver::new(resolved.view, ref_maps, roles));
        instance_scope = Some(Arc::new(
            crate::game_script::instance_reads(ctx, instance_id).map_err(|error| {
                crate::error::LauncherError::Generic {
                    code: "ERR_CRASH_INSTANCE_READ".to_string(),
                    message: format!("Could not read instance '{instance_id}': {error}"),
                }
            })?,
        ));
        if effective_version.is_none() {
            effective_version = resolved.runtime_version;
        }
        instance_notes = resolved.could_not_check;
        instance_game_id = Some(game_id);
    }

    let inspector = match (&instance_resolver, &instance_scope) {
        (Some(resolver), Some(scope)) => Some(InstanceInspector::new(
            input.host,
            input.packs,
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
        inspector.as_ref(),
    ) {
        (Some(game_id), Some(resolver), Some(inspector)) => Some(InstanceAnalysis {
            game_id,
            resolver,
            inspector,
            could_not_check: &instance_notes,
        }),
        _ => None,
    };

    let output = analyze(&AnalyzeInput {
        packs: input.packs,
        files: input.files,
        runtime_version: effective_version.as_deref(),
        host: input.host,
        built_in: input.built_in,
        instance,
    })?;
    if !output.read_any() {
        return Ok(SuppliedAnswer::NothingToRead {
            why: nothing_read_reason(&output),
        });
    }
    Ok(SuppliedAnswer::Analyzed {
        answer: Box::new(output.answer),
        files: output.files,
    })
}

/// Why nothing could be read, from the per-file report. Names the first file and its reason so the
/// screen can say what to try next instead of "nothing found".
fn nothing_read_reason(output: &AnalyzeOutput) -> String {
    for file in &output.files {
        if file.read_by.is_some() || !file.read_by_extra.is_empty() {
            continue;
        }
        if let Some(reason) = &file.reason {
            return format!("{}: {reason}", file.name);
        }
        if let Some(failure) = file.failures.first() {
            return format!(
                "{}: {} could not read it: {}",
                file.name, failure.plugin_name, failure.message
            );
        }
        return format!("{} could not be read.", file.name);
    }
    "No crash file could be read.".to_string()
}
