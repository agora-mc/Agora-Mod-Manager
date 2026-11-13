//! The pattern across several crashes: a pure count, never a verdict (MASTER_SPEC §30.4, §30.5).
//!
//! A person with a folder of crash logs wants each crash judged on its own and then the pattern
//! across them. [`summarize`] is that count. It takes per-crash records and says, for every suspect
//! owner, how many crashes named it, how many named it first, and how many distinct crash places
//! it came up at. It holds no game knowledge: a crash place comes from the contract's `core.*`
//! facts (`core.exception` with a module ref and an `offset`, else the first `core.frame`).
//!
//! The same function is what a live session will use to produce a `core.history` fact. It does
//! nothing on its own: the caller supplies what each crash found.

use std::collections::{BTreeMap, BTreeSet};

use agora_game_api::crash::{
    AttrValue, BucketPart, Candidate, Fact, Family, Finding, Level, LocalText, Owner, Role, Scalar,
    Scope, Signal, SourceAttribution,
};
use serde::{Deserialize, Serialize};

use super::{owner_key, owner_name, ParsedArtifact};

/// The rule id the engine's own cross-crash finding names, so a screen can tell it apart from a
/// pack's rules.
pub const RECURRING_SUSPECT_RULE: &str = "core.recurring_suspect";

/// The rule id Agora's own local confirmation history names.
pub const LOCAL_CONFIRMATION_RULE: &str = "core.local_confirmation";

/// A fix the user confirmed on this PC for one crash bucket. It is boost-only: it may raise a
/// suspect another family already named and never creates one (MASTER_SPEC §30.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalConfirmation {
    pub bucket: String,
    pub owner_key: String,
    pub count: u32,
}

/// A place a crash happened: the faulting module and offset, or the top frame's identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrashPlace {
    /// The ref kind that named it, such as `module` or `class`.
    pub kind: String,
    pub value: String,
    /// What tells two crashes in the same module or class apart: an offset, RVA, symbol or address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
}

impl CrashPlace {
    /// A stable key for counting distinct places. The pieces are separated by a NUL so no value can
    /// make two different places look the same.
    pub fn key(&self) -> String {
        format!(
            "{}\0{}\0{}",
            self.kind,
            self.value,
            self.location.as_deref().unwrap_or("")
        )
    }
}

/// One suspect as a crash named it, with the role the pack gave it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistorySuspect {
    pub owner: Owner,
    pub role: Option<Role>,
}

/// What one crash contributed to the pattern.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrashRecord {
    pub artifact: String,
    /// Whether a parser read the file. An unreadable file is counted, never treated as a clean one.
    pub readable: bool,
    /// Ranked as the answer ranked them. Empty means this crash named nobody.
    pub candidates: Vec<HistorySuspect>,
    pub place: Option<CrashPlace>,
}

/// One stored crash: its record, the parser's stable bucket tokens when it gave any, and when it
/// happened. The tokens are what make "the same bucket" mean more than "the same place".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedCrash {
    pub record: CrashRecord,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bucket: Vec<BucketPart>,
    /// When the log was written or the session started, in Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_unix_ms: Option<i64>,
}

impl RecordedCrash {
    /// The identity two crashes share when they are the same bug. The parser's bucket tokens win;
    /// without them the crash place is the best the contract offers, and without either there is
    /// no identity to count.
    pub fn bucket_key(&self) -> Option<String> {
        if self.bucket.is_empty() {
            self.record.place.as_ref().map(CrashPlace::key)
        } else {
            bucket_key(&self.bucket)
        }
    }
}

/// A stable key for the parser's bucket tokens, or `None` when it gave none.
pub fn bucket_key(bucket: &[BucketPart]) -> Option<String> {
    if bucket.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = bucket
        .iter()
        .map(|part| format!("{}\0{}", part.scheme, part.value))
        .collect();
    parts.sort();
    parts.dedup();
    Some(parts.join("\u{1f}"))
}

