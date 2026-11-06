//! Answers "who owns this ref, and what role has it" from an instance snapshot and a crash pack.

use agora_game_api::crash::{CrashPack, NameMatch, Owner, Ref, RefLookup, RefMap, Role, RoleRule};

use super::index::{matches_value, normalize_index_value, normalize_path, ArchiveIndex};
use super::view::InstanceView;
use crate::crash_engine::types::{RefResolver, Resolved};

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

        let Some(map) = self.ref_maps.iter().find(|map| map.kind == reference.kind) else {
            return Attribution {
                owner: Owner::Unknown,
                role,
                also_provided_by: Vec::new(),
                via: None,
                reason: Some(format!("no ref map for kind '{}'", reference.kind)),
            };
        };

        if value.is_empty() {
            return Attribution {
                owner: Owner::Unknown,
                role,
                also_provided_by: Vec::new(),
                via: None,
                reason: Some("the ref has an empty value".to_string()),
            };
        }

        let mut answer = match &map.lookup {
            RefLookup::Path { patterns, archives } => {
                self.resolve_path(map.matching, patterns, *archives, value, matched_role)
            }
            RefLookup::Index(index) => self.resolve_index(index, map.matching, value, matched_role),
        };
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
        let mut saw_key = false;
        let mut archive_unknown = false;
        for (pattern_index, pattern) in patterns.iter().enumerate() {
            let expanded = pattern.replace("{value}", value);
            let Some(key) = normalize_path(&expanded) else {
                continue;
            };
            saw_key = true;
            let positions = self.view.index().files(&key, matching);
            if !positions.is_empty() {
                matches.push(PathMatch {
                    pattern_index,
                    via: key.clone(),
                    positions,
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
                            let positions = self.view.index().files(&archive_key, matching);
                            if !positions.is_empty() {
                                matches.push(PathMatch {
                                    pattern_index,
                                    via: archive_key,
                                    positions,
                                });
                            }
                        }
                    }
                    None => archive_unknown = true,
                },
                None => archive_unknown = true,
            }
        }

        if matches.is_empty() {
            if !saw_key {
                return unknown("the ref value is not a path inside the instance");
            }
            if archive_unknown {
                return unknown(
                    "the archive index is unavailable, so a file that is not loose cannot be ruled out",
                );
            }
            return self.not_in_instance(role);
        }

        let candidates: Vec<Candidate> = matches
            .iter()
            .flat_map(|matched| {
                matched.positions.iter().map(move |&position| Candidate {
                    position,
                    pattern_index: matched.pattern_index,
                    via: matched.via.clone(),
                })
            })
            .collect();
        self.choose(candidates, role)
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
        let Some(value) = normalize_index_value(value) else {
            return unknown("the ref has an empty value");
        };
        let positions = self.view.index().items(index, &value, matching);
        if positions.is_empty() {
            return self.not_in_instance(role);
        }
        let candidates: Vec<Candidate> = positions
            .into_iter()
            .map(|position| Candidate {
                position,
                pattern_index: 0,
                via: index.to_string(),
            })
            .collect();
        self.choose(candidates, role)
    }

    /// Later position wins; a turned-off layer provides nothing. If every provider is turned off,
    /// name the latest one and say why instead of claiming the file is not in the instance.
    fn choose(&self, mut candidates: Vec<Candidate>, role: Option<&RoleRule>) -> Attribution {
        if candidates.is_empty() {
            return self.not_in_instance(role);
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
                candidate.via == winner.via && candidate.position != winner.position
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
    /// the game itself or a framework; otherwise it is genuinely not in the instance.
    fn not_in_instance(&self, role: Option<&RoleRule>) -> Attribution {
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
                            name: rule.value.clone(),
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PathMatch {
    pattern_index: usize,
    via: String,
    positions: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    position: usize,
    pattern_index: usize,
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
