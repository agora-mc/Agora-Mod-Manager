//! The linter for crash packs, and the checker for what a parser returns. `agora plugin validate`
//! and the plugin loader call these; so does the host on every parser output.
//!
//! Unknown fields are refused while parsing, so hand-written JSON should go through
//! [`lint_pack_json`] or [`lint_rule_json`], not plain `serde_json`.

use super::*;
use crate::condition::{compare_dotted, dotted};
use regex::{Regex, RegexBuilder};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// Which check a problem came from. Tests and tools match on this; people read the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LintCode {
    /// Not valid JSON, or an unknown field or value. Serde's message names it.
    Parse,
    ContractVersion,
    BadId,
    DuplicateId,
    BadKind,
    /// `core.*` kind or attribute that isn't in the vocabulary.
    UnknownCoreName,
    /// An empty or impossible condition, which would be vacuously true or never true.
    EmptyCondition,
    /// Nothing but negations, or a text match with no bound and no other condition.
    Unanchored,
    /// A regex that doesn't compile or is too long.
    BadRegex,
    /// A raw address without an exact runtime.
    RawAddress,
    /// Words that claim certainty.
    Wording,
    BadText,
    BadVersionGate,
    /// A variable used but not bound, or not bound to a ref where a ref is needed.
    BadVariable,
    /// A bad effect: nothing to do, a mechanism accusing someone, a weight out of range.
    BadEffect,
    BadSource,
    BadParser,
    /// A ref map or role that could match everything, or nothing.
    BadRefMap,
    /// A `save_risk` pattern that is empty, absolute or climbs out of the instance.
    BadSaveRisk,
    /// Parser output over a limit or breaking a shape rule.
    BadOutput,
}

/// One thing wrong, with where it is and what to do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintProblem {
    pub code: LintCode,
    /// `rule example_id`, `source log`, or empty for the whole file.
    pub at: String,
    pub message: String,
}

impl fmt::Display for LintProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.at.is_empty() {
            write!(f, "{}", self.message)
        } else {
            write!(f, "{}: {}", self.at, self.message)
        }
    }
}

impl std::error::Error for LintProblem {}

fn problem(code: LintCode, at: &str, message: impl Into<String>) -> LintProblem {
    LintProblem {
        code,
        at: at.to_string(),
        message: message.into(),
    }
}

/// Parse and lint a pack file. The pack comes back only if it is clean.
pub fn lint_pack_json(json: &str) -> Result<CrashPack, Vec<LintProblem>> {
    let pack: CrashPack = serde_json::from_str(json)
        .map_err(|e| vec![problem(LintCode::Parse, "", e.to_string())])?;
    let problems = lint_pack(&pack);
    if problems.is_empty() {
        Ok(pack)
    } else {
        Err(problems)
    }
}

/// Parse and lint one rule on its own.
pub fn lint_rule_json(json: &str) -> Result<Rule, Vec<LintProblem>> {
    let rule: Rule = serde_json::from_str(json)
        .map_err(|e| vec![problem(LintCode::Parse, "", e.to_string())])?;
    let problems = lint_rule(&rule);
    if problems.is_empty() {
        Ok(rule)
    } else {
        Err(problems)
    }
}