/// The union of every parser's bucket tokens for one analysis, in a stable order.
pub fn parsed_bucket(parsed: &[ParsedArtifact]) -> Vec<BucketPart> {
    let mut parts: Vec<BucketPart> = parsed
        .iter()
        .flat_map(|artifact| artifact.output.bucket.iter().cloned())
        .collect();
    parts.sort_by(|left, right| (&left.scheme, &left.value).cmp(&(&right.scheme, &right.value)));
    parts.dedup();
    parts
}

/// The bucket this analysis belongs to: the parsers' tokens, else the crash place they name.
pub fn current_bucket(parsed: &[ParsedArtifact]) -> Option<String> {
    let bucket = parsed_bucket(parsed);
    if bucket.is_empty() {
        crash_place(
            parsed
                .iter()
                .flat_map(|artifact| artifact.output.facts.iter()),
        )
        .map(|place| place.key())
    } else {
        bucket_key(&bucket)
    }
}

/// One owner's count across the crashes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryOwner {
    pub owner: Owner,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    /// How many readable crashes named it anywhere in their suspects.
    pub crashes_named: usize,
    /// How many of those named it first.
    pub crashes_top: usize,
    /// How many different crash places it was named at. One repeat of the same place is one bug.
    pub distinct_places: usize,
}

/// The pattern across several crashes. This is a count of how often each suspect came up, not a
/// verdict about any of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrashHistory {
    pub crashes: usize,
    /// Crashes no parser could read.
    pub unreadable: usize,
    /// Readable crashes whose answer named no owner.
    pub named_nobody: usize,
    /// Owners ranked by crashes named, then by distinct places.
    pub suspects: Vec<HistoryOwner>,
    /// Framework and hook-framework owners, listed apart: they are always running, so appearing in
    /// many crashes says less about them.
    pub always_running: Vec<HistoryOwner>,
}

struct Aggregate {
    owner: Owner,
    role: Option<Role>,
    crashes_named: usize,
    crashes_top: usize,
    places: BTreeSet<String>,
}

/// Count how often each suspect came up across the crashes a caller stored.
pub fn summarize_recorded(crashes: &[RecordedCrash]) -> CrashHistory {
    let records: Vec<CrashRecord> = crashes.iter().map(|crash| crash.record.clone()).collect();
    summarize(&records)
}

/// Count how often each suspect came up across `records`.
pub fn summarize(records: &[CrashRecord]) -> CrashHistory {
    let mut aggregates: BTreeMap<String, Aggregate> = BTreeMap::new();
    let mut unreadable = 0;
    let mut named_nobody = 0;

    for record in records {
        if !record.readable {
            unreadable += 1;
            continue;
        }
        if record.candidates.is_empty() {
            named_nobody += 1;
            continue;
        }
        let place = record.place.as_ref().map(CrashPlace::key);
        let mut seen_here: BTreeSet<String> = BTreeSet::new();
        for (index, suspect) in record.candidates.iter().enumerate() {
            let key = owner_key(&suspect.owner);
            if !seen_here.insert(key.clone()) {
                continue;
            }
            let aggregate = aggregates.entry(key).or_insert_with(|| Aggregate {
                owner: suspect.owner.clone(),
                role: suspect.role,
                crashes_named: 0,
                crashes_top: 0,
                places: BTreeSet::new(),
            });
            if aggregate.role.is_none() {
                aggregate.role = suspect.role;
            }
            aggregate.crashes_named += 1;
            if index == 0 {
                aggregate.crashes_top += 1;
            }
            if let Some(place) = &place {
                aggregate.places.insert(place.clone());
            }
        }
    }

    let mut suspects = Vec::new();
    let mut always_running = Vec::new();
    for (key, aggregate) in aggregates {
        let owner = HistoryOwner {
            owner: aggregate.owner.clone(),
            role: aggregate.role,
            crashes_named: aggregate.crashes_named,
            crashes_top: aggregate.crashes_top,
            distinct_places: aggregate.places.len(),
        };
        let always = matches!(owner.role, Some(Role::Framework | Role::HookFramework))
            || matches!(owner.owner, Owner::Framework { .. });
        if always {
            always_running.push((key, owner));
        } else {
            suspects.push((key, owner));
        }
    }

    CrashHistory {
        crashes: records.len(),
        unreadable,
        named_nobody,
        suspects: rank(suspects),
        always_running: rank(always_running),
    }
}

