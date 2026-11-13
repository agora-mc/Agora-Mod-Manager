//! The boundary around a parser or inspector, the same whatever runs behind it.
//!
//! # The call
//!
//! The host calls one entry with one JSON value, a [`ParserRequest`], and gets one JSON value back,
//! a [`ParserOutput`] (an inspector returns an [`InspectorOutput`]). A QuickJS export is
//! `async function parseCrash(request)`. A WebAssembly export takes and returns the same JSON as
//! bytes in linear memory. A built-in generic parser takes the same request.
//!
//! # Reading
//!
//! The artifact's bytes are not in the request. A parser pulls what it needs with two host
//! operations, [`HostRead`], and those are **its only way to see anything**. They reach the
//! artifact and its children by id and nothing else: no paths, no file system, no network, no other
//! artifact.
//!
//! | Operation | QuickJS | WebAssembly | Reply |
//! |---|---|---|---|
//! | `read_bytes` | `crash.readBytes(id, offset, len)` | host import | raw bytes: an `ArrayBuffer`, or copied into linear memory, never base64. Shorter than asked at the end of the file |
//! | `read_lines` | `crash.readLines(id, fromLine, maxLines)` | host import | [`LinesReply`] as JSON |
//!
//! A refused read comes back as a [`ReadRefused`] (a rejected promise, or an error code), and a
//! parser should report what it had, with a note, rather than give up.
//!
//! # What the host does with the output
//!
//! It is untrusted. The host runs [`validate_parser_output`](super::validate_parser_output). An
//! invalid output, a timeout, a thrown error or running out of memory all become a
//! [`CouldNotCheck`](super::CouldNotCheck) naming the plugin, **never "nothing found"**. The host
//! stamps the source; a parser cannot say who it is.

use super::Fact;
use serde::{Deserialize, Serialize};

/// The limits one call runs under. The request states them so a parser can plan its reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParserLimits {
    /// Wall clock for the whole call.
    pub max_ms: u64,
    /// The most one `read_bytes` returns.
    pub max_read_bytes: u64,
    /// The most all reads of this call together return.
    pub max_total_read_bytes: u64,
    /// The most lines one `read_lines` returns.
    pub max_lines: u32,
    /// A longer line is cut, and [`LinesReply::cut`] says so.
    pub max_line_len: u32,
    pub max_facts: u32,
    pub max_refs_per_fact: u32,
    /// Bytes in one text attribute or ref value.
    pub max_attr_len: u32,
    /// Entries in one list attribute.
    pub max_list_len: u32,
    pub max_bucket_parts: u32,
    pub max_notes: u32,
}

impl ParserLimits {
    /// Sized from a measured run on QuickJS (MASTER_SPEC §30.2): generous for logs and tables of
    /// contents, and too small to walk a large binary, which is a job for a named host service.
    pub const DEFAULT: Self = Self {
        max_ms: 10_000,
        max_read_bytes: 4 * 1024 * 1024,
        max_total_read_bytes: 64 * 1024 * 1024,
        max_lines: 100_000,
        max_line_len: 8 * 1024,
        max_facts: 20_000,
        max_refs_per_fact: 16,
        max_attr_len: 4 * 1024,
        max_list_len: 64,
        max_bucket_parts: 8,
        max_notes: 64,
    };

    /// The lower of each limit. A plugin may ask for less, never more.
    pub fn lower_of(self, other: Self) -> Self {
        Self {
            max_ms: self.max_ms.min(other.max_ms),
            max_read_bytes: self.max_read_bytes.min(other.max_read_bytes),
            max_total_read_bytes: self.max_total_read_bytes.min(other.max_total_read_bytes),
            max_lines: self.max_lines.min(other.max_lines),
            max_line_len: self.max_line_len.min(other.max_line_len),
            max_facts: self.max_facts.min(other.max_facts),
            max_refs_per_fact: self.max_refs_per_fact.min(other.max_refs_per_fact),
            max_attr_len: self.max_attr_len.min(other.max_attr_len),
            max_list_len: self.max_list_len.min(other.max_list_len),
            max_bucket_parts: self.max_bucket_parts.min(other.max_bucket_parts),
            max_notes: self.max_notes.min(other.max_notes),
        }
    }
}

impl Default for ParserLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// What a parser receives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParserRequest {
    pub contract: u32,
    pub parser: String,
    /// The artifact, with its children when it's a folder.
    pub artifact: ArtifactHandle,
    pub game: GameHandle,
    pub limits: ParserLimits,
}

