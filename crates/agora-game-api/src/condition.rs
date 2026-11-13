//! One condition language for everything a package decides at run time (MASTER_SPEC §30.12).
//!
//! A condition asks about the instance: whether a path is in its merged view (`present`), which
//! version of a framework is installed (`framework`), or the game's own version (`version`). It
//! combines those with `all`, `any` and `not`. Every answer may be unknown: absent is not false,
//! and a check nobody could make stays unknown rather than falling to a no.
//!
//! The truth type and version gate live here rather than in the crash contract so every caller
//! shares them. Crash rules keep their own, richer `Condition`, which asks about facts instead of
//! an instance.

use crate::{FrameworkId, RelPath};
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// Three-valued logic (Kleene).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Truth {
    True,
    False,
    Unknown,
}

impl Truth {
    pub fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::False, _) | (_, Self::False) => Self::False,
            (Self::True, Self::True) => Self::True,
            _ => Self::Unknown,
        }
    }

    pub fn or(self, other: Self) -> Self {
        match (self, other) {
            (Self::True, _) | (_, Self::True) => Self::True,
            (Self::False, Self::False) => Self::False,
            _ => Self::Unknown,
        }
    }

    pub fn negate(self) -> Self {
        match self {
            Self::True => Self::False,
            Self::False => Self::True,
            Self::Unknown => Self::Unknown,
        }
    }
}

impl From<bool> for Truth {
    fn from(value: bool) -> Self {
        if value {
            Self::True
        } else {
            Self::False
        }
    }
}

/// A version check that works on game versions, which are not semver (`1.2.3.4`, a snapshot name).
/// Either `exact` names versions, or `min` (inclusive) and `below` (exclusive) bound a range of
/// dotted numbers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VersionGate {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exact: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub below: Option<String>,
}

impl VersionGate {
    /// Whether `version` passes. `None` means it can't be judged: a range was asked of a version
    /// that isn't dotted numbers, which must read as could-not-check rather than as a no.
    pub fn matches(&self, version: &str) -> Option<bool> {
        if !self.exact.is_empty() {
            return Some(self.exact.iter().any(|v| v == version));
        }
        let version = dotted(version)?;
        let above_min = match &self.min {
            Some(min) => compare_dotted(&version, &dotted(min)?).is_ge(),
            None => true,
        };
        let under_below = match &self.below {
            Some(below) => compare_dotted(&version, &dotted(below)?).is_lt(),
            None => true,
        };
        Some(above_min && under_below)
    }
}

pub(crate) fn dotted(version: &str) -> Option<Vec<u64>> {
    version.split('.').map(|p| p.parse().ok()).collect()
}

pub(crate) fn compare_dotted(a: &[u64], b: &[u64]) -> std::cmp::Ordering {
    let at = |v: &[u64], i: usize| v.get(i).copied().unwrap_or(0);
    (0..a.len().max(b.len()))
        .map(|i| at(a, i).cmp(&at(b, i)))
        .find(|o| o.is_ne())
        .unwrap_or(std::cmp::Ordering::Equal)
}

/// What a condition can ask of an instance. Each answer may be unknown, and unknown is not false.
pub trait ConditionFacts {
    /// Whether `path` exists in the instance's merged view. `None` when it cannot be told.
    fn present(&self, path: &RelPath) -> Option<bool>;

    /// The installed version of `framework`.
    ///
    /// The outer `None` means the fact cannot be told. `Some(None)` means the framework is known
    /// not to be installed, while `Some(Some(version))` is its installed version.
    fn framework(&self, framework: &FrameworkId) -> Option<Option<String>>;

    /// The game's own version. `None` when it cannot be told.
    fn game_version(&self) -> Option<String>;
}

/// When something applies. Exactly one key is written: `{ "present": "..." }`,
/// `{ "framework": "red4ext", "version": { ... } }`, `{ "version": { ... } }`,
/// `{ "all": [ ... ] }`, `{ "any": [ ... ] }` or `{ "not": { ... } }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Condition {
    Present(RelPath),
    Framework {
        framework: FrameworkId,
        version: Option<VersionGate>,
    },
    Version(VersionGate),
    All(Vec<Condition>),
    Any(Vec<Condition>),
    Not(Box<Condition>),
}