fn rank(mut owners: Vec<(String, HistoryOwner)>) -> Vec<HistoryOwner> {
    owners.sort_by(|(a_key, a), (b_key, b)| {
        b.crashes_named
            .cmp(&a.crashes_named)
            .then_with(|| b.distinct_places.cmp(&a.distinct_places))
            .then_with(|| a_key.cmp(b_key))
    });
    owners.into_iter().map(|(_, owner)| owner).collect()
}

/// The crash place a set of facts names, as the contract defines it: the exception's module and
/// offset, or the top frame when there is no such exception site.
pub fn crash_place<'a>(facts: impl IntoIterator<Item = &'a Fact>) -> Option<CrashPlace> {
    let mut best_exception: Option<(i64, CrashPlace)> = None;
    let mut best_frame: Option<(i64, CrashPlace)> = None;

    for fact in facts {
        match fact.kind.as_str() {
            "core.exception" => {
                let Some(offset) = fact.attrs.get("offset").and_then(scalar_text) else {
                    continue;
                };
                let Some(module) = fact
                    .refs
                    .iter()
                    .find(|reference| reference.kind == "module")
                else {
                    continue;
                };
                if module.value.trim().is_empty() || offset.trim().is_empty() {
                    continue;
                }
                let depth = fact
                    .attrs
                    .get("depth")
                    .and_then(scalar_number)
                    .unwrap_or(0.0) as i64;
                let place = CrashPlace {
                    kind: module.kind.clone(),
                    value: module.value.clone(),
                    location: Some(offset),
                };
                if best_exception
                    .as_ref()
                    .is_none_or(|(best, _)| depth < *best)
                {
                    best_exception = Some((depth, place));
                }
            }
            "core.frame" => {
                let index = fact
                    .attrs
                    .get("index")
                    .and_then(scalar_number)
                    .unwrap_or(0.0) as i64;
                let Some(place) = frame_place(fact) else {
                    continue;
                };
                if best_frame.as_ref().is_none_or(|(best, _)| index < *best) {
                    best_frame = Some((index, place));
                }
            }
            _ => {}
        }
    }

    best_exception
        .map(|(_, place)| place)
        .or_else(|| best_frame.map(|(_, place)| place))
}

fn frame_place(fact: &Fact) -> Option<CrashPlace> {
    let location = ["rva", "symbol", "address", "symbol_offset"]
        .iter()
        .find_map(|name| fact.attrs.get(*name).and_then(scalar_text))
        .filter(|value| !value.trim().is_empty());
    let reference = fact
        .refs
        .iter()
        .find(|reference| reference.kind == "module")
        .or_else(|| fact.refs.iter().find(|reference| reference.kind == "class"))
        .or_else(|| fact.refs.first());
    match (reference, location) {
        (Some(reference), location) if !reference.value.trim().is_empty() => Some(CrashPlace {
            kind: reference.kind.clone(),
            value: reference.value.clone(),
            location,
        }),
        (None, Some(location)) => Some(CrashPlace {
            kind: "frame".to_string(),
            value: location.clone(),
            location: None,
        }),
        _ => None,
    }
}

fn scalar_text(value: &AttrValue) -> Option<String> {
    match value {
        AttrValue::Scalar(scalar) => Some(display_scalar(scalar)),
        AttrValue::List(_) => None,
    }
}

fn scalar_number(value: &AttrValue) -> Option<f64> {
    match value {
        AttrValue::Scalar(Scalar::Number(number)) => Some(*number),
        AttrValue::Scalar(Scalar::Text(text)) => text.trim().parse().ok(),
        _ => None,
    }
}

