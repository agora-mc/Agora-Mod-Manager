//! Rule evaluation: facts in, signals and findings out. Conditions use Kleene three-valued logic,
//! because an absent section is not a clean one.

use super::*;
use agora_game_api::crash::{
    AttrMatch, AttrValue, Cause, Condition, CouldNotCheck, CouldNotCheckReason, CountQuery,
    Coverage, CoverageState, Effect, EvidenceLine, Fact, FactQuery, Family, Finding, InspectResult,
    InspectionRecord, Level, LocalText, MatchOps, NextStepKind, ParseNoteKind, Ref, RepairOffer,
    RepairRequest, ResolvedRepair, Role, Rule, RuleText, Scalar, Scope, Signal, SourceAttribution,
    Strength, TextMatch, TrustNote, Truth,
};
use std::collections::{BTreeMap, BTreeSet};

/// Everything evaluate needs. Borrowed, so a caller keeps ownership of packs and parser output.
pub struct EvaluateInput<'a> {
    pub packs: &'a [PackInput],
    pub parsed: &'a [ParsedArtifact],
    pub engine_facts: &'a EngineFacts,
    pub runtime_version: Option<&'a str>,
    pub resolver: &'a dyn RefResolver,
    pub inspector: &'a dyn Inspector,
}

/// What the rules said. Consensus and the answer take it from here.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EvaluateOutput {
    pub signals: Vec<Signal>,
    pub findings: Vec<Finding>,
    /// What each deeper check decided, with its detail and facts, for the answer to show.
    pub inspections: Vec<InspectionRecord>,
    pub could_not_check: Vec<CouldNotCheck>,
    pub trust_notes: Vec<TrustNote>,
    /// The weakest cap a `trust` effect gave, if any.
    pub level_cap: Option<Level>,
    pub repairs: Vec<RepairOffer>,
    pub next_steps: Vec<NextStepKind>,
    /// The role of each candidate owner, for consensus's demotion rule.
    pub roles: BTreeMap<String, Role>,
}

#[derive(Debug, Clone)]
struct ParserStamp {
    plugin: PluginIdentity,
    parser_id: String,
}

#[derive(Debug, Clone)]
struct FactEntry {
    fact: Fact,
    parser: Option<ParserStamp>,
    artifact: String,
    source: Option<String>,
    producer_format: String,
    producer_version: Option<String>,
}

impl FactEntry {
    fn evidence(&self) -> EvidenceLine {
        let span = self.fact.span.clone().unwrap_or_default();
        let artifact = span
            .artifact
            .clone()
            .unwrap_or_else(|| self.artifact.clone());
        let quote = self.fact.attrs.get("text").and_then(|value| match value {
            AttrValue::Scalar(Scalar::Text(text)) => Some(text.clone()),
            _ => None,
        });
        EvidenceLine {
            artifact,
            span,
            quote,
        }
    }

    fn sort_key(&self) -> String {
        let fact = serde_json::to_string(&self.fact).unwrap_or_else(|_| format!("{:?}", self.fact));
        let parser = self
            .parser
            .as_ref()
            .map(|stamp| format!("{}\0{}", stamp.plugin.id, stamp.parser_id))
            .unwrap_or_default();
        format!(
            "{}\0{}\0{}\0{}\0{}\0{}",
            fact,
            self.artifact,
            self.source.as_deref().unwrap_or(""),
            self.producer_format,
            self.producer_version.as_deref().unwrap_or(""),
            parser
        )
    }
}

/// Add the engine attribute rules may use: how many actionable frames precede this one.
fn annotate_frames(facts: &mut [FactEntry], resolver: &dyn RefResolver) {
    let mut frames: Vec<usize> = facts
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.fact.kind == "core.frame")
        .map(|(index, _)| index)
        .collect();
    frames.sort_by(|a, b| {
        let a = &facts[*a];
        let b = &facts[*b];
        frame_artifact(a)
            .cmp(&frame_artifact(b))
            .then_with(|| frame_index(a).cmp(&frame_index(b)))
            .then_with(|| frame_content(a).cmp(&frame_content(b)))
    });
    let mut actionable = 0u64;
    for index in frames {
        let dropped = facts[index].fact.refs.iter().any(|reference| {
            matches!(
                resolver.resolve(reference).role,
                Some(Role::System | Role::Platform)
            )
        });
        if dropped {
            continue;
        }
        facts[index].fact.attrs.insert(
            "actionable_index".to_string(),
            AttrValue::Scalar(Scalar::Number(actionable as f64)),
        );
        actionable += 1;
    }
}

fn frame_artifact(entry: &FactEntry) -> String {
    entry
        .fact
        .span
        .as_ref()
        .and_then(|span| span.artifact.clone())
        .unwrap_or_else(|| entry.artifact.clone())
}

fn frame_index(entry: &FactEntry) -> u64 {
    entry
        .fact
        .attrs
        .get("index")
        .and_then(first_scalar)
        .and_then(scalar_number)
        .map(|value| value as u64)
        .unwrap_or(u64::MAX)
}

