//! One archive reader (MASTER_SPEC §32.3).
//!
//! Dozens of places in Agora open a zip, a 7z, a tar.gz or a RAR, and each one decided for itself
//! which entry names were acceptable, how large an archive may be, and what a damaged archive
//! means. This is the one reader: it lists an archive, holds every entry's name and size to one
//! rule, and writes entries under a destination root through [`join_under`] and
//! [`validate_path_set`].
//!
//! Three rules the types make hard to forget:
//!
//! * **An entry name is not a file name until something writes it.** Reading a jar's `aux.class` is
//!   fine; only [`SafeArchive::extract`] applies the strict rule, because only then does a name
//!   become a file on a disk.
//! * **Limits are named presets.** A caller picks the job (`PACK`, `BACKUP`, `PLUGIN_PACKAGE`,
//!   `JAR_METADATA`, `RUNTIME`, `GAME_MOD`, `TECHNIC_PACK`) instead of inventing numbers, so two
//!   call sites that do the same job cannot drift apart.
//! * **Fail closed.** A corrupt, truncated, password-protected or unreadable archive is an error
//!   with the reason. An entry whose declared size lies is refused at the byte where the lie shows.
//!   There is no "empty archive" answer to a question that could not be answered.
//!
//! RAR is read out of process on Windows, by `tar.exe`. Starting a process is not this contract's
//! job, so this module declares the [`RarReader`] seam and core supplies an implementation.

use std::collections::HashMap;
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::path::{join_under, validate_path_set, PathError, PathSetError, RelPath, StrictRelPath};

/// How much of one archive a job accepts.
///
/// A preset is a *job*, not a format: the same numbers apply to a zip and a 7z of the same pack.
/// `preset` appears in every refusal, so a person can see which rule they hit.
///
/// Carried over from the call sites this reader replaced (§32.3); where two call sites of one preset
/// disagreed, the stricter value:
///
/// | preset | entries | per entry | total | ratio |
/// |---|---|---|---|---|
/// | `PACK` | 5 000 (`MAX_MRPACK_OVERRIDE_FILES`, `MAX_TECHNIC_ZIP_FILES`) | 500 MiB (`MAX_MRPACK_FILE_BYTES`) | 2 GiB (`MAX_MRPACK_OVERRIDE_BYTES`) | 200 (new) |
/// | `TECHNIC_PACK` | 5 000 (`MAX_TECHNIC_ZIP_FILES`) | 500 MiB (`MAX_TECHNIC_ENTRY_BYTES`) | 2 GiB (`MAX_TECHNIC_ZIP_BYTES`) | 200 (new) |
/// | `GAME_MOD` | none | none | none | none (as today) |
/// | `BACKUP` | 10 000 (`MAX_BACKUP_ENTRIES`) | 1 GiB (`MAX_BACKUP_SINGLE_FILE_SIZE`) | 64 GiB (`MAX_BACKUP_TOTAL_SIZE`) | none (Agora wrote it) |
/// | `PLUGIN_PACKAGE` | 2 000 (`install::MAX_ENTRIES`) | 16 MiB (`MAX_ENTRY_BYTES`) | 64 MiB (`MAX_TOTAL_BYTES`) | 200 (`MAX_EXPANSION_RATIO`) |
/// | `JAR_METADATA` | not bounded (none today) | 32 MiB (`jar_metadata::MAX_ENTRY_BYTES`), on each read | not bounded | none |
/// | `RUNTIME` | 10 000 (`runtime_manager::MAX_ENTRIES`) | 500 MiB (`PER_ENTRY_LIMIT`) | 2 GiB (`AGGREGATE_LIMIT`) | none today |
///
/// The ratio rules marked *new* close a hole: neither call site had one, and a compression ratio is
/// the check that catches a bomb while its declared sizes still look reasonable. 200 is the value
/// the plugin package already used.
///
/// `JAR_METADATA` reads a manifest and the nested jars it names and nothing else, so its numbers are
/// the ones a *read* is held to. What the listing is held to is [`ArchiveLimits::bounds_the_listing`]:
/// a jar of 130 MiB of textures is bigger than the nested-byte budget that call site had, and the
/// budget was never about the jar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveLimits {
    /// The job this preset is for, as it appears in a refusal.
    pub preset: &'static str,
    /// Most file entries an archive may hold.
    pub max_entries: usize,
    /// Most bytes one entry may declare, and then deliver.
    pub max_entry_bytes: u64,
    /// Most bytes all entries together may declare, and then deliver.
    pub max_total_bytes: u64,
    /// Most uncompressed bytes one compressed byte may become. `0` does not check.
    pub max_ratio: u64,
    /// Whether the listing itself is held to [`Self::max_entries`], [`Self::max_entry_bytes`] and
    /// [`Self::max_total_bytes`].
    ///
    /// It is `true` for a job that reads or writes every entry, where one oversized entry — or a
    /// listing that adds up to more than the job may write — is a refusal whatever happens next. A
    /// job that reads only a handful of entries says `false`: a jar's metadata opens its manifest and
    /// its nested jars and nothing else, so a 130 MiB of textures no reader ever asks for is no
    /// reason to lose the mod's id. The per-entry limit still applies to each entry as it is read,
    /// which is where such a job's real bound lives.
    pub bounds_the_listing: bool,
    /// Whether [`Self::max_entries`] counts the folders an archive records as well as its files.
    ///
    /// Carried over per call site: the runtime, plugin package, world, pack and mod-archive counters
    /// were `ZipArchive::len()`, which counts every record including a folder's, so a package of
    /// empty folders is not a way around them. A backup's counter was its manifest's file list, so
    /// it counts files.
    pub counts_folders: bool,
}

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

impl ArchiveLimits {
    /// A modpack or an overrides bundle the user chose to import.
    pub const PACK: Self = Self {
        preset: "pack",
        max_entries: 5_000,
        max_entry_bytes: 500 * MIB,
        max_total_bytes: 2 * GIB,
        max_ratio: 200,
        bounds_the_listing: true,
        counts_folders: true,
    };

    /// A technic-style modpack zip. Its numbers are `import.rs`'s own (`MAX_TECHNIC_*`); it is the
    /// preset for that one job, not for every mod archive a player has.
    pub const TECHNIC_PACK: Self = Self {
        preset: "technic pack",
        max_entries: 5_000,
        max_entry_bytes: 500 * MIB,
        max_total_bytes: 2 * GIB,
        max_ratio: 200,
        bounds_the_listing: true,
        counts_folders: true,
    };

    /// A mod archive added to the content store: a zip, 7z or rar the player picked from a curated
    /// source.
    ///
    /// Carried over exactly from `content_store`, which held neither a per-entry nor a total size
    /// cap and no entry cap for these, and meant it: a Skyrim texture or BSA/BA2 mod is routinely
    /// past the 500 MiB per entry and 2 GiB total a Technic pack is held to, and a store that
    /// refuses the mod the player downloaded is a store that does not work. What the store did
    /// check — each entry's path, a zip symlink, case-insensitive collisions — is checked by the
    /// reader's own rules, unchanged.
    pub const GAME_MOD: Self = Self {
        preset: "game mod",
        max_entries: 0,
        max_entry_bytes: u64::MAX,
        max_total_bytes: 0,
        max_ratio: 0,
        bounds_the_listing: false,
        counts_folders: false,
    };

    /// An instance backup or a snapshot: written by Agora, so its sizes are its own claim. The
    /// ratio is not checked, because a backup of already-compressed data would trip it.
    pub const BACKUP: Self = Self {
        preset: "backup",
        max_entries: 10_000,
        max_entry_bytes: GIB,
        max_total_bytes: 64 * GIB,
        max_ratio: 0,
        bounds_the_listing: true,
        counts_folders: false,
    };

    /// A downloaded plugin package: small, and code, so the tightest preset.
    pub const PLUGIN_PACKAGE: Self = Self {
        preset: "plugin package",
        max_entries: 2_000,
        max_entry_bytes: 16 * MIB,
        max_total_bytes: 64 * MIB,
        max_ratio: 200,
        bounds_the_listing: true,
        counts_folders: true,
    };

    /// Reading metadata out of a jar, nested jars included. The nesting depth and the number of
    /// nested jars are the caller's business; this bounds one entry as it is read, and nothing else.
    ///
    /// The listing is not held to anything: the job reads a manifest and the nested jars the manifest
    /// names, and a jar of textures it never opens is not a bigger risk for being big. The counts and
    /// totals below are `0` because the call sites this replaced had none — a jar with a hundred
    /// thousand entries was read before, and refusing it now would refuse jars that used to parse.
    pub const JAR_METADATA: Self = Self {
        preset: "jar metadata",
        max_entries: 0,
        max_entry_bytes: 32 * MIB,
        max_total_bytes: 0,
        max_ratio: 0,
        bounds_the_listing: false,
        counts_folders: false,
    };

    /// A downloaded game runtime: a JRE zip or tar.gz. Large files, few entries, and Agora checks
    /// the download against its published hash before this sees it.
    pub const RUNTIME: Self = Self {
        preset: "runtime",
        max_entries: 10_000,
        max_entry_bytes: 500 * MIB,
        max_total_bytes: 2 * GIB,
        max_ratio: 0,
        bounds_the_listing: true,
        counts_folders: true,
    };

    /// A world a player exported, or one they downloaded: a folder of region files, one of which
    /// may be hundreds of megabytes. Carried over from `worlds::MAX_IMPORT_ENTRIES` and
    /// `MAX_IMPORT_BYTES`, which had no per-entry limit of its own — hence 2 GiB, which is the
    /// largest a region file plausibly gets and no reason to hold a 16 GiB one open (owner's call,
    /// §32.3). The total stays at what the old code allowed.
    pub const WORLD_ARCHIVE: Self = Self {
        preset: "world archive",
        max_entries: 200_000,
        max_entry_bytes: 2 * GIB,
        max_total_bytes: 16 * GIB,
        max_ratio: 0,
        bounds_the_listing: true,
        counts_folders: true,
    };
}

/// The archive formats one reader handles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    Zip,
    SevenZ,
    Tar,
    /// A gzip-compressed tar; the gzip stream is the tar's, not an entry's.
    TarGz,
    /// Read out of process; see [`RarReader`].
    Rar,
}

impl ArchiveFormat {
    /// The format's name, as a refusal or a report spells it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Zip => "zip",
            Self::SevenZ => "7z",
            Self::Tar => "tar",
            Self::TarGz => "tar.gz",
            Self::Rar => "rar",
        }
    }
}

/// What one archive entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
    /// A symbolic link, a hard link, a device or anything else that would write a name that is not
    /// a file. Refused at extraction on every platform: following it is what an archive must not do.
    Link,
}

/// One entry, as the archive declares it.
#[derive(Debug, Clone)]
pub struct ArchiveEntry {
    /// The name exactly as the archive stores it. Not a file name, and not yet checked.
    pub name: String,
    pub kind: EntryKind,
    /// The uncompressed size the archive declares. Checked against the bytes delivered.
    pub size: u64,
    /// The compressed size, where the format stores one per entry.
    pub compressed: Option<u64>,
    /// The Unix mode the archive records, where it records one. Extraction keeps the executable
    /// bit and drops everything else (`setuid`, `setgid`, sticky), because a runtime's `bin/java`
    /// that cannot be executed is a broken install and a downloaded `setuid` binary is worse.
    pub unix_mode: Option<u32>,
}

/// An entry the caller was told about instead of given a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedEntry {
    /// The name as the archive stores it.
    pub name: String,
    /// Why it was not written, in words that name the problem.
    pub reason: String,
}

