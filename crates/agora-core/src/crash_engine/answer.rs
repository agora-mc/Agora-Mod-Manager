//! The answer: rank the evidence, or say plainly that Agora is not sure, and always give a next
//! step. A verdict that is not a crash names nobody.

use super::*;
use agora_game_api::crash::{
    Cause, CrashAnswer, Finding, Level, LocalText, NextStep, NextStepKind, Scope, SessionVerdict,
    CRASH_CONTRACT_VERSION,
};
use std::collections::{BTreeMap, BTreeSet};

pub struct AnswerInput<'a> {
    pub verdict: SessionVerdict,
    pub evaluated: &'a EvaluateOutput,
    pub consensus: &'a ConsensusOutput,
}

pub fn answer(input: &AnswerInput<'_>) -> CrashAnswer {
    let not_a_crash = matches!(
        input.verdict,
        SessionVerdict::Normal | SessionVerdict::Stopped
    );

    let closed_unknown = input.verdict == SessionVerdict::ClosedUnknown;
    let trust_cap = input.evaluated.level_cap;

    let mut level = if not_a_crash {
        Level::NotSure
    } else {
        input.consensus.level
    };
    if let Some(cap) = trust_cap {
        level = level.min(cap);
    }
    if closed_unknown {
        level = level.min(Level::Possible);
    }

    let mut candidates = if not_a_crash {
        Vec::new()
    } else {
        input.consensus.candidates.clone()
    };
    for candidate in &mut candidates {
        if let Some(cap) = trust_cap {
            candidate.level = candidate.level.min(cap);
        }
        candidate.level = candidate.level.min(level);
    }
    let candidate_levels: BTreeMap<String, Level> = candidates
        .iter()
        .map(|candidate| (owner_key(&candidate.owner), candidate.level))
        .collect();
    let abstained = not_a_crash || input.consensus.abstained;

    let mut findings: Vec<Finding> = input.evaluated.findings.clone();
    for finding in &mut findings {
        let mut cap = Level::VeryLikely;
        if let Some(trust) = trust_cap {
            cap = cap.min(trust);
        }
        if closed_unknown {
            cap = cap.min(Level::Possible);
        }
        if not_a_crash {
            cap = cap.min(Level::NotSure);
        }
        if let Some(owner) = &finding.candidate {
            match candidate_levels.get(&owner_key(owner)) {
                Some(candidate_level) => cap = cap.min(*candidate_level),
                None => cap = cap.min(Level::Possible),
            }
        } else if finding.scope == Scope::RootCause {
            // A root cause that names nobody cannot be stronger than possible: consensus could
            // not weigh it.
            cap = cap.min(Level::Possible);
        }
        finding.level = finding.level.min(cap);
    }
    if not_a_crash {
        findings.retain(|finding| finding.candidate.is_none());
    }
    let findings = clean_findings(findings);

    // A finding that accuses nobody is a complete answer in its own right. When it is stronger
    // than the top suspect (or the only thing there is), the headline, level and cause follow it:
    // a headline may never contradict the strongest finding.
    let top_candidate_level = candidates.first().map_or(Level::NotSure, |top| top.level);
    let strongest = findings
        .iter()
        .filter(|finding| finding.level > Level::NotSure)
        .min_by_key(|finding| {
            (
                std::cmp::Reverse(finding.level),
                scope_rank(finding.scope),
                std::cmp::Reverse(cause_rank(finding.cause)),
            )
        });
    let finding_wins = strongest.is_some_and(|finding| {
        candidates.is_empty()
            || finding.level > top_candidate_level
            // A specific non-mod cause at the same level as a suspect is the better answer: a mod
            // on the stack during an out-of-memory is not what the log says caused it.
            || (finding.level == top_candidate_level
                && !matches!(finding.cause, Cause::Mod | Cause::Other))
    });
    if finding_wins {
        if let Some(finding) = strongest {
            level = level.max(finding.level);
        }
    }
    let cause = if not_a_crash {
        None
    } else if finding_wins {
        strongest.map(|finding| finding.cause)
    } else {
        top_cause(&candidates, &findings)
    };

    let headline = match strongest.filter(|_| finding_wins) {
        Some(finding) if closed_unknown => english(&format!(
            "The game ended, and Agora cannot tell whether this was a crash. {}",
            finding
                .headline
                .get("en")
                .map(String::as_str)
                .unwrap_or_default()
        )),
        Some(finding) => finding.headline.clone(),
        None => headline(
            input.verdict,
            &candidates,
            level,
            abstained,
            not_a_crash,
            closed_unknown,
        ),
    };
    let strongest_level = findings
        .iter()
        .map(|finding| finding.level)
        .max()
        .unwrap_or(Level::NotSure);
    let next = next_step(input, not_a_crash, abstained, &candidates, strongest_level);
    let repairs = if not_a_crash {
        Vec::new()
    } else {
        input.evaluated.repairs.clone()
    };
    let actions = if not_a_crash {
        Vec::new()
    } else {
        input.evaluated.actions.clone()
    };

    CrashAnswer {
        contract: CRASH_CONTRACT_VERSION,
        verdict: input.verdict,
        headline,
        level,
        cause,
        candidates,
        findings,
        inspections: input.evaluated.inspections.clone(),
        could_not_check: input.evaluated.could_not_check.clone(),
        trust_notes: input.evaluated.trust_notes.clone(),
        next,
        repairs,
        actions,
        artifacts: Vec::new(),
    }
}