fn frame_content(entry: &FactEntry) -> String {
    let fact = serde_json::to_string(&entry.fact).unwrap_or_default();
    let parser = entry
        .parser
        .as_ref()
        .map(|stamp| format!("{}\0{}", stamp.plugin.id, stamp.parser_id))
        .unwrap_or_default();
    format!(
        "{fact}\0{}\0{}\0{}\0{}",
        entry.source.as_deref().unwrap_or(""),
        entry.producer_format,
        entry.producer_version.as_deref().unwrap_or(""),
        parser
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoverageClass {
    Covered,
    Unknown(CouldNotCheckReason),
}

struct View {
    facts: Vec<FactEntry>,
    coverage: Vec<Coverage>,
}

impl View {
    fn build(
        parsed: &[&ParsedArtifact],
        engine_facts: &[Fact],
        include_engine: bool,
        resolver: &dyn RefResolver,
    ) -> Self {
        let mut facts = Vec::new();
        let mut coverage = Vec::new();
        for artifact in parsed {
            for fact in &artifact.output.facts {
                facts.push(FactEntry {
                    fact: fact.clone(),
                    parser: Some(ParserStamp {
                        plugin: artifact.plugin.clone(),
                        parser_id: artifact.parser_id.clone(),
                    }),
                    artifact: artifact.artifact.clone(),
                    source: artifact.source.clone(),
                    producer_format: artifact.output.producer.format.clone(),
                    producer_version: artifact.output.producer.version.clone(),
                });
            }
            coverage.extend(artifact.output.coverage.iter().cloned());
        }
        if include_engine {
            for fact in engine_facts {
                facts.push(FactEntry {
                    fact: fact.clone(),
                    parser: None,
                    artifact: "Agora".to_string(),
                    source: None,
                    producer_format: String::new(),
                    producer_version: None,
                });
            }
        }
        annotate_frames(&mut facts, resolver);
        // Each key serialises a fact, so compute it once per fact, not once per comparison.
        facts.sort_by_cached_key(FactEntry::sort_key);
        Self { facts, coverage }
    }

    fn facts_of_kind<'a>(&'a self, kind: &'a str) -> impl Iterator<Item = &'a FactEntry> {
        self.facts
            .iter()
            .filter(move |entry| entry.fact.kind == kind)
    }

    fn coverage_of(&self, kind: &str) -> CoverageClass {
        if self.facts.iter().any(|entry| entry.fact.kind == kind) {
            return CoverageClass::Covered;
        }
        let mut checked = false;
        let mut absent = false;
        let mut not_applicable = false;
        for coverage in &self.coverage {
            if coverage.kind != kind {
                continue;
            }
            match coverage.state {
                CoverageState::Checked => checked = true,
                CoverageState::Absent => absent = true,
                CoverageState::NotApplicable => not_applicable = true,
            }
        }
        if checked {
            CoverageClass::Covered
        } else if absent {
            CoverageClass::Unknown(CouldNotCheckReason::AbsentFromArtifact)
        } else if not_applicable {
            CoverageClass::Covered
        } else {
            CoverageClass::Unknown(CouldNotCheckReason::NotCovered)
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
struct Bindings {
    values: BTreeMap<String, Scalar>,
    refs: BTreeMap<String, BoundRef>,
}

#[derive(Debug, Clone, PartialEq)]
struct BoundRef {
    reference: Ref,
    parser: Option<String>,
    evidence: Option<EvidenceLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UnknownNote {
    subject: String,
    reason: CouldNotCheckReason,
}

#[derive(Debug, Clone)]
struct Branch {
    truth: Truth,
    bindings: Bindings,
    evidence: Vec<EvidenceLine>,
    unknown: Vec<UnknownNote>,
}

impl Branch {
    fn initial(bindings: Bindings) -> Self {
        Self {
            truth: Truth::True,
            bindings,
            evidence: Vec::new(),
            unknown: Vec::new(),
        }
    }

    fn with_truth(mut self, truth: Truth) -> Self {
        self.truth = truth;
        self
    }
}

struct EvalResult {
    branches: Vec<Branch>,
    overflow: bool,
}

fn cap_branches(mut branches: Vec<Branch>, limit: usize, overflow: &mut bool) -> Vec<Branch> {
    if branches.len() > limit {
        *overflow = true;
        branches.truncate(limit);
    }
    branches
}

fn merge_unknown(a: &[UnknownNote], b: &[UnknownNote]) -> Vec<UnknownNote> {
    let mut out = a.to_vec();
    for note in b {
        if !out.contains(note) {
            out.push(note.clone());
        }
    }
    out
}

fn eval_condition(
    condition: &Condition,
    view: &View,
    limit: usize,
    resolver: &dyn RefResolver,
    bindings: &Bindings,
) -> EvalResult {
    match condition {
        Condition::All(list) => {
            let mut result = EvalResult {
                branches: vec![Branch::initial(bindings.clone())],
                overflow: false,
            };
            for child in list {
                let mut next = Vec::new();
                for branch in result.branches {
                    if branch.truth == Truth::False {
                        next.push(branch);
                        continue;
                    }
                    let child_result =
                        eval_condition(child, view, limit, resolver, &branch.bindings);
                    result.overflow |= child_result.overflow;
                    for outcome in child_result.branches {
                        let truth = branch.truth.and(outcome.truth);
                        let mut evidence = branch.evidence.clone();
                        evidence.extend(outcome.evidence);
                        next.push(Branch {
                            truth,
                            bindings: outcome.bindings,
                            evidence,
                            unknown: merge_unknown(&branch.unknown, &outcome.unknown),
                        });
                    }
                }
                result.branches = cap_branches(next, limit, &mut result.overflow);
            }
            result
        }
        Condition::Any(list) => {
            // A disjunction starts false. Starting true would skip every child.
            let mut result = EvalResult {
                branches: vec![Branch {
                    truth: Truth::False,
                    bindings: bindings.clone(),
                    evidence: Vec::new(),
                    unknown: Vec::new(),
                }],
                overflow: false,
            };
            for child in list {
                let mut next = Vec::new();
                for branch in &result.branches {
                    if branch.truth == Truth::True {
                        next.push(branch.clone());
                        continue;
                    }
                    let child_result =
                        eval_condition(child, view, limit, resolver, &branch.bindings);
                    result.overflow |= child_result.overflow;
                    for outcome in child_result.branches {
                        let truth = branch.truth.or(outcome.truth);
                        // Only a true child contributes bindings and evidence. A speculative
                        // binding from an unknown child must not leak into a later true branch.
                        let bindings = if outcome.truth == Truth::True {
                            outcome.bindings
                        } else {
                            branch.bindings.clone()
                        };
                        let mut evidence = branch.evidence.clone();
                        if outcome.truth == Truth::True {
                            evidence.extend(outcome.evidence);
                        }
                        next.push(Branch {
                            truth,
                            bindings,
                            evidence,
                            unknown: merge_unknown(&branch.unknown, &outcome.unknown),
                        });
                    }
                }
                result.branches = cap_branches(next, limit, &mut result.overflow);
            }
            result
        }
        Condition::Not(inner) => {
            let evaluated = eval_condition(inner, view, limit, resolver, bindings);
            let any_true = evaluated
                .branches
                .iter()
                .any(|branch| branch.truth == Truth::True);
            let mut unknown = Vec::new();
            for branch in &evaluated.branches {
                if branch.truth == Truth::Unknown {
                    unknown = merge_unknown(&unknown, &branch.unknown);
                }
            }
            let truth = if any_true {
                Truth::False
            } else if !unknown.is_empty()
                || evaluated.branches.iter().any(|b| b.truth == Truth::Unknown)
            {
                Truth::Unknown
            } else {
                Truth::True
            };
            EvalResult {
                branches: vec![Branch {
                    truth,
                    bindings: bindings.clone(),
                    evidence: Vec::new(),
                    unknown,
                }],
                overflow: evaluated.overflow,
            }
        }
        Condition::AtLeast { n, of } => {
            let mut true_branches = Vec::new();
            let mut true_count = 0usize;
            let mut unknown_count = 0usize;
            let mut evidence = Vec::new();
            let mut unknown = Vec::new();
            let mut overflow = false;
            for child in of {
                let evaluated = eval_condition(child, view, limit, resolver, bindings);
                overflow |= evaluated.overflow;
                let mut child_true: Option<Branch> = None;
                let mut child_unknown = false;
                for branch in evaluated.branches {
                    match branch.truth {
                        Truth::True => {
                            if child_true.is_none() {
                                child_true = Some(branch);
                            }
                        }
                        Truth::Unknown => {
                            child_unknown = true;
                            unknown = merge_unknown(&unknown, &branch.unknown);
                        }
                        Truth::False => {}
                    }
                }
                if let Some(branch) = child_true {
                    true_count += 1;
                    evidence.extend(branch.evidence);
                    true_branches.push(branch.bindings);
                } else if child_unknown {
                    unknown_count += 1;
                }
            }
            let needed = (*n as usize).max(1);
            let truth = if true_count >= needed {
                Truth::True
            } else if true_count + unknown_count >= needed {
                Truth::Unknown
            } else {
                Truth::False
            };
            let merged = if truth == Truth::True {
                common_bindings(&true_branches)
            } else {
                bindings.clone()
            };
            EvalResult {
                branches: vec![Branch {
                    truth,
                    bindings: merged,
                    evidence: if truth == Truth::True {
                        evidence
                    } else {
                        Vec::new()
                    },
                    unknown: if truth == Truth::Unknown {
                        unknown
                    } else {
                        Vec::new()
                    },
                }],
                overflow,
            }
        }
        Condition::Fact(query) => eval_fact(query, view, limit, resolver, bindings),
        Condition::Count(query) => eval_count(query, view, limit, resolver, bindings),
        Condition::Text(text) => eval_text(text, view, limit, bindings),
    }
}

fn common_bindings(branches: &[Bindings]) -> Bindings {
    let Some(first) = branches.first() else {
        return Bindings::default();
    };
    let mut out = first.clone();
    out.values.retain(|name, value| {
        branches
            .iter()
            .all(|branch| branch.values.get(name) == Some(value))
    });
    out.refs.retain(|name, value| {
        branches
            .iter()
            .all(|branch| branch.refs.get(name) == Some(value))
    });
    out
}

fn eval_fact(
    query: &FactQuery,
    view: &View,
    limit: usize,
    resolver: &dyn RefResolver,
    bindings: &Bindings,
) -> EvalResult {
    let facts: Vec<&FactEntry> = view.facts_of_kind(&query.kind).collect();
    if facts.is_empty() {
        return match view.coverage_of(&query.kind) {
            CoverageClass::Covered => EvalResult {
                branches: vec![Branch::initial(bindings.clone()).with_truth(Truth::False)],
                overflow: false,
            },
            CoverageClass::Unknown(reason) => EvalResult {
                branches: vec![Branch {
                    truth: Truth::Unknown,
                    bindings: bindings.clone(),
                    evidence: Vec::new(),
                    unknown: vec![UnknownNote {
                        subject: query.kind.clone(),
                        reason,
                    }],
                }],
                overflow: false,
            },
        };
    }
    let mut branches = Vec::new();
    let mut overflow = false;
    let mut any_unknown = false;
    let mut reason = None;
    for entry in facts {
        let outcome = fact_matches(entry, query, bindings, resolver);
        match outcome.truth {
            Truth::True => {
                for (bindings, evidence) in outcome.branches {
                    branches.push(Branch {
                        truth: Truth::True,
                        bindings,
                        evidence: vec![evidence],
                        unknown: Vec::new(),
                    });
                }
            }
            Truth::Unknown => {
                any_unknown = true;
                reason = merge_reason(reason, outcome.reason);
            }
            Truth::False => {}
        }
        if branches.len() > limit {
            overflow = true;
            branches.truncate(limit);
            break;
        }
    }
    if branches.is_empty() {
        if any_unknown {
            branches.push(Branch {
                truth: Truth::Unknown,
                bindings: bindings.clone(),
                evidence: Vec::new(),
                unknown: vec![UnknownNote {
                    subject: query.kind.clone(),
                    reason: reason.unwrap_or(CouldNotCheckReason::NotCovered),
                }],
            });
        } else {
            branches.push(Branch::initial(bindings.clone()).with_truth(Truth::False));
        }
    }
    EvalResult { branches, overflow }
}

/// Every way one fact fits the query: one per matching ref when the query names one.
struct FactOutcome {
    truth: Truth,
    branches: Vec<(Bindings, EvidenceLine)>,
    reason: Option<CouldNotCheckReason>,
}

fn merge_reason(
    a: Option<CouldNotCheckReason>,
    b: Option<CouldNotCheckReason>,
) -> Option<CouldNotCheckReason> {
    match (a, b) {
        (None, None) => None,
        (Some(reason), None) | (None, Some(reason)) => Some(reason),
        (Some(a), Some(b)) => Some(if reason_rank(a) >= reason_rank(b) {
            a
        } else {
            b
        }),
    }
}

fn attr_unknown_reason(entry: &FactEntry, name: &str, test: &AttrMatch) -> CouldNotCheckReason {
    let Some(attr) = entry.fact.attrs.get(name) else {
        return CouldNotCheckReason::NotCovered;
    };
    let AttrMatch::Test(ops) = test else {
        return CouldNotCheckReason::NotCovered;
    };
    if let Some(pattern) = &ops.regex {
        if regex::Regex::new(pattern).is_err() {
            return CouldNotCheckReason::CodeFailed;
        }
    }
    if let Some(gate) = &ops.version {
        let can_judge = match attr {
            AttrValue::Scalar(Scalar::Text(text)) => gate.matches(text).is_some(),
            AttrValue::List(list) => list
                .iter()
                .any(|scalar| matches!(scalar, Scalar::Text(text) if gate.matches(text).is_some())),
            _ => false,
        };
        if !can_judge {
            return CouldNotCheckReason::VersionUnknown;
        }
    }
    CouldNotCheckReason::NotCovered
}

fn fact_matches(
    entry: &FactEntry,
    query: &FactQuery,
    bindings: &Bindings,
    resolver: &dyn RefResolver,
) -> FactOutcome {
    let mut truth = Truth::True;
    let mut reason = None;
    for (name, test) in &query.filter {
        match attr_matches(entry, name, test, bindings) {
            Truth::False => {
                return FactOutcome {
                    truth: Truth::False,
                    branches: Vec::new(),
                    reason: None,
                }
            }
            Truth::Unknown => {
                truth = truth.and(Truth::Unknown);
                reason = merge_reason(reason, Some(attr_unknown_reason(entry, name, test)));
            }
            Truth::True => {}
        }
    }
    let mut starts = vec![bindings.clone()];
    for (variable, attr) in &query.bind {
        let Some(value) = entry.fact.attrs.get(attr).and_then(first_scalar).cloned() else {
            truth = truth.and(Truth::Unknown);
            reason = merge_reason(reason, Some(CouldNotCheckReason::NotCovered));
            continue;
        };
        let mut next = Vec::new();
        for mut start in starts {
            if bind_value(&mut start, variable, value.clone()) {
                next.push(start);
            }
        }
        if next.is_empty() {
            return FactOutcome {
                truth: Truth::False,
                branches: Vec::new(),
                reason: None,
            };
        }
        starts = next;
    }
    let evidence = entry.evidence();
    // An attribute that cannot be judged leaves the fact unknown, unless its ref rules it out:
    // unknown and false is false (a fact about another module is simply not a match).
    let attr_truth = truth;
    let attr_reason = reason;
    if attr_truth != Truth::True && query.reference.is_none() {
        return FactOutcome {
            truth: attr_truth,
            branches: Vec::new(),
            reason: attr_reason,
        };
    }
    let Some(reference_query) = &query.reference else {
        return FactOutcome {
            truth: Truth::True,
            branches: starts
                .into_iter()
                .map(|bindings| (bindings, evidence.clone()))
                .collect(),
            reason: None,
        };
    };
    let mut out = Vec::new();
    let mut ref_truth = Truth::False;
    let mut ref_reason = None;
    for reference in &entry.fact.refs {
        if reference.kind != reference_query.kind {
            continue;
        }
        if let Some(strength) = reference_query.strength {
            if reference.strength < strength {
                continue;
            }
        }
        if let Some(role) = reference_query.role {
            if resolver.resolve(reference).role != Some(role) {
                continue;
            }
        }
        if let Some(value) = &reference_query.value {
            match scalar_test_text(value, &reference.value, bindings) {
                Truth::False => continue,
                Truth::Unknown => {
                    ref_truth = Truth::Unknown;
                    ref_reason = merge_reason(ref_reason, Some(CouldNotCheckReason::NotCovered));
                    continue;
                }
                Truth::True => {}
            }
        }
        for start in &starts {
            let mut next = start.clone();
            if let Some(variable) = &reference_query.bind {
                let bound = BoundRef {
                    reference: reference.clone(),
                    parser: entry.parser.as_ref().map(|stamp| stamp.parser_id.clone()),
                    evidence: Some(evidence.clone()),
                };
                if !bind_ref(&mut next, variable, bound) {
                    continue;
                }
            }
            out.push((next, evidence.clone()));
        }
    }
    if out.is_empty() {
        let truth = attr_truth.and(ref_truth);
        let reason = if truth == Truth::Unknown {
            merge_reason(attr_reason, ref_reason)
        } else {
            None
        };
        FactOutcome {
            truth,
            branches: Vec::new(),
            reason,
        }
    } else if attr_truth != Truth::True {
        FactOutcome {
            truth: attr_truth,
            branches: Vec::new(),
            reason: attr_reason,
        }
    } else {
        FactOutcome {
            truth: Truth::True,
            branches: out,
            reason: None,
        }
    }
}

fn fact_truth(
    entry: &FactEntry,
    filter: &BTreeMap<String, AttrMatch>,
    reference: Option<&agora_game_api::crash::RefQuery>,
    bindings: &Bindings,
    resolver: &dyn RefResolver,
) -> (Truth, Option<CouldNotCheckReason>) {
    let mut truth = Truth::True;
    let mut reason = None;
    for (name, test) in filter {
        match attr_matches(entry, name, test, bindings) {
            Truth::False => return (Truth::False, None),
            Truth::Unknown => {
                truth = truth.and(Truth::Unknown);
                reason = merge_reason(reason, Some(attr_unknown_reason(entry, name, test)));
            }
            Truth::True => {}
        }
    }
    if truth != Truth::True {
        return (truth, reason);
    }
    let Some(reference_query) = reference else {
        return (Truth::True, None);
    };
    let mut ref_truth = Truth::False;
    let mut ref_reason = None;
    for reference in &entry.fact.refs {
        if reference.kind != reference_query.kind {
            continue;
        }
        if let Some(strength) = reference_query.strength {
            if reference.strength < strength {
                continue;
            }
        }
        if let Some(role) = reference_query.role {
            if resolver.resolve(reference).role != Some(role) {
                continue;
            }
        }
        if let Some(value) = &reference_query.value {
            match scalar_test_text(value, &reference.value, bindings) {
                Truth::False => continue,
                Truth::Unknown => {
                    ref_truth = Truth::Unknown;
                    ref_reason = merge_reason(ref_reason, Some(CouldNotCheckReason::NotCovered));
                    continue;
                }
                Truth::True => return (Truth::True, None),
            }
        } else {
            return (Truth::True, None);
        }
    }
    (ref_truth, ref_reason)
}

fn eval_count(
    query: &CountQuery,
    view: &View,
    _limit: usize,
    resolver: &dyn RefResolver,
    bindings: &Bindings,
) -> EvalResult {
    let facts: Vec<&FactEntry> = view.facts_of_kind(&query.kind).collect();
    if facts.is_empty() {
        return match view.coverage_of(&query.kind) {
            CoverageClass::Covered => EvalResult {
                branches: vec![Branch {
                    truth: count_truth(query, 0, 0),
                    bindings: bindings.clone(),
                    evidence: Vec::new(),
                    unknown: Vec::new(),
                }],
                overflow: false,
            },
            CoverageClass::Unknown(reason) => EvalResult {
                branches: vec![Branch {
                    truth: Truth::Unknown,
                    bindings: bindings.clone(),
                    evidence: Vec::new(),
                    unknown: vec![UnknownNote {
                        subject: query.kind.clone(),
                        reason,
                    }],
                }],
                overflow: false,
            },
        };
    }
    let mut known = 0u32;
    let mut unknown = 0u32;
    let mut reason = None;
    for entry in facts {
        match fact_truth(
            entry,
            &query.filter,
            query.reference.as_ref(),
            bindings,
            resolver,
        ) {
            (Truth::True, _) => known += 1,
            (Truth::Unknown, why) => {
                unknown += 1;
                reason = merge_reason(reason, why);
            }
            (Truth::False, _) => {}
        }
    }
    let truth = count_truth(query, known, unknown);
    let unknown = if truth == Truth::Unknown {
        vec![UnknownNote {
            subject: query.kind.clone(),
            reason: reason.unwrap_or(CouldNotCheckReason::NotCovered),
        }]
    } else {
        Vec::new()
    };
    EvalResult {
        branches: vec![Branch {
            truth,
            bindings: bindings.clone(),
            evidence: Vec::new(),
            unknown,
        }],
        overflow: false,
    }
}

fn count_truth(query: &CountQuery, known: u32, unknown: u32) -> Truth {
    let at_least = match query.at_least {
        Some(n) if known >= n => Truth::True,
        Some(n) if known + unknown >= n => Truth::Unknown,
        Some(_) => Truth::False,
        None => Truth::True,
    };
    let at_most = match query.at_most {
        Some(n) if known > n => Truth::False,
        Some(n) if known + unknown <= n => Truth::True,
        Some(_) => Truth::Unknown,
        None => Truth::True,
    };
    at_least.and(at_most)
}

fn eval_text(text: &TextMatch, view: &View, limit: usize, bindings: &Bindings) -> EvalResult {
    let lines: Vec<&FactEntry> = view.facts_of_kind("core.log_line").collect();
    if lines.is_empty() {
        return match view.coverage_of("core.log_line") {
            CoverageClass::Covered => EvalResult {
                branches: vec![Branch::initial(bindings.clone()).with_truth(Truth::False)],
                overflow: false,
            },
            CoverageClass::Unknown(reason) => EvalResult {
                branches: vec![Branch {
                    truth: Truth::Unknown,
                    bindings: bindings.clone(),
                    evidence: Vec::new(),
                    unknown: vec![UnknownNote {
                        subject: "core.log_line".to_string(),
                        reason,
                    }],
                }],
                overflow: false,
            },
        };
    }
    let regex = match regex::Regex::new(&text.regex) {
        Ok(regex) => regex,
        Err(_) => {
            return EvalResult {
                branches: vec![Branch {
                    truth: Truth::Unknown,
                    bindings: bindings.clone(),
                    evidence: Vec::new(),
                    unknown: vec![UnknownNote {
                        subject: "core.log_line".to_string(),
                        reason: CouldNotCheckReason::CodeFailed,
                    }],
                }],
                overflow: false,
            };
        }
    };
    let mut branches = Vec::new();
    let mut overflow = false;
    for entry in lines {
        if let Some(section) = &text.section {
            if entry
                .fact
                .attrs
                .get("section")
                .and_then(scalar_text)
                .as_deref()
                != Some(section.as_str())
            {
                continue;
            }
        }
        if let Some(source) = &text.source {
            if entry.source.as_deref() != Some(source.as_str()) {
                continue;
            }
        }
        let Some(line_text) = entry.fact.attrs.get("text").and_then(scalar_text) else {
            continue;
        };
        for captures in regex.captures_iter(&line_text) {
            let mut next = bindings.clone();
            let mut ok = true;
            for name in regex.capture_names().flatten() {
                let Some(found) = captures.name(name) else {
                    continue;
                };
                let value = found.as_str().to_string();
                match text.refs.get(name) {
                    Some(kind) => {
                        let bound = BoundRef {
                            reference: Ref {
                                kind: kind.clone(),
                                value,
                                strength: Strength::Mention,
                            },
                            parser: entry.parser.as_ref().map(|stamp| stamp.parser_id.clone()),
                            evidence: Some(entry.evidence()),
                        };
                        if !bind_ref(&mut next, name, bound) {
                            ok = false;
                            break;
                        }
                    }
                    None => {
                        if !bind_value(&mut next, name, Scalar::Text(value)) {
                            ok = false;
                            break;
                        }
                    }
                }
            }
            if ok {
                branches.push(Branch {
                    truth: Truth::True,
                    bindings: next,
                    evidence: vec![entry.evidence()],
                    unknown: Vec::new(),
                });
                if branches.len() > limit {
                    overflow = true;
                    branches.truncate(limit);
                    break;
                }
            }
        }
        if overflow {
            break;
        }
    }
    if branches.is_empty() {
        branches.push(Branch::initial(bindings.clone()).with_truth(Truth::False));
    }
    EvalResult { branches, overflow }
}

fn bind_value(bindings: &mut Bindings, name: &str, value: Scalar) -> bool {
    match bindings.values.get(name) {
        Some(existing) => *existing == value,
        None => {
            bindings.values.insert(name.to_string(), value);
            true
        }
    }
}

fn bind_ref(bindings: &mut Bindings, name: &str, value: BoundRef) -> bool {
    match bindings.refs.get(name) {
        Some(existing) => existing.reference.value == value.reference.value,
        None => {
            bindings.refs.insert(name.to_string(), value);
            true
        }
    }
}

fn first_scalar(value: &AttrValue) -> Option<&Scalar> {
    match value {
        AttrValue::Scalar(scalar) => Some(scalar),
        AttrValue::List(list) => list.first(),
    }
}

fn scalar_text(value: &AttrValue) -> Option<String> {
    match value {
        AttrValue::Scalar(scalar) => Some(display_scalar(scalar)),
        AttrValue::List(_) => None,
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

fn attr_matches(entry: &FactEntry, name: &str, test: &AttrMatch, bindings: &Bindings) -> Truth {
    let attr = entry.fact.attrs.get(name);
    if let AttrMatch::Test(ops) = test {
        if let Some(expected) = ops.present {
            if !expected {
                return if attr.is_none() {
                    Truth::True
                } else {
                    Truth::False
                };
            }
            if attr.is_none() {
                return Truth::False;
            }
        }
    }
    let Some(attr) = attr else {
        // A test on a missing attribute is unknown, not a clean no.
        return Truth::Unknown;
    };
    match attr {
        AttrValue::Scalar(scalar) => scalar_matches(scalar, test, bindings),
        AttrValue::List(list) => {
            let mut unknown = false;
            for scalar in list {
                match scalar_matches(scalar, test, bindings) {
                    Truth::True => return Truth::True,
                    Truth::Unknown => unknown = true,
                    Truth::False => {}
                }
            }
            if unknown {
                Truth::Unknown
            } else {
                Truth::False
            }
        }
    }
}

fn scalar_matches(scalar: &Scalar, test: &AttrMatch, bindings: &Bindings) -> Truth {
    match test {
        AttrMatch::Is(expected) => match resolve_scalar(expected, bindings) {
            Some(expected) => {
                if scalar_eq(&expected, scalar) {
                    Truth::True
                } else {
                    Truth::False
                }
            }
            None => Truth::Unknown,
        },
        AttrMatch::Test(ops) => scalar_ops(scalar, ops, bindings),
    }
}

fn scalar_ops(scalar: &Scalar, ops: &MatchOps, bindings: &Bindings) -> Truth {
    let text = match scalar {
        Scalar::Text(text) => Some(text.as_str()),
        _ => None,
    };
    let mut truth = Truth::True;
    if let Some(expected) = &ops.eq {
        match resolve_scalar(expected, bindings) {
            Some(expected) => {
                truth = truth.and(if scalar_eq(&expected, scalar) {
                    Truth::True
                } else {
                    Truth::False
                })
            }
            None => truth = truth.and(Truth::Unknown),
        }
    }
    if let Some(list) = &ops.any_of {
        let mut any_unknown = false;
        let mut any_true = false;
        for expected in list {
            match resolve_scalar(expected, bindings) {
                Some(expected) => {
                    if scalar_eq(&expected, scalar) {
                        any_true = true;
                        break;
                    }
                }
                None => any_unknown = true,
            }
        }
        truth = truth.and(if any_true {
            Truth::True
        } else if any_unknown {
            Truth::Unknown
        } else {
            Truth::False
        });
    }
    if let Some(pattern) = &ops.glob {
        let Some(text) = text else {
            return Truth::False;
        };
        truth = truth.and(if glob_match(pattern, text) {
            Truth::True
        } else {
            Truth::False
        });
    }
    if let Some(pattern) = &ops.regex {
        let Some(text) = text else {
            return Truth::False;
        };
        match regex::Regex::new(pattern) {
            Ok(regex) => {
                truth = truth.and(if regex.is_match(text) {
                    Truth::True
                } else {
                    Truth::False
                })
            }
            Err(_) => truth = truth.and(Truth::Unknown),
        }
    }
    if let Some(gte) = ops.gte {
        let Some(number) = scalar_number(scalar) else {
            return Truth::False;
        };
        truth = truth.and(if number >= gte {
            Truth::True
        } else {
            Truth::False
        });
    }
    if let Some(lte) = ops.lte {
        let Some(number) = scalar_number(scalar) else {
            return Truth::False;
        };
        truth = truth.and(if number <= lte {
            Truth::True
        } else {
            Truth::False
        });
    }
    if let Some(gate) = &ops.version {
        let Some(text) = text else {
            return Truth::False;
        };
        truth = truth.and(match gate.matches(text) {
            Some(true) => Truth::True,
            Some(false) => Truth::False,
            None => Truth::Unknown,
        });
    }
    truth
}

fn scalar_test_text(test: &AttrMatch, text: &str, bindings: &Bindings) -> Truth {
    scalar_matches(&Scalar::Text(text.to_string()), test, bindings)
}

fn resolve_scalar(scalar: &Scalar, bindings: &Bindings) -> Option<Scalar> {
    let Scalar::Text(text) = scalar else {
        return Some(scalar.clone());
    };
    let Some(name) = text.strip_prefix('$') else {
        return Some(scalar.clone());
    };
    bindings.values.get(name).cloned()
}

fn scalar_eq(a: &Scalar, b: &Scalar) -> bool {
    match (a, b) {
        (Scalar::Bool(a), Scalar::Bool(b)) => a == b,
        (Scalar::Number(a), Scalar::Number(b)) => a == b,
        (Scalar::Text(a), Scalar::Text(b)) => a == b,
        _ => false,
    }
}

fn scalar_number(scalar: &Scalar) -> Option<f64> {
    match scalar {
        Scalar::Number(value) => Some(*value),
        Scalar::Text(text) => text.parse().ok(),
        Scalar::Bool(_) => None,
    }
}

fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.to_lowercase().chars().collect();
    let text: Vec<char> = text.to_lowercase().chars().collect();
    let (mut p, mut t) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            p += 1;
            mark = t;
        } else if let Some(star_at) = star {
            p = star_at + 1;
            mark += 1;
            t = mark;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

#[derive(Debug, Default)]
struct RuleProducts {
    plugin_id: String,
    rule_id: String,
    fired: bool,
    signals: Vec<Signal>,
    findings: Vec<Finding>,
    inspections: Vec<InspectionRecord>,
    could_not_check: Vec<CouldNotCheck>,
    trust_notes: Vec<TrustNote>,
    level_caps: Vec<Level>,
    repairs: Vec<RepairOffer>,
    next_steps: Vec<NextStepKind>,
    roles: BTreeMap<String, Role>,
}

pub fn evaluate(input: &EvaluateInput<'_>) -> EvaluateOutput {
    let mut rules: Vec<(&PluginIdentity, &Rule)> = Vec::new();
    for pack in input.packs {
        for rule in &pack.pack.rules {
            rules.push((&pack.plugin, rule));
        }
    }
    rules.sort_by_key(|(plugin, rule)| {
        format!(
            "{}\0{}\0{}\0{}",
            plugin.id,
            rule.id,
            rule.version,
            serde_json::to_string(rule).unwrap_or_default()
        )
    });

    // Most rules see every parsed artifact plus the engine's facts, so they share one view.
    let all: Vec<&ParsedArtifact> = input.parsed.iter().collect();
    let shared = View::build(&all, input.engine_facts, true, input.resolver);
    let mut products: Vec<RuleProducts> = Vec::new();
    for (plugin, rule) in rules {
        let Some(product) = evaluate_rule(input, plugin, rule, &shared) else {
            continue;
        };
        products.push(product);
    }

    let fired: BTreeSet<(String, String)> = products
        .iter()
        .filter(|product| product.fired)
        .map(|product| (product.plugin_id.clone(), product.rule_id.clone()))
        .collect();
    let mut superseded: BTreeSet<(String, String)> = BTreeSet::new();
    for pack in input.packs {
        for rule in &pack.pack.rules {
            if !fired.contains(&(pack.plugin.id.clone(), rule.id.clone())) {
                continue;
            }
            for gone in &rule.supersedes {
                superseded.insert((pack.plugin.id.clone(), gone.clone()));
            }
        }
    }

    let mut signals = Vec::new();
    let mut findings = Vec::new();
    let mut inspections = Vec::new();
    let mut could_not_check = Vec::new();
    let mut trust_notes = Vec::new();
    let mut repairs = Vec::new();
    let mut next_steps = Vec::new();
    let mut roles = BTreeMap::new();
    let mut cap: Option<Level> = None;
    for product in products {
        if superseded.contains(&(product.plugin_id.clone(), product.rule_id.clone())) {
            continue;
        }
        signals.extend(product.signals);
        findings.extend(product.findings);
        inspections.extend(product.inspections);
        could_not_check.extend(product.could_not_check);
        trust_notes.extend(product.trust_notes);
        repairs.extend(product.repairs);
        next_steps.extend(product.next_steps);
        roles.extend(product.roles);
        for level in product.level_caps {
            cap = lower_cap(cap, level);
        }
    }

    // A parser that says the log stops early is a trust note too, whether or not a rule says so.
    for (source, note) in input.parsed.iter().flat_map(|artifact| {
        artifact
            .output
            .notes
            .iter()
            .filter(|note| {
                matches!(
                    note.kind,
                    ParseNoteKind::Truncated | ParseNoteKind::BudgetSpent
                )
            })
            .map(|note| {
                (
                    artifact.plugin.parser_source(&artifact.parser_id),
                    note.kind,
                )
            })
    }) {
        let text = match note {
            ParseNoteKind::Truncated => "The log stops early, so Agora could not read everything.",
            ParseNoteKind::BudgetSpent => {
                "Reading the log used its whole budget, so Agora could not reach the end."
            }
            _ => continue,
        };
        trust_notes.push(TrustNote {
            text: english(text),
            source,
        });
        cap = lower_cap(cap, Level::Likely);
    }

    if bucket_predates_change(input.engine_facts) {
        // A change since the last good launch cannot explain a bucket that predates it.
        signals.retain(|signal| signal.family != Family::ChangeCorrelation);
    }

    signals.sort_by_key(signal_key);
    signals.dedup();
    could_not_check.sort_by_key(note_key);
    could_not_check.dedup_by_key(|note| note_key(note));
    trust_notes.sort_by_key(|note| {
        format!(
            "{}\0{}",
            note.text.get("en").cloned().unwrap_or_default(),
            source_key(&note.source)
        )
    });
    trust_notes.dedup_by(|a, b| a.text == b.text && source_key(&a.source) == source_key(&b.source));
    repairs.sort_by_key(|repair| {
        format!(
            "{}\0{}",
            serde_json::to_string(&repair.request).unwrap_or_default(),
            source_key(&repair.source)
        )
    });
    repairs.dedup_by(|a, b| a.request == b.request && a.source == b.source);
    // Two rules may inspect the same file; show one row, not one per rule.
    inspections.sort_by_key(|inspection| {
        format!(
            "{}\0{}\0{:?}\0{}",
            inspection.inspector,
            inspection.target,
            inspection.result,
            inspection.detail.as_deref().unwrap_or("")
        )
    });
    inspections.dedup_by(|a, b| {
        a.inspector == b.inspector
            && a.target == b.target
            && a.result == b.result
            && a.detail == b.detail
    });
    next_steps.sort_by_key(|kind| next_step_rank(*kind));
    next_steps.dedup();

    EvaluateOutput {
        signals,
        findings: clean_findings(findings),
        inspections,
        could_not_check,
        trust_notes,
        level_cap: cap,
        repairs,
        next_steps,
        roles,
    }
}

fn evaluate_rule(
    input: &EvaluateInput<'_>,
    plugin: &PluginIdentity,
    rule: &Rule,
    shared: &View,
) -> Option<RuleProducts> {
    let producer = rule.scope.producer.as_ref();
    let mut scope_unknown = Vec::new();
    let allowed: Vec<&ParsedArtifact>;
    if let Some(gate) = producer {
        let matching: Vec<&ParsedArtifact> = input
            .parsed
            .iter()
            .filter(|artifact| artifact.output.producer.format == gate.format)
            .collect();
        if matching.is_empty() {
            return None;
        }
        if let Some(version) = &gate.version {
            let mut matched = Vec::new();
            let mut unknown = false;
            for artifact in &matching {
                match artifact
                    .output
                    .producer
                    .version
                    .as_deref()
                    .and_then(|actual| version.matches(actual))
                {
                    Some(true) => matched.push(*artifact),
                    Some(false) => {}
                    None => unknown = true,
                }
            }
            if matched.is_empty() {
                if !unknown {
                    return None;
                }
                scope_unknown.push(UnknownNote {
                    subject: format!("producer {} version", gate.format),
                    reason: CouldNotCheckReason::VersionUnknown,
                });
                allowed = matching;
            } else {
                allowed = matched;
            }
        } else {
            allowed = matching;
        }
    } else {
        allowed = input.parsed.iter().collect();
    }

    if let Some(gate) = &rule.scope.runtime {
        match input
            .runtime_version
            .and_then(|version| gate.matches(version))
        {
            Some(true) => {}
            Some(false) => return None,
            None => scope_unknown.push(UnknownNote {
                subject: format!(
                    "game version {}",
                    input.runtime_version.unwrap_or("(unknown)")
                ),
                reason: CouldNotCheckReason::VersionUnknown,
            }),
        }
    }

    if let Some(detail) = condition_bad_regex(&rule.when) {
        return Some(RuleProducts {
            plugin_id: plugin.id.clone(),
            rule_id: rule.id.clone(),
            could_not_check: vec![CouldNotCheck {
                reason: CouldNotCheckReason::CodeFailed,
                subject: rule.id.clone(),
                detail: Some(detail),
                source: Some(rule_source(plugin, rule, None)),
            }],
            ..RuleProducts::default()
        });
    }

    let gated;
    let view = if producer.is_none() {
        shared
    } else {
        gated = View::build(&allowed, input.engine_facts, false, input.resolver);
        &gated
    };
    let limit = rule.branch_limit();
    let result = eval_condition(
        &rule.when,
        view,
        limit,
        input.resolver,
        &Bindings::default(),
    );

    let mut product = RuleProducts {
        plugin_id: plugin.id.clone(),
        rule_id: rule.id.clone(),
        ..RuleProducts::default()
    };

    if !scope_unknown.is_empty() {
        let could_fire = result
            .branches
            .iter()
            .any(|branch| branch.truth == Truth::True || branch.truth == Truth::Unknown);
        if could_fire {
            for note in scope_unknown {
                product.could_not_check.push(CouldNotCheck {
                    reason: note.reason,
                    subject: note.subject,
                    detail: None,
                    source: Some(rule_source(plugin, rule, None)),
                });
            }
        }
        if result.overflow {
            product.could_not_check.push(overflow_note(plugin, rule));
        }
        return Some(product);
    }

    if result.overflow {
        // The match set cannot be trusted, so the rule does not fire.
        product.could_not_check.push(overflow_note(plugin, rule));
        return Some(product);
    }

    let true_branches: Vec<&Branch> = result
        .branches
        .iter()
        .filter(|branch| branch.truth == Truth::True)
        .collect();
    if true_branches.is_empty() {
        for branch in &result.branches {
            if branch.truth == Truth::Unknown {
                for note in &branch.unknown {
                    product.could_not_check.push(CouldNotCheck {
                        reason: note.reason,
                        subject: note.subject.clone(),
                        detail: None,
                        source: Some(rule_source(plugin, rule, None)),
                    });
                }
            }
        }
        if result.overflow {
            product.could_not_check.push(overflow_note(plugin, rule));
        }
        return Some(product);
    }

    product.fired = true;
    for branch in true_branches {
        apply_effects(input, plugin, rule, &rule.effects, branch, &mut product);
        if let Some(verify) = &rule.verify {
            match branch.bindings.refs.get(&verify.target) {
                Some(target) => {
                    let outcome = input.inspector.inspect(&InspectionRequest {
                        plugin_id: &plugin.id,
                        inspector: &verify.inspector,
                        target: &target.reference,
                    });
                    match &outcome.result {
                        Inspection::Pass => apply_effects(
                            input,
                            plugin,
                            rule,
                            &verify.on_pass,
                            branch,
                            &mut product,
                        ),
                        Inspection::Fail => apply_effects(
                            input,
                            plugin,
                            rule,
                            &verify.on_fail,
                            branch,
                            &mut product,
                        ),
                        Inspection::Unknown => {
                            apply_effects(
                                input,
                                plugin,
                                rule,
                                &verify.on_unknown,
                                branch,
                                &mut product,
                            );
                            product.could_not_check.push(CouldNotCheck {
                                reason: CouldNotCheckReason::CodeFailed,
                                subject: verify.inspector.clone(),
                                detail: Some(format!(
                                    "{} could not decide about {}",
                                    verify.inspector, target.reference.value
                                )),
                                source: Some(rule_source(plugin, rule, None)),
                            });
                        }
                        Inspection::Missing => {
                            // An inspector that does not apply (a system module, a file another
                            // inspector owns, a loose file with no instance) found nothing to say.
                            // The rule's `on_unknown` decides whether that is worth a note; a
                            // per-target "no inspector is installed" line would only be noise.
                            apply_effects(
                                input,
                                plugin,
                                rule,
                                &verify.on_unknown,
                                branch,
                                &mut product,
                            );
                        }
                        Inspection::Failed(detail) => {
                            apply_effects(
                                input,
                                plugin,
                                rule,
                                &verify.on_unknown,
                                branch,
                                &mut product,
                            );
                            product.could_not_check.push(CouldNotCheck {
                                reason: CouldNotCheckReason::CodeFailed,
                                subject: verify.inspector.clone(),
                                detail: Some(detail.clone()),
                                source: Some(rule_source(plugin, rule, None)),
                            });
                        }
                    }
                    let result = match &outcome.result {
                        Inspection::Pass => Some(InspectResult::Pass),
                        Inspection::Fail => Some(InspectResult::Fail),
                        Inspection::Unknown => Some(InspectResult::Unknown),
                        Inspection::Missing | Inspection::Failed(_) => None,
                    };
                    if let Some(result) = result {
                        product.inspections.push(InspectionRecord {
                            inspector: verify.inspector.clone(),
                            target: target.reference.value.clone(),
                            result,
                            detail: outcome.detail,
                            facts: outcome.facts,
                            source: rule_source(plugin, rule, None),
                        });
                    }
                }
                None => {
                    apply_effects(
                        input,
                        plugin,
                        rule,
                        &verify.on_unknown,
                        branch,
                        &mut product,
                    );
                    product.could_not_check.push(CouldNotCheck {
                        reason: CouldNotCheckReason::CodeFailed,
                        subject: verify.inspector.clone(),
                        detail: Some(format!("`{}` was not bound", verify.target)),
                        source: Some(rule_source(plugin, rule, None)),
                    });
                }
            }
        }
        for repair in &rule.repairs {
            if let Some(offer) =
                resolve_repair(repair, &branch.bindings, input.resolver, plugin, rule)
            {
                product.repairs.push(offer);
            }
        }
        if let Some(next) = rule.next_step {
            product.next_steps.push(next);
        }
    }
    if result.overflow {
        product.could_not_check.push(overflow_note(plugin, rule));
    }
    Some(product)
}

fn condition_bad_regex(condition: &Condition) -> Option<String> {
    fn check_test(test: &AttrMatch) -> Option<String> {
        let AttrMatch::Test(ops) = test else {
            return None;
        };
        let pattern = ops.regex.as_ref()?;
        regex::Regex::new(pattern)
            .err()
            .map(|error| format!("this rule's regex `{pattern}` is invalid: {error}"))
    }
    match condition {
        Condition::All(list) | Condition::Any(list) | Condition::AtLeast { of: list, .. } => {
            list.iter().find_map(condition_bad_regex)
        }
        Condition::Not(inner) => condition_bad_regex(inner),
        Condition::Fact(query) => query.filter.values().find_map(check_test).or_else(|| {
            query
                .reference
                .as_ref()
                .and_then(|reference| reference.value.as_ref())
                .and_then(check_test)
        }),
        Condition::Count(query) => query.filter.values().find_map(check_test).or_else(|| {
            query
                .reference
                .as_ref()
                .and_then(|reference| reference.value.as_ref())
                .and_then(check_test)
        }),
        Condition::Text(text) => regex::Regex::new(&text.regex)
            .err()
            .map(|error| format!("this rule's regex `{}` is invalid: {error}", text.regex)),
    }
}

fn overflow_note(plugin: &PluginIdentity, rule: &Rule) -> CouldNotCheck {
    CouldNotCheck {
        reason: CouldNotCheckReason::TooManyMatches,
        subject: rule.id.clone(),
        detail: Some(format!(
            "the rule matched more than {} branches",
            rule.branch_limit()
        )),
        source: Some(rule_source(plugin, rule, None)),
    }
}

/// The kind a finding has when the pack states none: a root cause about a mod is `mod`, everything
/// else is `other`. A root cause about the game or a generated file is not a mod's fault.
fn default_cause(scope: Scope, owner: Option<&Owner>) -> Cause {
    if scope == Scope::RootCause
        && matches!(owner, Some(Owner::Item { .. } | Owner::Framework { .. }))
    {
        Cause::Mod
    } else {
        Cause::Other
    }
}

fn culprit_owner(owner: &Owner, role: Option<Role>) -> bool {
    if matches!(role, Some(Role::System | Role::Platform)) {
        return false;
    }
    !matches!(owner, Owner::System | Owner::NotInInstance | Owner::Unknown)
}

fn unknown_owner_note(
    product: &mut RuleProducts,
    plugin: &PluginIdentity,
    rule: &Rule,
    variable: &str,
    branch: &Branch,
) {
    let Some(bound) = branch.bindings.refs.get(variable) else {
        return;
    };
    product.could_not_check.push(CouldNotCheck {
        reason: CouldNotCheckReason::NotCovered,
        subject: bound.reference.value.clone(),
        detail: Some("Agora could not tell who owns this.".to_string()),
        source: Some(rule_source(plugin, rule, bound.parser.as_deref())),
    });
}

fn apply_effects(
    input: &EvaluateInput<'_>,
    plugin: &PluginIdentity,
    rule: &Rule,
    effects: &[Effect],
    branch: &Branch,
    product: &mut RuleProducts,
) {
    for effect in effects {
        match effect {
            Effect::Signal {
                family,
                candidate,
                weight,
            } => {
                if let Some((owner, parser, role)) =
                    resolve_owner(&candidate.owner_of, branch, input.resolver)
                {
                    if !culprit_owner(&owner, role) {
                        if matches!(owner, Owner::Unknown) {
                            unknown_owner_note(product, plugin, rule, &candidate.owner_of, branch);
                        }
                        continue;
                    }
                    let source = rule_source(plugin, rule, parser.as_deref());
                    if let Some(role) = role {
                        product.roles.insert(owner_key(&owner), role);
                    }
                    product.signals.push(Signal {
                        family: *family,
                        candidate: owner,
                        weight: *weight,
                        source,
                        evidence: clean_evidence(branch.evidence.clone()),
                    });
                }
            }
            Effect::Finding {
                scope,
                level,
                cause,
                candidate,
                family,
                weight,
                text,
            } => {
                let (mut owner, parser, role) = match candidate {
                    Some(candidate) => {
                        match resolve_owner(&candidate.owner_of, branch, input.resolver) {
                            Some(found) => (Some(found.0), found.1, found.2),
                            None => (None, None, None),
                        }
                    }
                    None => (None, None, None),
                };
                let mut effective_scope = *scope;
                if let Some(found) = &owner {
                    if !culprit_owner(found, role) {
                        if matches!(found, Owner::Unknown) {
                            if let Some(candidate) = candidate {
                                unknown_owner_note(
                                    product,
                                    plugin,
                                    rule,
                                    &candidate.owner_of,
                                    branch,
                                );
                            }
                        }
                        owner = None;
                        effective_scope = Scope::Environment;
                    }
                }
                // A mechanism or environment finding never fills the "who did it" slot, even if
                // a pack that skipped the linter tried to give it one.
                if effective_scope != Scope::RootCause {
                    owner = None;
                }
                let source = rule_source(plugin, rule, parser.as_deref());
                let mut rendered =
                    render_text(text.as_ref().unwrap_or(&rule.text), branch, input.resolver);
                if rendered
                    .headline
                    .get("en")
                    .is_none_or(|value| value.trim().is_empty())
                {
                    rendered.headline = english("");
                }
                if let Some(owner) = &owner {
                    if let Some(role) = role {
                        product.roles.insert(owner_key(owner), role);
                    }
                }
                product.findings.push(Finding {
                    scope: effective_scope,
                    level: *level,
                    cause: cause.unwrap_or_else(|| default_cause(effective_scope, owner.as_ref())),
                    candidate: owner.clone(),
                    headline: rendered.headline,
                    why_not_sure: rendered.why_not_sure,
                    source: source.clone(),
                    evidence: clean_evidence(branch.evidence.clone()),
                });
                if effective_scope == Scope::RootCause {
                    if let (Some(owner), Some(family), Some(weight)) = (owner, family, weight) {
                        product.signals.push(Signal {
                            family: *family,
                            candidate: owner,
                            weight: *weight,
                            source,
                            evidence: clean_evidence(branch.evidence.clone()),
                        });
                    }
                }
            }
            Effect::Trust { cap, text } => {
                product.level_caps.push(*cap);
                let rule_text = RuleText {
                    headline: text.clone(),
                    why_not_sure: None,
                };
                let rendered = render_text(&rule_text, branch, input.resolver);
                product.trust_notes.push(TrustNote {
                    text: rendered.headline,
                    source: rule_source(plugin, rule, None),
                });
            }
            Effect::Demote { of, role } => {
                if let Some(bound) = branch.bindings.refs.get(of) {
                    let resolved = input.resolver.resolve(&bound.reference);
                    product.roles.insert(owner_key(&resolved.owner), *role);
                }
            }
            Effect::Context { text } => {
                let rule_text = RuleText {
                    headline: text.clone(),
                    why_not_sure: None,
                };
                let rendered = render_text(&rule_text, branch, input.resolver);
                product.findings.push(Finding {
                    scope: Scope::Environment,
                    level: Level::NotSure,
                    cause: Cause::Other,
                    candidate: None,
                    headline: rendered.headline,
                    why_not_sure: None,
                    source: rule_source(plugin, rule, None),
                    evidence: clean_evidence(branch.evidence.clone()),
                });
            }
        }
    }
}

fn resolve_owner(
    variable: &str,
    branch: &Branch,
    resolver: &dyn RefResolver,
) -> Option<(Owner, Option<String>, Option<Role>)> {
    let bound = branch.bindings.refs.get(variable)?;
    let resolved = resolver.resolve(&bound.reference);
    Some((resolved.owner, bound.parser.clone(), resolved.role))
}

fn resolve_repair(
    repair: &RepairRequest,
    bindings: &Bindings,
    resolver: &dyn RefResolver,
    plugin: &PluginIdentity,
    rule: &Rule,
) -> Option<RepairOffer> {
    let source = rule_source(plugin, rule, None);
    let content_key = |owner: &agora_game_api::crash::OwnerOf| {
        bindings
            .refs
            .get(&owner.owner_of)
            .map(|bound| resolver.resolve(&bound.reference).owner)
            .and_then(|owner| owner_content_key(&owner))
    };
    let request = match repair {
        RepairRequest::DisableContent { key } => ResolvedRepair::DisableContent {
            key: content_key(key)?,
        },
        RepairRequest::EnableContent { key } => ResolvedRepair::EnableContent {
            key: content_key(key)?,
        },
        RepairRequest::PinContentUpdate { key } => ResolvedRepair::PinContentUpdate {
            key: content_key(key)?,
        },
        RepairRequest::UnpinContentUpdate { key } => ResolvedRepair::UnpinContentUpdate {
            key: content_key(key)?,
        },
        RepairRequest::SetJvmMemory { memory_mb } => ResolvedRepair::SetJvmMemory {
            memory_mb: *memory_mb,
        },
        RepairRequest::ResetJvmArgs => ResolvedRepair::ResetJvmArgs,
        RepairRequest::CreateSnapshot { label } => ResolvedRepair::CreateSnapshot {
            label: label.clone(),
        },
    };
    Some(RepairOffer { request, source })
}

fn rule_source(plugin: &PluginIdentity, rule: &Rule, parser: Option<&str>) -> SourceAttribution {
    SourceAttribution {
        plugin_id: plugin.id.clone(),
        plugin_name: plugin.name.clone(),
        plugin_version: plugin.version.clone(),
        rule_id: Some(rule.id.clone()),
        rule_version: Some(rule.version),
        parser_id: parser.map(|parser| parser.to_string()),
    }
}

fn render_text(text: &RuleText, branch: &Branch, resolver: &dyn RefResolver) -> RenderedText {
    RenderedText {
        headline: text
            .headline
            .iter()
            .map(|(locale, words)| (locale.clone(), substitute(words, branch, resolver)))
            .collect(),
        why_not_sure: text.why_not_sure.as_ref().map(|why| {
            why.iter()
                .map(|(locale, words)| (locale.clone(), substitute(words, branch, resolver)))
                .collect()
        }),
    }
}

struct RenderedText {
    headline: LocalText,
    why_not_sure: Option<LocalText>,
}

fn substitute(template: &str, branch: &Branch, resolver: &dyn RefResolver) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        rest = &rest[open + 1..];
        let Some(close) = rest.find('}') else {
            out.push('{');
            break;
        };
        let inner = &rest[..close];
        rest = &rest[close + 1..];
        match inner.strip_prefix("owner:") {
            Some(name) => {
                if let Some(bound) = branch.bindings.refs.get(name) {
                    out.push_str(&owner_name(&resolver.resolve(&bound.reference).owner));
                }
            }
            None => {
                if let Some(value) = branch.bindings.values.get(inner) {
                    out.push_str(&display_scalar(value));
                } else if let Some(bound) = branch.bindings.refs.get(inner) {
                    out.push_str(&bound.reference.value);
                }
            }
        }
    }
    out.push_str(rest);
    out
}

fn bucket_predates_change(facts: &[Fact]) -> bool {
    facts
        .iter()
        .filter(|fact| fact.kind == "core.history")
        .any(|fact| {
            fact.attrs.get("predates_last_change") == Some(&AttrValue::Scalar(Scalar::Bool(true)))
        })
}

fn signal_key(signal: &Signal) -> String {
    let evidence: Vec<String> = signal.evidence.iter().map(evidence_key).collect();
    format!(
        "{}\0{}\0{}\0{}\0{}",
        signal.family as u8,
        owner_key(&signal.candidate),
        signal.weight,
        source_key(&signal.source),
        evidence.join("\u{1f}")
    )
}

fn note_key(note: &CouldNotCheck) -> String {
    format!(
        "{}\0{}\0{}\0{}",
        reason_rank(note.reason),
        note.subject,
        note.detail.as_deref().unwrap_or(""),
        note.source.as_ref().map(source_key).unwrap_or_default()
    )
}

fn reason_rank(reason: CouldNotCheckReason) -> u8 {
    match reason {
        CouldNotCheckReason::NotCovered => 0,
        CouldNotCheckReason::AbsentFromArtifact => 1,
        CouldNotCheckReason::VersionUnknown => 2,
        CouldNotCheckReason::CodeFailed => 3,
        CouldNotCheckReason::InspectorMissing => 4,
        CouldNotCheckReason::NotRetained => 5,
        CouldNotCheckReason::TooManyMatches => 6,
        CouldNotCheckReason::InstanceIncomplete => 7,
    }
}

fn next_step_rank(kind: NextStepKind) -> u8 {
    match kind {
        NextStepKind::ApplyRepair => 0,
        NextStepKind::ConfirmExperiment => 1,
        NextStepKind::Bisect => 2,
        NextStepKind::RestoreSave => 3,
        NextStepKind::Recapture => 4,
        NextStepKind::SecondOpinion => 5,
        NextStepKind::NothingNeeded => 6,
    }
}

pub(crate) fn english(text: &str) -> LocalText {
    let mut out = LocalText::new();
    out.insert("en".to_string(), text.to_string());
    out
}
