//! The crash doctor's contract (MASTER_SPEC §30.11): the shapes shared by the engine in core and
//! by every game plugin, official or community. Nothing here knows a game, and nothing assumes a
//! parser is written in Rust.
//!
//! The pipeline in terms of these types:
//!
//! ```text
//! session verdict -> CrashSources find Artifacts -> ParserDecls turn each into Facts and Refs
//!   -> RefMaps and RoleRules say who owns each Ref -> Rules over Facts give Signals and Findings
//!   -> consensus gives Candidates -> a CrashAnswer, with repairs and could-not-check notes
//! ```
//!
//! A plugin contributes [`CrashPack`]s, each a JSON file named by the manifest's
//! `contributions.crashPacks`. A game's normal exit codes stay in `GameDefinition::session`.
//!
//! The version is [`CRASH_CONTRACT_VERSION`]. The contract is not frozen until the Creation Engine
//! parsers exist (§30.7), so there is no migration machinery: a pack of another version is refused
//! with a message.

mod answer;
mod builtin;
mod facts;
mod lint;
mod pack;
mod parser;
mod rule;
mod session;

pub use answer::*;
pub use builtin::*;
pub use facts::*;
pub use lint::*;
pub use pack::*;
pub use parser::*;
pub use rule::*;
pub use session::*;

/// The version every pack and parser output states in its `contract` field.
pub const CRASH_CONTRACT_VERSION: u32 = 1;
