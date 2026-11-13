//! Answers "who owns this ref, and what role has it" from an instance snapshot and a crash pack.

use agora_game_api::crash::{
    AttrValue, CrashPack, Fact, NameMatch, Owner, Ref, RefLookup, RefMap, Role, RoleRule, Scalar,
};

use super::index::{
    class_segments, expand_pattern, matches_segments, matches_value, normalize_index_value,
    normalize_path, normalize_segment, ArchiveIndex, PatternError,
};
use super::view::InstanceView;
use crate::crash_engine::types::{ParsedArtifact, RefResolver, Resolved};

/// What attribution found out about one ref. [`Resolver::resolve`] drops the detail the evaluator's
/// trait does not carry; the rest is kept for the engine facts and for showing `also_provided_by`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribution {
    pub owner: Owner,
    pub role: Option<Role>,
    /// Content keys (or a short layer label) of the other enabled layers that provide the same
    /// file. Sorted and deduplicated, so it does not depend on listing order.
    pub also_provided_by: Vec<String>,
    /// The file the answer came from: the game-relative path, or an archive holding it.
    pub via: Option<String>,
    /// Why the owner is `Unknown`, or why a turned-off layer is named.
    pub reason: Option<String>,
}

/// The real [`RefResolver`]: one instance snapshot, one pack's lookup rules and roles.
pub struct Resolver {
    view: InstanceView,
    ref_maps: Vec<RefMap>,
    roles: Vec<RoleRule>,
    archives: Option<Box<dyn ArchiveIndex>>,
}

impl Resolver {
    pub fn new(view: InstanceView, ref_maps: Vec<RefMap>, roles: Vec<RoleRule>) -> Self {
        Self {
            view,
            ref_maps,
            roles,
            archives: None,
        }
    }

    /// A resolver for a pack's `ref_maps` and `roles`, cloned out of it so it outlives the pack.
    pub fn from_pack(view: InstanceView, pack: &CrashPack) -> Self {
        Self::new(view, pack.ref_maps.clone(), pack.roles.clone())
    }

    /// Supply the archive index. Without one, a map that asks for `archives` cannot rule a loose
    /// file out, so a miss is `Unknown`.
    pub fn with_archive_index(mut self, index: impl ArchiveIndex + 'static) -> Self {
        self.archives = Some(Box::new(index));
        self
    }

    pub fn view(&self) -> &InstanceView {
        &self.view
    }

    /// Everything attribution knows about `reference`.
    pub fn attribute(&self, reference: &Ref) -> Attribution {
        let value = reference.value.trim();
        let matched_role = self.matching_role(reference);
        let role = matched_role.map(|rule| rule.role);

        let maps: Vec<&RefMap> = self
            .ref_maps
            .iter()
            .filter(|map| map.kind == reference.kind)
            .collect();
        if maps.is_empty() {
            return Attribution {
                owner: Owner::Unknown,
                role,
                also_provided_by: Vec::new(),
                via: None,
                reason: Some(format!("no ref map for kind '{}'", reference.kind)),
            };
        }

        if value.is_empty() {
            return Attribution {
                owner: Owner::Unknown,
                role,
                also_provided_by: Vec::new(),
                via: None,
                reason: Some("the ref has an empty value".to_string()),
            };
        }

        // A pack may list more than one map for a kind: an earlier one that finds nothing lets
        // the next try, and the last miss is the answer when none finds anything.
        let mut fallback = None;
        for map in maps {
            let mut answer = match &map.lookup {
                RefLookup::Path { patterns, archives } => {
                    self.resolve_path(map.matching, patterns, *archives, value, matched_role)
                }
                RefLookup::Index(index) => {
                    self.resolve_index(index, map.matching, value, matched_role)
                }
                RefLookup::Facts { .. } => unknown(
                    "this resolver has no parsed facts for a `facts` ref map; only a loose file has",
                ),
            };
            answer.role = role;
            if !matches!(answer.owner, Owner::Unknown | Owner::NotInInstance) {
                return answer;
            }
            fallback = Some(answer);
        }
        let mut answer = fallback.expect("there was at least one ref map");
        answer.role = role;
        answer
    }