/// What an extraction wrote, and what it left out.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExtractReport {
    /// The paths written, relative to the destination, in the order they were written.
    pub written: Vec<String>,
    /// The entries a disk cannot hold, present only when the caller chose
    /// [`NamePolicy::WithoutThem`]. A caller shows these to the person who asked for the import.
    pub skipped: Vec<SkippedEntry>,
    /// The bytes actually written.
    pub bytes_written: u64,
}

/// What an extraction does with one entry, when the caller has a rule of its own.
///
/// A caller's rules — a pack's whitelist of folders, a rule that only the top-level file is taken —
/// are not the reader's business, but they must not become a second place where paths are decided.
/// So the caller answers with one of these, and the reader keeps doing the writing.
///
/// The rule is asked about **the path the file would have**, already classified (`config/x` for an
/// entry named `./config/x` or `config\x`), never the raw entry name. An entry whose name a disk
/// cannot hold is not put to the rule at all: [`NamePolicy`] answers that one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Select {
    /// Write it, under the reader's own name rules.
    Write,
    /// Leave it out, and tell the caller why. It comes back in [`ExtractReport::skipped`].
    LeaveOut(String),
    /// Refuse the whole extraction: this entry is not one the caller will accept at all. The
    /// message is the reason.
    Refuse(String),
}

/// What an extraction does with an entry whose name a disk cannot hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NamePolicy {
    /// Refuse the whole extraction, naming every offending entry and why. The default, because an
    /// import that silently drops files is worse than one that stops.
    #[default]
    Refuse,
    /// Write the entries that are usable and return the rest in [`ExtractReport::skipped`]. Never
    /// silent: the caller has the list. For a user-facing import, where the person can be told which
    /// files were left out.
    WithoutThem,
}

/// What an entry's name is, before anything writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryName {
    /// A path [`SafeArchive::extract`] may write under the destination root.
    Writable(StrictRelPath),
    /// A name that leaves the extraction root: `..`, an absolute path, a drive letter, a UNC path.
    /// Always refused, never skipped, because it is not a name a disk cannot hold — it is a name
    /// that points somewhere else.
    Escapes { reason: String },
    /// A name a disk cannot hold as a file: a Windows reserved name, a trailing dot or space, a `:`
    /// (an NTFS stream), an illegal character, an empty or `.` component. Refused by default and
    /// skippable, because such an archive is usually not attacking anything: it was built on a
    /// platform with different rules.
    Unwritable { reason: String },
}

/// Hold one entry name to the rules, and say which rule it broke.
///
/// Two questions, and the difference matters to the caller: a name that *escapes* is refused
/// whatever the caller's policy, and a name a disk cannot *hold* is the one a user-facing import may
/// choose to extract without.
pub fn classify_entry_name(name: &str) -> EntryName {
    let normalized = name.replace('\\', "/");
    if normalized.starts_with('/') {
        return EntryName::Escapes {
            reason: "it is absolute, so it leaves the extraction root".to_string(),
        };
    }
    let mut characters = normalized.chars();
    if let (Some(first), Some(':')) = (characters.next(), characters.next()) {
        if first.is_ascii_alphabetic() {
            return EntryName::Escapes {
                reason: "it starts with a drive letter, so it leaves the extraction root"
                    .to_string(),
            };
        }
    }
    if normalized.split('/').any(|part| part == "..") {
        return EntryName::Escapes {
            reason: "it has a '..' component, so it leaves the extraction root".to_string(),
        };
    }
    // A `.` component names the folder it is in. Zip tools write `./main.js`, and dropping the part
    // cannot leave the root, so it is not a name a disk cannot hold — and refusing it would refuse
    // packages that unpack perfectly well.
    let normalized = normalized
        .split('/')
        .filter(|part| *part != ".")
        .collect::<Vec<&str>>()
        .join("/");
    match RelPath::strict(normalized) {
        Ok(strict) => EntryName::Writable(strict),
        Err(error) => EntryName::Unwritable {
            reason: error.reason().to_string(),
        },
    }
}

/// An archive that could not be read, could not be held to its preset, or could not be extracted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArchiveError {
    /// The file could not be opened or read at all.
    #[error("cannot read archive '{path}': {error}")]
    Unreadable { path: String, error: String },
    /// The bytes are not an archive of a format this reader knows. Read by content, never by
    /// extension, so a `.zip` that is something else is refused here.
    #[error("'{path}' is not a zip, 7z, tar, tar.gz or rar archive")]
    UnknownFormat { path: String },
    /// A decoder refused the archive: corrupt, truncated, or not what its own headers claim.
    #[error("'{path}' is a damaged {format} archive: {reason}", format = .format.name())]
    Corrupt {
        path: String,
        format: ArchiveFormat,
        reason: String,
    },
    /// The archive needs a password. Nothing is written from an archive that cannot be read.
    #[error("'{path}' is password-protected, which Agora does not read")]
    PasswordProtected { path: String },
    /// Two or more entries share one name, so an archive holds bytes no reader can tell apart.
    ///
    /// The zip crate keeps only the last of several same-named records, so this cannot be found in
    /// a listing that goes through it: the reader counts what the archive itself declares. Nothing
    /// is read from an archive whose content is ambiguous.
    #[error(
        "'{path}' holds more than one entry named {}",
        .names.join(", ")
    )]
    DuplicateNames {
        path: String,
        format: ArchiveFormat,
        names: Vec<String>,
    },
    /// More file entries than the preset allows.
    #[error(
        "'{path}' holds {count} files, more than the {limit} a {format} archive of this kind may",
        format = .format.name()
    )]
    TooManyEntries {
        path: String,
        format: ArchiveFormat,
        preset: &'static str,
        count: usize,
        limit: usize,
    },
    /// One entry, or all of them together, declares more bytes than the preset allows.
    #[error(
        "'{path}' declares {bytes} bytes for '{entry}', more than the {limit} bytes a {format} \
         archive of this kind may",
        format = .format.name()
    )]
    TooLarge {
        path: String,
        format: ArchiveFormat,
        preset: &'static str,
        entry: String,
        bytes: u64,
        limit: u64,
    },
    /// A compression ratio the preset does not allow: the shape of a decompression bomb.
    #[error(
        "'{path}' compresses '{entry}' {ratio} times, more than the {limit} times a {format} \
         archive of this kind may",
        format = .format.name()
    )]
    RatioExceeded {
        path: String,
        format: ArchiveFormat,
        preset: &'static str,
        entry: String,
        ratio: u64,
        limit: u64,
    },
    /// An entry delivered a different number of bytes than it declared, so its own header lied.
    #[error("'{path}' entry '{entry}' declared {declared} bytes and delivered {delivered}")]
    EntrySizeLies {
        path: String,
        entry: String,
        declared: u64,
        delivered: u64,
    },
    /// An entry name leaves the extraction root. Never skippable.
    #[error("'{path}' entry '{entry}' cannot be extracted: {reason}")]
    Escapes {
        path: String,
        entry: String,
        reason: String,
    },
    /// Entries whose names a disk cannot hold, refused because the caller did not opt out of them.
    #[error("{}", refused_message(path, entries))]
    UnsafeNames {
        path: String,
        entries: Vec<SkippedEntry>,
    },
    /// An entry the archive says is a link.
    #[error("'{path}' entry '{entry}' is a link, which Agora never extracts")]
    LinkEntry { path: String, entry: String },
    /// A caller's own rule refused an entry outright, so the whole extraction stops.
    #[error("'{path}' entry '{entry}' is refused: {reason}")]
    Refused {
        path: String,
        entry: String,
        reason: String,
    },
    /// The destination could not be used.
    #[error("cannot extract into '{path}': {reason}")]
    Destination { path: String, reason: String },
    /// A file could not be written.
    #[error("cannot write '{path}': {error}")]
    Write { path: String, error: String },
    /// A RAR archive with no reader configured. RAR is read out of process, and only a caller that
    /// supplies the reader can read one.
    #[error("'{path}' is a RAR archive, and this caller gave no RAR reader")]
    NoRarReader { path: String },
    /// The RAR reader refused the archive or one entry.
    #[error("cannot read RAR archive '{path}': {reason}")]
    Rar { path: String, reason: String },
    /// The caller's own operation was cancelled, so the extraction stopped between entries.
    #[error("extracting '{path}' was cancelled")]
    Cancelled { path: String },
}

impl ArchiveError {
    /// Whether the caller may retry the same extraction without the offending entries. Only names a
    /// disk cannot hold qualify; everything else is a damaged archive or an escape, and neither gets
    /// better by leaving files out.
    pub fn may_skip_entries(&self) -> bool {
        matches!(self, Self::UnsafeNames { .. })
    }
}

fn refused_message(path: &str, entries: &[SkippedEntry]) -> String {
    let mut message = format!(
        "'{path}' has {} entries a disk cannot hold, so nothing was extracted:",
        entries.len()
    );
    for entry in entries {
        message.push_str(&format!("\n  '{}' {}", entry.name, entry.reason));
    }
    message
}

/// One entry a RAR reader found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RarEntry {
    pub name: String,
    pub size: u64,
    pub is_directory: bool,
}

/// Reading a RAR, out of process.
///
/// RAR is the one format with no usable in-process decoder, so the reader is a seam: this crate says
/// what it needs, and the caller who can run the platform's own tool supplies it. On Windows that is
/// `tar.exe`, in core (`content_store::rar`).
pub trait RarReader: Send + Sync {
    /// The archive's entries, in the order the reader reports them.
    fn list(&self, archive: &Path) -> Result<Vec<RarEntry>, String>;
    /// Prepare the archive for reading, given the listing the reader's limits were applied to.
    ///
    /// A backend that can only serve a member by unpacking the whole archive unpacks it here, once,
    /// and checks what it found against `admitted` — the same names, sizes and count — because the
    /// limits and the name rules were applied to *that* listing: bytes from a file that changed
    /// since it was listed are bytes nothing checked. A backend that reads a member directly does
    /// nothing.
    fn stage(&self, _archive: &Path, _admitted: &[RarEntry]) -> Result<(), String> {
        Ok(())
    }
    /// One entry's bytes, written to `out`. Returns how many bytes it wrote.
    fn read_entry(&self, archive: &Path, name: &str, out: &mut dyn Write) -> Result<u64, String>;
}

/// The listing the limits were applied to, in the shape a RAR backend stages against.
fn rar_admitted(entries: &[ArchiveEntry]) -> Vec<RarEntry> {
    entries
        .iter()
        .map(|entry| RarEntry {
            name: entry.name.clone(),
            size: entry.size,
            is_directory: entry.kind == EntryKind::Directory,
        })
        .collect()
}

/// Where an archive comes from: a file, or bytes already in hand.
#[derive(Clone)]
enum Source {
    Path(PathBuf),
    Bytes(Arc<[u8]>),
}

impl Source {
    fn label(&self) -> String {
        match self {
            Self::Path(path) => path.display().to_string(),
            Self::Bytes(bytes) => format!("{} bytes in memory", bytes.len()),
        }
    }

    /// The source for a file, refused with the reason when it cannot be read.
    fn from_path(path: &Path) -> Result<Self, ArchiveError> {
        match std::fs::metadata(path) {
            Ok(_) => Ok(Self::Path(path.to_path_buf())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(ArchiveError::Unreadable {
                    path: path.display().to_string(),
                    error: "it is missing".to_string(),
                })
            }
            Err(error) => Err(ArchiveError::Unreadable {
                path: path.display().to_string(),
                error: error.to_string(),
            }),
        }
    }
}

trait ReadSeek: Read + Seek {}
impl<T: Read + Seek> ReadSeek for T {}

/// What a walk should do after the entry it just handed over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Visit {
    /// Carry on with the next entry.
    Next,
    /// The visitor has what it came for: the rest of the archive is not read.
    Stop,
}

/// One entry's bytes, as a function the visitor calls with the writer it wants them in.
///
/// A reader from the archive would not do: a RAR comes from a process that writes into a writer, not
/// from something this side can read, and one shape serves both.
type EntryBytes<'a> = dyn FnMut(&mut dyn Write) -> Result<u64, ArchiveError> + 'a;

