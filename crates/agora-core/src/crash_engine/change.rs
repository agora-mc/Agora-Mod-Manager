//! The generic rule that only a live session can state: what changed since the last good launch.
//!
//! This is core, and game-free. The ledger stores both manifests, so the difference between them
//! is data every game shares. The change family is boost-only: it may raise a suspect another
//! family named, and it never creates one alone. When nothing else is named, a finding says what
//! changed instead, which is the honest, actionable answer.

use crate::crash_packs::built_in_identity;
use crate::session_ledger::{ChangeKind, ManifestChange};
use agora_game_api::crash::{
    AttrValue, Fact, Family, Finding, Level, LocalText, Owner, Ref, Scalar, Scope, Signal,
    SourceAttribution,
};
use std::collections::BTreeMap;

/// The ref kind a changed content item is named under. It is the same word the engine facts
/// document, so a pack's rule can query it like any other fact.
pub const CHANGE_REF_KIND: &str = "content";

/// The engine facts for a finished session: how it ended, and what changed since the last good
/// launch. `runtime_ms` is how long the game itself ran, when that was seen.
pub fn session_facts(
    verdict: &str,
    exit_code: Option<u32>,
    runtime_ms: Option<u64>,
    changes: &[ManifestChange],
) -> Vec<Fact> {
    let mut attrs = BTreeMap::new();
    attrs.insert(
        "verdict".to_string(),
        AttrValue::Scalar(Scalar::Text(verdict.to_string())),
    );
    if let Some(code) = exit_code {
        attrs.insert(
            "exit_code".to_string(),
            AttrValue::Scalar(Scalar::Number(code as f64)),
        );
    }
    if let Some(ms) = runtime_ms {
        attrs.insert(
            "runtime_ms".to_string(),
            AttrValue::Scalar(Scalar::Number(ms as f64)),
        );
    }
    let mut facts = vec![Fact {
        kind: "core.session".to_string(),
        attrs,
        refs: Vec::new(),
        span: None,
    }];
    facts.extend(change_facts(changes));
    facts
}

/// One `core.change` fact per difference. An item change names the item under a `content` ref;
/// a load-order or settings change has no item to name.
pub fn change_facts(changes: &[ManifestChange]) -> Vec<Fact> {
    changes
        .iter()
        .map(|change| {
            let mut attrs = BTreeMap::new();
            attrs.insert(
                "change".to_string(),
                AttrValue::Scalar(Scalar::Text(change.kind.as_str().to_string())),
            );
            if let Some(from) = &change.from_version {
                attrs.insert(
                    "from_version".to_string(),
                    AttrValue::Scalar(Scalar::Text(from.clone())),
                );
            }
            if let Some(to) = &change.to_version {
                attrs.insert(
                    "to_version".to_string(),
                    AttrValue::Scalar(Scalar::Text(to.clone())),
                );
            }
            Fact {
                kind: "core.change".to_string(),
                attrs,
                refs: change
                    .key
                    .as_ref()
                    .map(|key| {
                        vec![Ref {
                            kind: CHANGE_REF_KIND.to_string(),
                            value: key.clone(),
                            strength: Default::default(),
                        }]
                    })
                    .unwrap_or_default(),
                span: None,
            }
        })
        .collect()
}

fn change_weight(kind: ChangeKind) -> u32 {
    match kind {
        ChangeKind::Added | ChangeKind::Updated => 3,
        ChangeKind::Enabled | ChangeKind::Disabled | ChangeKind::Moved => 2,
        ChangeKind::Removed => 1,
        ChangeKind::LoadOrder | ChangeKind::Settings => 1,
    }
}

/// Boost signals for the items that changed. Consensus drops these unless another family named the
/// same owner, so this can only raise a suspect, never invent one.
pub fn change_signals(changes: &[ManifestChange]) -> Vec<Signal> {
    let source = agora_source();
    changes
        .iter()
        .filter_map(|change| {
            let key = change.key.as_ref()?;
            Some(Signal {
                family: Family::ChangeCorrelation,
                candidate: Owner::Item {
                    key: key.clone(),
                    name: key.clone(),
                },
                weight: change_weight(change.kind),
                source: source.clone(),
                evidence: Vec::new(),
            })
        })
        .collect()
}

fn agora_source() -> SourceAttribution {
    let identity = built_in_identity();
    SourceAttribution {
        plugin_id: identity.id,
        plugin_name: identity.name,
        plugin_version: identity.version,
        rule_id: Some("core.change_since_last_good".to_string()),
        rule_version: Some(1),
        parser_id: None,
    }
}

/// How one change reads in the finding's sentence.
pub fn describe(change: &ManifestChange) -> String {
    let key = change.key.as_deref().unwrap_or("");
    match change.kind {
        ChangeKind::Added => format!("added {key}"),
        ChangeKind::Removed => format!("removed {key}"),
        ChangeKind::Updated => match (&change.from_version, &change.to_version) {
            (Some(from), Some(to)) if from != to => format!("updated {key} from {from} to {to}"),
            _ => format!("updated {key}"),
        },
        ChangeKind::Enabled => format!("switched on {key}"),
        ChangeKind::Disabled => format!("switched off {key}"),
        ChangeKind::Moved => format!("moved {key} in the load order"),
        ChangeKind::LoadOrder => "changed the load order".to_string(),
        ChangeKind::Settings => "changed the instance's settings".to_string(),
    }
}

/// The finding for a crash that named nobody: what changed, said as a lead and not as a verdict.
pub fn change_finding(changes: &[ManifestChange]) -> Option<Finding> {
    if changes.is_empty() {
        return None;
    }
    let mut seen = Vec::new();
    for change in changes {
        let text = describe(change);
        if !seen.contains(&text) {
            seen.push(text);
        }
    }
    let shown = 12usize;
    let mut list = seen
        .iter()
        .take(shown)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if seen.len() > shown {
        list.push_str(&format!(", and {} more", seen.len() - shown));
    }
    let mut headline = LocalText::new();
    headline.insert(
        "en".to_string(),
        format!("This started after you changed: {list}."),
    );
    let mut why = LocalText::new();
    why.insert(
        "en".to_string(),
        "A change near a crash is a lead, not proof. If the crash began right after one of these, \
         testing that change first is reasonable."
            .to_string(),
    );
    Some(Finding {
        scope: Scope::Environment,
        level: Level::Possible,
        candidate: None,
        headline,
        why_not_sure: Some(why),
        source: agora_source(),
        evidence: Vec::new(),
    })
}