/// How specific a cause is when two findings tie on level and scope. `other` is the open door, so
/// it is the last choice; `mod` is more specific than `other` but less than a named kind.
fn cause_rank(cause: Cause) -> u8 {
    match cause {
        Cause::Other => 0,
        Cause::Mod => 1,
        _ => 2,
    }
}

fn top_cause(
    candidates: &[agora_game_api::crash::Candidate],
    findings: &[Finding],
) -> Option<Cause> {
    if let Some(top) = candidates.first() {
        // The top candidate's cause already comes from its strongest clue that states one (and
        // a root-cause finding feeds a clue), so the answer follows the top candidate.
        return Some(top.cause);
    }
    // Nobody is named. A finding that states a non-mod cause is the whole answer, so the strongest
    // specific kind wins over `other`, which is the open door.
    findings
        .iter()
        .filter(|finding| finding.cause != Cause::Other)
        .max_by_key(|finding| {
            (
                finding.level,
                std::cmp::Reverse(scope_rank(finding.scope)),
                std::cmp::Reverse(finding.cause),
            )
        })
        .or_else(|| {
            findings
                .iter()
                .max_by_key(|finding| (finding.level, std::cmp::Reverse(scope_rank(finding.scope))))
        })
        .map(|finding| finding.cause)
}

fn headline(
    verdict: SessionVerdict,
    candidates: &[agora_game_api::crash::Candidate],
    level: Level,
    abstained: bool,
    not_a_crash: bool,
    closed_unknown: bool,
) -> LocalText {
    if not_a_crash {
        let text = match verdict {
            SessionVerdict::Normal => "The game closed normally. There is no crash to explain.",
            _ => "You stopped the game. There is no crash to explain.",
        };
        return english(text);
    }
    if closed_unknown {
        let base = "The game ended, and Agora cannot tell whether this was a crash.";
        if candidates.is_empty() {
            return english(base);
        }
        return english(&format!(
            "{base} {} is possible evidence.",
            owner_name(&candidates[0].owner)
        ));
    }
    let subject = match verdict {
        SessionVerdict::FailedToStart => "The game did not start",
        SessionVerdict::Froze => "The game froze",
        SessionVerdict::Crashed { .. } => "The game crashed",
        _ => "The game ended",
    };
    if candidates.is_empty() {
        return english(&format!(
            "{subject}, and Agora could not name one cause. A bisect can narrow it down, or you can ask another analyzer for a second opinion. Removing mods partway through a playthrough can damage a save, so back it up or test on a new game first."
        ));
    }
    if abstained && candidates.len() > 1 {
        let names: Vec<String> = candidates
            .iter()
            .take(2)
            .map(|candidate| owner_name(&candidate.owner))
            .collect();
        return english(&format!(
            "Not sure: {} both have evidence. Try disabling one at a time, after backing up your save: removing a mod partway through a playthrough can damage it.",
            names.join(" and ")
        ));
    }
    let name = owner_name(&candidates[0].owner);
    let text = match level {
        Level::VeryLikely => format!("{subject}, and {name} is very likely the cause."),
        Level::Likely => format!("{subject}, and {name} is likely the cause."),
        Level::Possible => format!("{subject}, and {name} is possible evidence."),
        Level::NotSure => format!("{subject}, and Agora is not sure what caused it."),
    };
    english(&text)
}

fn next_step(
    input: &AnswerInput<'_>,
    not_a_crash: bool,
    abstained: bool,
    candidates: &[agora_game_api::crash::Candidate],
    strongest_finding: Level,
) -> NextStep {
    if not_a_crash {
        return NextStep {
            kind: NextStepKind::NothingNeeded,
            keys: Vec::new(),
        };
    }
    let mut keys: BTreeSet<String> = BTreeSet::new();
    for candidate in candidates {
        if let Some(key) = owner_content_key(&candidate.owner) {
            keys.insert(key);
        }
    }
    // A bisect is only useful when nothing is at least likely, and never when no mod loaded: there
    // is nothing to halve. Prefer whatever non-bisect step a rule offered.
    let bisect_is_wrong =
        strongest_finding >= Level::Likely || input.evaluated.content_loaded == Some(false);
    let preferred = input
        .evaluated
        .next_steps
        .iter()
        .copied()
        .find(|kind| *kind != NextStepKind::Bisect);
    let kind = if candidates.is_empty() {
        let offered = input
            .evaluated
            .next_steps
            .first()
            .copied()
            .unwrap_or(NextStepKind::Bisect);
        if offered == NextStepKind::Bisect && bisect_is_wrong {
            preferred.unwrap_or(NextStepKind::SecondOpinion)
        } else {
            offered
        }
    } else if abstained {
        if bisect_is_wrong {
            preferred.unwrap_or(NextStepKind::SecondOpinion)
        } else {
            NextStepKind::Bisect
        }
    } else {
        let offered = match input.evaluated.next_steps.first().copied() {
            Some(NextStepKind::NothingNeeded) | None => NextStepKind::ConfirmExperiment,
            Some(kind) => kind,
        };
        if offered == NextStepKind::Bisect && bisect_is_wrong {
            preferred.unwrap_or(NextStepKind::SecondOpinion)
        } else {
            offered
        }
    };
    let keys: Vec<String> = match kind {
        NextStepKind::ConfirmExperiment | NextStepKind::ApplyRepair => candidates
            .first()
            .and_then(|candidate| owner_content_key(&candidate.owner))
            .into_iter()
            .collect(),
        NextStepKind::Bisect => keys.into_iter().collect(),
        _ => Vec::new(),
    };
    NextStep { kind, keys }
}
