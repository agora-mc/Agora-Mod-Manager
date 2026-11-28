//! A crash pack a compiled game package supplies instead of an installed plugin.
//!
//! A built-in pack is the same JSON a plugin writes, linted by the same linter and reaching the
//! engine through the same request and output shapes. What differs is only who runs its parsers:
//! where a plugin names a script export, a built-in pack names a parser compiled into the binary.
//!
//! The engine in `agora-core` never learns a game's name from this: it loads whatever packs a game
//! crate hands it and attributes their findings to Agora itself.

use super::{LinesReply, ParserOutput, ParserRequest};

/// Why a built-in parser could not produce an output. The host turns this into "could not check",
/// never "nothing found".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltInParserError {
    pub message: String,
}

impl BuiltInParserError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// A refused read, with the same stable codes as a script's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParserReadError {
    pub code: String,
    pub message: String,
}

impl ParserReadError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// The reads a built-in parser may use: the same two operations a script parser has, and no path.
pub trait ParserReads: Send + Sync {
    fn read_bytes(&self, artifact: &str, offset: u64, len: u64)
        -> Result<Vec<u8>, ParserReadError>;

    fn read_lines(
        &self,
        artifact: &str,
        from_line: u64,
        max_lines: u32,
    ) -> Result<LinesReply, ParserReadError>;
}

/// A parser compiled into the binary, behind the same boundary as a script.
pub trait BuiltInParser: Send + Sync {
    fn parse(
        &self,
        request: &ParserRequest,
        reads: &dyn ParserReads,
    ) -> Result<ParserOutput, BuiltInParserError>;
}

/// A crash pack a compiled game package ships. The JSON is linted exactly like a plugin's.
pub struct BuiltInPack {
    /// The file name used in problems, as a plugin manifest would give.
    pub file: String,
    /// The pack JSON.
    pub json: String,
    /// Each `entry.built_in.name` this pack uses, with the code that runs it.
    pub parsers: Vec<(String, Box<dyn BuiltInParser>)>,
}

impl std::fmt::Debug for BuiltInPack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuiltInPack")
            .field("file", &self.file)
            .field(
                "parsers",
                &self
                    .parsers
                    .iter()
                    .map(|(name, _)| name)
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}
