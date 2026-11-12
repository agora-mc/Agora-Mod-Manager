//! Whether removing a suspect is safe for a save (MASTER_SPEC §30.4; owner's point, 2026-11-10).
//!
//! A pack declares, as data, which files make a mod a risk to an existing save. Core holds no game
//! words: it merges the patterns the instance's packs declare and classifies the item's files from
//! the instance view the resolver already has. Without an instance nothing is claimed, and a file
//! the pack says nothing about is `unknown`, never `safe`.

use agora_game_api::crash::{Candidate, Owner, SaveRisk, SaveRiskLabel};
use agora_game_api::fold_path_case;

use super::RefResolver;

/// Merge every contributing pack's `save_risk` patterns. Returns `None` when no pack says anything,
/// so the answer carries no labels rather than guessing.
pub fn merge<'a>(rules: impl IntoIterator<Item = &'a SaveRisk>) -> Option<SaveRisk> {
    let mut risky = Vec::new();
    let mut safe = Vec::new();
    for rule in rules {
        risky.extend(rule.risky.iter().cloned());
        safe.extend(rule.safe.iter().cloned());
    }
    if risky.is_empty() && safe.is_empty() {
        return None;
    }
    risky.sort();
    risky.dedup();
    safe.sort();
    safe.dedup();
    Some(SaveRisk { risky, safe })
}

/// Classify one item's files. `None` means the instance could not be listed; an empty list means
/// the item provides nothing. Both are `unknown`, never `safe`.
pub fn classify(files: Option<&[String]>, rules: &SaveRisk) -> SaveRiskLabel {
    let Some(files) = files else {
        return SaveRiskLabel::Unknown;
    };
    if files.is_empty() {
        return SaveRiskLabel::Unknown;
    }
    let mut every_file_is_safe = true;
    for file in files {
        let path = normalize(file);
        if matches_any(&rules.risky, &path) {
            return SaveRiskLabel::Risky;
        }
        if !matches_any(&rules.safe, &path) {
            every_file_is_safe = false;
        }
    }
    if every_file_is_safe {
        SaveRiskLabel::Safe
    } else {
        SaveRiskLabel::Unknown
    }
}

/// Label every installed-item suspect in `candidates`, using the files the resolver lists.
pub fn apply(candidates: &mut [Candidate], rules: &SaveRisk, resolver: &dyn RefResolver) {
    for candidate in candidates {
        let Owner::Item { key, .. } = &candidate.owner else {
            continue;
        };
        let files = resolver.item_files(key);
        candidate.save_risk = Some(classify(files.as_deref(), rules));
    }
}

fn normalize(path: &str) -> String {
    fold_path_case(path.trim().replace('\\', "/").trim_start_matches('/'))
}

fn matches_any(patterns: &[String], path: &str) -> bool {
    patterns
        .iter()
        .any(|pattern| agora_game_api::glob_match(&fold_path_case(pattern), path))
}
