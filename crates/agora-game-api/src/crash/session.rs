//! How a session ended. The judging lives in `agora_core::session_ledger`; these are the words for
//! it, here because an answer and an artifact carry them.

use serde::{Deserialize, Serialize};

/// How an artifact (a crash log) was tied to the session. Strongest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchedBy {
    /// The game's exit code was an NTSTATUS failure code and no log was found.
    ExitStatus,
    /// The log names the process id Agora held.
    Pid,
    /// The log names this instance's working directory.
    WorkingDir,
    /// The log's time falls inside the session. Likely, not certain.
    Time,
}

/// What Agora saw of the game's end.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExitObservation {
    /// The game's own process was held, and this is the code it exited with.
    Code { pid: u32, image: String, code: u32 },
    /// The exit could not be observed, and why. Never a guess.
    Unavailable { reason: String },
}

impl ExitObservation {
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::Unavailable {
            reason: reason.into(),
        }
    }
}

impl Default for ExitObservation {
    fn default() -> Self {
        Self::unavailable("the session was not watched")
    }
}

/// The game-free verdict on how a session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionVerdict {
    Crashed { how: MatchedBy },
    FailedToStart,
    Froze,
    Stopped,
    Normal,
    ClosedUnknown,
}