/// Everything wrong with a pack. Empty means clean.
pub fn lint_pack(pack: &CrashPack) -> Vec<LintProblem> {
    let mut out = Vec::new();
    if pack.contract != CRASH_CONTRACT_VERSION {
        out.push(problem(
            LintCode::ContractVersion,
            "",
            format!(
                "this pack is written for crash contract {}, and this Agora reads {}",
                pack.contract, CRASH_CONTRACT_VERSION
            ),
        ));
    }
    let mut ids = |what: &str, id: &str, seen: &mut BTreeSet<String>| {
        if !is_valid_id(id) {
            out.push(problem(
                LintCode::BadId,
                &format!("{what} `{id}`"),
                "an id is lower-case letters, digits, `_`, `-` and `.`",
            ));
        }
        if !seen.insert(id.to_string()) {
            out.push(problem(
                LintCode::DuplicateId,
                &format!("{what} `{id}`"),
                format!("two {what}s in this pack have this id"),
            ));
        }
    };
    let (mut sources, mut parsers, mut inspectors, mut rules) = Default::default();
    for source in &pack.sources {
        ids("source", &source.id, &mut sources);
    }
    for parser in &pack.parsers {
        ids("parser", &parser.id, &mut parsers);
    }
    for inspector in &pack.inspectors {
        ids("inspector", &inspector.id, &mut inspectors);
    }
    for rule in &pack.rules {
        ids("rule", &rule.id, &mut rules);
    }
    for source in &pack.sources {
        lint_source(source, &mut out);
    }
    for parser in &pack.parsers {
        let at = format!("parser `{}`", parser.id);
        for regex in parser.detect.all.iter().chain(&parser.detect.any) {
            check_regex(regex, &at, &mut out);
        }
        if let Some(magic) = &parser.detect.magic_hex {
            if magic.is_empty()
                || magic.len() % 2 != 0
                || !magic.chars().all(|c| c.is_ascii_hexdigit())
            {
                out.push(problem(
                    LintCode::BadParser,
                    &at,
                    "`magic_hex` is an even number of hex digits",
                ));
            }
        }
    }
    for inspector in &pack.inspectors {
        if inspector.accepts.is_empty() {
            out.push(problem(
                LintCode::BadParser,
                &format!("inspector `{}`", inspector.id),
                "`accepts` is empty, so nothing could ever be inspected",
            ));
        }
    }
    for map in &pack.ref_maps {
        let at = format!("ref map `{}`", map.kind);
        match &map.lookup {
            RefLookup::Path { patterns, .. } => {
                if patterns.is_empty() || patterns.iter().any(|p| !p.contains("{value}")) {
                    out.push(problem(
                        LintCode::BadRefMap,
                        &at,
                        "every path pattern needs `{value}`, or every ref would trace to one file",
                    ));
                }
                for pattern in patterns {
                    if let Err(message) = check_path_pattern(pattern) {
                        out.push(problem(LintCode::BadRefMap, &at, message));
                    }
                }
                refuse_segment(&mut out, &at, map.matching);
            }
            RefLookup::Index(index) => {
                if index.is_empty() {
                    out.push(problem(LintCode::BadRefMap, &at, "`index` is empty"));
                }
                refuse_segment(&mut out, &at, map.matching);
            }
            RefLookup::Facts { kind, key, .. } => {
                if kind.is_empty() {
                    out.push(problem(LintCode::BadRefMap, &at, "`facts.kind` is empty"));
                }
                if key.is_empty() {
                    out.push(problem(LintCode::BadRefMap, &at, "`facts.key` is empty"));
                }
                if map.matching != NameMatch::Segment {
                    out.push(problem(
                        LintCode::BadRefMap,
                        &at,
                        "a `facts` lookup is only valid with `match: \"segment\"`",
                    ));
                }
            }
        }
        if map.kind.is_empty() {
            out.push(problem(LintCode::BadRefMap, &at, "`kind` is empty"));
        }
    }
    for role in &pack.roles {
        if role.kind.is_empty() || role.value.is_empty() {
            out.push(problem(
                LintCode::BadRefMap,
                "a role",
                "a role needs a `kind` and a `value`, or it could match everything",
            ));
        }
    }
    if let Some(save_risk) = &pack.save_risk {
        lint_save_risk(save_risk, &mut out);
    }
    for rule in &pack.rules {
        out.extend(lint_rule(rule));
        for gone in &rule.supersedes {
            if !rules.contains(gone) {
                out.push(problem(
                    LintCode::BadId,
                    &format!("rule `{}`", rule.id),
                    format!("`supersedes` names `{gone}`, which isn't in this pack"),
                ));
            }
        }
    }
    out
}

/// `segment` is a `facts` match and compares against an analysis rather than an instance index.
fn refuse_segment(out: &mut Vec<LintProblem>, at: &str, matching: NameMatch) {
    if matching == NameMatch::Segment {
        out.push(problem(
            LintCode::BadRefMap,
            at,
            "`segment` matching is only valid with a `facts` lookup",
        ));
    }
}

fn lint_save_risk(save_risk: &SaveRisk, out: &mut Vec<LintProblem>) {
    if save_risk.is_empty() {
        out.push(problem(
            LintCode::BadSaveRisk,
            "save_risk",
            "`save_risk` needs at least one `risky` or `safe` pattern; omit it to say nothing",
        ));
        return;
    }
    for (label, patterns) in [("risky", &save_risk.risky), ("safe", &save_risk.safe)] {
        for pattern in patterns {
            let at = format!("save_risk.{label} `{pattern}`");
            if pattern.trim().is_empty() {
                out.push(problem(LintCode::BadSaveRisk, &at, "the pattern is empty"));
                continue;
            }
            if pattern.contains('\\') {
                out.push(problem(
                    LintCode::BadSaveRisk,
                    &at,
                    "write paths with `/`, not `\\`",
                ));
                continue;
            }
            if pattern.starts_with('/') || pattern.contains(':') {
                out.push(problem(
                    LintCode::BadSaveRisk,
                    &at,
                    "a `save_risk` pattern is game-relative; it cannot be absolute",
                ));
                continue;
            }
            if pattern
                .split('/')
                .any(|segment| segment.is_empty() || segment == ".." || segment == ".")
            {
                out.push(problem(
                    LintCode::BadSaveRisk,
                    &at,
                    "the pattern has an empty, `.` or `..` path segment",
                ));
            }
        }
    }
}

fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-' | '.'))
}

fn lint_source(source: &CrashSource, out: &mut Vec<LintProblem>) {
    let at = format!("source `{}`", source.id);
    if source.role == SourceRole::Primary && source.bind.is_empty() {
        out.push(problem(
            LintCode::BadSource,
            &at,
            "a primary source needs `bind`: without it, an old log would read as a new crash",
        ));
    }
    if source.matching.glob.trim().is_empty() {
        out.push(problem(LintCode::BadSource, &at, "`match.glob` is empty"));
    }
    if let Some(format) = &source.format {
        let invalid = match format.split_once(':') {
            Some((plugin_id, parser_id)) => !is_valid_id(plugin_id) || !is_valid_id(parser_id),
            None => !is_valid_id(format),
        };
        if invalid {
            out.push(problem(
                LintCode::BadSource,
                &at,
                "`format` is a parser id, or `\"<plugin id>:<parser id>\"` for a parser from a \
                 declared dependency",
            ));
        }
    }
    if source.unit == ArtifactUnit::File && !source.children.is_empty() {
        out.push(problem(
            LintCode::BadSource,
            &at,
            "`children` only means something when `unit` is `folder`",
        ));
    }
    for bind in &source.bind {
        if let SourceBind::PidInName { regex } = bind {
            if let Some(compiled) = check_regex(regex, &at, out) {
                if compiled.captures_len() < 2 {
                    out.push(problem(
                        LintCode::BadSource,
                        &at,
                        "`pid_in_name` needs a capture group holding the process id",
                    ));
                }
            }
        }
    }
}

