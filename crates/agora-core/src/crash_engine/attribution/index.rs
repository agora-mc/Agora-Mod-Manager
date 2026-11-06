//! The file index inside an [`InstanceView`](super::InstanceView), built once so resolving a ref
//! never scans every file.

use std::collections::HashMap;

use agora_game_api::crash::NameMatch;
use agora_game_api::fold_path_case;

use super::view::LayerView;

/// Where files inside archives come from. Parsing an archive is a Creation Engine plugin's job, so
/// the evaluator takes an index through this trait and never opens one itself.
pub trait ArchiveIndex {
    /// The game-relative archive files whose contents include `path`, or `None` when the index
    /// could not be read. An empty vector means the index was read and no archive holds `path`.
    fn archives_containing(&self, path: &str) -> Option<Vec<String>>;
}

/// Every file and named item value the instance provides, by the key each match mode compares.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FileIndex {
    /// Paths compared exactly, after slash and Windows-name normalisation.
    exact: HashMap<String, Vec<usize>>,
    /// Paths compared with the NTFS case fold.
    folded: HashMap<String, Vec<usize>>,
    /// Named item values, keyed by `index\0value`.
    index_exact: HashMap<String, Vec<usize>>,
    index_folded: HashMap<String, Vec<usize>>,
}

impl FileIndex {
    pub(crate) fn build(layers: &[LayerView]) -> Self {
        let mut this = Self::default();
        for (position, layer) in layers.iter().enumerate() {
            let mut seen = std::collections::HashSet::new();
            for file in &layer.files {
                let Some(key) = normalize_path(file) else {
                    continue;
                };
                if !seen.insert(key.clone()) {
                    continue;
                }
                this.exact.entry(key.clone()).or_default().push(position);
                this.folded
                    .entry(fold_path_case(&key))
                    .or_default()
                    .push(position);
            }
            for (name, values) in &layer.indexes {
                let mut seen = std::collections::HashSet::new();
                for value in values {
                    let Some(value) = normalize_index_value(value) else {
                        continue;
                    };
                    if !seen.insert(value.clone()) {
                        continue;
                    }
                    this.index_exact
                        .entry(join_index(name, &value))
                        .or_default()
                        .push(position);
                    this.index_folded
                        .entry(join_index(name, &fold_path_case(&value)))
                        .or_default()
                        .push(position);
                }
            }
        }
        this
    }

    /// The layer positions that provide the path `key`, lowest first.
    pub(crate) fn files(&self, key: &str, matching: NameMatch) -> Vec<usize> {
        match matching {
            NameMatch::Exact => self.exact.get(key).cloned().unwrap_or_default(),
            NameMatch::Casefold => self
                .folded
                .get(&fold_path_case(key))
                .cloned()
                .unwrap_or_default(),
            NameMatch::PrefixDot => dot_prefix_matches(&self.exact, key),
        }
    }

    /// The layer positions whose item declares a value in `index` matching `value`.
    pub(crate) fn items(&self, index: &str, value: &str, matching: NameMatch) -> Vec<usize> {
        match matching {
            NameMatch::Exact => self
                .index_exact
                .get(&join_index(index, value))
                .cloned()
                .unwrap_or_default(),
            NameMatch::Casefold => self
                .index_folded
                .get(&join_index(index, &fold_path_case(value)))
                .cloned()
                .unwrap_or_default(),
            NameMatch::PrefixDot => {
                let mut positions = Vec::new();
                let mut end = Some(value.len());
                while let Some(at) = end {
                    if let Some(found) = self.index_exact.get(&join_index(index, &value[..at])) {
                        positions.extend(found.iter().copied());
                    }
                    end = value[..at].rfind('.');
                }
                positions
            }
        }
    }
}

/// A key that cannot collide: the index name, a NUL that no validated name holds, and the value.
fn join_index(index: &str, value: &str) -> String {
    let mut key = String::with_capacity(index.len() + 1 + value.len());
    key.push_str(index);
    key.push('\0');
    key.push_str(value);
    key
}

/// Look up the value and every dot-boundary prefix of it, so a `prefix_dot` entry matches a longer
/// name without scanning the index.
fn dot_prefix_matches(map: &HashMap<String, Vec<usize>>, value: &str) -> Vec<usize> {
    let mut positions = Vec::new();
    let mut end = Some(value.len());
    while let Some(at) = end {
        if let Some(found) = map.get(&value[..at]) {
            positions.extend(found.iter().copied());
        }
        end = value[..at].rfind('.');
    }
    positions
}

/// Whether a role rule's `value` matches a ref's value under `matching`.
pub(crate) fn matches_value(matching: NameMatch, value: &str, rule: &str) -> bool {
    match matching {
        NameMatch::Exact => value == rule,
        NameMatch::Casefold => fold_path_case(value) == fold_path_case(rule),
        NameMatch::PrefixDot => {
            value == rule
                || value
                    .strip_prefix(rule)
                    .is_some_and(|rest| rest.starts_with('.'))
        }
    }
}

/// A path as the index stores it: `/` separators, no `..`, no absolute path, and Windows' dropped
/// trailing dots and spaces from every name. `None` means the text is not a path inside an
/// instance, so it can only be `Unknown`.
pub(crate) fn normalize_path(path: &str) -> Option<String> {
    let path = path.trim();
    if path.is_empty() {
        return None;
    }
    let slashed = path.replace('\\', "/");
    if slashed.starts_with('/') || slashed.contains(':') {
        return None;
    }
    let mut parts = Vec::new();
    for part in slashed.split('/') {
        if part == ".." {
            return None;
        }
        if part == "." {
            continue;
        }
        // Windows drops trailing dots and spaces from a name, so `Skyrim.` is `Skyrim`.
        let part = part.trim_end_matches(['.', ' ']);
        if part.is_empty() {
            continue;
        }
        parts.push(part);
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

/// A named item value (a mod id, a package) with only surrounding whitespace removed.
pub(crate) fn normalize_index_value(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}
