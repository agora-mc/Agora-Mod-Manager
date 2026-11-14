//! Record overlap detection for Creation Engine plugin files (MASTER_SPEC §31.3).
//!
//! The command reads one JSON request from stdin and writes one JSON reply to
//! stdout. The active plugins are mounted read-only under `/data`, and the
//! plugin's own saved indexes are mounted under `/index`. Each plugin is
//! indexed once; the index travels back in chunks the caller keeps in the
//! plugin's file store, and is reused while the file's name, size and
//! modification time and this module's index version are unchanged.
//!
//! Every rule this parser relies on comes from UESP's "Skyrim Mod:Mod File
//! Format" (<https://en.uesp.net/wiki/Skyrim_Mod:Mod_File_Format>): the 24-byte
//! record header, the 24-byte group header whose size includes the header, the
//! `GRUP` nesting, the 0x00000200 light-master flag, the 0x00040000 compressed
//! flag, the compressed-data prefix (a `uint32` decompressed size followed by
//! zlib bytes), and MAST/DATA master pairs. "Form ID"
//! (<https://en.uesp.net/wiki/Skyrim:Form_ID>) gives the leading digits as the
//! index into the plugin's master list, with the file's own records using the
//! index one past its last master. A top byte past the last master names no
//! master; it resolves to the file itself, as LOOT's reader does, because
//! Skyrim.esm contains such a record.

pub mod json;

use std::collections::HashMap;
use std::io::Read;

use json::Value;

const RECORD_HEADER: usize = 24;
const GROUP_HEADER: usize = 24;
const COMPRESSED_FLAG: u32 = 0x0004_0000;
const INDEX_MAGIC: &[u8; 8] = b"AGRIDX02";
pub const INDEX_VERSION: u32 = 2;
const MAX_GROUP_DEPTH: u32 = 64;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// One record in a plugin's index.
#[derive(Debug)]
pub struct Record {
    /// Index into `Index::files`: a master, or the plugin itself.
    pub file: u16,
    /// The low 24 bits of the record's FormID.
    pub low: u32,
    pub digest: u64,
    pub kind: [u8; 4],
    /// Index into `Index::strings`, or `u16::MAX` when the record has no EDID.
    pub edid: u16,
}

#[derive(Debug)]
pub struct Index {
    pub name: String,
    pub size: u64,
    pub modified: u64,
    /// The plugin's masters in MAST order, then the plugin's own name.
    pub files: Vec<String>,
    pub records: Vec<Record>,
    pub strings: Vec<String>,
}

pub struct ReplyIndex {
    pub name: String,
    pub key: String,
    pub size: u64,
    pub modified: u64,
    /// The whole index's length and this part's first byte, so a caller can ask
    /// for the rest without the whole index crossing the script boundary.
    pub total: u64,
    pub from: u64,
    pub bytes: Vec<u8>,
}

pub struct OverlapItem {
    pub name: String,
    pub digest: u64,
}

pub struct OverlapKey {
    pub key: String,
    pub label: String,
    pub items: Vec<OverlapItem>,
}

pub struct Note {
    pub item: String,
    pub text: String,
}

pub struct Reply {
    pub keys: Vec<OverlapKey>,
    pub notes: Vec<Note>,
    pub indexes: Vec<ReplyIndex>,
    /// Plugins whose saved index is missing or out of date. The caller indexes
    /// them one at a time and asks for the overlaps again; this keeps one
    /// reply small even on a cold run over every master.
    pub stale: Vec<String>,
    /// The whole overlap set's size and the first key in this page, so a
    /// caller can page through a large set without one huge script object.
    pub total: u64,
    pub offset: u64,
}

/// Run one request. `mode` is `index` (index the named plugins and return their
/// new indexes) or `overlaps` (compare the named plugins using cached indexes).
pub fn run_request(request_json: &str) -> Result<String, String> {
    let value = json::parse(request_json)
        .map_err(|error| format!("the request is not valid JSON: {error}"))?;
    let request = Request::from_value(&value)?;
    let reply = run(&request)?;
    Ok(write_reply(&reply))
}