/// Everything wrong with one rule. Empty means clean.
pub fn lint_rule(rule: &Rule) -> Vec<LintProblem> {
    let mut lint = RuleLint {
        at: format!("rule `{}`", rule.id),
        out: Vec::new(),
        raw_attr: None,
    };
    if !is_valid_id(&rule.id) {
        lint.push(
            LintCode::BadId,
            "an id is lower-case letters, digits, `_`, `-` and `.`",
        );
    }
    if let Some(limit) = rule.max_branches {
        let limit = limit as usize;
        if !(MAX_RULE_BRANCHES..=MAX_RULE_BRANCHES_CEILING).contains(&limit) {
            lint.push(
                LintCode::BadEffect,
                format!(
                    "`max_branches` is from {MAX_RULE_BRANCHES} to {MAX_RULE_BRANCHES_CEILING}, \
                     and this one is {limit}"
                ),
            );
        }
    }
    if let Some(runtime) = &rule.scope.runtime {
        lint.version_gate(runtime);
    }
    if let Some(producer) = &rule.scope.producer {
        if let Some(version) = &producer.version {
            lint.version_gate(version);
        }
    }

    let mut bound = Bound::default();
    lint.condition(&rule.when, &mut bound);

    match anchor(&rule.when) {
        Anchor::None => lint.push(
            LintCode::Unanchored,
            "`when` can hold with nothing positively found (it is only `not`s or an `at_most`), \
             so this rule would fire on an empty log. Add a condition on something that was found",
        ),
        Anchor::UnboundedText => lint.push(
            LintCode::Unanchored,
            "a text match with no `section` or `source` can fire on its own, which means any line \
             anywhere. Give it a `section` or `source`, or require another condition with it",
        ),
        Anchor::BoundedText | Anchor::Fact => {}
    }

    if let Some(attr) = lint.raw_attr.clone() {
        let exact = rule
            .scope
            .runtime
            .as_ref()
            .is_some_and(|r| !r.exact.is_empty());
        if !exact {
            lint.push(
                LintCode::RawAddress,
                format!(
                    "{attr} is a raw address, which moves with every build of the game. Pin the \
                     build with `scope.runtime.exact`, or match a stable symbol instead"
                ),
            );
        }
    }

    let verify_effects = rule
        .verify
        .iter()
        .flat_map(|v| v.on_pass.iter().chain(&v.on_fail).chain(&v.on_unknown));
    let any_effect = !rule.effects.is_empty() || verify_effects.clone().next().is_some();
    if !any_effect {
        lint.push(
            LintCode::BadEffect,
            "this rule has no effects, so it could never say anything",
        );
    }
    if let Some(verify) = &rule.verify {
        lint.need_ref(&verify.target, "`verify.target`", &bound);
    }
    let effects: Vec<_> = rule.effects.iter().chain(verify_effects).collect();
    for effect in &effects {
        lint.effect(effect, &bound);
    }
    lint.finding_causes(rule);

    lint.text(&rule.text, &bound);
    for repair in &rule.repairs {
        match repair {
            RepairRequest::DisableContent { key }
            | RepairRequest::EnableContent { key }
            | RepairRequest::PinContentUpdate { key }
            | RepairRequest::UnpinContentUpdate { key } => {
                lint.need_ref(&key.owner_of, "a repair's `key`", &bound);
            }
            RepairRequest::SetJvmMemory { .. }
            | RepairRequest::ResetJvmArgs
            | RepairRequest::CreateSnapshot { .. } => {}
        }
    }
    lint.out
}

/// The variables bound so far: name to whether it holds a ref.
#[derive(Debug, Clone, Default)]
struct Bound(BTreeMap<String, bool>);

impl Bound {
    fn merge(&mut self, other: Bound) {
        self.0.extend(other.0);
    }

    /// Only names bound in every one of `branches`. A variable bound in only some would be unbound
    /// when another branch is the one that held.
    fn common(branches: &[Bound]) -> Bound {
        let Some(first) = branches.first() else {
            return Bound::default();
        };
        let mut out = first.clone();
        out.0
            .retain(|name, _| branches.iter().all(|b| b.0.contains_key(name)));
        for (name, is_ref) in out.0.iter_mut() {
            *is_ref = branches.iter().all(|b| b.0.get(name) == Some(&true));
        }
        out
    }
}

