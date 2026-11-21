//! The scorer's numbers live here, in one value, so a game can tune them in one place.

use agora_game_api::crash::Family;

/// One level's bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Threshold {
    pub core_min: u32,
    /// How many independent evidence groups the top candidate needs.
    pub groups_min: u8,
    /// How far ahead of the runner-up the top candidate must be.
    pub lead_min: u32,
}

/// Every number consensus uses. The defaults are the design's starting profile, to be measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsensusProfile {
    pub high: Threshold,
    pub medium: Threshold,
    /// Below this core score Agora names nobody.
    pub abstain_below: u32,
    /// A hook framework keeps a quarter of its score (out of 1000).
    pub hook_framework_demote_permille: u32,
    /// A framework keeps a quarter of its score (out of 1000).
    pub framework_demote_permille: u32,
}

impl Default for ConsensusProfile {
    fn default() -> Self {
        Self {
            high: Threshold {
                core_min: 14,
                groups_min: 2,
                lead_min: 6,
            },
            medium: Threshold {
                core_min: 8,
                groups_min: 1,
                lead_min: 3,
            },
            abstain_below: 4,
            hook_framework_demote_permille: 250,
            framework_demote_permille: 250,
        }
    }
}

/// The independent group a family belongs to. Two families in one group are not independent.
pub(crate) fn independent_group(family: Family) -> Option<&'static str> {
    match family {
        Family::DecisiveVerdict => Some("loader"),
        Family::VerifiedAsset => Some("inspection"),
        Family::KnownSignature | Family::CuratedConflict => Some("knowledge"),
        Family::Execution => Some("execution"),
        Family::ObjectReference => Some("data"),
        Family::ResourceProvider
        | Family::ChangeCorrelation
        | Family::HistoryRepeat
        | Family::LocalConfirmation
        | Family::CoCrash
        | Family::NovelLogLine => None,
    }
}