struct Request {
    mode: String,
    plugins: Vec<String>,
    cached: Vec<CachedIndex>,
    from: u64,
    length: u64,
    offset: u64,
    limit: u64,
    /// How many digest changes a key needs before it can matter to the core.
    /// One change is a single mod editing a record (never a conflict); two or
    /// more distinct changes can be. The default filters the rest, because a
    /// real master set has tens of thousands of agreeing keys and the script
    /// has a fixed memory limit.
    min_changes: u64,
}

struct CachedIndex {
    name: String,
    chunks: Vec<String>,
}

impl Request {
    fn from_value(value: &Value) -> Result<Self, String> {
        let mode = match value.get("mode") {
            None => "overlaps".to_string(),
            Some(Value::String(mode)) => mode.clone(),
            Some(_) => return Err("'mode' must be a string".to_string()),
        };
        if mode != "index" && mode != "overlaps" {
            return Err(format!("'{mode}' is not an index or overlaps mode"));
        }
        let plugins = string_array(value, "plugins")?;
        let from = value.get("from").and_then(Value::as_u64).unwrap_or(0);
        let length = value
            .get("length")
            .and_then(Value::as_u64)
            .unwrap_or(u64::MAX);
        let offset = value.get("offset").and_then(Value::as_u64).unwrap_or(0);
        let limit = value
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(u64::MAX);
        let min_changes = value
            .get("min_changes")
            .and_then(Value::as_u64)
            .unwrap_or(2);
        let mut cached = Vec::new();
        if let Some(entries) = value.get("cached") {
            let entries = entries
                .as_array()
                .ok_or_else(|| "'cached' must be an array".to_string())?;
            for entry in entries {
                let name = entry
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "every cached entry needs a string 'name'".to_string())?;
                let chunks = entry
                    .get("chunks")
                    .and_then(Value::as_array)
                    .ok_or_else(|| "every cached entry needs an array 'chunks'".to_string())?;
                let chunks = chunks
                    .iter()
                    .map(|chunk| {
                        chunk
                            .as_str()
                            .map(str::to_owned)
                            .ok_or_else(|| "every chunk name must be a string".to_string())
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                cached.push(CachedIndex {
                    name: name.to_string(),
                    chunks,
                });
            }
        }
        Ok(Self {
            mode,
            plugins,
            cached,
            from,
            length,
            offset,
            limit,
            min_changes,
        })
    }
}

fn string_array(value: &Value, key: &str) -> Result<Vec<String>, String> {
    let array = value
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("the request needs an array '{key}'"))?;
    array
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("every entry of '{key}' must be a string"))
        })
        .collect()
}