/// Everything wrong with what an inspector returned. An inspector may emit the same facts a parser
/// may, so the fact rules are exactly a parser's; `detail` is length-bounded too. Empty means valid.
pub fn validate_inspector_output(
    output: &InspectorOutput,
    limits: &ParserLimits,
) -> Vec<LintProblem> {
    // An inspector has no producer, bucket, coverage or notes, so a parser shell carries its facts
    // and contract through the same checks.
    let shell = ParserOutput {
        contract: output.contract,
        producer: Producer {
            format: "inspector".to_string(),
            version: None,
        },
        facts: output.facts.clone(),
        bucket: Vec::new(),
        coverage: Vec::new(),
        notes: Vec::new(),
    };
    let mut out = validate_parser_output(&shell, limits);
    if let Some(detail) = &output.detail {
        if detail.len() > limits.max_attr_len as usize {
            out.push(problem(
                LintCode::BadOutput,
                "",
                format!("`detail` is longer than {} bytes", limits.max_attr_len),
            ));
        }
    }
    out
}

struct RuleLint {
    at: String,
    out: Vec<LintProblem>,
    raw_attr: Option<String>,
}

impl RuleLint {
    fn push(&mut self, code: LintCode, message: impl Into<String>) {
        self.out.push(problem(code, &self.at, message));
    }

    fn condition(&mut self, condition: &Condition, bound: &mut Bound) {
        match condition {
            Condition::All(list) => {
                if list.is_empty() {
                    self.push(
                        LintCode::EmptyCondition,
                        "`all` is empty, which is always true",
                    );
                }
                for child in list {
                    self.condition(child, bound);
                }
            }
            Condition::Any(list) => {
                if list.is_empty() {
                    self.push(
                        LintCode::EmptyCondition,
                        "`any` is empty, which is never true",
                    );
                }
                self.branches(list, bound);
            }
            Condition::AtLeast { n, of } => {
                if *n == 0 || *n as usize > of.len() {
                    self.push(
                        LintCode::EmptyCondition,
                        format!(
                            "`at_least` asks for {n} of {} conditions; it must be from 1 to {}",
                            of.len(),
                            of.len()
                        ),
                    );
                }
                self.branches(of, bound);
            }
            Condition::Not(inner) => {
                // Sees what is bound, but its own bindings don't get out.
                let mut scratch = bound.clone();
                self.condition(inner, &mut scratch);
            }
            Condition::Fact(query) => {
                self.kind_and_filter(&query.kind, &query.filter, query.reference.as_ref(), bound);
                let attrs = query
                    .kind
                    .starts_with("core.")
                    .then(|| core_kind_attrs(&query.kind));
                for (variable, attr) in &query.bind {
                    if let Some(Some(known)) = &attrs {
                        if !known.contains(&attr.as_str()) {
                            self.push(
                                LintCode::UnknownCoreName,
                                format!("`{}` has no attribute `{attr}`", query.kind),
                            );
                        }
                    }
                    bound.0.insert(variable.clone(), false);
                }
                if let Some(variable) = query.reference.as_ref().and_then(|r| r.bind.as_ref()) {
                    bound.0.insert(variable.clone(), true);
                }
            }
            Condition::Count(query) => {
                self.kind_and_filter(&query.kind, &query.filter, query.reference.as_ref(), bound);
                if query.at_least.is_none() && query.at_most.is_none() {
                    self.push(
                        LintCode::EmptyCondition,
                        "`count` needs `at_least` or `at_most`",
                    );
                }
                if query.at_least == Some(0) {
                    self.push(LintCode::EmptyCondition, "`at_least: 0` is always true");
                }
                if query.reference.as_ref().is_some_and(|r| r.bind.is_some()) {
                    self.push(LintCode::BadVariable, "`count` can't bind a variable");
                }
            }
            Condition::Text(text) => {
                let Some(compiled) = check_regex(&text.regex, &self.at.clone(), &mut self.out)
                else {
                    return;
                };
                self.raw_in_regex(&text.regex, "a text match");
                let names: BTreeSet<&str> = compiled.capture_names().flatten().collect();
                for name in &names {
                    bound
                        .0
                        .insert((*name).to_string(), text.refs.contains_key(*name));
                }
                for (name, kind) in &text.refs {
                    if !names.contains(name.as_str()) {
                        self.push(
                            LintCode::BadVariable,
                            format!("`refs` names `{name}`, and the regex has no such capture"),
                        );
                    }
                    if kind.is_empty() {
                        self.push(LintCode::BadKind, format!("`refs.{name}` has no ref kind"));
                    }
                }
            }
        }
    }

    fn branches(&mut self, list: &[Condition], bound: &mut Bound) {
        let results: Vec<Bound> = list
            .iter()
            .map(|child| {
                let mut branch = bound.clone();
                self.condition(child, &mut branch);
                branch
            })
            .collect();
        bound.merge(Bound::common(&results));
    }