/// One archive, listed and held to its preset, ready to be read or extracted.
pub struct SafeArchive {
    source: Source,
    format: ArchiveFormat,
    limits: ArchiveLimits,
    entries: Vec<ArchiveEntry>,
    total_declared: u64,
    rar: Option<Box<dyn RarReader>>,
    /// The paths the last extraction wrote, kept when it then failed, so a caller can clean up.
    written_so_far: Vec<String>,
    /// How many times the archive's own bytes have been parsed for its entries: one to list it, one
    /// per pass since. Only the tests read it, so a caller cannot come to depend on it.
    #[cfg(test)]
    parses: usize,
}

impl SafeArchive {
    /// Open an archive from a file and list it, holding it to `limits`.
    pub fn open(path: &Path, limits: ArchiveLimits) -> Result<Self, ArchiveError> {
        Self::open_inner(Source::from_path(path)?, limits, None)
    }

    /// [`SafeArchive::open`] for a caller that can read a RAR: the archive is listed through `rar`,
    /// so a RAR is listed by the same call as every other format.
    pub fn open_with_rar(
        path: &Path,
        limits: ArchiveLimits,
        rar: Box<dyn RarReader>,
    ) -> Result<Self, ArchiveError> {
        Self::open_inner(Source::from_path(path)?, limits, Some(rar))
    }

    /// Open an archive from bytes already in hand: a download, or a plugin package the host already
    /// verified. Nothing is written before the limits are applied.
    pub fn open_bytes(
        bytes: impl Into<Arc<[u8]>>,
        limits: ArchiveLimits,
    ) -> Result<Self, ArchiveError> {
        Self::open_inner(Source::Bytes(bytes.into()), limits, None)
    }

    /// [`SafeArchive::open_bytes`] with the RAR reader the caller can supply.
    pub fn open_bytes_with_rar(
        bytes: impl Into<Arc<[u8]>>,
        limits: ArchiveLimits,
        rar: Box<dyn RarReader>,
    ) -> Result<Self, ArchiveError> {
        Self::open_inner(Source::Bytes(bytes.into()), limits, Some(rar))
    }

    fn open_inner(
        source: Source,
        limits: ArchiveLimits,
        rar: Option<Box<dyn RarReader>>,
    ) -> Result<Self, ArchiveError> {
        let stored = match &source {
            Source::Path(path) => Some(std::fs::metadata(path).map(|metadata| metadata.len())),
            Source::Bytes(bytes) => Some(Ok(bytes.len() as u64)),
        };
        let stored = match stored {
            Some(Ok(len)) => len,
            Some(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(ArchiveError::Unreadable {
                    path: source.label(),
                    error: "it is missing".to_string(),
                })
            }
            Some(Err(error)) => {
                return Err(ArchiveError::Unreadable {
                    path: source.label(),
                    error: error.to_string(),
                })
            }
            None => 0,
        };
        let archive = Self::from_source(source, limits, rar)?;
        // A format that records no compressed size per entry cannot be held to the ratio entry by
        // entry, so the whole archive's claim against the bytes it is becomes the check: a tar.gz's
        // stream, a 7z's blocks (solid ones share a pack, so a member's own size says nothing about
        // what it cost). A zip keeps its per-entry check, which is the tighter of the two.
        let per_entry_compressed = archive
            .entries
            .iter()
            .any(|entry| entry.compressed.is_some());
        if !per_entry_compressed && archive.limits.max_ratio > 0 {
            let ratio = ratio_of(archive.total_declared, stored);
            if ratio > archive.limits.max_ratio {
                return Err(ArchiveError::RatioExceeded {
                    path: archive.label(),
                    format: archive.format,
                    preset: archive.limits.preset,
                    entry: "the whole archive".to_string(),
                    ratio,
                    limit: archive.limits.max_ratio,
                });
            }
        }
        Ok(archive)
    }

    fn from_source(
        source: Source,
        limits: ArchiveLimits,
        rar: Option<Box<dyn RarReader>>,
    ) -> Result<Self, ArchiveError> {
        let format = detect_format(&source)?;
        let mut archive = Self {
            source,
            format,
            limits,
            entries: Vec::new(),
            total_declared: 0,
            rar,
            written_so_far: Vec::new(),
            #[cfg(test)]
            parses: 0,
        };
        let entries = archive.list()?;
        archive.entries = entries;
        Ok(archive)
    }

    fn open_reader(&self) -> Result<Box<dyn ReadSeek>, ArchiveError> {
        match &self.source {
            Source::Path(path) => {
                let file = std::fs::File::open(path).map_err(|error| ArchiveError::Unreadable {
                    path: path.display().to_string(),
                    error: error.to_string(),
                })?;
                Ok(Box::new(file))
            }
            Source::Bytes(bytes) => Ok(Box::new(std::io::Cursor::new(Arc::clone(bytes)))),
        }
    }

    /// The format the bytes are, whatever the file is called.
    pub fn format(&self) -> ArchiveFormat {
        self.format
    }

    /// The preset this archive is held to.
    pub fn limits(&self) -> ArchiveLimits {
        self.limits
    }

    /// Every entry, directories and links included, as the archive declares them.
    pub fn entries(&self) -> &[ArchiveEntry] {
        &self.entries
    }

    /// The declared uncompressed bytes of every file entry together.
    pub fn total_declared_bytes(&self) -> u64 {
        self.total_declared
    }

    /// The archive's label for a message: its path, or that it was bytes in memory.
    pub fn label(&self) -> String {
        self.source.label()
    }