fn run(request: &Request) -> Result<Reply, String> {
    let mut notes = Vec::new();
    let mut stale = Vec::new();
    let cached: HashMap<&str, &CachedIndex> = request
        .cached
        .iter()
        .map(|entry| (entry.name.as_str(), entry))
        .collect();

    let mut indexes = Vec::with_capacity(request.plugins.len());
    let mut reply_indexes = Vec::new();
    for name in &request.plugins {
        if !plain_file_name(name) {
            notes.push(Note {
                item: name.clone(),
                text: format!("{name} could not be read: it is not a plain file name"),
            });
            continue;
        }
        let path = format!("/data/{name}");
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                notes.push(Note {
                    item: name.clone(),
                    text: format!("{name} could not be read: {error}"),
                });
                continue;
            }
        };
        let size = metadata.len();
        let modified = modified_ns(&metadata);

        if request.mode == "overlaps" {
            let Some(entry) = cached.get(name.as_str()) else {
                stale.push(name.clone());
                continue;
            };
            match read_cached(entry, name, size, modified) {
                Some(index) => indexes.push(index),
                None => stale.push(name.clone()),
            }
            continue;
        }

        match parse_plugin_file(name, &path, size) {
            Ok(mut index) => {
                index.size = size;
                index.modified = modified;
                let key = index_key(&index);
                let serialized = index.serialize();
                let total = serialized.len() as u64;
                let start = usize::try_from(request.from)
                    .unwrap_or(usize::MAX)
                    .min(serialized.len());
                let end = start
                    .saturating_add(usize::try_from(request.length).unwrap_or(usize::MAX))
                    .min(serialized.len());
                reply_indexes.push(ReplyIndex {
                    name: index.name.clone(),
                    key,
                    size,
                    modified,
                    total,
                    from: start as u64,
                    bytes: serialized[start..end].to_vec(),
                });
                indexes.push(index);
            }
            Err(error) => notes.push(Note {
                item: name.clone(),
                text: format!("{name} could not be read: {error}"),
            }),
        }
    }

    let mut keys = if request.mode == "overlaps" && stale.is_empty() {
        let mut keys = build_overlaps(&indexes).0;
        let min_changes = usize::try_from(request.min_changes).unwrap_or(usize::MAX);
        keys.retain(|key| digest_changes(&key.items) >= min_changes);
        keys
    } else {
        Vec::new()
    };
    let total = keys.len() as u64;
    if request.mode == "overlaps" {
        let start = usize::try_from(request.offset)
            .unwrap_or(usize::MAX)
            .min(keys.len());
        let end = start
            .saturating_add(usize::try_from(request.limit).unwrap_or(usize::MAX))
            .min(keys.len());
        keys = keys.drain(start..end).collect();
    }
    Ok(Reply {
        keys,
        notes,
        indexes: reply_indexes,
        stale,
        total,
        offset: request.offset,
    })
}

fn plain_file_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.chars().any(char::is_control)
}

fn modified_ns(metadata: &std::fs::Metadata) -> u64 {
    use std::time::UNIX_EPOCH;
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0)
}

fn read_cached(entry: &CachedIndex, name: &str, size: u64, modified: u64) -> Option<Index> {
    let mut bytes = Vec::new();
    for chunk in &entry.chunks {
        let mut piece = Vec::new();
        std::fs::File::open(format!("/index/{chunk}"))
            .ok()?
            .read_to_end(&mut piece)
            .ok()?;
        bytes.extend_from_slice(&piece);
    }
    let index = Index::deserialize(&bytes).ok()?;
    (index.name == name && index.size == size && index.modified == modified).then_some(index)
}

/// The store key for an index: the file it describes and what it was built from.
fn index_key(index: &Index) -> String {
    let mut hash = FNV_OFFSET;
    for byte in index.name.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    for byte in index
        .size
        .to_le_bytes()
        .iter()
        .chain(index.modified.to_le_bytes().iter())
    {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("overlaps-records-{hash:016x}-{INDEX_VERSION}")
}

/// Read bytes at an offset. A file source only moves forward, which is all the
/// sequential walk needs; a slice source is used by the tests.
trait Source {
    fn len(&self) -> u64;
    fn read(&mut self, offset: u64, length: usize) -> Result<Vec<u8>, String>;
}

struct SliceSource<'a> {
    bytes: &'a [u8],
}

impl Source for SliceSource<'_> {
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn read(&mut self, offset: u64, length: usize) -> Result<Vec<u8>, String> {
        let start = usize::try_from(offset).map_err(|_| "an offset is too large".to_string())?;
        let end = start
            .checked_add(length)
            .ok_or_else(|| "a read length overflows".to_string())?;
        self.bytes
            .get(start..end)
            .map(<[u8]>::to_vec)
            .ok_or_else(|| "the file is truncated".to_string())
    }
}

/// A forward-only reader over a large file. Blocks are read ahead, so a walk
/// over a million records costs one system call per block instead of two per
/// record, which matters inside WebAssembly.
struct FileSource {
    file: std::fs::File,
    pos: u64,
    len: u64,
    block_start: u64,
    block: Vec<u8>,
}