    fn kind_and_filter(
        &mut self,
        kind: &str,
        filter: &BTreeMap<String, AttrMatch>,
        reference: Option<&RefQuery>,
        bound: &Bound,
    ) {
        if !is_valid_kind(kind) {
            self.push(
                LintCode::BadKind,
                format!("`{kind}` isn't a fact kind; a kind is `namespace.name`"),
            );
        }
        let known = if kind.starts_with("core.") {
            let attrs = core_kind_attrs(kind);
            if attrs.is_none() {
                self.push(
                    LintCode::UnknownCoreName,
                    format!("`{kind}` isn't in the core vocabulary"),
                );
            }
            attrs
        } else {
            None
        };
        for (attr, test) in filter {
            if let Some(known) = &known {
                if !known.contains(&attr.as_str()) {
                    self.push(
                        LintCode::UnknownCoreName,
                        format!("`{kind}` has no attribute `{attr}`"),
                    );
                }
            }
            self.attr_match(attr, test, bound);
        }
        if let Some(reference) = reference {
            if reference.kind.is_empty() {
                self.push(LintCode::BadKind, "`ref.kind` is empty");
            }
            if let Some(value) = &reference.value {
                self.attr_match("ref value", value, bound);
            }
        }
    }

    fn attr_match(&mut self, attr: &str, test: &AttrMatch, bound: &Bound) {
        if is_raw_address_attr(attr)
            && !matches!(
                test,
                AttrMatch::Test(MatchOps {
                    present: Some(_),
                    ..
                })
            )
        {
            self.raw_attr = Some(format!("`{attr}`"));
        }
        match test {
            AttrMatch::Is(scalar) => self.scalar(attr, scalar, bound),
            AttrMatch::Test(ops) => {
                if *ops == MatchOps::default() {
                    self.push(
                        LintCode::EmptyCondition,
                        format!("the test on `{attr}` is empty"),
                    );
                }
                if let Some(scalar) = &ops.eq {
                    self.scalar(attr, scalar, bound);
                }
                if let Some(list) = &ops.any_of {
                    if list.is_empty() {
                        self.push(
                            LintCode::EmptyCondition,
                            format!("`in` on `{attr}` is empty, which is never true"),
                        );
                    }
                    for scalar in list {
                        self.scalar(attr, scalar, bound);
                    }
                }
                if let Some(glob) = &ops.glob {
                    if looks_like_module_offset(glob) {
                        self.raw_attr = Some(format!("the value of `{attr}`"));
                    }
                }
                if let Some(regex) = &ops.regex {
                    check_regex(regex, &self.at.clone(), &mut self.out);
                    self.raw_in_regex(regex, &format!("the regex on `{attr}`"));
                }
                if let Some(gate) = &ops.version {
                    self.version_gate(gate);
                }
            }
        }
    }

    fn scalar(&mut self, attr: &str, scalar: &Scalar, bound: &Bound) {
        let Scalar::Text(text) = scalar else {
            return;
        };
        if let Some(name) = text.strip_prefix('$') {
            if !bound.0.contains_key(name) {
                self.push(
                    LintCode::BadVariable,
                    format!("`{attr}` uses `${name}`, which isn't bound by an earlier condition"),
                );
            }
        } else if looks_like_module_offset(text) || (attr == "symbol_scheme" && text == "rva") {
            self.raw_attr = Some(format!("the value of `{attr}`"));
        }
    }

    fn raw_in_regex(&mut self, regex: &str, what: &str) {
        if regex_names_address(regex) {
            self.raw_attr = Some(what.to_string());
        }
    }

    fn version_gate(&mut self, gate: &VersionGate) {
        if !gate.exact.is_empty() {
            if gate.min.is_some() || gate.below.is_some() {
                self.push(
                    LintCode::BadVersionGate,
                    "a version gate is either `exact`, or `min` and `below`, not both",
                );
            }
            return;
        }
        if gate.min.is_none() && gate.below.is_none() {
            self.push(LintCode::BadVersionGate, "this version gate is empty");
        }
        let parts = [gate.min.as_deref(), gate.below.as_deref()].map(|v| v.map(|v| (v, dotted(v))));
        for (text, parsed) in parts.iter().flatten() {
            if parsed.is_none() {
                self.push(
                    LintCode::BadVersionGate,
                    format!("`{text}` isn't dotted numbers; name such a version with `exact`"),
                );
            }
        }
        if let [Some((min_text, Some(min))), Some((below_text, Some(below)))] = &parts {
            if compare_dotted(min, below).is_ge() {
                self.push(
                    LintCode::BadVersionGate,
                    format!("`min` {min_text} isn't below `below` {below_text}, so nothing passes"),
                );
            }
        }
    }

    fn need_ref(&mut self, variable: &str, what: &str, bound: &Bound) {
        match bound.0.get(variable) {
            None => self.push(
                LintCode::BadVariable,
                format!(
                    "{what} uses `{variable}`, which `when` doesn't always bind. A name bound in \
                     `not` or in only some branches of `any` can't be used"
                ),
            ),
            Some(false) => self.push(
                LintCode::BadVariable,
                format!(
                    "{what} uses `{variable}`, which holds a plain value. Bind it through `ref` so \
                     that its owner can be looked up"
                ),
            ),
            Some(true) => {}
        }
    }