fn display_scalar(scalar: &Scalar) -> String {
    match scalar {
        Scalar::Bool(value) => value.to_string(),
        Scalar::Number(value) => {
            if value.fract() == 0.0 && value.is_finite() {
                format!("{value:.0}")
            } else {
                value.to_string()
            }
        }
        Scalar::Text(value) => value.clone(),
    }
}

/// The engine's `core.history` facts for one crash. `predates_last_change` is only stated when
/// both the bucket's first occurrence and the moment the current setup began are known: an
/// unknown is left out, never guessed false.
pub fn engine_facts(
    prior: &[RecordedCrash],
    current_bucket: Option<&str>,
    sessions_since_first: &BTreeMap<String, u64>,
    setup_since_unix_ms: Option<i64>,
) -> Vec<Fact> {
    let Some(bucket) = current_bucket else {
        return Vec::new();
    };
    let mut attrs = BTreeMap::new();
    let same = prior
        .iter()
        .filter(|crash| crash.bucket_key().as_deref() == Some(bucket))
        .count();
    attrs.insert(
        "same_bucket_count".to_string(),
        AttrValue::Scalar(Scalar::Number(same as f64)),
    );
    if let Some(sessions) = sessions_since_first.get(bucket) {
        attrs.insert(
            "sessions_since_first".to_string(),
            AttrValue::Scalar(Scalar::Number(*sessions as f64)),
        );
    }
    let first_seen = prior
        .iter()
        .filter(|crash| crash.bucket_key().as_deref() == Some(bucket))
        .filter_map(|crash| crash.at_unix_ms)
        .min();
    if let (Some(first_seen), Some(setup_since)) = (first_seen, setup_since_unix_ms) {
        attrs.insert(
            "predates_last_change".to_string(),
            AttrValue::Scalar(Scalar::Bool(first_seen < setup_since)),
        );
    }
    vec![Fact {
        kind: "core.history".to_string(),
        attrs,
        refs: Vec::new(),
        span: None,
    }]
}

fn history_source() -> SourceAttribution {
    let identity = crate::crash_packs::built_in_identity();
    SourceAttribution {
        plugin_id: identity.id,
        plugin_name: identity.name,
        plugin_version: identity.version,
        rule_id: Some(RECURRING_SUSPECT_RULE.to_string()),
        rule_version: Some(1),
        parser_id: None,
    }
}

/// The boost a confirmed fix on this PC gives its suspect. It is boost-only by family, so it can
/// never create a candidate on its own.
pub fn local_confirmation_signal(owner: Owner, count: u32) -> Signal {
    Signal {
        family: Family::LocalConfirmation,
        candidate: owner,
        weight: 1 + count.min(3),
        source: confirmation_source(),
        evidence: Vec::new(),
        cause: None,
    }
}

fn confirmation_source() -> SourceAttribution {
    let identity = crate::crash_packs::built_in_identity();
    SourceAttribution {
        plugin_id: identity.id,
        plugin_name: identity.name,
        plugin_version: identity.version,
        rule_id: Some(LOCAL_CONFIRMATION_RULE.to_string()),
        rule_version: Some(1),
        parser_id: None,
    }
}

/// Boost signals for suspects that came up in more than one earlier crash. The family is
/// boost-only, so consensus lets this raise a suspect another family named and never creates one.
/// Framework and hook-framework owners are absent here: the summary lists them apart because they
/// are always running.
pub fn repeat_signals(summary: &CrashHistory) -> Vec<Signal> {
    summary
        .suspects
        .iter()
        .filter(|owner| owner.crashes_named >= 2)
        .map(|owner| Signal {
            family: Family::HistoryRepeat,
            candidate: owner.owner.clone(),
            weight: 1 + owner.crashes_named.min(3) as u32,
            source: history_source(),
            evidence: Vec::new(),
            cause: None,
        })
        .collect()
}