const READ_BLOCK: usize = 4 * 1024 * 1024;

impl Source for FileSource {
    fn len(&self) -> u64 {
        self.len
    }

    fn read(&mut self, offset: u64, length: usize) -> Result<Vec<u8>, String> {
        if offset != self.pos {
            return Err("the parser asked for bytes out of order".to_string());
        }
        let end = offset
            .checked_add(length as u64)
            .ok_or_else(|| "a read length overflows".to_string())?;
        if end > self.len {
            return Err("the file is truncated".to_string());
        }
        let covered =
            offset >= self.block_start && end <= self.block_start + self.block.len() as u64;
        if !covered {
            use std::io::{Read, Seek, SeekFrom};
            self.file
                .seek(SeekFrom::Start(offset))
                .map_err(|error| error.to_string())?;
            let wanted = READ_BLOCK.min((self.len - offset) as usize);
            let mut block = vec![0u8; wanted];
            self.file
                .read_exact(&mut block)
                .map_err(|error| error.to_string())?;
            self.block = block;
            self.block_start = offset;
        }
        let start = (offset - self.block_start) as usize;
        self.pos = end;
        Ok(self.block[start..start + length].to_vec())
    }
}

/// Parse one plugin file into its index.
pub fn parse_plugin(name: &str, bytes: &[u8]) -> Result<Index, String> {
    let mut source = SliceSource { bytes };
    let total = source.len();
    parse_source(name, &mut source, total)
}

/// Parse a plugin file from disk without loading the whole file into memory.
pub fn parse_plugin_file(name: &str, path: &str, size: u64) -> Result<Index, String> {
    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let mut source = FileSource {
        file,
        pos: 0,
        len: size,
        block_start: 0,
        block: Vec::new(),
    };
    parse_source(name, &mut source, size)
}

fn parse_source(name: &str, source: &mut dyn Source, total: u64) -> Result<Index, String> {
    if total < RECORD_HEADER as u64 {
        return Err("the file is shorter than one record header".to_string());
    }
    let header = source.read(0, RECORD_HEADER)?;
    if &header[0..4] != b"TES4" {
        return Err("the file does not start with a TES4 header record".to_string());
    }
    let header_size = u32::from_le_bytes(header[4..8].try_into().unwrap()) as u64;
    let data_start = RECORD_HEADER as u64;
    let data_end = data_start
        .checked_add(header_size)
        .ok_or_else(|| "the TES4 record size overflows".to_string())?;
    if data_end > total {
        return Err("the TES4 record is truncated".to_string());
    }
    let header_data = source.read(data_start, header_size as usize)?;

    let mut masters = Vec::new();
    scan_subrecords(&header_data, |kind, data| {
        if kind == b"MAST" {
            let end = data
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(data.len());
            if let Ok(text) = std::str::from_utf8(&data[..end]) {
                masters.push(text.to_string());
            }
        }
    });
    if masters.len() > 255 {
        return Err(format!(
            "the file names {} masters, more than a FormID can index",
            masters.len()
        ));
    }

    let mut files = masters;
    files.push(name.to_string());

    let mut index = Index {
        name: name.to_string(),
        size: total,
        modified: 0,
        files,
        records: Vec::new(),
        strings: Vec::new(),
    };
    let mut edids: HashMap<String, u16> = HashMap::new();
    let mut pos = data_end;
    while pos < total {
        pos = parse_entry(source, pos, total, &mut index, &mut edids, 0)?;
    }
    Ok(index)
}