    /// The file entries, in the order the archive stores them.
    pub fn file_entries(&self) -> impl Iterator<Item = &ArchiveEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::File)
    }

    /// One entry's bytes, bounded by the preset's per-entry limit.
    ///
    /// This is the read a jar's metadata takes: the name is used as the archive's own key, never as
    /// a path, so an entry called `aux.class` reads like any other.
    pub fn read(&mut self, name: &str) -> Result<Vec<u8>, ArchiveError> {
        let mut out = Vec::new();
        self.read_into(name, &mut out)?;
        Ok(out)
    }

    /// [`SafeArchive::read`] into a caller's buffer. Returns the bytes read.
    ///
    /// The archive is walked in its own order and the walk stops at the entry: one parse, whatever
    /// the archive's length (MASTER_SPEC §32.3, Rule B).
    pub fn read_into(&mut self, name: &str, out: &mut dyn Write) -> Result<u64, ArchiveError> {
        let path = self.label();
        let mut written = 0u64;
        let mut found = false;
        {
            let out = &mut *out;
            self.walk(&|| false, &mut |entry, bytes| {
                if found || entry.kind != EntryKind::File || entry.name != name {
                    return Ok(Visit::Next);
                }
                written = bytes(out)?;
                found = true;
                Ok(Visit::Stop)
            })?;
        }
        if !found {
            return Err(ArchiveError::Corrupt {
                path,
                format: self.format,
                reason: format!("no entry is named '{name}'"),
            });
        }
        Ok(written)
    }

    /// Every file entry's bytes, in archive order, into one buffer each.
    ///
    /// One walk of the archive, not one per file: a `tar.gz` is inflated once rather than once per
    /// entry (MASTER_SPEC §32.3, Rule B).
    pub fn read_all(&mut self) -> Result<Vec<(String, Vec<u8>)>, ArchiveError> {
        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        self.walk(&|| false, &mut |entry, bytes| {
            if entry.kind != EntryKind::File {
                return Ok(Visit::Next);
            }
            let name = entry.name.clone();
            let mut buffer = Vec::with_capacity(entry.size as usize);
            bytes(&mut buffer)?;
            files.push((name, buffer));
            Ok(Visit::Next)
        })?;
        Ok(files)
    }

    /// The rules one entry's bytes are held to, in the one shape every format reports through.
    fn context_for<'a>(
        &self,
        entry: &ArchiveEntry,
        cancelled: &'a dyn Fn() -> bool,
    ) -> CopyContext<'a> {
        CopyContext {
            archive: self.label(),
            format: self.format,
            preset: self.limits.preset,
            limit: self.limits.max_entry_bytes,
            entry: entry.name.clone(),
            declared: entry.size,
            exact: self.format != ArchiveFormat::Rar,
            cancelled,
        }
    }

    /// Walk the archive **once**, in the order it stores its entries, handing each file entry to
    /// `visit` as a function that copies its bytes wherever the visitor wants them.
    ///
    /// One pass is the point (MASTER_SPEC §32.3, Rule B). A `tar.gz` is inflated once for the whole
    /// archive instead of once per entry, a fifty-thousand-entry zip is parsed once instead of fifty
    /// thousand times, and a **solid** 7z decodes its members in the order they were packed: its
    /// members share one compressed block, so an entry the visitor does not want is drained rather
    /// than stepped over — the library's own words are that "you cannot simply skip the previous
    /// data and only decompress the data in the back".
    ///
    /// `cancelled` is asked before every entry **and** inside every copy, so a person who stops a
    /// long extraction does not wait for the entry under way to finish.
    fn walk(
        &mut self,
        cancelled: &dyn Fn() -> bool,
        visit: &mut dyn FnMut(&ArchiveEntry, &mut EntryBytes<'_>) -> Result<Visit, ArchiveError>,
    ) -> Result<(), ArchiveError> {
        #[cfg(test)]
        {
            self.parses += 1;
        }
        let path = self.label();
        let entries = self.entries.clone();
        match self.format {
            ArchiveFormat::Zip => {
                let mut source = self.open_reader()?;
                let mut zip = zip::ZipArchive::new(source.as_mut()).map_err(|error| {
                    ArchiveError::Corrupt {
                        path: path.clone(),
                        format: ArchiveFormat::Zip,
                        reason: format!("{error}"),
                    }
                })?;
                for (index, entry) in entries.iter().enumerate() {
                    if entry.kind != EntryKind::File {
                        continue;
                    }
                    if cancelled() {
                        return Err(ArchiveError::Cancelled { path });
                    }
                    let mut file = zip.by_index(index).map_err(|error| ArchiveError::Corrupt {
                        path: path.clone(),
                        format: ArchiveFormat::Zip,
                        reason: format!("entry '{index}': {error}"),
                    })?;
                    let context = self.context_for(entry, cancelled);
                    let mut copy = |out: &mut dyn Write| copy_bounded(&mut file, out, &context);
                    if visit(entry, &mut copy)? == Visit::Stop {
                        return Ok(());
                    }
                }
                Ok(())
            }
            ArchiveFormat::Tar | ArchiveFormat::TarGz => {
                let source = self.open_reader()?;
                let reader: Box<dyn Read> = if self.format == ArchiveFormat::Tar {
                    source
                } else {
                    Box::new(flate2::read::GzDecoder::new(source))
                };
                let mut archive = tar::Archive::new(reader);
                let listed = archive.entries().map_err(|error| ArchiveError::Corrupt {
                    path: path.clone(),
                    format: self.format,
                    reason: format!("{error}"),
                })?;
                // One table for the whole walk: a `tar.gz` with fifty thousand entries would
                // otherwise search the listing once per entry it hands over.
                let by_name: HashMap<&str, &ArchiveEntry> = entries
                    .iter()
                    .map(|entry| (entry.name.as_str(), entry))
                    .collect();
                for entry in listed {
                    let mut entry = entry.map_err(|error| ArchiveError::Corrupt {
                        path: path.clone(),
                        format: self.format,
                        reason: format!("{error}"),
                    })?;
                    let name = entry_name_of(&entry, self.format, &path)?;
                    // The listing was taken from these same bytes, so an entry it did not show
                    // means the two disagree: refuse rather than read a name nothing checked.
                    let Some(known) = by_name.get(name.as_str()).copied() else {
                        return Err(ArchiveError::Corrupt {
                            path: path.clone(),
                            format: self.format,
                            reason: format!("an entry named '{name}' was not in the listing"),
                        });
                    };
                    if known.kind != EntryKind::File {
                        continue;
                    }
                    if cancelled() {
                        return Err(ArchiveError::Cancelled { path: path.clone() });
                    }
                    let context = self.context_for(known, cancelled);
                    let mut copy = |out: &mut dyn Write| copy_bounded(&mut entry, out, &context);
                    if visit(known, &mut copy)? == Visit::Stop {
                        return Ok(());
                    }
                }
                // The gzip trailer is after the tar end marker, and this walk reads the file again:
                // it checks the stream's own end rather than trusting the listing's read of it.
                drain_stream(archive.into_inner(), self.format, &path)?;
                Ok(())
            }
            ArchiveFormat::SevenZ => {
                let mut source = self.open_reader()?;
                let mut reader =
                    sevenz_rust2::ArchiveReader::new(&mut source, sevenz_rust2::Password::empty())
                        .map_err(|error| sevenz_error(&path, error))?;
                let mut failure: Option<ArchiveError> = None;
                let by_name: HashMap<&str, &ArchiveEntry> = entries
                    .iter()
                    .map(|entry| (entry.name.as_str(), entry))
                    .collect();
                let result = reader.for_each_entries(|entry, entry_reader| {
                    let Some(known) = by_name.get(entry.name.as_str()).copied() else {
                        failure = Some(ArchiveError::Corrupt {
                            path: path.clone(),
                            format: ArchiveFormat::SevenZ,
                            reason: format!(
                                "an entry named '{}' was not in the listing",
                                entry.name
                            ),
                        });
                        return Err(sevenz_rust2::Error::Other("stopped".into()));
                    };
                    if known.kind != EntryKind::File {
                        return Ok(true);
                    }
                    if cancelled() {
                        failure = Some(ArchiveError::Cancelled { path: path.clone() });
                        return Err(sevenz_rust2::Error::Other("stopped".into()));
                    }
                    let context = self.context_for(known, cancelled);
                    let mut copy =
                        |out: &mut dyn Write| copy_bounded(&mut *entry_reader, out, &context);
                    let next = match visit(known, &mut copy) {
                        Ok(next) => next,
                        Err(error) => {
                            failure = Some(error);
                            return Err(sevenz_rust2::Error::Other("stopped".into()));
                        }
                    };
                    if next == Visit::Stop {
                        return Ok(false);
                    }
                    // Members of one block share the decoder: this one's bytes are read to the end
                    // whether or not they were wanted, or the next member decodes from a position
                    // that is not its own. A person who asked to stop should not wait for a member
                    // nobody wanted, so cancellation is asked between buffers here too.
                    if let Err(error) =
                        drain_unwanted(&mut *entry_reader, cancelled, &path, entry.name.as_str())
                    {
                        failure = Some(error);
                        return Err(sevenz_rust2::Error::Other("stopped".into()));
                    }
                    Ok(true)
                });
                if let Some(error) = failure {
                    return Err(error);
                }
                result.map_err(|error| sevenz_error(&path, error))
            }
            ArchiveFormat::Rar => {
                let Some(rar) = self.rar.as_deref() else {
                    return Err(ArchiveError::NoRarReader { path });
                };
                let Source::Path(archive) = &self.source else {
                    return Err(ArchiveError::NoRarReader { path });
                };
                let archive = archive.clone();
                // The backend stages the archive against the listing the limits were applied to, so
                // a member served here is a member that was checked (MASTER_SPEC §32.3).
                rar.stage(&archive, &rar_admitted(&entries))
                    .map_err(|reason| ArchiveError::Rar {
                        path: path.clone(),
                        reason,
                    })?;
                for entry in &entries {
                    if entry.kind != EntryKind::File {
                        continue;
                    }
                    if cancelled() {
                        return Err(ArchiveError::Cancelled { path: path.clone() });
                    }
                    let context = self.context_for(entry, cancelled);
                    let mut copy = |out: &mut dyn Write| {
                        let mut sink = Sink::new(out, &context);
                        let written =
                            rar.read_entry(&archive, &entry.name, &mut sink)
                                .map_err(|reason| ArchiveError::Rar {
                                    path: path.clone(),
                                    reason,
                                })?;
                        sink.finish()?;
                        Ok(written)
                    };
                    if visit(entry, &mut copy)? == Visit::Stop {
                        return Ok(());
                    }
                }
                Ok(())
            }
        }
    }

    /// List the archive's entries, holding every one of them to the preset.
    fn list(&mut self) -> Result<Vec<ArchiveEntry>, ArchiveError> {
        #[cfg(test)]
        {
            self.parses += 1;
        }
        let entries = match self.format {
            ArchiveFormat::Zip => self.list_zip()?,
            ArchiveFormat::SevenZ => self.list_sevenz()?,
            ArchiveFormat::Tar => {
                let source = self.open_reader()?;
                list_tar(source, ArchiveFormat::Tar, &self.label(), &self.limits)?
            }
            ArchiveFormat::TarGz => {
                let source = self.open_reader()?;
                let decoder = flate2::read::GzDecoder::new(source);
                list_tar(
                    Box::new(decoder),
                    ArchiveFormat::TarGz,
                    &self.label(),
                    &self.limits,
                )?
            }
            ArchiveFormat::Rar => self.list_rar()?,
        };
        self.apply_limits(&entries)?;
        Ok(entries)
    }

    fn list_rar(&self) -> Result<Vec<ArchiveEntry>, ArchiveError> {
        let path = self.label();
        let Some(rar) = self.rar.as_deref() else {
            return Err(ArchiveError::NoRarReader { path });
        };
        let Source::Path(archive) = &self.source else {
            return Err(ArchiveError::NoRarReader { path: self.label() });
        };
        let listed = rar.list(archive).map_err(|reason| ArchiveError::Rar {
            path: path.clone(),
            reason,
        })?;
        Ok(listed
            .into_iter()
            .map(|entry| ArchiveEntry {
                name: entry.name,
                kind: if entry.is_directory {
                    EntryKind::Directory
                } else {
                    EntryKind::File
                },
                size: entry.size,
                compressed: None,
                unix_mode: None,
            })
            .collect())
    }

    fn list_zip(&self) -> Result<Vec<ArchiveEntry>, ArchiveError> {
        let path = self.label();
        let mut source = self.open_reader()?;
        let mut zip =
            zip::ZipArchive::new(source.as_mut()).map_err(|error| ArchiveError::Corrupt {
                path: path.clone(),
                format: ArchiveFormat::Zip,
                reason: format!("{error}"),
            })?;
        let central_directory_at = zip.central_directory_start();
        let mut entries = Vec::with_capacity(zip.len());
        for index in 0..zip.len() {
            let entry = zip.by_index(index).map_err(|error| ArchiveError::Corrupt {
                path: path.clone(),
                format: ArchiveFormat::Zip,
                reason: format!("entry {index}: {error}"),
            })?;
            let name = entry
                .name()
                .map_err(|error| ArchiveError::Corrupt {
                    path: path.clone(),
                    format: ArchiveFormat::Zip,
                    reason: format!("entry {index} has no readable name: {error}"),
                })?
                .to_string();
            let kind = zip_entry_kind(&entry, &name);
            entries.push(ArchiveEntry {
                name,
                kind,
                size: entry.size(),
                compressed: Some(entry.compressed_size()),
                unix_mode: entry.unix_mode(),
            });
        }
        drop(zip);
        // The crate keeps only the last of several same-named records, so the listing above cannot
        // show them: the central directory is counted instead, and an archive that hides one entry
        // behind another is refused rather than silently read as the last one (MASTER_SPEC §32.13).
        let declared =
            zip_declared_entry_names(source.as_mut(), central_directory_at).map_err(|error| {
                ArchiveError::Corrupt {
                    path: path.clone(),
                    format: ArchiveFormat::Zip,
                    reason: format!("its central directory could not be read: {error}"),
                }
            })?;
        let duplicated = names_declared_more_than_once(&declared);
        if !duplicated.is_empty() {
            return Err(ArchiveError::DuplicateNames {
                path,
                format: ArchiveFormat::Zip,
                names: duplicated,
            });
        }
        Ok(entries)
    }

    fn list_sevenz(&self) -> Result<Vec<ArchiveEntry>, ArchiveError> {
        let path = self.label();
        let mut source = self.open_reader()?;
        let archive =
            match sevenz_rust2::Archive::read(&mut source, &sevenz_rust2::Password::empty()) {
                Ok(archive) => archive,
                Err(error) => return Err(sevenz_error(&path, error)),
            };
        let mut entries = Vec::with_capacity(archive.files.len());
        for entry in &archive.files {
            let kind = if entry.is_directory || entry.name.ends_with('/') {
                EntryKind::Directory
            } else if entry.is_anti_item || claims_to_be_a_link(entry) {
                // An archive stores the reparse *attribute* and not the tag, so an entry that
                // claims one cannot be told from a link: a link would point outside the
                // destination, so the claim is refused rather than guessed at.
                EntryKind::Link
            } else {
                EntryKind::File
            };
            entries.push(ArchiveEntry {
                name: entry.name.clone(),
                kind,
                size: entry.size,
                compressed: None,
                unix_mode: sevenz_unix_mode(entry),
            });
        }
        Ok(entries)
    }

    /// Hold the whole listing to the preset, before a single byte is written.
    fn apply_limits(&mut self, entries: &[ArchiveEntry]) -> Result<(), ArchiveError> {
        let path = self.label();
        let mut files = 0usize;
        let mut total = 0u64;
        // A tar, a 7z and a RAR list every record they hold, so two entries with one name show up
        // here — a name that is a file once and a folder once included; a zip hides the second
        // behind the first, so `list_zip` counts that one itself.
        let declared: Vec<DeclaredName> = entries
            .iter()
            .map(|entry| DeclaredName {
                name: entry.name.trim_end_matches('/').to_string(),
                is_directory: entry.kind == EntryKind::Directory,
                alias: None,
            })
            .collect();
        let duplicated = names_declared_more_than_once(&declared);
        if !duplicated.is_empty() {
            return Err(ArchiveError::DuplicateNames {
                path,
                format: self.format,
                names: duplicated,
            });
        }
        // The whole listing is still totalled, because the ratio check and `total_declared_bytes`
        // are about the archive and not about the job; only the *refusals* follow the preset.
        let held_to_the_listing = self.limits.bounds_the_listing;
        let mut records = 0usize;
        for entry in entries {
            records += 1;
            if entry.kind != EntryKind::File {
                continue;
            }
            files += 1;
            total = total.saturating_add(entry.size);
            if !held_to_the_listing {
                continue;
            }
            let counted = if self.limits.counts_folders {
                records
            } else {
                files
            };
            hold_running_listing(
                &self.limits,
                self.format,
                &path,
                &entry.name,
                counted,
                entry.size,
                total,
            )?;
            if self.limits.max_ratio > 0 {
                if let Some(compressed) = entry.compressed {
                    let ratio = ratio_of(entry.size, compressed);
                    if ratio > self.limits.max_ratio {
                        return Err(ArchiveError::RatioExceeded {
                            path,
                            format: self.format,
                            preset: self.limits.preset,
                            entry: entry.name.clone(),
                            ratio,
                            limit: self.limits.max_ratio,
                        });
                    }
                }
            }
        }
        let counted = if self.limits.counts_folders {
            records
        } else {
            files
        };
        if held_to_the_listing && counted > self.limits.max_entries {
            return Err(ArchiveError::TooManyEntries {
                path,
                format: self.format,
                preset: self.limits.preset,
                count: counted,
                limit: self.limits.max_entries,
            });
        }
        self.total_declared = total;
        Ok(())
    }

    /// Write the archive's files under `dest`, through [`join_under`], holding every name to the
    /// strict rule and the set to [`validate_path_set`].
    ///
    /// `dest` is the caller's own choice of root and the caller holds the lock for it; a link on the
    /// way inside it is refused by [`join_under`]. Names that escape the root are always refused.
    /// Names a disk cannot hold are refused under [`NamePolicy::Refuse`] and left out under
    /// [`NamePolicy::WithoutThem`], where they come back in the report.
    pub fn extract(
        &mut self,
        dest: &Path,
        policy: NamePolicy,
    ) -> Result<ExtractReport, ArchiveError> {
        self.extract_where(dest, policy, &mut |_| Select::Write)
    }

    /// [`SafeArchive::extract`] with the caller's own rule about which entries it wants. The rule
    /// answers per entry (see [`Select`]); the reader still owns every path decision and every write,
    /// so a caller's rule cannot become a second path validator.
    ///
    /// The entries a caller leaves out, and the entries whose names a disk cannot hold, both come
    /// back in [`ExtractReport::skipped`] with their reasons, so nothing disappears quietly.
    pub fn extract_where(
        &mut self,
        dest: &Path,
        policy: NamePolicy,
        select: &mut dyn FnMut(&str) -> Select,
    ) -> Result<ExtractReport, ArchiveError> {
        self.extract_checked(dest, policy, select, &|| false)
    }

    /// What an extraction would write, and what it would leave out: the naming rules and the case
    /// collision check, decided once, before any file is created.
    ///
    /// A preview that a later install must match calls [`SafeArchive::check_names`], which runs this
    /// same decision, so "may these names be written" has one answer rather than two.
    fn plan(
        &self,
        select: &mut dyn FnMut(&str) -> Select,
    ) -> Result<PlannedExtraction, ArchiveError> {
        let path = self.label();
        let mut plan: Vec<(StrictRelPath, String, u64)> = Vec::new();
        let mut skipped: Vec<SkippedEntry> = Vec::new();
        // Only the names a disk cannot hold make the refusal: an entry the caller's own rule leaves
        // out is the caller's decision, and is reported rather than refused.
        let mut unwritable: Vec<SkippedEntry> = Vec::new();
        for entry in &self.entries {
            match entry.kind {
                // A folder record names a folder, so nothing is written for it — but its name is
                // read like any other: `../outside/` is a way out of the destination whichever
                // record it is written as, and it is refused rather than stepped over (§32.3).
                EntryKind::Directory => {
                    if let EntryName::Escapes { reason } = classify_entry_name(&entry.name) {
                        return Err(ArchiveError::Escapes {
                            path: path.clone(),
                            entry: entry.name.clone(),
                            reason,
                        });
                    }
                    continue;
                }
                EntryKind::Link => {
                    return Err(ArchiveError::LinkEntry {
                        path: path.clone(),
                        entry: entry.name.clone(),
                    })
                }
                EntryKind::File => {}
            }
            // The name is classified **before** the caller's rule is asked (§32.3). Two reasons: the
            // rule is about the file the entry would become, so it is handed that path and not the
            // raw entry name — `./config/x` and `config\x` name `config/x`; and a name a disk cannot
            // hold is answered by the naming rule, not by the caller's rule, so `config/run.bat.` is
            // reported as a name Windows would rewrite rather than as a file the caller banned.
            let strict = match classify_entry_name(&entry.name) {
                EntryName::Writable(strict) => strict,
                EntryName::Escapes { reason } => {
                    return Err(ArchiveError::Escapes {
                        path: path.clone(),
                        entry: entry.name.clone(),
                        reason,
                    })
                }
                EntryName::Unwritable { reason } => {
                    let left_out = SkippedEntry {
                        name: entry.name.clone(),
                        reason,
                    };
                    unwritable.push(left_out.clone());
                    skipped.push(left_out);
                    continue;
                }
            };
            match select(strict.as_str()) {
                Select::Write => plan.push((strict, entry.name.clone(), entry.size)),
                Select::LeaveOut(reason) => skipped.push(SkippedEntry {
                    name: entry.name.clone(),
                    reason,
                }),
                Select::Refuse(reason) => {
                    return Err(ArchiveError::Refused {
                        path: path.clone(),
                        entry: entry.name.clone(),
                        reason,
                    })
                }
            }
        }

        // Names a case-insensitive disk stores as one file are the same refusal: writing both would
        // silently lose one. The game contract's rule answers it, so that "may these two files exist
        // together" has one home. It reports one offender at a time, so this asks again with that
        // one set aside, and every offending name ends up in the list.
        loop {
            let paths: Vec<StrictRelPath> =
                plan.iter().map(|(strict, _, _)| strict.clone()).collect();
            match validate_path_set(&paths) {
                Ok(()) | Err(PathSetError::EmptySet) => break,
                Err(
                    error @ (PathSetError::Collides { .. } | PathSetError::FolderIsAFile { .. }),
                ) => {
                    let offending = match &error {
                        PathSetError::Collides { path } => path.clone(),
                        PathSetError::FolderIsAFile { path, .. } => path.clone(),
                        PathSetError::EmptySet => break,
                    };
                    let Some(index) = plan
                        .iter()
                        .position(|(strict, _, _)| strict.as_str() == offending)
                    else {
                        return Err(ArchiveError::Corrupt {
                            path: path.clone(),
                            format: self.format,
                            reason: format!("'{offending}' collides and is not an entry"),
                        });
                    };
                    let (_, name, _) = plan.remove(index);
                    let left_out = SkippedEntry {
                        name,
                        reason: error.reason(),
                    };
                    unwritable.push(left_out.clone());
                    skipped.push(left_out);
                }
            }
        }

        Ok(PlannedExtraction {
            write: plan,
            skipped,
            unwritable,
        })
    }

    /// The entries an extraction would leave out, and the refusal it would make, decided without
    /// creating anything.
    ///
    /// A preview and the install that follows it must answer the same way, so both ask this instead
    /// of each carrying a copy of the naming rules (§32.3).
    pub fn left_out_names(&self, policy: NamePolicy) -> Result<Vec<SkippedEntry>, ArchiveError> {
        let planned = self.plan(&mut |_| Select::Write)?;
        if !planned.unwritable.is_empty() && policy == NamePolicy::Refuse {
            return Err(ArchiveError::UnsafeNames {
                path: self.label(),
                entries: planned.unwritable,
            });
        }
        Ok(planned.skipped)
    }

    /// [`SafeArchive::extract_where`] for a caller whose operation a person can stop: `cancelled` is
    /// asked before every entry, and an answer of `true` ends the extraction with
    /// [`ArchiveError::Cancelled`] after the entries already written stay written (the caller's
    /// staging directory is thrown away, as it was before the bytes arrived).
    pub fn extract_checked(
        &mut self,
        dest: &Path,
        policy: NamePolicy,
        select: &mut dyn FnMut(&str) -> Select,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<ExtractReport, ArchiveError> {
        self.written_so_far.clear();
        let path = self.label();
        std::fs::create_dir_all(dest).map_err(|error| ArchiveError::Destination {
            path: dest.display().to_string(),
            reason: error.to_string(),
        })?;

        let PlannedExtraction {
            write: plan,
            skipped,
            unwritable,
        } = self.plan(select)?;
        if !unwritable.is_empty() && policy == NamePolicy::Refuse {
            return Err(ArchiveError::UnsafeNames {
                path: path.clone(),
                entries: unwritable,
            });
        }

        let mut report = ExtractReport::default();
        let mut remaining_total = self.limits.max_total_bytes;
        let preset_total = self.limits.max_total_bytes;
        let preset = self.limits.preset;
        let format = self.format;
        let limit = self.limits.max_entry_bytes;
        let exact = self.format != ArchiveFormat::Rar;
        // The plan is in the archive's own order, so one walk of the archive meets every entry it
        // names exactly once. This table is how the walk finds its plan without searching it per
        // entry, and `delivered` is what makes an entry the archive never handed over a refusal
        // rather than a file that quietly is not there.
        let by_name: HashMap<&str, usize> = plan
            .iter()
            .enumerate()
            .map(|(index, (_, name, _))| (name.as_str(), index))
            .collect();
        let modes: Vec<Option<u32>> = {
            // One table for the whole plan: a fifty-thousand-entry world would otherwise scan the
            // listing once per entry to find its own mode.
            let modes_by_name: HashMap<&str, Option<u32>> = self
                .entries
                .iter()
                .map(|entry| (entry.name.as_str(), entry.unix_mode))
                .collect();
            plan.iter()
                .map(|(_, name, _)| modes_by_name.get(name.as_str()).copied().flatten())
                .collect()
        };
        let mut delivered = vec![false; plan.len()];
        let mut written_now: Vec<String> = Vec::new();

        let outcome = {
            let plan = &plan;
            let mut write_entry = |entry: &ArchiveEntry,
                                   bytes: &mut EntryBytes<'_>|
             -> Result<Visit, ArchiveError> {
                let Some(&index) = by_name.get(entry.name.as_str()) else {
                    return Ok(Visit::Next);
                };
                if delivered[index] {
                    return Ok(Visit::Next);
                }
                let (strict, name, declared) = &plan[index];
                let target = join_under(dest, strict).map_err(|error| match error {
                    PathError::UnsafePath { reason, .. } => ArchiveError::UnsafeNames {
                        path: path.clone(),
                        entries: vec![SkippedEntry {
                            name: name.clone(),
                            reason,
                        }],
                    },
                    other => ArchiveError::Destination {
                        path: dest.display().to_string(),
                        reason: other.to_string(),
                    },
                })?;
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| ArchiveError::Write {
                        path: parent.display().to_string(),
                        error: error.to_string(),
                    })?;
                }
                let file = std::fs::File::create(&target).map_err(|error| ArchiveError::Write {
                    path: target.display().to_string(),
                    error: error.to_string(),
                })?;
                let mut out = CountingFile {
                    file,
                    written: 0,
                    remaining_total,
                    context: CopyContext {
                        archive: path.clone(),
                        format,
                        preset,
                        limit,
                        entry: name.clone(),
                        declared: *declared,
                        exact,
                        cancelled,
                    },
                    preset_total,
                    preset,
                };
                let bytes_written = bytes(&mut out)?;
                remaining_total = remaining_total.saturating_sub(bytes_written);
                delivered[index] = true;
                keep_executable_bit(&target, modes[index]);
                written_now.push(strict.as_str().to_string());
                report.written.push(strict.as_str().to_string());
                report.bytes_written += bytes_written;
                Ok(Visit::Next)
            };
            self.walk(cancelled, &mut write_entry)
        };
        self.written_so_far = std::mem::take(&mut written_now);
        outcome?;

        if let Some(index) = delivered.iter().position(|done| !done) {
            return Err(ArchiveError::Corrupt {
                path,
                format: self.format,
                reason: format!(
                    "the listing showed '{}' and the archive did not deliver it",
                    plan[index].1
                ),
            });
        }
        report.skipped = skipped;
        Ok(report)
    }

    /// The paths an extraction wrote, including a run that then failed: a caller that promises to
    /// leave nothing behind after a refusal deletes these.
    pub fn written_so_far(&self) -> &[String] {
        &self.written_so_far
    }

    /// How many times the archive's bytes have been parsed for its entries (a test's view, §32.3).
    #[cfg(test)]
    pub(crate) fn parses(&self) -> usize {
        self.parses
    }

    /// Extract into a folder of its own beside `destination`, and publish it only if every entry was
    /// written.
    ///
    /// This is the shape an extraction into a **live** folder takes: `destination` is an instance's
    /// `config/`, with the player's own files in it, so a refusal half-way through may not have
    /// replaced any of them. The reader writes into `.<destination name>.agora-extract-<id>` next to
    /// it, removes that folder on any error, and hands back a [`StagedExtraction`] whose
    /// [`StagedExtraction::publish_into`] moves the files into place — the caller publishes, the
    /// reader never does (MASTER_SPEC §32.3).
    ///
    /// A caller whose destination is already a fresh folder of its own (a runtime's staging
    /// directory, a world's, a plugin package's) needs no staging on top: it calls
    /// [`SafeArchive::extract_checked`] with that folder.
    pub fn stage(
        &mut self,
        destination: &Path,
        policy: NamePolicy,
        select: &mut dyn FnMut(&str) -> Select,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<StagedExtraction, ArchiveError> {
        // The destination is settled first: made if it is missing, its links resolved once, and the
        // staging folder created beside *that* folder — so publishing is a rename on one disk and a
        // junctioned instance folder cannot turn the last step into a copy that fails half way.
        std::fs::create_dir_all(destination).map_err(|error| ArchiveError::Destination {
            path: destination.display().to_string(),
            reason: format!("could not be made: {error}"),
        })?;
        let resolved = destination
            .canonicalize()
            .map_err(|error| ArchiveError::Destination {
                path: destination.display().to_string(),
                reason: error.to_string(),
            })?;
        let root = fresh_folder_beside(&resolved).map_err(|error| ArchiveError::Destination {
            path: destination.display().to_string(),
            reason: format!("could not make a staging folder: {error}"),
        })?;
        match self.extract_checked(&root, policy, select, cancelled) {
            Ok(report) => Ok(StagedExtraction {
                root,
                destination: resolved,
                report,
                replaced: None,
            }),
            Err(error) => {
                let _ = std::fs::remove_dir_all(&root);
                Err(error)
            }
        }
    }
}