/// A finding for each named suspect that recurs across several crashes at different places. The
/// count is the point: it is a lead, never a verdict, so the level stays `possible`.
pub fn repeat_findings(summary: &CrashHistory, candidates: &[Candidate]) -> Vec<Finding> {
    let mut findings = Vec::new();
    for candidate in candidates {
        let key = owner_key(&candidate.owner);
        let Some(owner) = summary
            .suspects
            .iter()
            .find(|owner| owner_key(&owner.owner) == key)
        else {
            continue;
        };
        if owner.crashes_named < 2 || owner.distinct_places < 2 {
            continue;
        }
        let name = owner_name(&candidate.owner);
        let mut headline = LocalText::new();
        headline.insert(
            "en".to_string(),
            format!(
                "{name} was involved in {} of your last {} crashes.",
                owner.crashes_named, summary.crashes
            ),
        );
        let mut why = LocalText::new();
        why.insert(
            "en".to_string(),
            "Coming up in several crashes at different places is a lead, not proof. Something else \
             may be triggering it, and a crash does not always happen at the same place. If it \
             keeps happening, testing without it is reasonable."
                .to_string(),
        );
        findings.push(Finding {
            scope: Scope::RootCause,
            level: Level::Possible,
            cause: agora_game_api::crash::Cause::Mod,
            candidate: Some(candidate.owner.clone()),
            headline,
            why_not_sure: Some(why),
            source: history_source(),
            evidence: Vec::new(),
        });
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use agora_game_api::crash::{BucketPart, Owner, Role, Scalar};

    fn item(key: &str) -> Owner {
        Owner::Item {
            key: key.to_string(),
            name: format!("{key} mod"),
        }
    }

    fn place(location: &str) -> CrashPlace {
        CrashPlace {
            kind: "module".to_string(),
            value: "Invented.dll".to_string(),
            location: Some(location.to_string()),
        }
    }

    fn recorded(candidates: Vec<Owner>, location: &str, at: i64) -> RecordedCrash {
        RecordedCrash {
            record: CrashRecord {
                artifact: "log.txt".to_string(),
                readable: true,
                candidates: candidates
                    .into_iter()
                    .map(|owner| HistorySuspect { owner, role: None })
                    .collect(),
                place: Some(place(location)),
            },
            bucket: Vec::new(),
            at_unix_ms: Some(at),
        }
    }

    fn bucket_fact(facts: &[Fact], name: &str) -> Option<AttrValue> {
        facts
            .iter()
            .find(|fact| fact.kind == "core.history")
            .and_then(|fact| fact.attrs.get(name).cloned())
    }

    #[test]
    fn the_same_place_is_the_same_bucket_when_the_parser_gave_no_tokens() {
        let first = recorded(vec![item("one")], "0x1000", 10);
        let second = recorded(vec![item("two")], "0x1000", 20);
        assert_eq!(first.bucket_key(), second.bucket_key());
    }

    #[test]
    fn history_facts_count_only_the_bucket_that_matches() {
        let prior = vec![
            recorded(vec![item("one")], "0x1000", 10),
            recorded(vec![item("one")], "0x1000", 20),
            recorded(vec![item("two")], "0x2000", 30),
        ];
        let current = prior[0].bucket_key();
        let mut sessions = BTreeMap::new();
        sessions.insert(current.clone().unwrap(), 7);
        let facts = engine_facts(&prior, current.as_deref(), &sessions, Some(15));
        assert_eq!(
            bucket_fact(&facts, "same_bucket_count"),
            Some(AttrValue::Scalar(Scalar::Number(2.0)))
        );
        assert_eq!(
            bucket_fact(&facts, "sessions_since_first"),
            Some(AttrValue::Scalar(Scalar::Number(7.0)))
        );
        // The first occurrence (10) is before the setup began (15).
        assert_eq!(
            bucket_fact(&facts, "predates_last_change"),
            Some(AttrValue::Scalar(Scalar::Bool(true)))
        );
    }

    #[test]
    fn an_unknown_setup_leaves_predating_unstated_rather_than_false() {
        let prior = vec![recorded(vec![item("one")], "0x1000", 10)];
        let current = prior[0].bucket_key();
        let facts = engine_facts(&prior, current.as_deref(), &BTreeMap::new(), None);
        assert_eq!(bucket_fact(&facts, "predates_last_change"), None);
    }

    #[test]
    fn a_crash_with_no_bucket_gets_no_history_facts() {
        let facts = engine_facts(&[], None, &BTreeMap::new(), None);
        assert!(facts.is_empty());
    }

    #[test]
    fn bucket_tokens_beat_the_place_and_are_order_independent() {
        let one = vec![
            BucketPart {
                scheme: "exception".to_string(),
                value: "a".to_string(),
            },
            BucketPart {
                scheme: "module".to_string(),
                value: "b".to_string(),
            },
        ];
        let mut two = one.clone();
        two.reverse();
        assert_eq!(bucket_key(&one), bucket_key(&two));
        let crash = RecordedCrash {
            record: CrashRecord {
                artifact: "x".to_string(),
                readable: true,
                candidates: Vec::new(),
                place: Some(place("0x1")),
            },
            bucket: one,
            at_unix_ms: None,
        };
        assert_eq!(crash.bucket_key(), bucket_key(&crash.bucket));
        assert_ne!(crash.bucket_key(), Some(place("0x1").key()));
    }

    #[test]
    fn a_repeat_is_boost_only_and_never_lists_an_always_running_owner() {
        let summary = summarize(&[
            CrashRecord {
                artifact: "a".to_string(),
                readable: true,
                candidates: vec![
                    HistorySuspect {
                        owner: item("one"),
                        role: None,
                    },
                    HistorySuspect {
                        owner: Owner::Framework {
                            name: "Invented Engine".to_string(),
                        },
                        role: Some(Role::Framework),
                    },
                ],
                place: Some(place("0x1")),
            },
            CrashRecord {
                artifact: "b".to_string(),
                readable: true,
                candidates: vec![HistorySuspect {
                    owner: item("one"),
                    role: None,
                }],
                place: Some(place("0x2")),
            },
        ]);
        let signals = repeat_signals(&summary);
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].candidate, item("one"));
        assert_eq!(signals[0].family, Family::HistoryRepeat);
        assert!(Family::HistoryRepeat.is_boost_only());
    }

    #[test]
    fn a_finding_needs_both_a_repeat_and_more_than_one_place() {
        let same_place = summarize(&[
            CrashRecord {
                artifact: "a".to_string(),
                readable: true,
                candidates: vec![HistorySuspect {
                    owner: item("one"),
                    role: None,
                }],
                place: Some(place("0x1")),
            },
            CrashRecord {
                artifact: "b".to_string(),
                readable: true,
                candidates: vec![HistorySuspect {
                    owner: item("one"),
                    role: None,
                }],
                place: Some(place("0x1")),
            },
        ]);
        let candidate = Candidate {
            owner: item("one"),
            level: Level::Possible,
            score: 12,
            cause: agora_game_api::crash::Cause::Mod,
            role: None,
            families: Vec::new(),
            sources: Vec::new(),
            also_provided_by: Vec::new(),
            save_risk: None,
        };
        assert!(repeat_findings(&same_place, std::slice::from_ref(&candidate)).is_empty());
    }
    #[test]
    fn a_confirmed_fix_is_boost_only_and_never_creates_a_candidate() {
        use crate::crash_engine::consensus::{consensus, ConsensusInput};
        use crate::crash_engine::ConsensusProfile;

        let signal = local_confirmation_signal(item("one"), 2);
        assert_eq!(signal.family, Family::LocalConfirmation);
        assert!(Family::LocalConfirmation.is_boost_only());

        let roles = BTreeMap::new();
        let out = consensus(
            &ConsensusInput {
                signals: std::slice::from_ref(&signal),
                roles: &roles,
                level_caps: &[],
            },
            &ConsensusProfile::default(),
        );
        assert!(
            out.candidates.is_empty(),
            "memory alone must never accuse a mod"
        );
    }
}