    fn matching_role<'a>(&'a self, reference: &Ref) -> Option<&'a RoleRule> {
        let value = reference.value.trim();
        self.roles.iter().find(|rule| {
            rule.kind == reference.kind && matches_value(rule.matching, value, &rule.value)
        })
    }

    fn resolve_path(
        &self,
        matching: NameMatch,
        patterns: &[String],
        archives: bool,
        value: &str,
        role: Option<&RoleRule>,
    ) -> Attribution {
        if !self.view.is_complete() {
            return unknown(
                self.view
                    .listing()
                    .reason()
                    .unwrap_or("the instance listing is unavailable"),
            );
        }

        let mut matches: Vec<PathMatch> = Vec::new();
        let mut problem: Option<String> = None;
        let mut could_not_rule_out: Option<String> = None;
        for (pattern_index, pattern) in patterns.iter().enumerate() {
            let plan = match expand_pattern(pattern, value) {
                Ok(plan) => plan,
                Err(PatternError::Unsupported(reason)) => {
                    problem.get_or_insert(reason);
                    continue;
                }
                Err(PatternError::Invalid(reason)) => {
                    problem.get_or_insert(reason);
                    continue;
                }
            };
            if plan.has_wildcard {
                if archives {
                    could_not_rule_out.get_or_insert_with(|| {
                        "an archive index cannot search a wildcard path, so a file that is not loose cannot be ruled out".to_string()
                    });
                }
                for record in self
                    .view
                    .index()
                    .files_by_final_name(&plan.final_name, matching)
                {
                    let file = self.view.index().record(record);
                    if !matches_segments(&plan.segments, &file.path, matching) {
                        continue;
                    }
                    matches.push(PathMatch {
                        pattern_index,
                        group: format!(
                            "{pattern_index}\0{final_name}",
                            final_name = plan.final_name
                        ),
                        via: file.path.clone(),
                        positions: vec![file.position],
                    });
                }
                continue;
            }

            let key = plan
                .full_key()
                .expect("a pattern without a wildcard has a full key");
            let records = self.view.index().files(&key, matching);
            if !records.is_empty() {
                matches.push(PathMatch {
                    pattern_index,
                    group: key.clone(),
                    via: key.clone(),
                    positions: records
                        .iter()
                        .map(|record| self.view.index().record(*record).position)
                        .collect(),
                });
            }
            if !archives {
                continue;
            }
            match self.archives.as_deref() {
                Some(index) => match index.archives_containing(&key) {
                    Some(archive_paths) => {
                        for archive in archive_paths {
                            let Some(archive_key) = normalize_path(&archive) else {
                                continue;
                            };
                            let records = self.view.index().files(&archive_key, matching);
                            if !records.is_empty() {
                                matches.push(PathMatch {
                                    pattern_index,
                                    group: archive_key.clone(),
                                    via: archive_key,
                                    positions: records
                                        .iter()
                                        .map(|record| self.view.index().record(*record).position)
                                        .collect(),
                                });
                            }
                        }
                    }
                    None => {
                        could_not_rule_out.get_or_insert_with(|| {
                            "the archive index is unavailable, so a file that is not loose cannot be ruled out".to_string()
                        });
                    }
                },
                None => {
                    could_not_rule_out.get_or_insert_with(|| {
                        "the archive index is unavailable, so a file that is not loose cannot be ruled out".to_string()
                    });
                }
            };
        }

        if matches.is_empty() {
            if let Some(reason) = problem {
                return unknown(reason);
            }
            if let Some(reason) = could_not_rule_out {
                return unknown(reason);
            }
            return self.not_in_instance(role, value);
        }

        let candidates: Vec<Candidate> = matches
            .iter()
            .flat_map(|matched| {
                matched.positions.iter().map(move |&position| Candidate {
                    position,
                    pattern_index: matched.pattern_index,
                    group: matched.group.clone(),
                    via: matched.via.clone(),
                })
            })
            .collect();
        self.choose(candidates, role, value)
    }

    fn resolve_index(
        &self,
        index: &str,
        matching: NameMatch,
        value: &str,
        role: Option<&RoleRule>,
    ) -> Attribution {
        if !self.view.is_complete() {
            return unknown(
                self.view
                    .listing()
                    .reason()
                    .unwrap_or("the instance listing is unavailable"),
            );
        }
        let Some(normalized) = normalize_index_value(value) else {
            return unknown("the ref has an empty value");
        };
        let matched = self.view.index().items(index, &normalized, matching);
        if matched.is_empty() {
            return self.not_in_instance(role, value);
        }

        // A package index names the folder a class lives in. The most specific entry wins, because
        // a class belongs to its deepest declared package, not to every parent of it. When two
        // enabled items declare that same package, nothing in the instance says which one shipped
        // the class, so the answer is ambiguous rather than a guess.
        if matching == NameMatch::PrefixDot {
            let deepest = matched
                .iter()
                .map(|&(_, depth)| depth)
                .max()
                .expect("there was at least one match");
            let mut positions: Vec<usize> = matched
                .into_iter()
                .filter(|&(_, depth)| depth == deepest)
                .map(|(position, _)| position)
                .collect();
            positions.sort_unstable();
            positions.dedup();
            let enabled: Vec<usize> = positions
                .iter()
                .copied()
                .filter(|&position| self.view.layers()[position].enabled)
                .collect();
            if enabled.len() > 1 {
                return unknown(format!(
                    "ambiguous: {} enabled layers declare the package '{}'",
                    enabled.len(),
                    &normalized[..deepest]
                ));
            }
            let candidates: Vec<Candidate> = positions
                .into_iter()
                .map(|position| Candidate {
                    position,
                    pattern_index: 0,
                    group: index.to_string(),
                    via: index.to_string(),
                })
                .collect();
            return self.choose(candidates, role, value);
        }

        let positions = matched.into_iter().map(|(position, _)| position);
        let candidates: Vec<Candidate> = positions
            .map(|position| Candidate {
                position,
                pattern_index: 0,
                group: index.to_string(),
                via: index.to_string(),
            })
            .collect();
        self.choose(candidates, role, value)
    }

    /// Later position wins; a turned-off layer provides nothing. If every provider is turned off,
    /// name the latest one and say why instead of claiming the file is not in the instance.
    fn choose(
        &self,
        mut candidates: Vec<Candidate>,
        role: Option<&RoleRule>,
        value: &str,
    ) -> Attribution {
        if candidates.is_empty() {
            return self.not_in_instance(role, value);
        }
        let any_enabled = candidates
            .iter()
            .any(|candidate| self.view.layers()[candidate.position].enabled);
        if !any_enabled {
            candidates.sort_by(best_candidate);
            let winner = &candidates[0];
            let layer = &self.view.layers()[winner.position];
            return Attribution {
                owner: layer.owner.to_owner(),
                role: None,
                also_provided_by: Vec::new(),
                via: Some(winner.via.clone()),
                reason: Some("the only layer that provides this file is turned off".to_string()),
            };
        }

        candidates.retain(|candidate| self.view.layers()[candidate.position].enabled);
        candidates.sort_by(best_candidate);
        let winner = &candidates[0];
        let mut also: Vec<String> = candidates
            .iter()
            .filter(|candidate| {
                candidate.group == winner.group && candidate.position != winner.position
            })
            .map(|candidate| self.view.layers()[candidate.position].owner.label())
            .collect();
        also.sort();
        also.dedup();
        let layer = &self.view.layers()[winner.position];
        Attribution {
            owner: layer.owner.to_owner(),
            role: None,
            also_provided_by: also,
            via: Some(winner.via.clone()),
            reason: None,
        }
    }

    /// A listing that is complete and has no provider. A role can still tell us this is the system,
    /// the game itself or a framework; otherwise it is genuinely not in the instance. A framework
    /// is named after the ref itself, not the role's matching pattern: a prefix or wildcard rule
    /// would otherwise show its pattern to the user.
    fn not_in_instance(&self, role: Option<&RoleRule>, value: &str) -> Attribution {
        if let Some(rule) = role {
            match rule.role {
                Role::System => {
                    return Attribution {
                        owner: Owner::System,
                        role: None,
                        also_provided_by: Vec::new(),
                        via: None,
                        reason: Some("the pack's roles say this is the system".to_string()),
                    }
                }
                Role::Platform => {
                    return Attribution {
                        owner: Owner::Base,
                        role: None,
                        also_provided_by: Vec::new(),
                        via: None,
                        reason: Some("the pack's roles say this is the game itself".to_string()),
                    }
                }
                Role::Framework | Role::HookFramework => {
                    return Attribution {
                        owner: Owner::Framework {
                            name: value.to_string(),
                        },
                        role: None,
                        also_provided_by: Vec::new(),
                        via: None,
                        reason: Some("the pack's roles say this is a framework".to_string()),
                    }
                }
            }
        }
        Attribution {
            owner: Owner::NotInInstance,
            role: None,
            also_provided_by: Vec::new(),
            via: None,
            reason: Some("no layer in the instance provides this".to_string()),
        }
    }
}

