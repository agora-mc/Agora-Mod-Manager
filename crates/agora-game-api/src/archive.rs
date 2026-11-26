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
//!   `JAR_METADATA`, `RUNTIME`, `MOD_ARCHIVE`) instead of inventing numbers, so two call sites that
//!   do the same job cannot drift apart.
//! * **Fail closed.** A corrupt, truncated, password-protected or unreadable archive is an error
//!   with the reason. An entry whose declared size lies is refused at the byte where the lie shows.
//!   There is no "empty archive" answer to a question that could not be answered.
//!
//! RAR is read out of process on Windows, by `tar.exe`. Starting a process is not this contract's
//! job, so this module declares the [`RarReader`] seam and core supplies an implementation.

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
/// | `MOD_ARCHIVE` | 5 000 (technic zip) | 500 MiB (`MAX_TECHNIC_ENTRY_BYTES`) | 2 GiB (`MAX_TECHNIC_ZIP_BYTES`) | 200 (new) |
/// | `BACKUP` | 10 000 (`MAX_BACKUP_ENTRIES`) | 1 GiB (`MAX_BACKUP_SINGLE_FILE_SIZE`) | 64 GiB (`MAX_BACKUP_TOTAL_SIZE`) | none (Agora wrote it) |
/// | `PLUGIN_PACKAGE` | 2 000 (`install::MAX_ENTRIES`) | 16 MiB (`MAX_ENTRY_BYTES`) | 64 MiB (`MAX_TOTAL_BYTES`) | 200 (`MAX_EXPANSION_RATIO`) |
/// | `JAR_METADATA` | 10 000 (none today) | 32 MiB (`jar_metadata::MAX_ENTRY_BYTES`) | 128 MiB (`MAX_TOTAL_NESTED_BYTES`) | none |
/// | `RUNTIME` | 10 000 (`runtime_manager::MAX_ENTRIES`) | 500 MiB (`PER_ENTRY_LIMIT`) | 2 GiB (`AGGREGATE_LIMIT`) | none today |
///
/// The ratio rules marked *new* close a hole: neither call site had one, and a compression ratio is
/// the check that catches a bomb while its declared sizes still look reasonable. 200 is the value
/// the plugin package already used.
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
    /// Whether [`Self::max_entry_bytes`] also refuses the whole archive at listing time.
    ///
    /// It is `true` for a job that reads or writes every entry, where one oversized entry is a
    /// refusal whatever happens next. A job that reads only some entries (a jar's metadata) says
    /// `false`: a 40 MiB texture no metadata reader ever asks for is no reason to lose the mod's id,
    /// and the limit still applies to each entry as it is read.
    pub per_entry_at_listing: bool,
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
        per_entry_at_listing: true,
    };

    /// A mod archive added to the content store, or a technic-style zip.
    pub const MOD_ARCHIVE: Self = Self {
        preset: "mod archive",
        max_entries: 5_000,
        max_entry_bytes: 500 * MIB,
        max_total_bytes: 2 * GIB,
        max_ratio: 200,
        per_entry_at_listing: true,
    };

    /// An instance backup or a snapshot: written by Agora, so its sizes are its own claim. The
    /// ratio is not checked, because a backup of already-compressed data would trip it.
    pub const BACKUP: Self = Self {
        preset: "backup",
        max_entries: 10_000,
        max_entry_bytes: GIB,
        max_total_bytes: 64 * GIB,
        max_ratio: 0,
        per_entry_at_listing: true,
    };

    /// A downloaded plugin package: small, and code, so the tightest preset.
    pub const PLUGIN_PACKAGE: Self = Self {
        preset: "plugin package",
        max_entries: 2_000,
        max_entry_bytes: 16 * MIB,
        max_total_bytes: 64 * MIB,
        max_ratio: 200,
        per_entry_at_listing: true,
    };

    /// Reading metadata out of a jar, nested jars included. The nesting depth and the number of
    /// nested jars are the caller's business; this bounds one archive's entries and bytes.
    ///
    /// The per-entry limit applies to each entry the reader asks for, not to the listing: a mod jar's
    /// big texture is never read, so it cannot fail the jar's metadata.
    pub const JAR_METADATA: Self = Self {
        preset: "jar metadata",
        max_entries: 10_000,
        max_entry_bytes: 32 * MIB,
        max_total_bytes: 128 * MIB,
        max_ratio: 0,
        per_entry_at_listing: false,
    };

    /// A downloaded game runtime: a JRE zip or tar.gz. Large files, few entries, and Agora checks
    /// the download against its published hash before this sees it.
    pub const RUNTIME: Self = Self {
        preset: "runtime",
        max_entries: 10_000,
        max_entry_bytes: 500 * MIB,
        max_total_bytes: 2 * GIB,
        max_ratio: 0,
        per_entry_at_listing: true,
    };

    /// A world a player exported, or one they downloaded: a folder of region files, one of which
    /// may be hundreds of megabytes. Carried over from `worlds::MAX_IMPORT_ENTRIES` and
    /// `MAX_IMPORT_BYTES`, which had no per-entry limit of its own, so the per-entry limit is the
    /// total: nothing a world archive holds is refused for its size alone.
    pub const WORLD_ARCHIVE: Self = Self {
        preset: "world archive",
        max_entries: 200_000,
        max_entry_bytes: 16 * GIB,
        max_total_bytes: 16 * GIB,
        max_ratio: 0,
        per_entry_at_listing: true,
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
    /// One entry's bytes, written to `out`. Returns how many bytes it wrote.
    fn read_entry(&self, archive: &Path, name: &str, out: &mut dyn Write) -> Result<u64, String>;
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
        // A compressed tar stores no per-entry compressed size, so the ratio it can be held to is
        // what the whole archive claims against how large it is.
        if archive.format == ArchiveFormat::TarGz && archive.limits.max_ratio > 0 {
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
    pub fn read_into(&mut self, name: &str, out: &mut dyn Write) -> Result<u64, ArchiveError> {
        let declared = self
            .entries
            .iter()
            .find(|entry| entry.name == name)
            .map(|entry| entry.size);
        let Some(declared) = declared else {
            return Err(ArchiveError::Corrupt {
                path: self.label(),
                format: self.format,
                reason: format!("no entry is named '{name}'"),
            });
        };
        self.copy_entry(name, declared, out)
    }

    /// Every file entry's bytes, in archive order, into one buffer each.
    pub fn read_all(&mut self) -> Result<Vec<(String, Vec<u8>)>, ArchiveError> {
        let names: Vec<(String, u64)> = self
            .file_entries()
            .map(|entry| (entry.name.clone(), entry.size))
            .collect();
        let mut files = Vec::with_capacity(names.len());
        for (name, declared) in names {
            let mut bytes = Vec::with_capacity(declared as usize);
            self.copy_entry(&name, declared, &mut bytes)?;
            files.push((name, bytes));
        }
        Ok(files)
    }

    fn copy_entry(
        &mut self,
        name: &str,
        declared: u64,
        out: &mut dyn Write,
    ) -> Result<u64, ArchiveError> {
        let path = self.label();
        let context = CopyContext {
            archive: path,
            format: self.format,
            preset: self.limits.preset,
            limit: self.limits.max_entry_bytes,
            entry: name.to_string(),
            declared,
            exact: self.format != ArchiveFormat::Rar,
        };
        match self.format {
            ArchiveFormat::Zip => {
                let index = self
                    .entries
                    .iter()
                    .position(|entry| entry.name == name)
                    .ok_or_else(|| ArchiveError::Corrupt {
                        path: self.label(),
                        format: ArchiveFormat::Zip,
                        reason: format!("no entry is named '{name}'"),
                    })?;
                let mut source = self.open_reader()?;
                let mut zip = zip::ZipArchive::new(source.as_mut()).map_err(|error| {
                    ArchiveError::Corrupt {
                        path: context.archive.clone(),
                        format: ArchiveFormat::Zip,
                        reason: format!("{error}"),
                    }
                })?;
                let mut entry = zip.by_index(index).map_err(|error| ArchiveError::Corrupt {
                    path: context.archive.clone(),
                    format: ArchiveFormat::Zip,
                    reason: format!("{error}"),
                })?;
                copy_bounded(&mut entry, out, &context)
            }
            ArchiveFormat::Tar => {
                let source = self.open_reader()?;
                copy_from_tar(source, name, out, &context)
            }
            ArchiveFormat::TarGz => {
                let source = self.open_reader()?;
                let decoder = flate2::read::GzDecoder::new(source);
                copy_from_tar(Box::new(decoder) as Box<dyn Read>, name, out, &context)
            }
            ArchiveFormat::SevenZ => self.copy_from_sevenz(name, out, &context, declared),
            ArchiveFormat::Rar => {
                let Some(rar) = self.rar.as_deref() else {
                    return Err(ArchiveError::NoRarReader {
                        path: context.archive.clone(),
                    });
                };
                let Source::Path(archive) = &self.source else {
                    return Err(ArchiveError::NoRarReader { path: self.label() });
                };
                let mut sink = Sink::new(out, &context);
                let written = rar.read_entry(archive, name, &mut sink).map_err(|reason| {
                    ArchiveError::Rar {
                        path: context.archive.clone(),
                        reason,
                    }
                })?;
                sink.finish()?;
                Ok(written)
            }
        }
    }

    fn copy_from_sevenz(
        &mut self,
        name: &str,
        out: &mut dyn Write,
        context: &CopyContext,
        declared: u64,
    ) -> Result<u64, ArchiveError> {
        let mut source = self.open_reader()?;
        let mut reader =
            sevenz_rust2::ArchiveReader::new(&mut source, sevenz_rust2::Password::empty())
                .map_err(|error| sevenz_error(&context.archive, error))?;
        let mut failure: Option<ArchiveError> = None;
        let mut written = 0u64;
        let wanted = name.to_string();
        let result = reader.for_each_entries(|entry, entry_reader| {
            if entry.name != wanted || entry.is_directory || entry.is_anti_item {
                return Ok(true);
            }
            let local = CopyContext {
                declared,
                ..context.clone()
            };
            match copy_bounded(entry_reader, out, &local) {
                Ok(bytes) => {
                    written = bytes;
                    Ok(false)
                }
                Err(error) => {
                    failure = Some(error);
                    Err(sevenz_rust2::Error::Other("stopped".into()))
                }
            }
        });
        if let Some(error) = failure {
            return Err(error);
        }
        match result {
            Ok(()) => {
                if written == 0 && declared > 0 {
                    return Err(ArchiveError::Corrupt {
                        path: context.archive.clone(),
                        format: ArchiveFormat::SevenZ,
                        reason: format!("entry '{name}' could not be read"),
                    });
                }
                Ok(written)
            }
            Err(error) => Err(sevenz_error(&context.archive, error)),
        }
    }

    /// List the archive's entries, holding every one of them to the preset.
    fn list(&mut self) -> Result<Vec<ArchiveEntry>, ArchiveError> {
        let entries = match self.format {
            ArchiveFormat::Zip => self.list_zip()?,
            ArchiveFormat::SevenZ => self.list_sevenz()?,
            ArchiveFormat::Tar => {
                let source = self.open_reader()?;
                list_tar(source, ArchiveFormat::Tar, &self.label())?
            }
            ArchiveFormat::TarGz => {
                let source = self.open_reader()?;
                let decoder = flate2::read::GzDecoder::new(source);
                list_tar(Box::new(decoder), ArchiveFormat::TarGz, &self.label())?
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
        // here; a zip hides the second behind the first, so `list_zip` counts that one itself.
        let file_names: Vec<String> = entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::File)
            .map(|entry| entry.name.clone())
            .collect();
        let duplicated = names_declared_more_than_once(&file_names);
        if !duplicated.is_empty() {
            return Err(ArchiveError::DuplicateNames {
                path,
                format: self.format,
                names: duplicated,
            });
        }
        for entry in entries {
            if entry.kind != EntryKind::File {
                continue;
            }
            files += 1;
            if self.limits.per_entry_at_listing && entry.size > self.limits.max_entry_bytes {
                return Err(ArchiveError::TooLarge {
                    path,
                    format: self.format,
                    preset: self.limits.preset,
                    entry: entry.name.clone(),
                    bytes: entry.size,
                    limit: self.limits.max_entry_bytes,
                });
            }
            total = total.saturating_add(entry.size);
            if total > self.limits.max_total_bytes {
                return Err(ArchiveError::TooLarge {
                    path,
                    format: self.format,
                    preset: self.limits.preset,
                    entry: entry.name.clone(),
                    bytes: total,
                    limit: self.limits.max_total_bytes,
                });
            }
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
        if files > self.limits.max_entries {
            return Err(ArchiveError::TooManyEntries {
                path,
                format: self.format,
                preset: self.limits.preset,
                count: files,
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
                EntryKind::Directory => continue,
                EntryKind::Link => {
                    return Err(ArchiveError::LinkEntry {
                        path: path.clone(),
                        entry: entry.name.clone(),
                    })
                }
                EntryKind::File => {}
            }
            let wanted = select(&entry.name);
            if let Select::Refuse(reason) = wanted {
                return Err(ArchiveError::Refused {
                    path: path.clone(),
                    entry: entry.name.clone(),
                    reason,
                });
            }
            match classify_entry_name(&entry.name) {
                EntryName::Writable(strict) => match wanted {
                    Select::Write => plan.push((strict, entry.name.clone(), entry.size)),
                    Select::LeaveOut(reason) => skipped.push(SkippedEntry {
                        name: entry.name.clone(),
                        reason,
                    }),
                    Select::Refuse(_) => unreachable!("answered above"),
                },
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
        for (strict, name, declared) in plan {
            if cancelled() {
                return Err(ArchiveError::Cancelled { path: path.clone() });
            }
            let target = join_under(dest, &strict).map_err(|error| match error {
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
                    format: self.format,
                    preset: self.limits.preset,
                    limit: self.limits.max_entry_bytes,
                    entry: name.clone(),
                    declared,
                    exact: self.format != ArchiveFormat::Rar,
                },
                preset_total: self.limits.max_total_bytes,
                preset: self.limits.preset,
            };
            let written = self.copy_entry(&name, declared, &mut out)?;
            remaining_total = remaining_total.saturating_sub(written);
            keep_executable_bit(&target, self.entry_mode(&name));
            self.written_so_far.push(strict.as_str().to_string());
            report.written.push(strict.as_str().to_string());
            report.bytes_written += written;
        }
        report.skipped = skipped;
        Ok(report)
    }

    /// The paths an extraction wrote, including a run that then failed: a caller that promises to
    /// leave nothing behind after a refusal deletes these.
    pub fn written_so_far(&self) -> &[String] {
        &self.written_so_far
    }

    /// The Unix mode an entry recorded, where it recorded one.
    fn entry_mode(&self, name: &str) -> Option<u32> {
        self.entries
            .iter()
            .find(|entry| entry.name == name)
            .and_then(|entry| entry.unix_mode)
    }
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
#[derive(Debug, Clone)]
struct CopyContext {
    archive: String,
    format: ArchiveFormat,
    preset: &'static str,
    limit: u64,
    entry: String,
    declared: u64,
    /// Whether the declared size is the archive's own binary header, which the bytes must match. A
    /// RAR listing is a text report from a process, so its numbers bound the read rather than pin it.
    exact: bool,
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
fn copy_bounded(
    reader: &mut dyn Read,
    out: &mut dyn Write,
    context: &CopyContext,
) -> Result<u64, ArchiveError> {
    let mut buffer = vec![0u8; 64 * 1024];
    let mut written = 0u64;
    loop {
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
) -> Result<Vec<ArchiveEntry>, ArchiveError> {
    let mut archive = tar::Archive::new(reader);
    let listed = archive.entries().map_err(|error| ArchiveError::Corrupt {
        path: path.to_string(),
        format,
        reason: format!("{error}"),
    })?;
    let mut entries = Vec::new();
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
        entries.push(ArchiveEntry {
            name,
            kind,
            size: entry.size(),
            compressed: None,
            unix_mode: Some(entry.header().mode().unwrap_or(0o644)),
        });
    }
    Ok(entries)
}

fn copy_from_tar(
    reader: Box<dyn Read>,
    name: &str,
    out: &mut dyn Write,
    context: &CopyContext,
) -> Result<u64, ArchiveError> {
    let mut archive = tar::Archive::new(reader);
    let entries = archive.entries().map_err(|error| ArchiveError::Corrupt {
        path: context.archive.clone(),
        format: context.format,
        reason: format!("{error}"),
    })?;
    for entry in entries {
        let mut entry = entry.map_err(|error| ArchiveError::Corrupt {
            path: context.archive.clone(),
            format: context.format,
            reason: format!("{error}"),
        })?;
        let entry_name = entry_name_of(&entry, context.format, &context.archive)?;
        if entry_name != name {
            continue;
        }
        return copy_bounded(&mut entry, out, context);
    }
    Err(ArchiveError::Corrupt {
        path: context.archive.clone(),
        format: context.format,
        reason: format!("no entry is named '{name}'"),
    })
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
fn zip_declared_entry_names(reader: &mut dyn ReadSeek, start: u64) -> std::io::Result<Vec<String>> {
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
        let mut name = vec![0u8; name_length];
        reader.read_exact(&mut name)?;
        reader.seek_relative((extra_length + comment_length) as i64)?;
        let name = String::from_utf8_lossy(&name).into_owned();
        if !name.ends_with('/') {
            names.push(name);
        }
    }
    Ok(names)
}

/// The names that appear more than once, each once, in the order they were first seen.
fn names_declared_more_than_once(names: &[String]) -> Vec<String> {
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut duplicated: Vec<String> = Vec::new();
    for name in names {
        if !seen.insert(name.as_str()) && !duplicated.iter().any(|known| known == name) {
            duplicated.push(name.clone());
        }
    }
    duplicated
}

fn zip_entry_kind<R: Read + ?Sized>(entry: &zip::read::ZipFile<'_, R>, name: &str) -> EntryKind {
    if entry.is_dir() || name.ends_with('/') {
        return EntryKind::Directory;
    }
    if let Some(mode) = entry.unix_mode() {
        if (mode & 0o170000) == 0o120000 {
            return EntryKind::Link;
        }
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
    context: &'a CopyContext,
    written: u64,
}

impl<'a> Sink<'a> {
    fn new(out: &'a mut dyn Write, context: &'a CopyContext) -> Self {
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
    context: CopyContext,
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