impl Condition {
    /// The condition in words a player can read, for a launch log or a check page.
    pub fn describe(&self) -> String {
        fn gate(version: &VersionGate) -> String {
            if !version.exact.is_empty() {
                return version.exact.join(" or ").to_string();
            }
            match (&version.min, &version.below) {
                (Some(min), Some(below)) => format!("at least {min} and below {below}"),
                (Some(min), None) => format!("at least {min}"),
                (None, Some(below)) => format!("below {below}"),
                (None, None) => "any version".to_string(),
            }
        }
        match self {
            Self::Present(path) => format!("`{}` is present", path.as_str()),
            Self::Framework { framework, version } => match version {
                Some(version) => format!("framework `{framework}` is {}", gate(version)),
                None => format!("framework `{framework}` is installed"),
            },
            Self::Version(version) => format!("the game version is {}", gate(version)),
            Self::All(conditions) => format!(
                "all of ({})",
                conditions
                    .iter()
                    .map(Self::describe)
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            Self::Any(conditions) => format!(
                "any of ({})",
                conditions
                    .iter()
                    .map(Self::describe)
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            Self::Not(condition) => format!("not ({})", condition.describe()),
        }
    }

    /// Evaluate against an instance. Unknown answers stay unknown; `all` of nothing is true and
    /// `any` of nothing is false, as in ordinary logic.
    pub fn evaluate(&self, facts: &dyn ConditionFacts) -> Truth {
        match self {
            Self::Present(path) => facts
                .present(path)
                .map(Truth::from)
                .unwrap_or(Truth::Unknown),
            Self::Framework { framework, version } => match facts.framework(framework) {
                None => Truth::Unknown,
                Some(None) => Truth::False,
                Some(Some(installed)) => match version {
                    None => Truth::True,
                    Some(gate) => gate
                        .matches(&installed)
                        .map(Truth::from)
                        .unwrap_or(Truth::Unknown),
                },
            },
            Self::Version(gate) => match facts.game_version() {
                None => Truth::Unknown,
                Some(version) => gate
                    .matches(&version)
                    .map(Truth::from)
                    .unwrap_or(Truth::Unknown),
            },
            Self::All(conditions) => conditions.iter().fold(Truth::True, |truth, condition| {
                truth.and(condition.evaluate(facts))
            }),
            Self::Any(conditions) => conditions.iter().fold(Truth::False, |truth, condition| {
                truth.or(condition.evaluate(facts))
            }),
            Self::Not(condition) => condition.evaluate(facts).negate(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConditionRepr {
    #[serde(default)]
    present: Option<RelPath>,
    #[serde(default)]
    framework: Option<FrameworkId>,
    #[serde(default)]
    version: Option<VersionGate>,
    #[serde(default)]
    all: Option<Vec<Condition>>,
    #[serde(default)]
    any: Option<Vec<Condition>>,
    #[serde(default)]
    not: Option<Box<Condition>>,
}

impl<'de> Deserialize<'de> for Condition {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let repr = ConditionRepr::deserialize(deserializer)?;
        match (
            repr.present,
            repr.framework,
            repr.version,
            repr.all,
            repr.any,
            repr.not,
        ) {
            (Some(path), None, None, None, None, None) => Ok(Self::Present(path)),
            (None, Some(framework), version, None, None, None) => {
                Ok(Self::Framework { framework, version })
            }
            (None, None, Some(version), None, None, None) => Ok(Self::Version(version)),
            (None, None, None, Some(all), None, None) => Ok(Self::All(all)),
            (None, None, None, None, Some(any), None) => Ok(Self::Any(any)),
            (None, None, None, None, None, Some(not)) => Ok(Self::Not(not)),
            _ => Err(de::Error::custom(
                "a condition must say exactly one of present, framework, version, all, any or not",
            )),
        }
    }
}

impl Serialize for Condition {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::SerializeMap;

        match self {
            Self::Present(path) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("present", path)?;
                map.end()
            }
            Self::Framework { framework, version } => {
                let mut map =
                    serializer.serialize_map(Some(if version.is_some() { 2 } else { 1 }))?;
                map.serialize_entry("framework", framework)?;
                if let Some(version) = version {
                    map.serialize_entry("version", version)?;
                }
                map.end()
            }
            Self::Version(version) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("version", version)?;
                map.end()
            }
            Self::All(all) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("all", all)?;
                map.end()
            }
            Self::Any(any) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("any", any)?;
                map.end()
            }
            Self::Not(not) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("not", not)?;
                map.end()
            }
        }
    }
}