/// An extraction written to a folder of its own, not yet published.
///
/// Dropping one removes the folder, so a caller that returns early on a later error leaves nothing
/// behind. Publishing moves the files into the destination the extraction was staged for, and either
/// every file arrives or none does (MASTER_SPEC §32.3).
#[derive(Debug)]
pub struct StagedExtraction {
    root: PathBuf,
    /// The destination as it resolves, fixed when the extraction was staged: publishing is a rename
    /// beside the folder it was staged in, never across a volume.
    destination: PathBuf,
    report: ExtractReport,
    /// The folder beside the staging folder that holds files a publish replaced, while it runs.
    replaced: Option<PathBuf>,
}

impl StagedExtraction {
    /// The folder the files are in, for a caller that wants to publish it as a whole.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// What was written, and what was left out.
    pub fn report(&self) -> &ExtractReport {
        &self.report
    }

    /// Move every staged file into the destination, or move nothing at all.
    ///
    /// Three steps, in this order, because a half-published folder is worse than an untouched one:
    ///
    /// 1. **Every target is decided before anything moves.** `join_under` answers for each one — a
    ///    link on the way, or a file where the path needs a folder, ends the publish here, with the
    ///    path named and the destination untouched.
    /// 2. **A file that is being replaced is set aside**, into the staging folder's `replaced/`
    ///    area, before the new one is moved in.
    /// 3. **Any failure undoes the whole run in reverse**: the new files go back into staging and the
    ///    originals go back where they were. The staging folder is owned by this value until the very
    ///    end, so `Drop` removes it however the publish ends.
    ///
    /// A crash *during* the publish is a different matter — the process stops between two renames,
    /// which leaves some files new and some set aside. Putting that right needs a journal, which is
    /// the staged-apply work (MASTER_SPEC §32.4), not this function.
    pub fn publish_into(mut self) -> Result<ExtractReport, ArchiveError> {
        let destination = self.destination.clone();
        let staging = self.root.clone();

        // 1. Preflight.
        let mut planned: Vec<(StrictRelPath, PathBuf, PathBuf)> = Vec::new();
        for rel in &self.report.written {
            let refused = |reason: String| ArchiveError::Destination {
                path: destination.display().to_string(),
                reason: format!("'{rel}' cannot be published: {reason}"),
            };
            let strict =
                StrictRelPath::new(rel.clone()).map_err(|error| refused(error.to_string()))?;
            let target =
                join_under(&destination, &strict).map_err(|error| refused(error.to_string()))?;
            match std::fs::symlink_metadata(&target) {
                Ok(metadata) if metadata.is_dir() => {
                    return Err(refused("a folder is where the file goes".to_string()))
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(refused(error.to_string())),
            }
            let replacement =
                join_under(&staging, &strict).map_err(|error| refused(error.to_string()))?;
            planned.push((strict, replacement, target));
        }

        // 2. Move, setting aside what is replaced. The set-aside area is a folder of its own beside
        //    the staging folder, never inside it, so no entry of the archive can share its path.
        let replaced_root = fresh_folder_beside(&staging).map_err(|error| ArchiveError::Write {
            path: staging.display().to_string(),
            error: format!("could not make a folder for the files being replaced: {error}"),
        })?;
        self.replaced = Some(replaced_root.clone());
        let mut done: Vec<(PathBuf, PathBuf, Option<PathBuf>)> = Vec::new();
        let mut made_folders: Vec<PathBuf> = Vec::new();
        for (strict, from, target) in planned {
            let Some(parent) = target.parent() else {
                continue;
            };
            note_missing_folders(&destination, parent, &mut made_folders);
            if let Err(error) = std::fs::create_dir_all(parent) {
                return Err(self.undo(&done, &made_folders, None, parent, error));
            }
            let aside = if std::fs::symlink_metadata(&target).is_ok() {
                match join_under(&replaced_root, &strict) {
                    Ok(where_to) => {
                        if let Some(parent) = where_to.parent() {
                            note_missing_folders(&replaced_root, parent, &mut made_folders);
                            let _ = std::fs::create_dir_all(parent);
                        }
                        match std::fs::rename(&target, &where_to) {
                            Ok(()) => Some(where_to),
                            Err(error) => {
                                return Err(self.undo(&done, &made_folders, None, &target, error));
                            }
                        }
                    }
                    Err(error) => {
                        let error = std::io::Error::other(error.to_string());
                        return Err(self.undo(&done, &made_folders, None, &target, error));
                    }
                }
            } else {
                None
            };
            if let Err(error) = std::fs::rename(&from, &target) {
                let pending = aside.map(|aside| (target.clone(), aside));
                return Err(self.undo(&done, &made_folders, pending, &target, error));
            }
            done.push((target, from, aside));
        }

        // 3. Committed: both folders have done their job and Drop must not look for them again.
        self.root.clear();
        self.replaced = None;
        let _ = std::fs::remove_dir_all(&staging);
        let _ = std::fs::remove_dir_all(&replaced_root);
        Ok(std::mem::take(&mut self.report))
    }

    /// Undo a publish that could not finish: the new files go back into staging, in reverse order,
    /// and each original comes back where it was. A folder this run created and left empty goes too.
    ///
    /// `pending` is the file being moved when the publish failed, if it had already been set aside.
    /// If any original cannot be put back, both folders are **kept** (Drop would otherwise delete the
    /// set-aside originals with them) and the error says where the player's files are.
    fn undo(
        &mut self,
        done: &[(PathBuf, PathBuf, Option<PathBuf>)],
        made_folders: &[PathBuf],
        pending: Option<(PathBuf, PathBuf)>,
        failed_at: &Path,
        error: std::io::Error,
    ) -> ArchiveError {
        let mut stranded: Vec<String> = Vec::new();
        if let Some((target, aside)) = pending {
            if std::fs::rename(&aside, &target).is_err() {
                stranded.push(aside.display().to_string());
            }
        }
        for (target, from, aside) in done.iter().rev() {
            let _ = std::fs::rename(target, from);
            if let Some(aside) = aside {
                if std::fs::rename(aside, target).is_err() {
                    stranded.push(aside.display().to_string());
                }
            }
        }
        for folder in made_folders.iter().rev() {
            let _ = std::fs::remove_dir(folder);
        }
        if stranded.is_empty() {
            return ArchiveError::Write {
                path: failed_at.display().to_string(),
                error: error.to_string(),
            };
        }
        // Keep both folders: they hold the only copy of what could not be put back.
        self.root.clear();
        self.replaced = None;
        ArchiveError::Write {
            path: failed_at.display().to_string(),
            error: format!(
                "{error}; the publish was undone except for {} file(s) that could not be put back, \
                 which are kept at: {}",
                stranded.len(),
                stranded.join(", ")
            ),
        }
    }
}

/// The folders between `root` and `folder` that do not exist yet, so a publish that fails can take
/// back the empty ones it created.
fn note_missing_folders(root: &Path, folder: &Path, made: &mut Vec<PathBuf>) {
    let mut chain: Vec<&Path> = Vec::new();
    let mut current = Some(folder);
    while let Some(path) = current {
        if path == root {
            break;
        }
        chain.push(path);
        current = path.parent();
    }
    for path in chain.into_iter().rev() {
        if !path.exists() && !made.iter().any(|known| known == path) {
            made.push(path.to_path_buf());
        }
    }
}

impl Drop for StagedExtraction {
    fn drop(&mut self) {
        if !self.root.as_os_str().is_empty() {
            let _ = std::fs::remove_dir_all(&self.root);
        }
        if let Some(replaced) = &self.replaced {
            let _ = std::fs::remove_dir_all(replaced);
        }
    }
}

/// A folder beside `destination` that did not exist, for staging an extraction into.
///
/// Beside it, not in the system temporary folder, so publishing is a rename on the same disk and a
/// half-written file never appears in the destination.
fn fresh_folder_beside(destination: &Path) -> std::io::Result<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    let name = destination
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "extract".to_string());
    for _ in 0..64 {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".{name}.agora-extract-{}-{id}", std::process::id()));
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "no unused staging folder name was available",
    ))
}