    /// A cause that rarely singles out one mod still names one. That can be right (a plugin
    /// refused for a missing dependency), but it needs its own explanation, or it reads as blame.
    fn finding_causes(&mut self, rule: &Rule) {
        let verify_effects = rule.verify.iter().flat_map(|verify| {
            verify
                .on_pass
                .iter()
                .chain(&verify.on_fail)
                .chain(&verify.on_unknown)
        });
        for effect in rule.effects.iter().chain(verify_effects) {
            let (cause, text, what) = match effect {
                Effect::Finding {
                    candidate: Some(_),
                    cause: Some(cause),
                    text,
                    ..
                } => (
                    *cause,
                    text.as_ref().and_then(|text| text.why_not_sure.as_ref()),
                    "finding",
                ),
                // A signal has no text of its own, so the rule's explanation is the only one.
                Effect::Signal {
                    cause: Some(cause), ..
                } => (*cause, None, "signal"),
                _ => continue,
            };
            if matches!(cause, Cause::Mod | Cause::VersionMismatch | Cause::File) {
                continue;
            }
            let why = text.or(rule.text.why_not_sure.as_ref());
            let has_reason =
                why.is_some_and(|why| why.get("en").is_some_and(|words| !words.trim().is_empty()));
            if !has_reason {
                self.push(
                    LintCode::BadEffect,
                    format!(
                        "a `{}` {what} names a candidate, so it needs `why_not_sure`: `{}` rarely \
                         singles out one mod, and the user must see why this one is named",
                        cause.label(),
                        cause.label()
                    ),
                );
            }
        }
    }

    fn effect(&mut self, effect: &Effect, bound: &Bound) {
        match effect {
            Effect::Signal {
                candidate, weight, ..
            } => {
                self.need_ref(&candidate.owner_of, "a signal's `candidate`", bound);
                self.weight(*weight);
            }
            Effect::Finding {
                scope,
                candidate,
                family,
                weight,
                text,
                ..
            } => {
                match (scope, candidate) {
                    (Scope::RootCause, None) => self.push(
                        LintCode::BadEffect,
                        "a `root_cause` finding must name a `candidate`; to describe what happened \
                         without blaming anyone, use `mechanism`",
                    ),
                    (Scope::Mechanism | Scope::Environment, Some(_)) => self.push(
                        LintCode::BadEffect,
                        "a `mechanism` or `environment` finding accuses nobody, so it can't have a \
                         `candidate`. To add a clue, use a `signal`",
                    ),
                    _ => {}
                }
                match candidate {
                    Some(owner) => {
                        self.need_ref(&owner.owner_of, "a finding's `candidate`", bound);
                        match (family, weight) {
                            (Some(family), Some(weight)) => {
                                self.weight(*weight);
                                if family.is_boost_only() {
                                    self.push(
                                        LintCode::BadEffect,
                                        format!(
                                            "a finding can't rest on `{family:?}`, which can only \
                                             raise a suspect other evidence named. Use a `signal`"
                                        ),
                                    );
                                }
                            }
                            _ => self.push(
                                LintCode::BadEffect,
                                "a finding with a `candidate` needs a `family` and a `weight`",
                            ),
                        }
                    }
                    None => {
                        if family.is_some() || weight.is_some() {
                            self.push(
                                LintCode::BadEffect,
                                "`family` and `weight` only mean something with a `candidate`",
                            );
                        }
                    }
                }
                if let Some(text) = text {
                    self.text(text, bound);
                }
            }
            Effect::Trust { text, .. } | Effect::Context { text } => {
                self.local_text(text, "text", bound);
            }
            Effect::Demote { of, .. } => self.need_ref(of, "`demote`", bound),
        }
    }

    fn weight(&mut self, weight: u32) {
        if weight == 0 || weight > MAX_RULE_WEIGHT {
            self.push(
                LintCode::BadEffect,
                format!("a weight is from 1 to {MAX_RULE_WEIGHT}, and this one is {weight}"),
            );
        }
    }

    fn text(&mut self, text: &RuleText, bound: &Bound) {
        self.local_text(&text.headline, "`headline`", bound);
        if let Some(why) = &text.why_not_sure {
            self.local_text(why, "`why_not_sure`", bound);
        }
    }

    fn local_text(&mut self, text: &LocalText, what: &str, bound: &Bound) {
        if text.get("en").is_none_or(|t| t.trim().is_empty()) {
            self.push(
                LintCode::BadText,
                format!("{what} needs English text under `en`"),
            );
        }
        for (locale, words) in text {
            if let Some(found) = certainty_claim(words) {
                self.push(
                    LintCode::Wording,
                    format!(
                        "{what} ({locale}) says \"{found}\". Agora never claims certainty: false \
                         positives are always possible. Say \"very likely\", \"likely\", \
                         \"possible\" or \"not sure\", and set the level to match"
                    ),
                );
            }
            for (variable, needs_ref) in placeholders(words) {
                match bound.0.get(&variable) {
                    None => self.push(
                        LintCode::BadVariable,
                        format!(
                            "{what} ({locale}) uses `{{{variable}}}`, which isn't always bound"
                        ),
                    ),
                    Some(false) if needs_ref => self.push(
                        LintCode::BadVariable,
                        format!(
                            "{what} ({locale}) asks for the owner of `{variable}`, which holds a \
                             plain value. Bind it through `ref`"
                        ),
                    ),
                    _ => {}
                }
            }
        }
    }
}

/// Words Agora must not use about a cause (MASTER_SPEC §30.5). Returns the text found.
pub fn certainty_claim(text: &str) -> Option<String> {
    const CLAIMS: &str = r"(?i)\b(definite|definitely|certainly|undoubtedly|guaranteed|without (a )?doubt|no doubt|for sure|for certain)\b|\b100\s?%";
    let claims = Regex::new(CLAIMS).expect("the pattern is a constant");
    claims.find(text).map(|m| m.as_str().to_string())
}

