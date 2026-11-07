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

/// One file a layer provides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileRecord {
    /// The layer that provides the file.
    pub(crate) position: usize,
    /// The game-relative path, normalised.
    pub(crate) path: String,
}

/// Every file and named item value the instance provides, by the key each match mode compares.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FileIndex {
    records: Vec<FileRecord>,
    /// Full paths compared exactly, after slash and Windows-name normalisation.
    exact: HashMap<String, Vec<usize>>,
    /// Full paths compared with the NTFS case fold.
    folded: HashMap<String, Vec<usize>>,
    /// Files by their final path segment, so a wildcard pattern only looks at files with the same
    /// name before comparing folders. Values are record indices.
    by_name_exact: HashMap<String, Vec<usize>>,
    by_name_folded: HashMap<String, Vec<usize>>,
    /// Named item values, keyed by `index\0value`. Values are layer positions.
    index_exact: HashMap<String, Vec<usize>>,
    index_folded: HashMap<String, Vec<usize>>,
}

impl FileIndex {
    pub(crate) fn build(layers: &[LayerView]) -> Self {
        let mut this = Self::default();
        for (position, layer) in layers.iter().enumerate() {
            let mut seen = std::collections::HashSet::new();
            for file in &layer.files {
                let Some(path) = normalize_path(file) else {
                    continue;
                };
                if !seen.insert(path.clone()) {
                    continue;
                }
                let record = this.records.len();
                this.records.push(FileRecord {
                    position,
                    path: path.clone(),
                });
                this.exact.entry(path.clone()).or_default().push(record);
                this.folded
                    .entry(fold_path_case(&path))
                    .or_default()
                    .push(record);
                let name = final_segment(&path);
                this.by_name_exact
                    .entry(name.to_string())
                    .or_default()
                    .push(record);
                this.by_name_folded
                    .entry(fold_path_case(name))
                    .or_default()
                    .push(record);
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

    pub(crate) fn record(&self, record: usize) -> &FileRecord {
        &self.records[record]
    }

    /// The records that provide the full path `key`, lowest layer first.
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

    /// The records whose final path segment is `name`, lowest layer first. A wildcard pattern then
    /// compares only these candidates' folders.
    pub(crate) fn files_by_final_name(&self, name: &str, matching: NameMatch) -> Vec<usize> {
        match matching {
            NameMatch::Exact => self.by_name_exact.get(name).cloned().unwrap_or_default(),
            NameMatch::Casefold => self
                .by_name_folded
                .get(&fold_path_case(name))
                .cloned()
                .unwrap_or_default(),
            NameMatch::PrefixDot => dot_prefix_matches(&self.by_name_exact, name),
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

/// One segment of an expanded lookup pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Segment {
    Literal(String),
    /// A pattern's whole `*` segment. It matches exactly one path segment.
    Wildcard,
}

/// An expanded pattern, ready to look up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PatternPlan {
    pub(crate) segments: Vec<Segment>,
    pub(crate) final_name: String,
    pub(crate) has_wildcard: bool,
}

impl PatternPlan {
    /// The full path to look up when there is no wildcard.
    pub(crate) fn full_key(&self) -> Option<String> {
        let mut parts = Vec::with_capacity(self.segments.len());
        for segment in &self.segments {
            match segment {
                Segment::Literal(literal) => parts.push(literal.as_str()),
                Segment::Wildcard => return None,
            }
        }
        Some(parts.join("/"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PatternError {
    /// The pattern uses `*` in a way the contract does not allow.
    Unsupported(String),
    /// The value does not name a path inside the instance.
    Invalid(String),
}

/// Expand a path map's pattern with a ref value. A template segment that is exactly `*` matches one
/// folder; any other `*` is refused. A `*` that arrives in the value is literal.
pub(crate) fn expand_pattern(pattern: &str, value: &str) -> Result<PatternPlan, PatternError> {
    let template = pattern.replace('\\', "/");
    if template.starts_with('/') || template.contains(':') || !value_is_inside(value) {
        return Err(PatternError::Invalid(
            "the ref value is not a path inside the instance".to_string(),
        ));
    }

    let mut segments = Vec::new();
    let mut has_wildcard = false;
    for segment in template.split('/') {
        if segment == "*" {
            has_wildcard = true;
            segments.push(Segment::Wildcard);
            continue;
        }
        if segment.contains('*') {
            return Err(PatternError::Unsupported(
                "`*` must be a whole path segment; `**` and `foo*.dll` are not supported"
                    .to_string(),
            ));
        }
        if segment.contains(':') {
            return Err(PatternError::Invalid(
                "the ref value is not a path inside the instance".to_string(),
            ));
        }
        let expanded = segment.replace("{value}", value);
        let expanded = expanded.replace('\\', "/");
        if expanded.starts_with('/') || expanded.contains(':') {
            return Err(PatternError::Invalid(
                "the ref value is not a path inside the instance".to_string(),
            ));
        }
        for part in expanded.split('/') {
            if part == ".." {
                return Err(PatternError::Invalid(
                    "the ref value is not a path inside the instance".to_string(),
                ));
            }
            if part == "." {
                continue;
            }
            // Windows drops trailing dots and spaces from a name, so `Skyrim.` is `Skyrim`.
            let part = part.trim_end_matches(['.', ' ']);
            if part.is_empty() {
                continue;
            }
            segments.push(Segment::Literal(part.to_string()));
        }
    }

    let final_name = match segments.last() {
        Some(Segment::Literal(name)) => name.clone(),
        Some(Segment::Wildcard) => {
            return Err(PatternError::Unsupported(
                "`*` must stand for one folder; the pattern must end with the file name"
                    .to_string(),
            ))
        }
        None => {
            return Err(PatternError::Invalid(
                "the ref value is not a path inside the instance".to_string(),
            ))
        }
    };
    Ok(PatternPlan {
        segments,
        final_name,
        has_wildcard,
    })
}

/// Whether an expanded pattern matches a file path. The last segments already matched by the
/// final-name index, but comparing the whole path here is what keeps `*` from crossing a `/`.
pub(crate) fn matches_segments(segments: &[Segment], path: &str, matching: NameMatch) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() != segments.len() {
        return false;
    }
    segments
        .iter()
        .zip(parts)
        .all(|(segment, part)| match segment {
            Segment::Wildcard => !part.is_empty(),
            Segment::Literal(literal) => matches_value(matching, part, literal),
        })
}

fn value_is_inside(value: &str) -> bool {
    let slashed = value.replace('\\', "/");
    !slashed.starts_with('/')
        && !slashed.contains(':')
        && !slashed.split('/').any(|part| part == "..")
}

fn final_segment(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
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
    let mut matches = Vec::new();
    let mut end = Some(value.len());
    while let Some(at) = end {
        if let Some(found) = map.get(&value[..at]) {
            matches.extend(found.iter().copied());
        }
        end = value[..at].rfind('.');
    }
    matches
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
