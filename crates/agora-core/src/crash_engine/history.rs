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

use agora_game_api::crash::{AttrValue, Fact, Owner, Role, Scalar};
use serde::{Deserialize, Serialize};

use super::owner_key;

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistorySuspect {
    pub owner: Owner,
    pub role: Option<Role>,
}

/// What one crash contributed to the pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashRecord {
    pub artifact: String,
    /// Whether a parser read the file. An unreadable file is counted, never treated as a clean one.
    pub readable: bool,
    /// Ranked as the answer ranked them. Empty means this crash named nobody.
    pub candidates: Vec<HistorySuspect>,
    pub place: Option<CrashPlace>,
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
