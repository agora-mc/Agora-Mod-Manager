//! Consensus: turn signals into ranked candidates, or abstain.
//!
//! The scorer's shape follows the design: within a family the strongest signal wins, core score
//! sums non-boost families, boosts may raise an existing candidate but never create one, and the
//! strongest level needs either independent groups and a lead or a decisive family.

use super::*;
use agora_game_api::crash::{Candidate, Family, Level, Owner, Role, Signal, SourceAttribution};
use std::collections::{BTreeMap, BTreeSet};

pub struct ConsensusInput<'a> {
    pub signals: &'a [Signal],
    /// Each candidate owner's role, by [`owner_key`]. Roles are what demotions act on.
    pub roles: &'a BTreeMap<String, Role>,
    /// Caps from trust notes. The weakest one wins.
    pub level_caps: &'a [Level],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsensusOutput {
    pub candidates: Vec<Candidate>,
    pub level: Level,
    pub abstained: bool,
}

struct Bucket {
    owner: Owner,
    families: BTreeMap<Family, u32>,
    sources: BTreeMap<String, SourceAttribution>,
}

impl Bucket {
    fn core(&self) -> u32 {
        self.families
            .iter()
            .filter(|(family, _)| !family.is_boost_only())
            .map(|(_, weight)| *weight)
            .sum()
    }

    fn boost(&self) -> u32 {
        self.families
            .iter()
            .filter(|(family, _)| family.is_boost_only())
            .map(|(_, weight)| *weight)
            .sum()
    }

    fn families(&self) -> Vec<Family> {
        self.families.keys().copied().collect()
    }

    fn sources(&self) -> Vec<SourceAttribution> {
        self.sources.values().cloned().collect()
    }
}

pub fn consensus(input: &ConsensusInput<'_>, profile: &ConsensusProfile) -> ConsensusOutput {
    let mut signals: Vec<&Signal> = input.signals.iter().collect();
    signals.sort_by_key(|signal| {
        (
            owner_key(&signal.candidate),
            signal.family,
            std::cmp::Reverse(signal.weight),
            source_key(&signal.source),
        )
    });

    let mut buckets: BTreeMap<String, Bucket> = BTreeMap::new();
    for signal in signals {
        let key = owner_key(&signal.candidate);
        let bucket = buckets.entry(key).or_insert_with(|| Bucket {
            owner: signal.candidate.clone(),
            families: BTreeMap::new(),
            sources: BTreeMap::new(),
        });
        bucket
            .families
            .entry(signal.family)
            .and_modify(|weight| *weight = (*weight).max(signal.weight))
            .or_insert(signal.weight);
        bucket
            .sources
            .entry(source_key(&signal.source))
            .or_insert_with(|| signal.source.clone());
    }

    struct Scored {
        owner: Owner,
        key: String,
        core: u32,
        score: u32,
        families: Vec<Family>,
        sources: Vec<SourceAttribution>,
        demoted: bool,
    }

    let mut scored: Vec<Scored> = Vec::new();
    for (key, bucket) in buckets {
        if matches!(
            bucket.owner,
            Owner::System | Owner::NotInInstance | Owner::Unknown
        ) {
            continue;
        }
        let raw_core = bucket.core();
        if raw_core == 0 {
            // Boost-only evidence never creates a candidate.
            continue;
        }
        let role = input.roles.get(&key).copied().or(match &bucket.owner {
            Owner::Framework { .. } => Some(Role::Framework),
            _ => None,
        });
        if matches!(role, Some(Role::System | Role::Platform)) {
            continue;
        }
        let core = demote(raw_core, role, profile);
        let boost = demote(bucket.boost(), role, profile);
        let families = bucket.families();
        let sources = bucket.sources();
        scored.push(Scored {
            owner: bucket.owner,
            key,
            core,
            score: core + boost,
            families,
            sources,
            demoted: matches!(role, Some(Role::HookFramework | Role::Framework)),
        });
    }

    let has_own_evidence = scored
        .iter()
        .any(|candidate| !candidate.demoted && candidate.core > 0);
    if has_own_evidence {
        // A hook framework or framework keeps its signal only when nothing else has its own.
        scored.retain(|candidate| !candidate.demoted);
    }
    // Core ranks; a boost only breaks an exact core tie. A candidate needs a non-boost clue,
    // and its total must at least reach the abstention floor.
    scored.retain(|candidate| candidate.core > 0 && candidate.score >= profile.abstain_below);
    scored.sort_by(|a, b| {
        b.core
            .cmp(&a.core)
            .then_with(|| b.score.cmp(&a.score))
            .then_with(|| a.key.cmp(&b.key))
    });

    let mut candidates: Vec<Candidate> = scored
        .iter()
        .map(|scored| Candidate {
            owner: scored.owner.clone(),
            level: Level::NotSure,
            score: scored.score,
            families: scored.families.clone(),
            sources: scored.sources.clone(),
            also_provided_by: Vec::new(),
            save_risk: None,
        })
        .collect();

    if candidates.is_empty() {
        return ConsensusOutput {
            candidates,
            level: Level::NotSure,
            abstained: true,
        };
    }

    let top = &scored[0];
    let second = scored.get(1);
    // The lead uses the total score, so a boost on the runner-up can make it negative.
    let lead: i64 = match second {
        Some(second) => i64::from(top.score) - i64::from(second.score),
        None => i64::MAX,
    };
    let decisive = top.families.contains(&Family::DecisiveVerdict)
        && !top.demoted
        && top.core >= profile.high.core_min;
    let groups: u8 = top
        .families
        .iter()
        .filter_map(|family| independent_group(*family))
        .collect::<BTreeSet<_>>()
        .len() as u8;

    let mut level = if decisive
        || (top.core >= profile.high.core_min
            && groups >= profile.high.groups_min
            && lead >= i64::from(profile.high.lead_min))
    {
        Level::VeryLikely
    } else if top.score >= profile.medium.core_min && lead >= i64::from(profile.medium.lead_min) {
        Level::Likely
    } else if top.score >= profile.abstain_below {
        Level::Possible
    } else {
        Level::NotSure
    };
    // A decisive family may skip the lead margin, but not an actual tie.
    let lead_ok =
        second.is_none() || lead >= i64::from(profile.medium.lead_min) || (decisive && lead > 0);
    if !lead_ok {
        level = Level::NotSure;
    }
    for cap in input.level_caps {
        level = level.min(*cap);
    }
    let abstained = level == Level::NotSure;

    if abstained {
        for candidate in &mut candidates {
            candidate.level = Level::NotSure;
        }
    } else {
        for (index, candidate) in candidates.iter_mut().enumerate() {
            candidate.level = if index == 0 { level } else { Level::Possible };
        }
    }

    ConsensusOutput {
        candidates,
        level,
        abstained,
    }
}

fn demote(score: u32, role: Option<Role>, profile: &ConsensusProfile) -> u32 {
    let permille = match role {
        Some(Role::System | Role::Platform) => 0,
        Some(Role::HookFramework) => profile.hook_framework_demote_permille,
        Some(Role::Framework) => profile.framework_demote_permille,
        None => return score,
    };
    scale(score, permille)
}

fn scale(score: u32, permille: u32) -> u32 {
    if score == 0 {
        return 0;
    }
    let scaled = (u64::from(score) * u64::from(permille)).div_ceil(1000);
    scaled as u32
}
