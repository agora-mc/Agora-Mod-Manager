//! Choosing how several profiles import (MASTER_SPEC §26.10).
//!
//! A setup with more than one profile can be imported as one instance with a
//! loadout per profile, or as one instance per profile. Agora suggests a
//! grouping from how much the profiles' enabled content overlaps: profiles that
//! share most of their mods belong together, while unrelated ones do not.
//!
//! Overlap is the Jaccard index — shared enabled items divided by the items
//! either profile enables. A pair is grouped when it is **more than half**
//! alike: at or below half, the profiles are more different than alike, and a
//! merged instance would hold a large set that each loadout switches mostly
//! off. The number is a suggestion only; the player decides.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// Pairs sharing more than this share of their combined enabled items are
/// suggested for one instance.
pub const OVERLAP_THRESHOLD: f64 = 0.5;

/// What an import of several profiles makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Grouping {
    /// One instance holding the union, with one loadout per profile.
    Merged,
    /// One instance per profile, each with only that profile's enabled mods.
    Separate,
}

/// One profile's enabled items, by the name that identifies them in the setup
/// (a Vortex mod id, or an MO2 mod folder).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileSet {
    pub name: String,
    pub enabled: BTreeSet<String>,
}

/// How much two profiles share.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OverlapPair {
    pub a: String,
    pub b: String,
    pub shared: usize,
    pub union: usize,
    /// `shared / union`, or 0 when neither profile enables anything.
    pub ratio: f64,
}

/// One suggested instance: the profiles it would hold.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SuggestedGroup {
    pub profiles: Vec<String>,
    /// Every pair inside the group, with its overlap numbers, so the reason is visible.
    pub pairs: Vec<OverlapPair>,
}

/// Agora's grouping of a setup's profiles.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Suggestion {
    pub threshold: f64,
    pub groups: Vec<SuggestedGroup>,
    /// Every pair, grouped or not, with its numbers, so a suggestion can show why.
    pub pairs: Vec<OverlapPair>,
    /// Why the threshold is what it is, in words the player can read.
    pub note: String,
}

fn overlap(a: &ProfileSet, b: &ProfileSet) -> OverlapPair {
    let shared = a.enabled.intersection(&b.enabled).count();
    let union = a.enabled.union(&b.enabled).count();
    OverlapPair {
        a: a.name.clone(),
        b: b.name.clone(),
        shared,
        union,
        ratio: if union == 0 {
            0.0
        } else {
            shared as f64 / union as f64
        },
    }
}

/// Group profiles whose enabled items overlap by more than [`OVERLAP_THRESHOLD`].
///
/// Grouping follows links: if A and B are alike, and B and C are alike, all
/// three go together even when A and C alone are not. That matches a player's
/// intent ("these belong together") better than requiring every pair in a group
/// to pass.
pub fn suggest_grouping(profiles: &[ProfileSet]) -> Suggestion {
    let mut parent: Vec<usize> = (0..profiles.len()).collect();
    fn root(parent: &mut Vec<usize>, i: usize) -> usize {
        if parent[i] != i {
            let r = root(parent, parent[i]);
            parent[i] = r;
        }
        parent[i]
    }
    let mut all_pairs = Vec::new();
    for i in 0..profiles.len() {
        for j in (i + 1)..profiles.len() {
            let pair = overlap(&profiles[i], &profiles[j]);
            if pair.ratio > OVERLAP_THRESHOLD {
                let (ri, rj) = (root(&mut parent, i), root(&mut parent, j));
                if ri != rj {
                    parent[ri] = rj;
                }
            }
            all_pairs.push((i, j, pair));
        }
    }

    let mut by_root: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..profiles.len() {
        let r = root(&mut parent, i);
        by_root.entry(r).or_default().push(i);
    }

    let mut groups = Vec::new();
    for members in by_root.into_values() {
        let mut names: Vec<String> = members.iter().map(|i| profiles[*i].name.clone()).collect();
        names.sort();
        let pairs = all_pairs
            .iter()
            .filter(|(i, j, _)| members.contains(i) && members.contains(j))
            .map(|(_, _, pair)| pair.clone())
            .collect();
        groups.push(SuggestedGroup {
            profiles: names,
            pairs,
        });
    }
    groups.sort_by(|a, b| a.profiles.cmp(&b.profiles));

    Suggestion {
        threshold: OVERLAP_THRESHOLD,
        groups,
        pairs: all_pairs.into_iter().map(|(_, _, pair)| pair).collect(),
        note: format!(
            "Profiles sharing more than {:.0}% of their enabled mods are suggested for one instance. At or below that, they are more different than alike, so keeping them as separate instances avoids a large merge whose loadouts mostly switch each other off.",
            OVERLAP_THRESHOLD * 100.0
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(name: &str, items: &[&str]) -> ProfileSet {
        ProfileSet {
            name: name.into(),
            enabled: items.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn high_overlap_groups_together_and_low_overlap_stays_apart() {
        let profiles = vec![
            set("Combat", &["a", "b", "c", "d"]),
            set("Combat Alt", &["a", "b", "c", "e"]),
            set("Graphics", &["x", "y", "z"]),
        ];
        let suggestion = suggest_grouping(&profiles);
        let groups: Vec<Vec<String>> = suggestion
            .groups
            .iter()
            .map(|g| g.profiles.clone())
            .collect();
        assert_eq!(
            groups,
            vec![
                vec!["Combat".to_string(), "Combat Alt".to_string()],
                vec!["Graphics".to_string()],
            ]
        );
        let pair = suggestion
            .pairs
            .iter()
            .find(|p| p.a == "Combat" && p.b == "Combat Alt")
            .unwrap();
        assert_eq!(pair.shared, 3);
        assert_eq!(pair.union, 5);
        assert!((pair.ratio - 0.6).abs() < 1e-9);
        // The graphics profile shares nothing, so its group has no pair.
        assert!(suggestion.groups[1].pairs.is_empty());
    }

    #[test]
    fn exactly_half_is_not_more_than_half() {
        let profiles = vec![set("A", &["1", "2"]), set("B", &["1", "3"])];
        let suggestion = suggest_grouping(&profiles);
        assert_eq!(suggestion.groups.len(), 2, "a half overlap is not grouped");
        assert!((suggestion.pairs[0].ratio - 1.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn empty_profiles_do_not_divide_by_zero() {
        let profiles = vec![set("A", &[]), set("B", &[])];
        let suggestion = suggest_grouping(&profiles);
        assert_eq!(suggestion.groups.len(), 2);
        assert_eq!(suggestion.pairs[0].ratio, 0.0);
    }
}