/// What a parser may know of an artifact: a name and an id, not a path, because a path holds the
/// user's name and a parser has no use for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactHandle {
    pub id: String,
    pub name: String,
    pub size: u64,
    /// RFC 3339, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_at: Option<String>,
    /// The crash source that found it. A swept or pasted artifact has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<ArtifactHandle>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GameHandle {
    pub id: String,
    /// The game's version when Agora knows it. A pasted log may tie to no instance, so a parser
    /// must not count on it, and should report what the log itself says in `core.process`'s
    /// `image_version`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_version: Option<String>,
}

/// The JSON form of the two read operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum HostRead {
    ReadBytes {
        artifact: String,
        offset: u64,
        len: u64,
    },
    ReadLines {
        artifact: String,
        from_line: u64,
        max_lines: u32,
    },
}

/// Where a stored head and tail stop being adjacent. Bytes between them were not kept, so a
/// parser reading across this point must not treat the lines as consecutive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LineGap {
    /// The zero-based line index where the gap falls. The first line read from the tail has this
    /// index; when the head did not end in a newline, it continues that line.
    pub at_line: u64,
    /// Bytes omitted between the head and the tail.
    pub omitted_bytes: u64,
}

/// Lines, decoded by the host: UTF-8, else the system's code page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinesReply {
    pub lines: Vec<String>,
    /// The line to ask for next. Absent at the end of the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_line: Option<u64>,
    /// Some line was longer than `max_line_len` and was cut.
    #[serde(default)]
    pub cut: bool,
    /// Present when a head and tail were joined and are not adjacent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gap: Option<LineGap>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadRefused {
    pub reason: ReadRefusal,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadRefusal {
    /// Not this call's artifact or one of its children.
    OutOfScope,
    /// A single read asked for more than the limit.
    TooLarge,
    /// This call's read budget is spent.
    BudgetSpent,
    /// Agora did not keep these bytes.
    NotRetained,
    Unreadable,
}

/// What a parser returns. Empty `facts` with no coverage is a legal way to say "nothing I can
/// read": every kind then reads as could-not-check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParserOutput {
    pub contract: u32,
    pub producer: Producer,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub facts: Vec<Fact>,
    /// Stable tokens for "is this the same crash as last time", most specific first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bucket: Vec<BucketPart>,
    /// What it looked for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub coverage: Vec<Coverage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<ParseNote>,
}

/// What wrote the artifact. Rules gate on this.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Producer {
    pub format: String,
    /// The version of the logger, when the artifact says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// One token of a crash's identity. Must survive a restart and a mod update: no line numbers, jar
/// names or addresses that shift between runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BucketPart {
    /// How to read the value, so that different schemes never compare equal by accident.
    pub scheme: String,
    pub value: String,
}

/// Whether a parser looked for a kind of fact, which separates "false" from "could not check". A
/// kind with facts counts as checked. A kind with neither facts nor an entry here is not checked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Coverage {
    pub kind: String,
    pub state: CoverageState,
    /// Why, shown to the user. Required for `absent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageState {
    /// It looked, and any such fact is in `facts`. None there means there were none.
    Checked,
    /// It couldn't look: the section isn't in this version of the log, say.
    Absent,
    /// This format can't hold such a thing, so a rule asking gets false and no note.
    NotApplicable,
}

/// Something the user or the plugin's author should know about the parse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParseNote {
    pub kind: ParseNoteKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParseNoteKind {
    /// This isn't the format after all. Any facts are ignored and another parser may try.
    WrongFormat,
    /// The artifact stops early.
    Truncated,
    /// It holds something this parser doesn't know.
    UnknownSection,
    /// A newer version of the format than this parser knows.
    UnknownVersion,
    /// The parser's budget ran out before the end.
    BudgetSpent,
    Other,
}

/// What an inspector returns. It receives a [`ParserRequest`] whose artifact is the target, under
/// the same read rules.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectorOutput {
    pub contract: u32,
    pub result: InspectResult,
    /// Why, for the answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Facts the inspection found, such as the version a file was built for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub facts: Vec<Fact>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectResult {
    /// Found nothing wrong.
    Pass,
    /// Found this file is bad.
    Fail,
    /// Could not tell.
    Unknown,
}