impl RefResolver for Resolver {
    fn resolve(&self, reference: &Ref) -> Resolved {
        let attribution = self.attribute(reference);
        Resolved {
            owner: attribution.owner,
            role: attribution.role,
        }
    }

    fn item_files(&self, content_key: &str) -> Option<Vec<String>> {
        self.view.item_files(content_key)
    }
}

/// The resolver for a crash file the user handed Agora with no instance behind it.
///
/// There is nothing in an instance to blame, so a ref names itself: `ExampleMod.dll` resolves to
/// an owner whose display name is that value. A pack's `roles` still win, and apply exactly as they
/// do for an installed instance, because a system module or a framework is one wherever it is seen.
/// A `facts` ref map may still name the owner from another fact in the same file: a crash report's
/// own mod list turns a Java class into the mod that shipped it. A framework is named after the ref
/// itself, not the role's matching pattern, so a prefix or wildcard rule does not show its pattern
/// to the user. Inspectors have no file to open here; that is the caller's business, not the
/// resolver's.
pub struct LooseResolver {
    roles: Vec<RoleRule>,
    maps: Vec<LooseMap>,
}

struct LooseMap {
    map: RefMap,
    facts: Option<FactsIndex>,
}

impl LooseResolver {
    pub fn new(roles: Vec<RoleRule>, ref_maps: Vec<RefMap>, parsed: &[ParsedArtifact]) -> Self {
        let maps = ref_maps
            .into_iter()
            .map(|map| {
                let facts = matches!(&map.lookup, RefLookup::Facts { .. })
                    .then(|| FactsIndex::build(&map.lookup, parsed));
                LooseMap { map, facts }
            })
            .collect();
        Self { roles, maps }
    }