impl fmt::Display for Truth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::True => f.write_str("true"),
            Self::False => f.write_str("false"),
            Self::Unknown => f.write_str("unknown"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Facts {
        present: Option<bool>,
        framework: Option<Option<String>>,
        game_version: Option<String>,
    }

    impl ConditionFacts for Facts {
        fn present(&self, _path: &RelPath) -> Option<bool> {
            self.present
        }

        fn framework(&self, _framework: &FrameworkId) -> Option<Option<String>> {
            self.framework.clone()
        }

        fn game_version(&self) -> Option<String> {
            self.game_version.clone()
        }
    }

    fn facts() -> Facts {
        Facts {
            present: Some(true),
            framework: Some(Some("1.20.0".into())),
            game_version: Some("2.0.0".into()),
        }
    }

    fn present(path: &str) -> Condition {
        Condition::Present(RelPath::new(path).unwrap())
    }

    #[test]
    fn an_unknown_condition_key_fails_to_parse() {
        let result = serde_json::from_str::<Condition>(r#"{"presentt":"r6/scripts/*"}"#);
        assert!(result.is_err(), "{result:?}");
    }

    #[test]
    fn present_on_an_unknown_answer_is_unknown_not_false() {
        let mut facts = facts();
        facts.present = None;
        assert_eq!(present("x").evaluate(&facts), Truth::Unknown);
    }

    #[test]
    fn not_of_unknown_is_unknown() {
        let mut facts = facts();
        facts.present = None;
        let condition = Condition::Not(Box::new(present("x")));
        assert_eq!(condition.evaluate(&facts), Truth::Unknown);
    }

    #[test]
    fn all_with_a_false_and_an_unknown_is_false() {
        let mut facts = facts();
        facts.present = None;
        let condition = Condition::All(vec![
            Condition::Version(VersionGate {
                exact: vec!["1.0".into()],
                ..Default::default()
            }),
            present("x"),
        ]);
        assert_eq!(condition.evaluate(&facts), Truth::False);
    }

    #[test]
    fn any_with_a_true_and_an_unknown_is_true() {
        let mut facts = facts();
        facts.present = None;
        let condition = Condition::Any(vec![
            Condition::Version(VersionGate {
                exact: vec!["2.0.0".into()],
                ..Default::default()
            }),
            present("x"),
        ]);
        assert_eq!(condition.evaluate(&facts), Truth::True);
    }

    #[test]
    fn empty_all_is_true_and_empty_any_is_false() {
        assert_eq!(Condition::All(vec![]).evaluate(&facts()), Truth::True);
        assert_eq!(Condition::Any(vec![]).evaluate(&facts()), Truth::False);
    }

    #[test]
    fn framework_absent_is_false_and_undetectable_is_unknown() {
        let mut facts = facts();
        facts.framework = Some(None);
        let condition = Condition::Framework {
            framework: FrameworkId::new("red4ext").unwrap(),
            version: None,
        };
        assert_eq!(condition.evaluate(&facts), Truth::False);
        facts.framework = None;
        assert_eq!(condition.evaluate(&facts), Truth::Unknown);
    }

    #[test]
    fn conditions_round_trip_as_one_key() {
        let conditions = vec![
            present("r6/scripts/*"),
            Condition::Framework {
                framework: FrameworkId::new("red4ext").unwrap(),
                version: Some(VersionGate {
                    min: Some("1.20".into()),
                    ..Default::default()
                }),
            },
            Condition::Version(VersionGate {
                below: Some("2.0".into()),
                ..Default::default()
            }),
        ];
        for condition in conditions {
            let json = serde_json::to_string(&condition).unwrap();
            let parsed: Condition = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, condition, "{json}");
        }
    }
}
