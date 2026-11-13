//! The inspector an instance can offer a rule (MASTER_SPEC §30.11).
//!
//! A rule names an inspector by id. Ids are local to a plugin, so this looks the inspector up only
//! in the pack of the plugin whose rule asked. The target ref is resolved with the same
//! [`Resolver`] the evaluator uses, and only then is the file opened: the script reads one artifact
//! by id and never sees a path.
//!
//! Nothing here interprets a game: the file is a name and some bytes, and the pack's script decides
//! what they mean.

use super::attribution::Resolver;
use super::{Inspection, InspectionOutcome, InspectionRequest, Inspector};
use crate::crash_packs::{
    artifact_handle, glob_matches, parser_request, run_inspector, InspectorDispatch,
    LoadedCrashPack,
};
use agora_game_api::crash::{GameHandle, InspectResult, Owner, ParserLimits, ParserRequest};
use agora_plugin_api::host::{GameReads, ScriptHost, ScriptReadError, ScriptReads};
use std::sync::Arc;

/// The id the target file is given. A script sees the name it resolves to, never a path.
const TARGET_ID: &str = "target";

/// One instance's inspectors, reached through the pack that declares each one.
pub struct InstanceInspector<'a> {
    host: &'a dyn ScriptHost,
    packs: &'a [LoadedCrashPack],
    resolver: &'a Resolver,
    reads: Arc<dyn GameReads>,
    game: GameHandle,
    limits: ParserLimits,
}

impl<'a> InstanceInspector<'a> {
    pub fn new(
        host: &'a dyn ScriptHost,
        packs: &'a [LoadedCrashPack],
        resolver: &'a Resolver,
        reads: Arc<dyn GameReads>,
        game: GameHandle,
    ) -> Self {
        Self {
            host,
            packs,
            resolver,
            reads,
            game,
            limits: ParserLimits::DEFAULT,
        }
    }

    /// The plugin's inspector by id. A plugin may contribute more than one pack, so every pack it
    /// contributed is searched; ids are local to the plugin.
    fn find_inspector(
        &self,
        plugin_id: &str,
        inspector_id: &str,
    ) -> Option<(
        &'a LoadedCrashPack,
        &'a agora_game_api::crash::InspectorDecl,
    )> {
        self.packs
            .iter()
            .filter(|pack| pack.plugin_id == plugin_id)
            .find_map(|pack| {
                pack.pack
                    .inspectors
                    .iter()
                    .find(|inspector| inspector.id == inspector_id)
                    .map(|inspector| (pack, inspector))
            })
    }

    fn plugin_name(&self, plugin_id: &str) -> String {
        self.packs
            .iter()
            .find(|pack| pack.plugin_id == plugin_id)
            .map(|pack| pack.plugin_name.as_str())
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(plugin_id)
            .to_string()
    }

    fn file_name(path: &str) -> &str {
        path.rsplit('/').next().unwrap_or(path)
    }

    fn failed(
        &self,
        plugin_id: &str,
        inspector: &str,
        target: &str,
        why: String,
    ) -> InspectionOutcome {
        let name = self.plugin_name(plugin_id);
        Inspection::Failed(format!(
            "the inspector `{inspector}` from `{name}` could not check `{target}`: {why}"
        ))
        .into()
    }
}

impl Inspector for InstanceInspector<'_> {
    fn inspect(&self, request: &InspectionRequest<'_>) -> InspectionOutcome {
        let Some((pack, inspector)) = self.find_inspector(request.plugin_id, request.inspector)
        else {
            return Inspection::Missing.into();
        };

        // The same resolution the evaluator uses. A ref that names no file in this instance, or a
        // role that has no file behind it, has nothing to inspect.
        let attribution = self.resolver.attribute(request.target);
        if matches!(attribution.owner, Owner::Unknown | Owner::NotInInstance) {
            return Inspection::Missing.into();
        }
        let Some(path) = attribution.via else {
            return Inspection::Missing.into();
        };
        let name = Self::file_name(&path);
        if !inspector
            .accepts
            .iter()
            .any(|pattern| glob_matches(pattern, name))
        {
            // This inspector does not handle this kind of file, which is not a failure.
            return Inspection::Missing.into();
        }

        let size = self.reads.size(&path).unwrap_or(0);
        let reads: Arc<dyn ScriptReads> = Arc::new(SingleFileReads {
            artifact: TARGET_ID.to_string(),
            path: path.clone(),
            reads: self.reads.clone(),
        });
        let parser_request: ParserRequest = parser_request(
            inspector.id.clone(),
            artifact_handle(TARGET_ID, name, size, Vec::new()),
            self.game.clone(),
            self.limits,
        );

        match run_inspector(
            self.host,
            &pack.plugin_id,
            inspector,
            &parser_request,
            reads,
        ) {
            InspectorDispatch::Inspected(output) => InspectionOutcome {
                result: match output.result {
                    InspectResult::Pass => Inspection::Pass,
                    InspectResult::Fail => Inspection::Fail,
                    InspectResult::Unknown => Inspection::Unknown,
                },
                detail: output.detail,
                facts: output.facts,
            },
            InspectorDispatch::Failed(failure) => self.failed(
                &pack.plugin_id,
                &inspector.id,
                &request.target.value,
                failure.message,
            ),
        }
    }
}

/// The read scope the script gets: one artifact id, mapped to one file the resolver chose.
struct SingleFileReads {
    artifact: String,
    path: String,
    reads: Arc<dyn GameReads>,
}

impl SingleFileReads {
    fn check(&self, artifact: &str) -> Result<(), ScriptReadError> {
        if artifact == self.artifact {
            Ok(())
        } else {
            Err(ScriptReadError::new(
                "out_of_scope",
                format!("`{artifact}` is not the file this inspection may read"),
            ))
        }
    }
}

impl ScriptReads for SingleFileReads {
    fn read_bytes(
        &self,
        artifact: &str,
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>, ScriptReadError> {
        self.check(artifact)?;
        self.reads.read_bytes(&self.path, offset, len)
    }

    fn read_lines(
        &self,
        artifact: &str,
        from_line: u64,
        max_lines: u32,
    ) -> Result<serde_json::Value, ScriptReadError> {
        self.check(artifact)?;
        self.reads.read_lines(&self.path, from_line, max_lines)
    }
}