    /// Every role rule and ref map the given packs declare, in pack order.
    pub fn from_packs<'a>(
        packs: impl IntoIterator<Item = &'a CrashPack>,
        parsed: &[ParsedArtifact],
    ) -> Self {
        let packs: Vec<&CrashPack> = packs.into_iter().collect();
        let roles = packs
            .iter()
            .flat_map(|pack| pack.roles.iter().cloned())
            .collect();
        let maps = packs
            .iter()
            .flat_map(|pack| pack.ref_maps.iter().cloned())
            .collect();
        Self::new(roles, maps, parsed)
    }

    /// Everything this resolver knows about a ref, including why it settled on the name it did.
    pub fn attribute(&self, reference: &Ref) -> Attribution {
        let value = reference.value.trim();
        if value.is_empty() {
            return unknown("the ref has an empty value");
        }

        if let Some(rule) = self.roles.iter().find(|rule| {
            rule.kind == reference.kind && matches_value(rule.matching, value, &rule.value)
        }) {
            let owner = match rule.role {
                Role::System => Owner::System,
                Role::Platform => Owner::Base,
                Role::Framework | Role::HookFramework => Owner::Framework {
                    name: value.to_string(),
                },
            };
            return Attribution {
                owner,
                role: Some(rule.role),
                also_provided_by: Vec::new(),
                via: None,
                reason: None,
            };
        }

        let mut ambiguous = None;
        for entry in &self.maps {
            if entry.map.kind != reference.kind {
                continue;
            }
            let Some(index) = &entry.facts else {
                continue;
            };
            match index.find(value, &entry.map.ignore) {
                FactMatch::Found(found) => {
                    return Attribution {
                        owner: Owner::Item {
                            key: found.key.clone(),
                            name: found.name.clone(),
                        },
                        role: None,
                        also_provided_by: Vec::new(),
                        via: None,
                        reason: None,
                    }
                }
                FactMatch::Ambiguous => {
                    ambiguous = Some(format!(
                        "ambiguous: the ref names more than one key in the pack's `{}` facts",
                        entry.map.kind
                    ));
                }
                FactMatch::Missing => {}
            }
        }
        Attribution {
            owner: Owner::Item {
                key: value.to_string(),
                name: value.to_string(),
            },
            role: None,
            also_provided_by: Vec::new(),
            via: None,
            reason: ambiguous,
        }
    }
}

