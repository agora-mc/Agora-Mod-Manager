//! Rule evaluation: facts in, signals and findings out. Conditions use Kleene three-valued logic,
//! because an absent section is not a clean one.

use super::*;
use agora_game_api::crash::{
    AttrMatch, AttrValue, Condition, CouldNotCheck, CouldNotCheckReason, CountQuery, Coverage,
    CoverageState, Effect, EvidenceLine, Fact, FactQuery, Finding, Level, LocalText, MatchOps,
    NextStepKind, ParseNoteKind, Ref, RepairOffer, RepairRequest, ResolvedRepair, Role, Rule,
    RuleText, Scalar, Scope, Signal, SourceAttribution, Strength, TextMatch, TrustNote, Truth,
    MAX_RULE_BRANCHES,
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
    frames.sort_by_key(|index| {
        facts[*index]
            .fact
            .attrs
            .get("index")
            .and_then(first_scalar)
            .and_then(scalar_number)
            .map(|value| value as u64)
            .unwrap_or(u64::MAX)
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
        facts.sort_by_key(FactEntry::sort_key);
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

fn cap_branches(mut branches: Vec<Branch>, overflow: &mut bool) -> Vec<Branch> {
    if branches.len() > MAX_RULE_BRANCHES {
        *overflow = true;
        branches.truncate(MAX_RULE_BRANCHES);
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
                    let child_result = eval_condition(child, view, resolver, &branch.bindings);
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
                result.branches = cap_branches(next, &mut result.overflow);
            }
            result
        }
        Condition::Any(list) => {
            let mut result = EvalResult {
                branches: vec![Branch::initial(bindings.clone())],
                overflow: false,
            };
            for child in list {
                let mut next = Vec::new();
                for branch in &result.branches {
                    if branch.truth == Truth::True {
                        next.push(branch.clone());
                        continue;
                    }
                    let child_result = eval_condition(child, view, resolver, &branch.bindings);
                    result.overflow |= child_result.overflow;
                    for outcome in child_result.branches {
                        let truth = branch.truth.or(outcome.truth);
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
                result.branches = cap_branches(next, &mut result.overflow);
            }
            result
        }
        Condition::Not(inner) => {
            let evaluated = eval_condition(inner, view, resolver, bindings);
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
                let evaluated = eval_condition(child, view, resolver, bindings);
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
        Condition::Fact(query) => eval_fact(query, view, resolver, bindings),
        Condition::Count(query) => eval_count(query, view, resolver, bindings),
        Condition::Text(text) => eval_text(text, view, bindings),
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
    for entry in facts {
        let Some(matches) = fact_matches(entry, query, bindings, resolver) else {
            continue;
        };
        for (bindings, evidence) in matches {
            branches.push(Branch {
                truth: Truth::True,
                bindings,
                evidence: vec![evidence],
                unknown: Vec::new(),
            });
        }
        if branches.len() > MAX_RULE_BRANCHES {
            overflow = true;
            branches.truncate(MAX_RULE_BRANCHES);
            break;
        }
    }
    if branches.is_empty() {
        branches.push(Branch::initial(bindings.clone()).with_truth(Truth::False));
    }
    EvalResult { branches, overflow }
}

/// Every way one fact fits the query: one per matching ref when the query names one.
fn fact_matches(
    entry: &FactEntry,
    query: &FactQuery,
    bindings: &Bindings,
    resolver: &dyn RefResolver,
) -> Option<Vec<(Bindings, EvidenceLine)>> {
    for (name, test) in &query.filter {
        if !attr_matches(entry, name, test, bindings) {
            return None;
        }
    }
    let mut starts = vec![bindings.clone()];
    for (variable, attr) in &query.bind {
        let value = entry.fact.attrs.get(attr).and_then(first_scalar).cloned()?;
        let mut next = Vec::new();
        for mut start in starts {
            if bind_value(&mut start, variable, value.clone()) {
                next.push(start);
            }
        }
        if next.is_empty() {
            return None;
        }
        starts = next;
    }
    let evidence = entry.evidence();
    let Some(reference_query) = &query.reference else {
        return Some(starts.into_iter().map(|b| (b, evidence.clone())).collect());
    };
    let mut out = Vec::new();
    for reference in &entry.fact.refs {
        if !reference_fits(reference, reference_query, bindings, resolver) {
            continue;
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
        None
    } else {
        Some(out)
    }
}

fn reference_fits(
    reference: &Ref,
    query: &agora_game_api::crash::RefQuery,
    bindings: &Bindings,
    resolver: &dyn RefResolver,
) -> bool {
    if reference.kind != query.kind {
        return false;
    }
    if let Some(value) = &query.value {
        if !scalar_test_text(value, &reference.value, bindings) {
            return false;
        }
    }
    if let Some(strength) = query.strength {
        if reference.strength < strength {
            return false;
        }
    }
    if let Some(role) = query.role {
        if resolver.resolve(reference).role != Some(role) {
            return false;
        }
    }
    true
}

fn eval_count(
    query: &CountQuery,
    view: &View,
    resolver: &dyn RefResolver,
    bindings: &Bindings,
) -> EvalResult {
    let facts: Vec<&FactEntry> = view.facts_of_kind(&query.kind).collect();
    if facts.is_empty() {
        return match view.coverage_of(&query.kind) {
            CoverageClass::Covered => {
                let count = 0u32;
                count_branch(query, count, bindings)
            }
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
    let mut count = 0u32;
    for entry in facts {
        if !fact_filter_matches(entry, &query.filter, bindings) {
            continue;
        }
        let reference_ok =
            match &query.reference {
                Some(reference_query) => entry.fact.refs.iter().any(|reference| {
                    reference_fits(reference, reference_query, bindings, resolver)
                }),
                None => true,
            };
        if reference_ok {
            count += 1;
        }
    }
    count_branch(query, count, bindings)
}

fn count_branch(query: &CountQuery, count: u32, bindings: &Bindings) -> EvalResult {
    let at_least = query.at_least.map(|n| count >= n).unwrap_or(true);
    let at_most = query.at_most.map(|n| count <= n).unwrap_or(true);
    let truth = if at_least && at_most {
        Truth::True
    } else {
        Truth::False
    };
    EvalResult {
        branches: vec![Branch {
            truth,
            bindings: bindings.clone(),
            evidence: Vec::new(),
            unknown: Vec::new(),
        }],
        overflow: false,
    }
}

fn fact_filter_matches(
    entry: &FactEntry,
    filter: &BTreeMap<String, AttrMatch>,
    bindings: &Bindings,
) -> bool {
    filter
        .iter()
        .all(|(name, test)| attr_matches(entry, name, test, bindings))
}

fn eval_text(text: &TextMatch, view: &View, bindings: &Bindings) -> EvalResult {
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
                if branches.len() > MAX_RULE_BRANCHES {
                    overflow = true;
                    branches.truncate(MAX_RULE_BRANCHES);
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

fn attr_matches(entry: &FactEntry, name: &str, test: &AttrMatch, bindings: &Bindings) -> bool {
    let attr = entry.fact.attrs.get(name);
    if let AttrMatch::Test(ops) = test {
        if let Some(expected) = ops.present {
            if !expected {
                return attr.is_none();
            }
            if attr.is_none() {
                return false;
            }
        }
    }
    let Some(attr) = attr else {
        return false;
    };
    match (attr, test) {
        (AttrValue::Scalar(scalar), test) => scalar_matches(scalar, test, bindings),
        (AttrValue::List(list), test) => list
            .iter()
            .any(|scalar| scalar_matches(scalar, test, bindings)),
    }
}

fn scalar_matches(scalar: &Scalar, test: &AttrMatch, bindings: &Bindings) -> bool {
    match test {
        AttrMatch::Is(expected) => scalar_eq(&resolve_scalar(expected, bindings), scalar),
        AttrMatch::Test(ops) => scalar_ops(scalar, ops, bindings),
    }
}

fn scalar_ops(scalar: &Scalar, ops: &MatchOps, bindings: &Bindings) -> bool {
    let text = match scalar {
        Scalar::Text(text) => Some(text.as_str()),
        _ => None,
    };
    if let Some(expected) = &ops.eq {
        if !scalar_eq(&resolve_scalar(expected, bindings), scalar) {
            return false;
        }
    }
    if let Some(list) = &ops.any_of {
        let found = list
            .iter()
            .any(|expected| scalar_eq(&resolve_scalar(expected, bindings), scalar));
        if !found {
            return false;
        }
    }
    if let Some(pattern) = &ops.glob {
        let Some(text) = text else { return false };
        if !glob_match(pattern, text) {
            return false;
        }
    }
    if let Some(pattern) = &ops.regex {
        let Some(text) = text else { return false };
        let Ok(regex) = regex::Regex::new(pattern) else {
            return false;
        };
        if !regex.is_match(text) {
            return false;
        }
    }
    if let Some(gte) = ops.gte {
        let Some(number) = scalar_number(scalar) else {
            return false;
        };
        if number < gte {
            return false;
        }
    }
    if let Some(lte) = ops.lte {
        let Some(number) = scalar_number(scalar) else {
            return false;
        };
        if number > lte {
            return false;
        }
    }
    if let Some(gate) = &ops.version {
        let Some(text) = text else { return false };
        if gate.matches(text) != Some(true) {
            return false;
        }
    }
    true
}

fn scalar_test_text(test: &AttrMatch, text: &str, bindings: &Bindings) -> bool {
    scalar_matches(&Scalar::Text(text.to_string()), test, bindings)
}

fn resolve_scalar(scalar: &Scalar, bindings: &Bindings) -> Scalar {
    let Scalar::Text(text) = scalar else {
        return scalar.clone();
    };
    let Some(name) = text.strip_prefix('$') else {
        return scalar.clone();
    };
    bindings
        .values
        .get(name)
        .cloned()
        .unwrap_or_else(|| scalar.clone())
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

/// Case-insensitive `*` and `?`.
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
    signals: Vec<Signal>,
    findings: Vec<Finding>,
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

    let mut products: Vec<RuleProducts> = Vec::new();
    for (plugin, rule) in rules {
        let Some(product) = evaluate_rule(input, plugin, rule) else {
            continue;
        };
        products.push(product);
    }

    let fired: BTreeSet<(String, String)> = products
        .iter()
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
    next_steps.sort_by_key(|kind| next_step_rank(*kind));
    next_steps.dedup();

    EvaluateOutput {
        signals,
        findings: clean_findings(findings),
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

    let view = View::build(
        &allowed,
        input.engine_facts,
        producer.is_none(),
        input.resolver,
    );
    let result = eval_condition(&rule.when, &view, input.resolver, &Bindings::default());

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

    for branch in true_branches {
        apply_effects(input, plugin, rule, &rule.effects, branch, &mut product);
        if let Some(verify) = &rule.verify {
            match branch.bindings.refs.get(&verify.target) {
                Some(target) => {
                    let inspection = input.inspector.inspect(&InspectionRequest {
                        inspector: &verify.inspector,
                        target: &target.reference,
                    });
                    match inspection {
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
                            apply_effects(
                                input,
                                plugin,
                                rule,
                                &verify.on_unknown,
                                branch,
                                &mut product,
                            );
                            product.could_not_check.push(CouldNotCheck {
                                reason: CouldNotCheckReason::InspectorMissing,
                                subject: verify.inspector.clone(),
                                detail: None,
                                source: Some(rule_source(plugin, rule, None)),
                            });
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
                                detail: Some(detail),
                                source: Some(rule_source(plugin, rule, None)),
                            });
                        }
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

fn overflow_note(plugin: &PluginIdentity, rule: &Rule) -> CouldNotCheck {
    CouldNotCheck {
        reason: CouldNotCheckReason::TooManyMatches,
        subject: rule.id.clone(),
        detail: Some(format!(
            "the rule matched more than {MAX_RULE_BRANCHES} branches"
        )),
        source: Some(rule_source(plugin, rule, None)),
    }
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
                // A mechanism or environment finding never fills the "who did it" slot, even if
                // a pack that skipped the linter tried to give it one.
                if *scope != Scope::RootCause {
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
                    scope: *scope,
                    level: *level,
                    candidate: owner.clone(),
                    headline: rendered.headline,
                    why_not_sure: rendered.why_not_sure,
                    source: source.clone(),
                    evidence: clean_evidence(branch.evidence.clone()),
                });
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
