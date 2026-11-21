//! The error every `games_*` command returns.
//!
//! `code` is a stable string the frontend may branch on, so a code is added here as a
//! constant rather than typed ad hoc at a call site. The message is for people.

use serde::Serialize;

/// A command that is registered but not built yet. Slices replace these.
pub const NOT_IMPLEMENTED: &str = "not_implemented";
/// agora-core refused the request; `core_code` carries its `ERR_...` code.
pub const CORE_FAILED: &str = "core_failed";
/// The blocking worker that did the work panicked or was cancelled.
pub const TASK_FAILED: &str = "task_failed";
/// An MO2 import found no install of the setup's game anywhere; the user adds one in Settings.
pub const NO_INSTALL: &str = "no_install";
/// Both a game file and the instance copy changed: the only conflict an explicit restore may
/// override. The frontend branches on this code, never on the message.
pub const USER_FILES_CONFLICT: &str = "user_files_conflict";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GamesError {
    pub code: String,
    pub message: String,
    /// The agora-core code (`ERR_...`) when the failure came from core.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub core_code: Option<String>,
    /// Extra facts a page needs to show the right words, such as a placement note.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

impl GamesError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
            core_code: None,
            details: None,
        }
    }

    /// Attach the facts a page needs beyond the message.
    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }

    /// Attach the stable agora-core code, when the failure came from core.
    pub fn with_core_code(mut self, code: impl Into<String>) -> Self {
        self.core_code = Some(code.into());
        self
    }

    /// The failure a stub returns. Names the command so a caller can see which one.
    pub fn not_implemented(command: &str) -> Self {
        Self::new(
            NOT_IMPLEMENTED,
            format!("{command} is not implemented yet."),
        )
    }
}

impl From<agora_core::error::LauncherError> for GamesError {
    fn from(error: agora_core::error::LauncherError) -> Self {
        Self {
            code: CORE_FAILED.to_string(),
            message: error.to_string(),
            core_code: Some(error.code()),
            details: None,
        }
    }
}
