//! The engine's `core.runtime_file` facts: framework files a session's plugins needed, from the
//! same check the instance and launch checks run. This stays game-free: the fact names a rule and
//! the expected file, and a pack decides whether that means anything for its game.

use std::collections::BTreeMap;

use agora_game_api::crash::{AttrValue, Fact, Scalar};
use agora_game_api::{RuntimeFileFinding, RuntimeFileProblem};

/// One fact per runtime-file finding. A finding that could not be checked is a fact too, so a rule
/// can say Agora could not tell rather than silently finding nothing.
pub fn runtime_file_facts(findings: &[RuntimeFileFinding]) -> Vec<Fact> {
    findings
        .iter()
        .map(|finding| {
            let problem = match &finding.problem {
                RuntimeFileProblem::WrongVersion => "wrong_version",
                RuntimeFileProblem::MissingFiles => "missing_files",
                RuntimeFileProblem::Missing => "missing",
                RuntimeFileProblem::CannotCheck { .. } => "cannot_check",
            };
            let mut attrs = BTreeMap::new();
            attrs.insert(
                "rule_id".to_string(),
                AttrValue::Scalar(Scalar::Text(finding.rule_id.clone())),
            );
            attrs.insert(
                "rule_name".to_string(),
                AttrValue::Scalar(Scalar::Text(finding.rule_name.clone())),
            );
            attrs.insert(
                "problem".to_string(),
                AttrValue::Scalar(Scalar::Text(problem.to_string())),
            );
            attrs.insert(
                "expected".to_string(),
                AttrValue::Scalar(Scalar::Text(finding.expected.clone())),
            );
            if let RuntimeFileProblem::CannotCheck { reason } = &finding.problem {
                attrs.insert(
                    "reason".to_string(),
                    AttrValue::Scalar(Scalar::Text(reason.clone())),
                );
            }
            Fact {
                kind: "core.runtime_file".to_string(),
                attrs,
                refs: Vec::new(),
                span: None,
            }
        })
        .collect()
}

/// The fact for a runtime-file check that could not run at all. It says so, never that nothing was
/// found.
pub fn runtime_file_check_failed(reason: &str) -> Fact {
    let mut attrs = BTreeMap::new();
    attrs.insert(
        "problem".to_string(),
        AttrValue::Scalar(Scalar::Text("cannot_check".to_string())),
    );
    attrs.insert(
        "reason".to_string(),
        AttrValue::Scalar(Scalar::Text(reason.to_string())),
    );
    Fact {
        kind: "core.runtime_file".to_string(),
        attrs,
        refs: Vec::new(),
        span: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(problem: RuntimeFileProblem) -> RuntimeFileFinding {
        RuntimeFileFinding {
            rule_id: "address-library".into(),
            rule_name: "Address Library for SKSE Plugins".into(),
            expected: "Data/SKSE/Plugins/versionlib-1-6-1170-0.bin".into(),
            found: vec!["Data/SKSE/Plugins/EngineFixes.dll".into()],
            repair: "Install the Address Library".into(),
            repair_from_source: false,
            required_total: None,
            problem,
        }
    }

    #[test]
    fn a_missing_runtime_file_is_a_fact_with_its_expected_path() {
        let facts = runtime_file_facts(&[finding(RuntimeFileProblem::Missing)]);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].kind, "core.runtime_file");
        assert_eq!(
            facts[0].attrs.get("problem"),
            Some(&AttrValue::Scalar(Scalar::Text("missing".into())))
        );
        assert_eq!(
            facts[0].attrs.get("expected"),
            Some(&AttrValue::Scalar(Scalar::Text(
                "Data/SKSE/Plugins/versionlib-1-6-1170-0.bin".into()
            )))
        );
    }

    #[test]
    fn a_check_that_could_not_run_is_a_fact_that_says_so() {
        let facts = runtime_file_facts(&[finding(RuntimeFileProblem::CannotCheck {
            reason: "the base folder is missing".into(),
        })]);
        assert_eq!(
            facts[0].attrs.get("problem"),
            Some(&AttrValue::Scalar(Scalar::Text("cannot_check".into())))
        );
        assert_eq!(
            facts[0].attrs.get("reason"),
            Some(&AttrValue::Scalar(Scalar::Text(
                "the base folder is missing".into()
            )))
        );
    }

    #[test]
    fn a_check_that_did_not_run_at_all_is_still_stated() {
        let fact = runtime_file_check_failed("the instance could not be read");
        assert_eq!(fact.kind, "core.runtime_file");
        assert_eq!(
            fact.attrs.get("problem"),
            Some(&AttrValue::Scalar(Scalar::Text("cannot_check".into())))
        );
    }
}