/// The `{name}` and `{owner:name}` placeholders in a text, with whether each needs a ref.
fn placeholders(text: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        rest = &rest[open + 1..];
        let Some(close) = rest.find('}') else {
            break;
        };
        let inner = &rest[..close];
        match inner.strip_prefix("owner:") {
            Some(name) => out.push((name.to_string(), true)),
            None => out.push((inner.to_string(), false)),
        }
        rest = &rest[close + 1..];
    }
    out
}

/// Attribute names that hold a raw address, by the vocabulary's own naming convention. A plugin's
/// own kinds are held to the same names.
fn is_raw_address_attr(attr: &str) -> bool {
    matches!(attr, "address" | "rva" | "module_offset")
        || attr.ends_with("_address")
        || attr.ends_with("_rva")
}

/// `Something.exe+A3AF01`.
fn looks_like_module_offset(text: &str) -> bool {
    let Some((module, offset)) = text.rsplit_once('+') else {
        return false;
    };
    let offset = offset.strip_prefix("0x").unwrap_or(offset);
    !module.is_empty() && offset.len() >= 4 && offset.chars().all(|c| c.is_ascii_hexdigit())
}

/// A regex with a literal offset (`\+0CEC9C8`) or a long hex address (`0x7ff6a000`) in it. A
/// heuristic, and only meant to catch the honest mistake.
fn regex_names_address(regex: &str) -> bool {
    let address = Regex::new(r"\\\+(0x)?[0-9A-Fa-f]{4,}|0[xX][0-9A-Fa-f]{5,}")
        .expect("the pattern is a constant");
    address.is_match(regex)
}

fn check_regex(regex: &str, at: &str, out: &mut Vec<LintProblem>) -> Option<Regex> {
    if regex.len() > MAX_RULE_REGEX_LEN {
        out.push(problem(
            LintCode::BadRegex,
            at,
            format!(
                "a regex is at most {MAX_RULE_REGEX_LEN} bytes, and this one is {}",
                regex.len()
            ),
        ));
        return None;
    }
    if regex.is_empty() {
        out.push(problem(
            LintCode::BadRegex,
            at,
            "this regex is empty, so it matches anything",
        ));
        return None;
    }
    match RegexBuilder::new(regex)
        .size_limit(RULE_REGEX_SIZE_LIMIT)
        .build()
    {
        Ok(compiled) => Some(compiled),
        Err(error) => {
            out.push(problem(
                LintCode::BadRegex,
                at,
                format!("this regex is refused: {error}"),
            ));
            None
        }
    }
}

/// How firmly a condition rests on something actually found. Weakest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Anchor {
    None,
    UnboundedText,
    BoundedText,
    Fact,
}

/// The weakest way this condition can hold.
fn anchor(condition: &Condition) -> Anchor {
    match condition {
        Condition::All(list) => list.iter().map(anchor).max().unwrap_or(Anchor::None),
        Condition::Any(list) => list.iter().map(anchor).min().unwrap_or(Anchor::None),
        Condition::AtLeast { n, of } => {
            // The n weakest could be the ones that hold, and then the best of those is what it
            // rests on.
            let mut anchors: Vec<Anchor> = of.iter().map(anchor).collect();
            anchors.sort();
            let last = (*n as usize).clamp(1, anchors.len().max(1)) - 1;
            anchors.get(last).copied().unwrap_or(Anchor::None)
        }
        Condition::Not(_) => Anchor::None,
        Condition::Fact(query) => {
            let regex_on_line = query.kind == "core.log_line"
                && matches!(
                    query.filter.get("text"),
                    Some(AttrMatch::Test(MatchOps { regex: Some(_), .. }))
                );
            match (regex_on_line, query.filter.contains_key("section")) {
                (true, true) => Anchor::BoundedText,
                (true, false) => Anchor::UnboundedText,
                (false, _) => Anchor::Fact,
            }
        }
        Condition::Count(query) => match query.at_least {
            Some(n) if n > 0 => Anchor::Fact,
            _ => Anchor::None,
        },
        Condition::Text(text) => {
            if text.section.is_some() || text.source.is_some() {
                Anchor::BoundedText
            } else {
                Anchor::UnboundedText
            }
        }
    }
}

/// Everything wrong with what an `extra` parser returned. It is a parser output plus one rule: it
/// may emit only facts in a namespace of its own, never `core.*`, so two readers cannot both count
/// the same core fact.
pub fn validate_extra_output(output: &ParserOutput, limits: &ParserLimits) -> Vec<LintProblem> {
    let mut out = validate_parser_output(output, limits);
    for (index, fact) in output.facts.iter().enumerate() {
        if fact.kind.starts_with("core.") {
            out.push(problem(
                LintCode::BadOutput,
                &format!("fact {index} `{}`", fact.kind),
                "an `extra` reader may not emit `core.*` facts; a core fact belongs to the primary \
                 reader, and counting it twice would double its weight",
            ));
        }
    }
    out
}

