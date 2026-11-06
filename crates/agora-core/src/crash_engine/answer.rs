//! The answer: rank the evidence, or say plainly that Agora is not sure, and always give a next
//! step. A verdict that is not a crash names nobody.

use super::*;
use agora_game_api::crash::{
    CrashAnswer, Finding, Level, LocalText, NextStep, NextStepKind, SessionVerdict,
    CRASH_CONTRACT_VERSION,
};
use std::collections::BTreeSet;

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

    let mut findings: Vec<Finding> = input.evaluated.findings.clone();
    if let Some(cap) = input.evaluated.level_cap {
        for finding in &mut findings {
            finding.level = finding.level.min(cap);
        }
    }
    if not_a_crash {
        findings.retain(|finding| finding.candidate.is_none());
    }
    let findings = clean_findings(findings);

    let mut level = if not_a_crash {
        Level::NotSure
    } else {
        input.consensus.level
    };
    if let Some(cap) = input.evaluated.level_cap {
        level = level.min(cap);
    }

    let candidates = if not_a_crash {
        Vec::new()
    } else {
        input.consensus.candidates.clone()
    };
    let abstained = not_a_crash || input.consensus.abstained;

    let headline = headline(input.verdict, &candidates, level, abstained, not_a_crash);
    let next = next_step(input, not_a_crash, abstained, &candidates);
    let repairs = if not_a_crash {
        Vec::new()
    } else {
        input.evaluated.repairs.clone()
    };

    CrashAnswer {
        contract: CRASH_CONTRACT_VERSION,
        verdict: input.verdict,
        headline,
        level,
        candidates,
        findings,
        could_not_check: input.evaluated.could_not_check.clone(),
        trust_notes: input.evaluated.trust_notes.clone(),
        next,
        repairs,
        artifacts: Vec::new(),
    }
}

fn headline(
    verdict: SessionVerdict,
    candidates: &[agora_game_api::crash::Candidate],
    level: Level,
    abstained: bool,
    not_a_crash: bool,
) -> LocalText {
    if not_a_crash {
        let text = match verdict {
            SessionVerdict::Normal => "The game closed normally. There is no crash to explain.",
            _ => "You stopped the game. There is no crash to explain.",
        };
        return english(text);
    }
    let subject = match verdict {
        SessionVerdict::FailedToStart => "The game did not start",
        SessionVerdict::Froze => "The game froze",
        SessionVerdict::Crashed { .. } => "The game crashed",
        _ => "The game ended",
    };
    if candidates.is_empty() {
        return english(&format!(
            "{subject}, and Agora could not name one cause. A bisect can narrow it down, or you can ask another analyzer for a second opinion."
        ));
    }
    if abstained && candidates.len() > 1 {
        let names: Vec<String> = candidates
            .iter()
            .take(2)
            .map(|candidate| owner_name(&candidate.owner))
            .collect();
        return english(&format!(
            "Not sure: {} both have evidence. Try disabling one at a time.",
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
    let kind = if candidates.is_empty() {
        input
            .evaluated
            .next_steps
            .first()
            .copied()
            .unwrap_or(NextStepKind::Bisect)
    } else if abstained {
        NextStepKind::Bisect
    } else {
        match input.evaluated.next_steps.first().copied() {
            Some(NextStepKind::NothingNeeded) | None => NextStepKind::ConfirmExperiment,
            Some(kind) => kind,
        }
    };
    let keys: Vec<String> = if matches!(
        kind,
        NextStepKind::ConfirmExperiment | NextStepKind::ApplyRepair
    ) {
        candidates
            .first()
            .and_then(|candidate| owner_content_key(&candidate.owner))
            .into_iter()
            .collect()
    } else {
        keys.into_iter().collect()
    };
    NextStep { kind, keys }
}
