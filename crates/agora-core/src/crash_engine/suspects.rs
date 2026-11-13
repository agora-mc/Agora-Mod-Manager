//! Which answer candidates the test-it flow may turn off (MASTER_SPEC §30.4, §30.5).
//!
//! An answer ranks evidence; it does not decide what is safe to disable. Only an installed item
//! can be turned off, a framework or hook framework is always running and must never be offered as
//! an experiment, and an item the instance can no longer list has no file to disable. This module
//! holds that rule once so the desktop, the CLI and any future screen agree.

use agora_game_api::crash::{CrashAnswer, Level, Owner, Role};

use super::attribution::InstanceView;

/// A candidate the test-it flow could act on, with the file the instance view names for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestCandidate {
    /// The opaque key the screen shows and the confirmation record stores.
    pub owner_key: String,
    /// The name the answer uses for the owner.
    pub name: String,
    /// The game-relative file the item provides, when the view could list it. `None` means the
    /// item has no file to disable, so it is never offered as an experiment.
    pub file: Option<String>,
    pub level: Level,
    pub score: u32,
}

/// A stable, user-invisible key for an owner. The screen and the local confirmation record use the
/// same form, so a confirmed fix lines up with the same suspect next time.
pub fn public_owner_key(owner: &Owner) -> String {
    match owner {
        Owner::Item { key, .. } => format!("item:{key}"),
        Owner::Base => "base".to_string(),
        Owner::Generated { tool } => format!("generated:{tool}"),
        Owner::Writable => "writable".to_string(),
        Owner::Framework { name } => format!("framework:{name}"),
        Owner::System => "system".to_string(),
        Owner::NotInInstance => "not_in_instance".to_string(),
        Owner::Unknown => "unknown".to_string(),
    }
}

/// The ranked candidates an experiment may use, in the answer's order.
///
/// Only an installed item qualifies. A framework, hook framework, platform or system owner is
/// never turned off, and an item the view cannot list has no file to disable. The list is still
/// useful for display when `view` is absent; the caller decides what to do with a missing file.
pub fn installable_candidates(
    answer: &CrashAnswer,
    view: Option<&InstanceView>,
) -> Vec<TestCandidate> {
    answer
        .candidates
        .iter()
        .filter(|candidate| matches!(candidate.owner, Owner::Item { .. }))
        .filter(|candidate| {
            !matches!(
                candidate.role,
                Some(Role::Framework | Role::HookFramework | Role::System | Role::Platform)
            )
        })
        .map(|candidate| {
            let owner_key = public_owner_key(&candidate.owner);
            let file = match (&candidate.owner, view) {
                (Owner::Item { key, .. }, Some(view)) => view
                    .item_files(key)
                    .and_then(|files| files.into_iter().next())
                    .and_then(|path| file_name(&path)),
                _ => None,
            };
            TestCandidate {
                owner_key,
                name: super::owner_name(&candidate.owner),
                file,
                level: candidate.level,
                score: candidate.score,
            }
        })
        .collect()
}

fn file_name(path: &str) -> Option<String> {
    path.rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crash_engine::attribution::{LayerOwner, LayerView};
    use agora_game_api::crash::{
        Candidate, Cause, CrashAnswer, LocalText, MatchedBy, NextStep, NextStepKind,
        SessionVerdict, CRASH_CONTRACT_VERSION,
    };

    fn answer(candidates: Vec<Candidate>) -> CrashAnswer {
        CrashAnswer {
            contract: CRASH_CONTRACT_VERSION,
            verdict: SessionVerdict::Crashed {
                how: MatchedBy::Supplied,
            },
            headline: LocalText::new(),
            level: Level::Likely,
            cause: Some(Cause::Mod),
            candidates,
            findings: Vec::new(),
            inspections: Vec::new(),
            could_not_check: Vec::new(),
            trust_notes: Vec::new(),
            next: NextStep {
                kind: NextStepKind::ConfirmExperiment,
                keys: Vec::new(),
            },
            repairs: Vec::new(),
            actions: Vec::new(),
            artifacts: Vec::new(),
        }
    }

    fn candidate(owner: Owner, role: Option<Role>, score: u32) -> Candidate {
        Candidate {
            owner,
            level: Level::Possible,
            score,
            cause: Cause::Mod,
            role,
            families: Vec::new(),
            sources: Vec::new(),
            also_provided_by: Vec::new(),
            save_risk: None,
        }
    }

    fn view() -> InstanceView {
        InstanceView::new(vec![LayerView::new(
            "mod:one",
            true,
            LayerOwner::Item {
                key: "jar:one.jar:abc".to_string(),
                name: "One".to_string(),
            },
            vec!["mods/one.jar".to_string()],
        )])
    }

    #[test]
    fn only_an_installable_item_is_offered_and_it_keeps_its_rank() {
        let answer = answer(vec![
            candidate(
                Owner::Item {
                    key: "jar:one.jar:abc".to_string(),
                    name: "One".to_string(),
                },
                None,
                9,
            ),
            candidate(
                Owner::Framework {
                    name: "Invented Engine".to_string(),
                },
                Some(Role::Framework),
                8,
            ),
            candidate(Owner::System, None, 7),
        ]);
        let candidates = installable_candidates(&answer, Some(&view()));
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].owner_key, "item:jar:one.jar:abc");
        assert_eq!(candidates[0].file.as_deref(), Some("one.jar"));
    }

    #[test]
    fn an_item_with_a_framework_role_is_never_offered() {
        let answer = answer(vec![candidate(
            Owner::Item {
                key: "jar:hook.jar:def".to_string(),
                name: "A hook framework".to_string(),
            },
            Some(Role::HookFramework),
            9,
        )]);
        assert!(installable_candidates(&answer, Some(&view())).is_empty());
    }

    #[test]
    fn a_missing_view_leaves_the_file_unset_rather_than_guessing() {
        let answer = answer(vec![candidate(
            Owner::Item {
                key: "jar:one.jar:abc".to_string(),
                name: "One".to_string(),
            },
            None,
            9,
        )]);
        let candidates = installable_candidates(&answer, None);
        assert_eq!(candidates.len(), 1);
        assert!(candidates[0].file.is_none());
    }
}