/// An extraction decided but not yet performed: what would be written, what the caller's own rule
/// leaves out, and the names a disk cannot hold (which refuse the extraction unless the caller opted
/// in to doing without them).
#[derive(Debug, Clone, Default)]
struct PlannedExtraction {
    write: Vec<(StrictRelPath, String, u64)>,
    skipped: Vec<SkippedEntry>,
    unwritable: Vec<SkippedEntry>,
}

/// Where one entry's bytes are going, and what they are allowed to be. One shape for every format,
/// so the "declared size lied" and "passed the preset" messages read the same whichever decoder
/// delivered the bytes.
#[derive(Clone)]
struct CopyContext<'a> {
    archive: String,
    format: ArchiveFormat,
    preset: &'static str,
    limit: u64,
    entry: String,
    declared: u64,
    /// Whether the declared size is the archive's own binary header, which the bytes must match. A
    /// RAR listing is a text report from a process, so its numbers bound the read rather than pin it.
    exact: bool,
    /// Asked between buffers as well as between entries: a single entry can be gigabytes long, and a
    /// person who asked to stop should not wait for it to finish.
    cancelled: &'a dyn Fn() -> bool,
}

/// Keep the one permission bit a downloaded archive is allowed to set.
///
/// A game runtime's `bin/java` has to be executable, so the bit travels with the file. Everything
/// else in the mode is dropped: `setuid`, `setgid` and sticky bits from an archive would let a
/// downloaded file run as whoever unpacks it, which no launcher should arrange.
fn keep_executable_bit(path: &Path, mode: Option<u32>) {
    #[cfg(unix)]
    {
        if let Some(mode) = mode {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o755));
        }
    }
    #[cfg(not(unix))]
    {
        // Windows has no mode to keep: an `.exe` is executable by its extension.
        let _ = (path, mode);
    }
}

/// The compression ratio of one entry. A zero compressed size that declares bytes means the archive
/// claims to store something for nothing: the largest ratio a `u64` can say.
fn ratio_of(uncompressed: u64, compressed: u64) -> u64 {
    if uncompressed == 0 {
        return 1;
    }
    if compressed == 0 {
        return u64::MAX;
    }
    uncompressed / compressed
}