/// Parse one record or group at `start`; return the offset just past it.
fn parse_entry(
    source: &mut dyn Source,
    start: u64,
    end: u64,
    index: &mut Index,
    edids: &mut HashMap<String, u16>,
    depth: u32,
) -> Result<u64, String> {
    if depth > MAX_GROUP_DEPTH {
        return Err("the groups are nested too deeply".to_string());
    }
    if end - start < RECORD_HEADER as u64 {
        return Err("a partial record header ends the file".to_string());
    }
    let header = source.read(start, RECORD_HEADER)?;
    let kind = &header[0..4];
    if kind == b"GRUP" {
        let size = u32::from_le_bytes(header[4..8].try_into().unwrap()) as u64;
        let group_end = start
            .checked_add(size)
            .ok_or_else(|| "a group size overflows".to_string())?;
        if size < GROUP_HEADER as u64 || group_end > end {
            return Err("a group overruns its parent".to_string());
        }
        let mut pos = start + GROUP_HEADER as u64;
        while pos < group_end {
            pos = parse_entry(source, pos, group_end, index, edids, depth + 1)?;
        }
        return Ok(group_end);
    }

    let data_size = u32::from_le_bytes(header[4..8].try_into().unwrap()) as u64;
    let flags = u32::from_le_bytes(header[8..12].try_into().unwrap());
    let form_id = u32::from_le_bytes(header[12..16].try_into().unwrap());
    let data_start = start + RECORD_HEADER as u64;
    let data_end = data_start
        .checked_add(data_size)
        .ok_or_else(|| "a record size overflows".to_string())?;
    if data_end > end {
        return Err("a record overruns its group".to_string());
    }
    let data = source.read(data_start, data_size as usize)?;

    let mut record_kind = [0u8; 4];
    record_kind.copy_from_slice(kind);
    if &record_kind != b"TES4" {
        let top = (form_id >> 24) as usize;
        let file_index = if top < index.files.len() {
            top
        } else {
            index.files.len() - 1
        };
        let file = &index.files[file_index];
        if file.is_empty() {
            return Err("a record names an empty master file".to_string());
        }
        let owned;
        let fields: &[u8] = if flags & COMPRESSED_FLAG != 0 {
            owned = decompress(&data)?;
            &owned
        } else {
            &data
        };
        let digest = digest(record_kind, flags & !COMPRESSED_FLAG, fields);
        let edid = match editor_id(fields) {
            Some(text) => {
                let next = index.strings.len() as u16;
                match edids.get(&text) {
                    Some(existing) => *existing,
                    None => {
                        edids.insert(text.clone(), next);
                        index.strings.push(text);
                        next
                    }
                }
            }
            None => u16::MAX,
        };
        index.records.push(Record {
            file: file_index as u16,
            low: form_id & 0x00FF_FFFF,
            digest,
            kind: record_kind,
            edid,
        });
    }
    Ok(data_end)
}

fn decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.len() < 4 {
        return Err("a compressed record has no decompressed size".to_string());
    }
    let expected = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
    let plain = miniz_oxide::inflate::decompress_to_vec_zlib(&data[4..])
        .map_err(|error| format!("a compressed record cannot be decompressed: {error:?}"))?;
    if plain.len() != expected {
        return Err(format!(
            "a compressed record says it holds {expected} bytes but gave {}",
            plain.len()
        ));
    }
    Ok(plain)
}

