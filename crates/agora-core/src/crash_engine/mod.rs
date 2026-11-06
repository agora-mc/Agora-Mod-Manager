//! The crash doctor's evaluator, consensus and answer (MASTER_SPEC §30.4, §30.5, §30.11).
//!
//! Pure logic: nothing here reads a file, touches the network or opens a database. A caller
//! supplies crash packs, what parsers found, and the two traits the engine needs (who owns a ref,
//! and how to inspect one file). The same input in any order gives the same answer.

mod answer;
pub mod attribution;
mod consensus;
mod evaluate;
mod profile;
mod types;

pub use answer::*;
pub use consensus::*;
pub use evaluate::*;
pub use profile::*;
pub use types::*;

use agora_game_api::crash::{EvidenceLine, Level, Owner, SourceAttribution};

/// A stable key for an owner, used to group signals and to sort output without relying on input
/// order. It is not shown to anyone.
pub(crate) fn owner_key(owner: &Owner) -> String {
    match owner {
        Owner::Item { key, .. } => format!("item\0{key}"),
        Owner::Base => "base".to_string(),
        Owner::Generated { tool } => format!("generated\0{tool}"),
        Owner::Writable => "writable".to_string(),
        Owner::Framework { name } => format!("framework\0{name}"),
        Owner::System => "system".to_string(),
        Owner::NotInInstance => "not_in_instance".to_string(),
        Owner::Unknown => "unknown".to_string(),
    }
}

/// A stable key for an attribution, for deduplicating and sorting.
pub(crate) fn source_key(source: &SourceAttribution) -> String {
    format!(
        "{}\0{}\0{}\0{}\0{}\0{}",
        source.plugin_id,
        source.plugin_name,
        source.plugin_version,
        source.rule_id.as_deref().unwrap_or(""),
        source
            .rule_version
            .map(|version| version.to_string())
            .unwrap_or_default(),
        source.parser_id.as_deref().unwrap_or("")
    )
}

/// The words for an owner in a sentence. Never a certainty claim.
pub(crate) fn owner_name(owner: &Owner) -> String {
    match owner {
        Owner::Item { name, .. } => name.clone(),
        Owner::Base => "the game".to_string(),
        Owner::Generated { tool } => format!("{tool}'s output"),
        Owner::Writable => "a file Agora did not install".to_string(),
        Owner::Framework { name } => name.clone(),
        Owner::System => "the system".to_string(),
        Owner::NotInInstance => "something not installed".to_string(),
        Owner::Unknown => "something Agora could not identify".to_string(),
    }
}

/// The content key a next step can act on.
pub(crate) fn owner_content_key(owner: &Owner) -> Option<String> {
    match owner {
        Owner::Item { key, .. } => Some(key.clone()),
        _ => None,
    }
}

/// A stable key for an evidence line.
pub(crate) fn evidence_key(line: &EvidenceLine) -> String {
    format!(
        "{}\0{}\0{}\0{}\0{}\0{}",
        line.artifact,
        line.span.artifact.as_deref().unwrap_or(""),
        line.span.section.as_deref().unwrap_or(""),
        line.span
            .line_start
            .map(|n| n.to_string())
            .unwrap_or_default(),
        line.span
            .line_end
            .map(|n| n.to_string())
            .unwrap_or_default(),
        line.quote.as_deref().unwrap_or("")
    )
}

/// Deduplicate and order evidence lines so the answer does not depend on match order.
pub(crate) fn clean_evidence(mut lines: Vec<EvidenceLine>) -> Vec<EvidenceLine> {
    lines.sort_by_key(evidence_key);
    lines.dedup_by_key(|line| evidence_key(line));
    lines
}

/// Sort and deduplicate findings by everything a user can see.
pub(crate) fn clean_findings(
    mut findings: Vec<agora_game_api::crash::Finding>,
) -> Vec<agora_game_api::crash::Finding> {
    fn key(finding: &agora_game_api::crash::Finding) -> String {
        let candidate = finding
            .candidate
            .as_ref()
            .map(owner_key)
            .unwrap_or_default();
        let candidate_name = finding
            .candidate
            .as_ref()
            .map(owner_name)
            .unwrap_or_default();
        let headline = finding.headline.get("en").cloned().unwrap_or_default();
        let why = finding
            .why_not_sure
            .as_ref()
            .and_then(|text| text.get("en"))
            .cloned()
            .unwrap_or_default();
        let evidence: Vec<String> = finding.evidence.iter().map(evidence_key).collect();
        format!(
            "{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}",
            scope_rank(finding.scope),
            finding.level.label(),
            candidate,
            candidate_name,
            headline,
            why,
            source_key(&finding.source),
            evidence.join("\u{1f}")
        )
    }
    findings.sort_by_key(key);
    findings.dedup_by_key(|finding| key(finding));
    findings
}

pub(crate) fn scope_rank(scope: agora_game_api::crash::Scope) -> u8 {
    use agora_game_api::crash::Scope;
    match scope {
        Scope::RootCause => 0,
        Scope::Mechanism => 1,
        Scope::Environment => 2,
    }
}

/// The cap a trust note gives, with the weaker of two caps winning.
pub(crate) fn lower_cap(current: Option<Level>, next: Level) -> Option<Level> {
    Some(match current {
        Some(level) => level.min(next),
        None => next,
    })
}