/// Look at the first bytes and say what the archive is. Content decides, never the extension: a
/// `.zip` that is really a 7z is read as a 7z, and a file that is neither is refused.
fn detect_format(source: &Source) -> Result<ArchiveFormat, ArchiveError> {
    let mut head = [0u8; 512];
    let read = match source {
        Source::Path(path) => {
            let mut file = std::fs::File::open(path).map_err(|error| ArchiveError::Unreadable {
                path: path.display().to_string(),
                error: error.to_string(),
            })?;
            read_up_to(&mut file, &mut head, &source.label())?
        }
        Source::Bytes(bytes) => {
            let take = head.len().min(bytes.len());
            head[..take].copy_from_slice(&bytes[..take]);
            take
        }
    };
    let zip_magic = read >= 4
        && (&head[..4] == b"PK\x03\x04"
            || &head[..4] == b"PK\x05\x06"
            || &head[..4] == b"PK\x07\x08");
    if zip_magic {
        return Ok(ArchiveFormat::Zip);
    }
    if read >= 6 && head[..6] == [0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C] {
        return Ok(ArchiveFormat::SevenZ);
    }
    if (read >= 7 && &head[..7] == b"Rar!\x1a\x07\x00")
        || (read >= 8 && &head[..8] == b"Rar!\x1a\x07\x01\x00")
    {
        return Ok(ArchiveFormat::Rar);
    }
    if read >= 2 && head[0] == 0x1f && head[1] == 0x8b {
        return Ok(ArchiveFormat::TarGz);
    }
    if read >= 262 && &head[257..262] == b"ustar" {
        return Ok(ArchiveFormat::Tar);
    }
    Err(ArchiveError::UnknownFormat {
        path: source.label(),
    })
}

fn read_up_to<R: Read>(
    reader: &mut R,
    buffer: &mut [u8],
    path: &str,
) -> Result<usize, ArchiveError> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                return Err(ArchiveError::Unreadable {
                    path: path.to_string(),
                    error: error.to_string(),
                })
            }
        }
    }
    Ok(filled)
}

/// Copy one entry's bytes into `out`, refusing the moment they disagree with its own declaration: a
/// header that lies about its size is caught at the first byte it did not declare.
///
/// The caller's "stop" is asked between buffers here too, so a person stopping a download-sized
/// entry is answered within a buffer rather than when the entry ends.
fn copy_bounded(
    reader: &mut dyn Read,
    out: &mut dyn Write,
    context: &CopyContext<'_>,
) -> Result<u64, ArchiveError> {
    let mut buffer = vec![0u8; 64 * 1024];
    let mut written = 0u64;
    loop {
        if (context.cancelled)() {
            return Err(ArchiveError::Cancelled {
                path: context.archive.clone(),
            });
        }
        let read = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                return Err(ArchiveError::Corrupt {
                    path: context.archive.clone(),
                    format: context.format,
                    reason: format!("entry '{}': {error}", context.entry),
                })
            }
        };
        written = written.saturating_add(read as u64);
        if written > context.limit {
            return Err(ArchiveError::TooLarge {
                path: context.archive.clone(),
                format: context.format,
                preset: context.preset,
                entry: context.entry.clone(),
                bytes: written,
                limit: context.limit,
            });
        }
        if context.exact && written > context.declared {
            return Err(ArchiveError::EntrySizeLies {
                path: context.archive.clone(),
                entry: context.entry.clone(),
                declared: context.declared,
                delivered: written,
            });
        }
        out.write_all(&buffer[..read])
            .map_err(|error| ArchiveError::Write {
                path: context.entry.clone(),
                error: error.to_string(),
            })?;
    }
    if context.exact && written != context.declared {
        return Err(ArchiveError::EntrySizeLies {
            path: context.archive.clone(),
            entry: context.entry.clone(),
            declared: context.declared,
            delivered: written,
        });
    }
    Ok(written)
}

/// The name a tar entry stores, or a refusal: a name that is not UTF-8 cannot be held to the rules.
fn entry_name_of<R: Read>(
    entry: &tar::Entry<R>,
    format: ArchiveFormat,
    path: &str,
) -> Result<String, ArchiveError> {
    let entry_path = entry.path_bytes();
    match std::str::from_utf8(&entry_path) {
        Ok(name) => Ok(name.to_string()),
        Err(_) => Err(ArchiveError::Corrupt {
            path: path.to_string(),
            format,
            reason: "an entry's name is not UTF-8, so it cannot be checked".to_string(),
        }),
    }
}

fn list_tar(
    reader: Box<dyn Read>,
    format: ArchiveFormat,
    path: &str,
    limits: &ArchiveLimits,
) -> Result<Vec<ArchiveEntry>, ArchiveError> {
    let mut archive = tar::Archive::new(reader);
    let listed = archive.entries().map_err(|error| ArchiveError::Corrupt {
        path: path.to_string(),
        format,
        reason: format!("{error}"),
    })?;
    let mut entries: Vec<ArchiveEntry> = Vec::new();
    let mut files = 0usize;
    let mut total = 0u64;
    for entry in listed {
        let entry = entry.map_err(|error| ArchiveError::Corrupt {
            path: path.to_string(),
            format,
            reason: format!("{error}"),
        })?;
        let name = entry_name_of(&entry, format, path)?;
        let kind = match entry.header().entry_type() {
            tar::EntryType::Directory => EntryKind::Directory,
            tar::EntryType::Regular | tar::EntryType::Continuous => EntryKind::File,
            // A link, a device, a FIFO or anything else tar can hold writes a name that is not a
            // file, and every one of them is a way out of the destination.
            _ => EntryKind::Link,
        };
        let size = entry.size();
        if kind == EntryKind::File {
            files += 1;
            total = total.saturating_add(size);
        }
        // A tar.gz is decompressed just to be listed, so the stream is refused the moment it claims
        // more than the preset allows rather than after all of it has been read (MASTER_SPEC §32.3).
        // A folder record counts where the preset counts folders: an archive of empty folders is
        // otherwise the one shape that passes a count no file ever reaches.
        let counted = if limits.counts_folders {
            entries.len() + 1
        } else {
            files
        };
        hold_running_listing(limits, format, path, &name, counted, size, total)?;
        entries.push(ArchiveEntry {
            name,
            kind,
            size,
            compressed: None,
            unix_mode: Some(entry.header().mode().unwrap_or(0o644)),
        });
    }
    // A `tar.gz`'s gzip trailer — the CRC-32 and the uncompressed length — sits *after* the tar end
    // marker. Stopping at the marker never looks at it, so a stream whose CRC is wrong or whose
    // trailer is cut short is reported as a good archive (MASTER_SPEC §32.13).
    drain_stream(archive.into_inner(), format, path)?;
    Ok(entries)
}

/// Read a decompressed stream to its end, so the decoder checks its own trailer.
///
/// For a plain tar this reads the padding after the end marker and finds nothing; for a `tar.gz` it
/// is the only place the gzip trailer is checked at all.
fn drain_stream(
    mut reader: Box<dyn Read>,
    format: ArchiveFormat,
    path: &str,
) -> Result<(), ArchiveError> {
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                return Err(ArchiveError::Corrupt {
                    path: path.to_string(),
                    format,
                    reason: format!("the stream does not end where its trailer says: {error}"),
                })
            }
        }
    }
}

/// Drain one member of a solid 7z block that nobody asked for, asking between buffers whether the
/// person who started this has stopped.
///
/// The bytes have to be read for the next member to decode from where it should — that is the solid
/// block's own rule — but a member can be gigabytes long, and "stop" that waits for it is not a stop.
fn drain_unwanted(
    mut reader: impl Read,
    cancelled: &dyn Fn() -> bool,
    path: &str,
    entry: &str,
) -> Result<(), ArchiveError> {
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        if cancelled() {
            return Err(ArchiveError::Cancelled {
                path: path.to_string(),
            });
        }
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                return Err(ArchiveError::Corrupt {
                    path: path.to_string(),
                    format: ArchiveFormat::SevenZ,
                    reason: format!("entry '{entry}': {error}"),
                })
            }
        }
    }
}

/// The count and total a listing has reached so far, held to the preset.
///
/// Shared by the walk over a finished listing and by the walk over a `tar.gz` as it decompresses,
/// which is the same rules decided at two moments: a compressed tar has to be inflated to be listed
/// at all, so refusing it only once every entry has been read is refusing it after the work was done.
fn hold_running_listing(
    limits: &ArchiveLimits,
    format: ArchiveFormat,
    path: &str,
    entry: &str,
    counted: usize,
    size: u64,
    total: u64,
) -> Result<(), ArchiveError> {
    if !limits.bounds_the_listing {
        return Ok(());
    }
    if size > limits.max_entry_bytes {
        return Err(ArchiveError::TooLarge {
            path: path.to_string(),
            format,
            preset: limits.preset,
            entry: entry.to_string(),
            bytes: size,
            limit: limits.max_entry_bytes,
        });
    }
    if total > limits.max_total_bytes {
        return Err(ArchiveError::TooLarge {
            path: path.to_string(),
            format,
            preset: limits.preset,
            entry: entry.to_string(),
            bytes: total,
            limit: limits.max_total_bytes,
        });
    }
    if counted > limits.max_entries {
        return Err(ArchiveError::TooManyEntries {
            path: path.to_string(),
            format,
            preset: limits.preset,
            count: counted,
            limit: limits.max_entries,
        });
    }
    Ok(())
}
/// The entry names a zip declares more than once, and nothing else.
///
/// [`SafeArchive::open`] refuses such an archive, so a caller that reads its own listing does not
/// need this: it exists for one that streams a jar itself and must still answer "is any entry hiding
/// behind another name" with the reader's rule rather than its own copy. Empty for a zip whose names
/// are all distinct; a file that is not a zip is [`ArchiveError::UnknownFormat`].
pub fn duplicate_zip_entry_names(path: &Path) -> Result<Vec<String>, ArchiveError> {
    let source = Source::from_path(path)?;
    if detect_format(&source)? != ArchiveFormat::Zip {
        return Err(ArchiveError::UnknownFormat {
            path: source.label(),
        });
    }
    let mut reader = match &source {
        Source::Path(path) => {
            let file = std::fs::File::open(path).map_err(|error| ArchiveError::Unreadable {
                path: path.display().to_string(),
                error: error.to_string(),
            })?;
            Box::new(file) as Box<dyn ReadSeek>
        }
        Source::Bytes(bytes) => Box::new(std::io::Cursor::new(Arc::clone(bytes))),
    };
    let label = source.label();
    let start = {
        let archive =
            zip::ZipArchive::new(reader.as_mut()).map_err(|error| ArchiveError::Corrupt {
                path: label.clone(),
                format: ArchiveFormat::Zip,
                reason: format!("{error}"),
            })?;
        archive.central_directory_start()
    };
    let declared = zip_declared_entry_names(reader.as_mut(), start).map_err(|error| {
        ArchiveError::Corrupt {
            path: label,
            format: ArchiveFormat::Zip,
            reason: format!("its central directory could not be read: {error}"),
        }
    })?;
    Ok(names_declared_more_than_once(&declared))
}

/// The file names a zip's own central directory declares, in the order it declares them.
///
/// `zip::ZipArchive` presents one entry per *name*, so it cannot answer "does this archive hold two
/// records with the same name". Reading the directory's fixed headers can: the name length is at
/// offset 28, the extra and comment lengths at 30 and 32, and a name needs no zip64 field to be read.
/// A directory record (a name ending in `/`) is left out: one folder recorded twice is the same
/// folder, not two different contents under one name.
/// One name a zip's central directory declares, and whether that record is a folder.
struct DeclaredName {
    name: String,
    is_directory: bool,
    /// The Info-ZIP Unicode Path (0x7075) spelling of the same record, when it carries one whose
    /// check value matches. A second spelling of one name, not a second path: nothing is ever
    /// written under it, and it is only here so that two records that claim one path are refused.
    alias: Option<String>,
}