/// A 64-bit FNV-1a over the record type, the flags without the compressed bit,
/// and the data (decompressed). Version-control fields never reach the digest.
fn digest(kind: [u8; 4], flags: u32, data: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET;
    for byte in kind
        .iter()
        .chain(flags.to_le_bytes().iter())
        .chain(data.iter())
    {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// The EDID string in a record's fields, if it has one.
fn editor_id(data: &[u8]) -> Option<String> {
    let mut found = None;
    scan_subrecords(data, |kind, value| {
        if kind == b"EDID" && found.is_none() {
            let end = value
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(value.len());
            if let Ok(text) = std::str::from_utf8(&value[..end]) {
                if !text.is_empty() {
                    found = Some(text.to_string());
                }
            }
        }
    });
    found
}

/// Walk a record's fields. Handles the XXXX large-size form, whose size lives
/// in the XXXX field and whose following field has a zero `uint16` size.
fn scan_subrecords(data: &[u8], mut visit: impl FnMut(&[u8; 4], &[u8])) {
    let mut pos = 0;
    while pos + 6 <= data.len() {
        let mut kind = [0u8; 4];
        kind.copy_from_slice(&data[pos..pos + 4]);
        let mut size = u16::from_le_bytes(data[pos + 4..pos + 6].try_into().unwrap()) as usize;
        let mut header = 6;
        if &kind == b"XXXX" {
            if pos + 10 > data.len() {
                return;
            }
            size = u32::from_le_bytes(data[pos + 6..pos + 10].try_into().unwrap()) as usize;
            header = 10;
            if pos + header + 6 > data.len() {
                return;
            }
            kind.copy_from_slice(&data[pos + header..pos + header + 4]);
            header += 6;
        }
        let start = pos + header;
        let end = match start.checked_add(size) {
            Some(end) if end <= data.len() => end,
            _ => return,
        };
        visit(&kind, &data[start..end]);
        pos = end;
    }
}

/// How many times the record's digest changes between consecutive providers in
/// load order. The first provider defines the record; each later provider whose
/// digest differs is a changer. Zero or one changer is always "agree".
fn digest_changes(items: &[OverlapItem]) -> usize {
    items
        .windows(2)
        .filter(|pair| pair[0].digest != pair[1].digest)
        .count()
}

/// Report only the keys that two or more indexes have, with each index's digest.
pub fn build_overlaps(indexes: &[Index]) -> (Vec<OverlapKey>, Vec<Note>) {
    // A plugin's master indexes are local to its own MAST list, so resolve every
    // record to the defining file's name before comparing across plugins.
    let mut global_files: Vec<String> = Vec::new();
    let mut global_ids: HashMap<String, u32> = HashMap::new();
    let mut local_to_global: Vec<Vec<u32>> = Vec::with_capacity(indexes.len());
    for index in indexes {
        let mut mapping = Vec::with_capacity(index.files.len());
        for file in &index.files {
            let folded = file.to_lowercase();
            let id = match global_ids.get(&folded) {
                Some(id) => *id,
                None => {
                    let id = global_files.len() as u32;
                    global_ids.insert(folded.clone(), id);
                    global_files.push(folded);
                    id
                }
            };
            mapping.push(id);
        }
        local_to_global.push(mapping);
    }

    let mut counts: HashMap<(u32, u32), u8> = HashMap::new();
    for (index_number, index) in indexes.iter().enumerate() {
        let mapping = &local_to_global[index_number];
        let mut seen: HashMap<(u32, u32), ()> = HashMap::new();
        for record in &index.records {
            seen.entry((mapping[record.file as usize], record.low))
                .or_insert(());
        }
        for key in seen.keys() {
            let entry = counts.entry(*key).or_insert(0);
            if *entry < 2 {
                *entry += 1;
            }
        }
    }

    let mut keys: Vec<OverlapKey> = Vec::new();
    let mut positions: HashMap<(u32, u32), usize> = HashMap::new();
    for (index_number, index) in indexes.iter().enumerate() {
        let mapping = &local_to_global[index_number];
        for record in &index.records {
            let file = mapping[record.file as usize];
            let local = (file, record.low);
            if counts.get(&local).copied().unwrap_or(0) < 2 {
                continue;
            }
            let position = match positions.get(&local).copied() {
                Some(position) => position,
                None => {
                    let position = keys.len();
                    positions.insert(local, position);
                    keys.push(OverlapKey {
                        key: format!("{}:{:06x}", global_files[file as usize], record.low),
                        label: String::new(),
                        items: Vec::new(),
                    });
                    position
                }
            };
            let key_entry = &mut keys[position];
            let digest = record.digest;
            match key_entry
                .items
                .iter_mut()
                .find(|item| item.name == index.name)
            {
                Some(item) => item.digest = digest,
                None => key_entry.items.push(OverlapItem {
                    name: index.name.clone(),
                    digest,
                }),
            }
            if key_entry.label.is_empty() {
                let kind = record_kind_label(record.kind);
                let edid = if record.edid != u16::MAX {
                    index.strings.get(record.edid as usize).map(String::as_str)
                } else {
                    None
                };
                key_entry.label = match edid {
                    Some(edid) => format!("{kind} {edid}"),
                    None => format!("{kind} {}", key_entry.key),
                };
            }
        }
    }

    keys.retain(|key| key.items.len() > 1);
    keys.sort_by(|left, right| left.key.cmp(&right.key));
    (keys, Vec::new())
}

fn record_kind_label(kind: [u8; 4]) -> String {
    kind.iter()
        .map(|byte| {
            if byte.is_ascii_graphic() {
                char::from(*byte)
            } else {
                char::from(b'?')
            }
        })
        .collect()
}

impl Index {
    /// The compact on-disk form kept in the plugin's file store.
    pub fn serialize(&self) -> Vec<u8> {
        let mut types: Vec<[u8; 4]> = Vec::new();
        let mut type_ids: HashMap<[u8; 4], u8> = HashMap::new();
        let mut record_types = Vec::with_capacity(self.records.len());
        for record in &self.records {
            let id = match type_ids.get(&record.kind) {
                Some(id) => *id,
                None => {
                    let id = types.len() as u8;
                    types.push(record.kind);
                    type_ids.insert(record.kind, id);
                    id
                }
            };
            record_types.push(id);
        }

        let mut out = Vec::new();
        out.extend_from_slice(INDEX_MAGIC);
        out.extend_from_slice(&INDEX_VERSION.to_le_bytes());
        write_str16(&mut out, &self.name);
        out.extend_from_slice(&self.size.to_le_bytes());
        out.extend_from_slice(&self.modified.to_le_bytes());
        out.extend_from_slice(&(self.files.len() as u16).to_le_bytes());
        for file in &self.files {
            write_str16(&mut out, file);
        }
        out.extend_from_slice(&(types.len() as u8).to_le_bytes()[..]);
        for kind in &types {
            out.extend_from_slice(kind);
        }
        out.extend_from_slice(&(self.records.len() as u32).to_le_bytes());
        for (record, kind_id) in self.records.iter().zip(&record_types) {
            out.push(*kind_id);
            out.extend_from_slice(&record.file.to_le_bytes());
            out.extend_from_slice(&record.low.to_le_bytes()[..3]);
            out.extend_from_slice(&record.digest.to_le_bytes());
            out.extend_from_slice(&record.edid.to_le_bytes());
        }
        out.extend_from_slice(&(self.strings.len() as u32).to_le_bytes());
        for text in &self.strings {
            write_str16(&mut out, text);
        }
        out
    }

    pub fn deserialize(bytes: &[u8]) -> Result<Self, String> {
        let mut reader = Reader { bytes, pos: 0 };
        if reader.take(8)? != &INDEX_MAGIC[..] {
            return Err("the index has the wrong magic".to_string());
        }
        let version = reader.u32()?;
        if version != INDEX_VERSION {
            return Err(format!(
                "the index version {version} is not {INDEX_VERSION}"
            ));
        }
        let name = reader.str16()?;
        let size = reader.u64()?;
        let modified = reader.u64()?;
        let file_count = reader.u16()? as usize;
        let mut files = Vec::with_capacity(file_count);
        for _ in 0..file_count {
            files.push(reader.str16()?);
        }
        let type_count = reader.u8()? as usize;
        let mut types = Vec::with_capacity(type_count);
        for _ in 0..type_count {
            let mut kind = [0u8; 4];
            kind.copy_from_slice(reader.take(4)?);
            types.push(kind);
        }
        let record_count = reader.u32()? as usize;
        let mut records = Vec::with_capacity(record_count);
        for _ in 0..record_count {
            let kind_id = reader.u8()? as usize;
            let kind = *types
                .get(kind_id)
                .ok_or_else(|| "an index record names an unknown type".to_string())?;
            let file = reader.u16()?;
            let low_bytes = reader.take(3)?;
            let low = u32::from(low_bytes[0])
                | (u32::from(low_bytes[1]) << 8)
                | (u32::from(low_bytes[2]) << 16);
            let digest = reader.u64()?;
            let edid = reader.u16()?;
            records.push(Record {
                file,
                low,
                digest,
                kind,
                edid,
            });
        }
        let string_count = reader.u32()? as usize;
        let mut strings = Vec::with_capacity(string_count);
        for _ in 0..string_count {
            strings.push(reader.str16()?);
        }
        Ok(Self {
            name,
            size,
            modified,
            files,
            records,
            strings,
        })
    }
}

fn write_str16(out: &mut Vec<u8>, text: &str) {
    let bytes = text.as_bytes();
    let length = u16::try_from(bytes.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(&bytes[..length as usize]);
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8], String> {
        let end = self
            .pos
            .checked_add(length)
            .ok_or_else(|| "the index length overflows".to_string())?;
        let slice = self
            .bytes
            .get(self.pos..end)
            .ok_or_else(|| "the index is truncated".to_string())?;
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn str16(&mut self) -> Result<String, String> {
        let length = self.u16()? as usize;
        let bytes = self.take(length)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| "the index text is not UTF-8".to_string())
    }
}

fn write_reply(reply: &Reply) -> String {
    let mut out = String::new();
    out.push_str("{\"keys\":[");
    for (index, key) in reply.keys.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str("{\"key\":");
        json::write_string(&key.key, &mut out);
        out.push_str(",\"label\":");
        json::write_string(&key.label, &mut out);
        out.push_str(",\"items\":[");
        for (item_index, item) in key.items.iter().enumerate() {
            if item_index > 0 {
                out.push(',');
            }
            out.push_str("{\"name\":");
            json::write_string(&item.name, &mut out);
            out.push_str(",\"digest\":");
            json::write_string(&format!("{:016x}", item.digest), &mut out);
            out.push('}');
        }
        out.push_str("]}");
    }
    out.push_str("],\"notes\":[");
    for (index, note) in reply.notes.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str("{\"item\":");
        json::write_string(&note.item, &mut out);
        out.push_str(",\"level\":\"warn\",\"text\":");
        json::write_string(&note.text, &mut out);
        out.push('}');
    }
    out.push_str("],\"indexes\":[");
    for (index, built) in reply.indexes.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str("{\"name\":");
        json::write_string(&built.name, &mut out);
        out.push_str(",\"key\":");
        json::write_string(&built.key, &mut out);
        out.push_str(",\"size\":");
        out.push_str(&built.size.to_string());
        out.push_str(",\"modified\":");
        out.push_str(&built.modified.to_string());
        out.push_str(",\"version\":");
        out.push_str(&INDEX_VERSION.to_string());
        out.push_str(",\"total\":");
        out.push_str(&built.total.to_string());
        out.push_str(",\"from\":");
        out.push_str(&built.from.to_string());
        out.push_str(",\"bytes\":");
        json::write_string(&base64_encode(&built.bytes), &mut out);
        out.push('}');
    }
    out.push_str("],\"stale\":[");
    for (index, name) in reply.stale.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        json::write_string(name, &mut out);
    }
    out.push_str("],\"total\":");
    out.push_str(&reply.total.to_string());
    out.push_str(",\"offset\":");
    out.push_str(&reply.offset.to_string());
    out.push_str("}\n");
    out
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = u32::from(chunk[0]);
        let second = chunk.get(1).copied().map(u32::from).unwrap_or(0);
        let third = chunk.get(2).copied().map(u32::from).unwrap_or(0);
        let value = (first << 16) | (second << 8) | third;
        out.push(ALPHABET[(value >> 18) as usize & 63] as char);
        out.push(ALPHABET[(value >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(value >> 6) as usize & 63] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[value as usize & 63] as char);
        } else {
            out.push('=');
        }
    }
    out
}