/// Everything wrong with what a parser returned. The output is untrusted, so the host calls this on
/// every one and treats any problem as a failed parse. Empty means valid.
pub fn validate_parser_output(output: &ParserOutput, limits: &ParserLimits) -> Vec<LintProblem> {
    let mut out = Vec::new();
    let mut bad = |at: &str, message: String| out.push(problem(LintCode::BadOutput, at, message));
    if output.contract != CRASH_CONTRACT_VERSION {
        bad(
            "",
            format!(
                "output says contract {}, and this Agora reads {CRASH_CONTRACT_VERSION}",
                output.contract
            ),
        );
    }
    if output.producer.format.is_empty() {
        bad("", "`producer.format` is empty".to_string());
    }
    if output.facts.len() > limits.max_facts as usize {
        bad(
            "",
            format!(
                "{} facts, over the limit of {}",
                output.facts.len(),
                limits.max_facts
            ),
        );
    }
    if output.bucket.len() > limits.max_bucket_parts as usize {
        bad(
            "",
            format!(
                "{} bucket parts, over the limit of {}",
                output.bucket.len(),
                limits.max_bucket_parts
            ),
        );
    }
    if output.notes.len() > limits.max_notes as usize {
        bad(
            "",
            format!(
                "{} notes, over the limit of {}",
                output.notes.len(),
                limits.max_notes
            ),
        );
    }
    let max_len = limits.max_attr_len as usize;
    for part in &output.bucket {
        if part.scheme.is_empty() || part.value.is_empty() || part.value.len() > max_len {
            bad(
                "bucket",
                format!("a bucket part from `{}` is empty or too long", part.scheme),
            );
        }
    }

    let with_facts: BTreeSet<&str> = output.facts.iter().map(|f| f.kind.as_str()).collect();
    for coverage in &output.coverage {
        if !is_valid_kind(&coverage.kind) {
            bad("coverage", format!("`{}` isn't a fact kind", coverage.kind));
        }
        match coverage.state {
            CoverageState::Absent if coverage.why.as_deref().unwrap_or("").is_empty() => {
                bad(
                    "coverage",
                    format!("`{}` is `absent` with no `why`", coverage.kind),
                );
            }
            CoverageState::Absent | CoverageState::NotApplicable
                if with_facts.contains(coverage.kind.as_str()) =>
            {
                bad(
                    "coverage",
                    format!(
                        "`{}` says it wasn't checked, and there are facts of it",
                        coverage.kind
                    ),
                );
            }
            _ => {}
        }
    }

    // Report the first few and stop: 20,000 bad facts are one mistake.
    let mut fact_problems = 0;
    for (index, fact) in output.facts.iter().enumerate() {
        let at = format!("fact {index} `{}`", fact.kind);
        let mut note = |message: String| {
            fact_problems += 1;
            if fact_problems <= 10 {
                bad(&at, message);
            }
        };
        if !is_valid_kind(&fact.kind) {
            note("a kind is `namespace.name`".to_string());
        }
        let allowed = if fact.kind.starts_with("core.") {
            match parser_kind_attrs(&fact.kind) {
                Some(attrs) => Some(attrs),
                None => {
                    note(
                        "a parser can't emit this: it is the engine's, or not in the vocabulary"
                            .to_string(),
                    );
                    None
                }
            }
        } else {
            None
        };
        if fact.refs.len() > limits.max_refs_per_fact as usize {
            note(format!(
                "{} refs, over the limit of {}",
                fact.refs.len(),
                limits.max_refs_per_fact
            ));
        }
        for reference in &fact.refs {
            if reference.kind.is_empty() || reference.value.is_empty() {
                note("a ref needs a kind and a value".to_string());
            }
            if reference.value.len() > max_len {
                note(format!("the `{}` ref is too long", reference.kind));
            }
        }
        for (name, value) in &fact.attrs {
            if let Some(allowed) = allowed {
                if !allowed.contains(&name.as_str()) {
                    note(format!("`{}` has no attribute `{name}`", fact.kind));
                }
            }
            let scalars: &[Scalar] = match value {
                AttrValue::Scalar(scalar) => std::slice::from_ref(scalar),
                AttrValue::List(list) => {
                    if list.len() > limits.max_list_len as usize {
                        note(format!(
                            "`{name}` has {} entries, over the limit",
                            list.len()
                        ));
                    }
                    list
                }
            };
            if scalars
                .iter()
                .any(|s| matches!(s, Scalar::Text(t) if t.len() > max_len))
            {
                note(format!("`{name}` is longer than {max_len} bytes"));
            }
            if scalars
                .iter()
                .any(|s| matches!(s, Scalar::Number(n) if !n.is_finite()))
            {
                note(format!("`{name}` isn't a finite number"));
            }
        }
        if fact.kind == "core.frame"
            && !matches!(
                fact.attrs.get("index"),
                Some(AttrValue::Scalar(Scalar::Number(_)))
            )
        {
            note("a frame needs a numeric `index`".to_string());
        }
        if fact.kind == "core.exception" {
            let category = match fact.attrs.get("category") {
                Some(AttrValue::Scalar(Scalar::Text(c))) => Some(c.as_str()),
                _ => None,
            };
            if !category.is_some_and(|c| EXCEPTION_CATEGORIES.contains(&c)) {
                note(format!(
                    "`category` must be one of {EXCEPTION_CATEGORIES:?}"
                ));
            }
        }
    }
    out
}