fn zip_declared_entry_names(
    reader: &mut dyn ReadSeek,
    start: u64,
) -> std::io::Result<Vec<DeclaredName>> {
    use std::io::SeekFrom;
    const SIGNATURE: [u8; 4] = [0x50, 0x4b, 0x01, 0x02];
    reader.seek(SeekFrom::Start(start))?;
    let mut header = [0u8; 46];
    let mut names = Vec::new();
    loop {
        match reader.read_exact(&mut header) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(error),
        }
        if header[..4] != SIGNATURE {
            break;
        }
        let field = |at: usize| usize::from(u16::from_le_bytes([header[at], header[at + 1]]));
        let (name_length, extra_length, comment_length) = (field(28), field(30), field(32));
        let mut raw_name = vec![0u8; name_length];
        reader.read_exact(&mut raw_name)?;
        let mut extra = vec![0u8; extra_length];
        reader.read_exact(&mut extra)?;
        reader.seek_relative(comment_length as i64)?;
        // Decoded the way the zip crate decodes it, so two records it reads as one name are seen
        // as one name here too, and two it reads as different names are not collapsed into one by a
        // lossy conversion (MASTER_SPEC §32.3). The Info-ZIP Unicode Path field is a *second*
        // spelling of the same record, and it counts whatever the general-purpose flag says.
        let name = decode_zip_name(&raw_name);
        let is_directory = name.ends_with('/');
        let name = name.trim_end_matches('/').to_string();
        let alias = unicode_path_name(&raw_name, &extra)
            .map(|unicode| unicode.trim_end_matches('/').to_string())
            .filter(|unicode| *unicode != name);
        names.push(DeclaredName {
            name,
            is_directory,
            alias,
        });
    }
    Ok(names)
}

/// The name a zip record declares, decoded the way the zip crate decodes it.
///
/// `zip` 9.0.5 decides by the bytes themselves, not by the general-purpose flag: `[0xC3,0xA9]` is
/// `é` in UTF-8 and a UTF-8 flag would be redundant, while `[0x82]` is `é` in CP437 and cannot be
/// UTF-8 at all (`ZipFileData::name`). Decoding by the flag instead would name an entry something
/// the extractor never writes, and would miss two records the crate reads as one name.
fn decode_zip_name(raw: &[u8]) -> String {
    match std::str::from_utf8(raw) {
        Ok(text) => text.to_string(),
        Err(_) => cp437(raw),
    }
}

/// The 0x7075 field: a UTF-8 spelling of the name, valid only when its CRC is the raw name's.
fn unicode_path_name(raw: &[u8], extra: &[u8]) -> Option<String> {
    const UNICODE_PATH: u16 = 0x7075;
    let mut rest = extra;
    while rest.len() >= 4 {
        let id = u16::from_le_bytes([rest[0], rest[1]]);
        let size = usize::from(u16::from_le_bytes([rest[2], rest[3]]));
        let data = rest.get(4..4 + size)?;
        rest = rest.get(4 + size..).unwrap_or_default();
        if id != UNICODE_PATH || size < 5 || data[0] != 1 {
            continue;
        }
        let expected = u32::from_le_bytes([data[1], data[2], data[3], data[4]]);
        let mut crc = flate2::Crc::new();
        crc.update(raw);
        if crc.sum() != expected {
            continue;
        }
        return String::from_utf8(data[5..].to_vec()).ok();
    }
    None
}

/// CP437's upper half, which is what those bytes mean in a zip that predates UTF-8 names.
const CP437_HIGH: [char; 128] = [
    '\u{00C7}', '\u{00FC}', '\u{00E9}', '\u{00E2}', '\u{00E4}', '\u{00E0}', '\u{00E5}', '\u{00E7}',
    '\u{00EA}', '\u{00EB}', '\u{00E8}', '\u{00EF}', '\u{00EE}', '\u{00EC}', '\u{00C4}', '\u{00C5}',
    '\u{00C9}', '\u{00E6}', '\u{00C6}', '\u{00F4}', '\u{00F6}', '\u{00F2}', '\u{00FB}', '\u{00F9}',
    '\u{00FF}', '\u{00D6}', '\u{00DC}', '\u{00A2}', '\u{00A3}', '\u{00A5}', '\u{20A7}', '\u{0192}',
    '\u{00E1}', '\u{00ED}', '\u{00F3}', '\u{00FA}', '\u{00F1}', '\u{00D1}', '\u{00AA}', '\u{00BA}',
    '\u{00BF}', '\u{2310}', '\u{00AC}', '\u{00BD}', '\u{00BC}', '\u{00A1}', '\u{00AB}', '\u{00BB}',
    '\u{2591}', '\u{2592}', '\u{2593}', '\u{2502}', '\u{2524}', '\u{2561}', '\u{2562}', '\u{2556}',
    '\u{2555}', '\u{2563}', '\u{2551}', '\u{2557}', '\u{255D}', '\u{255C}', '\u{255B}', '\u{2510}',
    '\u{2514}', '\u{2534}', '\u{252C}', '\u{251C}', '\u{2500}', '\u{253C}', '\u{255E}', '\u{255F}',
    '\u{255A}', '\u{2554}', '\u{2569}', '\u{2566}', '\u{2560}', '\u{2550}', '\u{256C}', '\u{2567}',
    '\u{2568}', '\u{2564}', '\u{2565}', '\u{2559}', '\u{2558}', '\u{2552}', '\u{2553}', '\u{256B}',
    '\u{256A}', '\u{2518}', '\u{250C}', '\u{2588}', '\u{2584}', '\u{258C}', '\u{2590}', '\u{2580}',
    '\u{03B1}', '\u{00DF}', '\u{0393}', '\u{03C0}', '\u{03A3}', '\u{03C3}', '\u{00B5}', '\u{03C4}',
    '\u{03A6}', '\u{0398}', '\u{03A9}', '\u{03B4}', '\u{221E}', '\u{03C6}', '\u{03B5}', '\u{2229}',
    '\u{2261}', '\u{00B1}', '\u{2265}', '\u{2264}', '\u{2320}', '\u{2321}', '\u{00F7}', '\u{2248}',
    '\u{00B0}', '\u{2219}', '\u{00B7}', '\u{221A}', '\u{207F}', '\u{00B2}', '\u{25A0}', '\u{00A0}',
];

fn cp437(raw: &[u8]) -> String {
    raw.iter()
        .map(|byte| {
            if *byte < 0x80 {
                char::from(*byte)
            } else {
                CP437_HIGH[usize::from(*byte) - 0x80]
            }
        })
        .collect()
}

/// The names that appear more than once, each once, in the order they were first seen.
///
/// A folder recorded twice is one folder and is left alone. A name that is a file once and a folder
/// once is not: no disk holds both, and whichever the reader picked would be the other one silently
/// lost. A record's Unicode Path spelling counts as a spelling of *that* record, so two records that
/// claim one path are refused whichever spelling each of them was written with.
fn names_declared_more_than_once(names: &[DeclaredName]) -> Vec<String> {
    let mut files: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut folders: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut duplicated: Vec<String> = Vec::new();
    for declared in names {
        let mut spellings: Vec<&str> = vec![declared.name.as_str()];
        if let Some(alias) = declared.alias.as_deref() {
            if alias != declared.name {
                spellings.push(alias);
            }
        }
        for spelling in spellings {
            let seen_twice = if declared.is_directory {
                // A folder recorded twice is one folder: archive tools write the parent before every
                // child, and refusing that would refuse ordinary archives.
                folders.insert(spelling);
                false
            } else {
                !files.insert(spelling)
            };
            let clash = files.contains(spelling) && folders.contains(spelling);
            if (seen_twice || clash) && !duplicated.iter().any(|known| known == spelling) {
                duplicated.push(spelling.to_string());
            }
        }
    }
    duplicated
}

fn zip_entry_kind<R: Read + ?Sized>(entry: &zip::read::ZipFile<'_, R>, name: &str) -> EntryKind {
    // A record that claims a symlink's mode is a link however it is spelled, and that is asked
    // first: a link written as `textures/` rather than `textures` must not pass as a folder.
    if let Some(mode) = entry.unix_mode() {
        if (mode & 0o170000) == 0o120000 {
            return EntryKind::Link;
        }
    }
    if entry.is_dir() || name.ends_with('/') {
        return EntryKind::Directory;
    }
    EntryKind::File
}

/// The Unix mode a 7z entry stores, in the high half of its Windows attributes, where it stores one.
fn sevenz_unix_mode(entry: &sevenz_rust2::ArchiveEntry) -> Option<u32> {
    if !entry.has_windows_attributes {
        return None;
    }
    let mode = entry.windows_attributes >> 16;
    (mode != 0).then_some(mode)
}

fn claims_to_be_a_link(entry: &sevenz_rust2::ArchiveEntry) -> bool {
    if !entry.has_windows_attributes {
        return false;
    }
    let attributes = entry.windows_attributes;
    let is_reparse_point = (attributes & 0x0400) != 0;
    let unix_mode = (attributes >> 16) as u16;
    let is_unix_symlink = (unix_mode & 0o170000) == 0o120000;
    is_reparse_point || is_unix_symlink
}

fn sevenz_error(path: &str, error: sevenz_rust2::Error) -> ArchiveError {
    match error {
        sevenz_rust2::Error::PasswordRequired | sevenz_rust2::Error::MaybeBadPassword(_) => {
            ArchiveError::PasswordProtected {
                path: path.to_string(),
            }
        }
        other => ArchiveError::Corrupt {
            path: path.to_string(),
            format: ArchiveFormat::SevenZ,
            reason: format!("{other}"),
        },
    }
}

/// A writer that holds a `RarReader`'s output to the same declaration every other format is held to.
struct Sink<'a> {
    out: &'a mut dyn Write,
    context: &'a CopyContext<'a>,
    written: u64,
}

impl<'a> Sink<'a> {
    fn new(out: &'a mut dyn Write, context: &'a CopyContext<'a>) -> Self {
        Self {
            out,
            context,
            written: 0,
        }
    }

    fn finish(self) -> Result<(), ArchiveError> {
        if self.context.exact && self.written != self.context.declared {
            return Err(ArchiveError::EntrySizeLies {
                path: self.context.archive.clone(),
                entry: self.context.entry.clone(),
                declared: self.context.declared,
                delivered: self.written,
            });
        }
        Ok(())
    }
}

impl Write for Sink<'_> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let after = self.written.saturating_add(buffer.len() as u64);
        let over_declared = self.context.exact && after > self.context.declared;
        if over_declared || after > self.context.limit {
            return Err(std::io::Error::other(
                "entry delivered more bytes than it declared",
            ));
        }
        self.out.write_all(buffer)?;
        self.written = after;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}

/// The file one extraction writes, holding the whole run to the preset's total as well as this one
/// entry to its declaration.
struct CountingFile<'a> {
    file: std::fs::File,
    written: u64,
    remaining_total: u64,
    context: CopyContext<'a>,
    preset_total: u64,
    preset: &'a str,
}

impl Write for CountingFile<'_> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let after = self.written.saturating_add(buffer.len() as u64);
        if after > self.remaining_total {
            return Err(std::io::Error::other(format!(
                "extraction passed the {} bytes a {} archive may hold",
                self.preset_total, self.preset
            )));
        }
        if let Err(error) = self.file.write_all(buffer) {
            return Err(std::io::Error::new(
                error.kind(),
                format!("cannot write '{}': {error}", self.context.entry),
            ));
        }
        self.written = after;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests;