impl RefResolver for LooseResolver {
    fn resolve(&self, reference: &Ref) -> Resolved {
        let attribution = self.attribute(reference);
        Resolved {
            owner: attribution.owner,
            role: attribution.role,
        }
    }
}

/// One displayable key a `facts` ref map found, prepared for `segment` matching.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FactValue {
    key: String,
    normalized: String,
    name: String,
}

/// The attribute values a pack's `facts` lookup indexes, built once from one analysis.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct FactsIndex {
    values: Vec<FactValue>,
}

impl FactsIndex {
    fn build(lookup: &RefLookup, parsed: &[ParsedArtifact]) -> Self {
        let RefLookup::Facts { kind, key, name } = lookup else {
            return Self::default();
        };
        let mut values = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for artifact in parsed {
            for fact in &artifact.output.facts {
                if &fact.kind != kind {
                    continue;
                }
                let Some(key_value) = fact_text(fact, key) else {
                    continue;
                };
                let key_value = key_value.trim();
                if key_value.is_empty() {
                    continue;
                }
                let name_value = fact_text(fact, name)
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .unwrap_or(key_value)
                    .to_string();
                if seen.insert((key_value.to_string(), name_value.clone())) {
                    values.push(FactValue {
                        key: key_value.to_string(),
                        normalized: normalize_segment(key_value),
                        name: name_value,
                    });
                }
            }
        }
        Self { values }
    }

    fn find(&self, value: &str, ignore: &[String]) -> FactMatch<'_> {
        let ignored: Vec<String> = ignore.iter().map(|word| normalize_segment(word)).collect();
        let segments = class_segments(value);
        let matches: Vec<&FactValue> = self
            .values
            .iter()
            .filter(|entry| {
                entry.normalized.len() >= 3
                    && !ignored.iter().any(|word| word == &entry.normalized)
                    && segments.contains(&entry.normalized)
            })
            .collect();
        if matches.is_empty() {
            return FactMatch::Missing;
        }
        // The longest key wins. Two different keys of the same length are ambiguous, and an
        // ambiguous answer is no answer rather than a guess.
        let longest_len = matches
            .iter()
            .map(|entry| entry.normalized.len())
            .max()
            .unwrap_or(0);
        let longest: Vec<&FactValue> = matches
            .into_iter()
            .filter(|entry| entry.normalized.len() == longest_len)
            .collect();
        if longest
            .iter()
            .any(|entry| entry.normalized != longest[0].normalized)
        {
            return FactMatch::Ambiguous;
        }
        FactMatch::Found(longest[0])
    }
}

enum FactMatch<'a> {
    Found(&'a FactValue),
    Ambiguous,
    Missing,
}

fn fact_text<'a>(fact: &'a Fact, attr: &str) -> Option<&'a str> {
    match fact.attrs.get(attr) {
        Some(AttrValue::Scalar(Scalar::Text(text))) => Some(text),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PathMatch {
    pattern_index: usize,
    /// What counts as the same file for `also_provided_by`.
    group: String,
    via: String,
    positions: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    position: usize,
    pattern_index: usize,
    group: String,
    via: String,
}

/// The winner sorts first: the latest layer, then the earliest pattern, then the smaller path.
/// This makes the answer independent of the order the layers happened to list their files in.
fn best_candidate(a: &Candidate, b: &Candidate) -> std::cmp::Ordering {
    b.position
        .cmp(&a.position)
        .then_with(|| a.pattern_index.cmp(&b.pattern_index))
        .then_with(|| a.via.cmp(&b.via))
}

fn unknown(reason: impl Into<String>) -> Attribution {
    Attribution {
        owner: Owner::Unknown,
        role: None,
        also_provided_by: Vec::new(),
        via: None,
        reason: Some(reason.into()),
    }
}
