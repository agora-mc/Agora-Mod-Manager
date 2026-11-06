//! Windows minidumps, read with bounded byte reads and no external crates.
//!
//! The structures and their sizes follow Microsoft's documentation:
//! <https://learn.microsoft.com/en-us/windows/win32/api/minidumpapiset/ns-minidumpapiset-minidump_header>
//! and its sibling `ns-minidumpapiset-minidump_*` pages. All integers are little-endian.
//!
//! The parser never trusts a count from the file: every list is bounded by the artifact's size and
//! read one entry at a time, so a corrupt count costs a short read, not an allocation. It emits no
//! raw memory: only module names, addresses, versions and exception codes.

use super::{
    attr_scalar, output, producer_with, push_note, truncate_to, GenericError, ProducerConfig,
};
use agora_game_api::crash::{
    AttrValue, BucketPart, Coverage, CoverageState, Fact, ParseNote, ParseNoteKind, ParserOutput,
    ParserRequest, Producer, Ref, Scalar, SourceSpan, Strength,
};
use agora_plugin_api::host::ScriptReads;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

const SIGNATURE: u32 = 0x504D_444D;
const HEADER_BYTES: u64 = 32;
const DIRECTORY_BYTES: u64 = 12;
const EXCEPTION_STREAM_BYTES: u64 = 168;
const MODULE_BYTES: u64 = 108;
const THREAD_BYTES: u64 = 48;
const MEMORY64_ENTRY_BYTES: u64 = 16;

const STREAM_THREAD_LIST: u32 = 3;
const STREAM_MODULE_LIST: u32 = 4;
const STREAM_EXCEPTION: u32 = 6;
const STREAM_MEMORY64_LIST: u32 = 9;

const STACK_CHUNK_BYTES: u64 = 64 * 1024;
const NAME_BYTES_CAP: u64 = 4096;

/// Hard ceilings for lists whose counts come from the file. A real dump is far below these; the
/// point is that a corrupt count costs bounded work, never an allocation or a loop sized by it.
const MAX_DIRECTORY_ENTRIES: u64 = 4096;
const MAX_THREADS: u64 = 4096;
const MAX_MEMORY64_RANGES: u64 = 65_536;

const EXCEPTION_ACCESS_VIOLATION: u32 = 0xC000_0005;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct MinidumpConfig {
    #[serde(default)]
    producer: Option<ProducerConfig>,
    #[serde(default)]
    max_stack_bytes: Option<u64>,
}

fn load(config: &serde_json::Value) -> Result<MinidumpConfig, String> {
    let value = if config.is_null() {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        config.clone()
    };
    let parsed: MinidumpConfig = serde_json::from_value(value)
        .map_err(|error| format!("the minidump config is malformed: {error}"))?;
    if let Some(producer) = &parsed.producer {
        if producer.format.as_deref() == Some("") {
            return Err("`producer.format` is empty".to_string());
        }
    }
    Ok(parsed)
}

pub(crate) fn validate(config: &serde_json::Value) -> Result<(), String> {
    load(config).map(|_| ())
}

#[derive(Debug, Clone, Copy)]
struct StreamRef {
    rva: u32,
    size: u32,
}

impl StreamRef {
    /// How many `entry_size` entries fit after a `header` inside the stream's advertised size.
    fn entries(&self, header: u32, entry_size: u64) -> u64 {
        u64::from(self.size.saturating_sub(header)) / entry_size.max(1)
    }
}

struct ModuleRange {
    end: u64,
    name: Option<String>,
}

struct MemRange {
    start: u64,
    size: u64,
    file_offset: u64,
}

struct Dump<'a> {
    reads: &'a dyn ScriptReads,
    artifact: &'a str,
    limits: agora_game_api::crash::ParserLimits,
    size: u64,
    /// Bytes the reads have actually returned, so a stack scan can stop at the request's budget
    /// instead of asking for more and being refused.
    read_total: u64,
    notes: Vec<ParseNote>,
    budget_spent: bool,
}

impl<'a> Dump<'a> {
    fn read(&mut self, offset: u64, len: u64) -> Option<Vec<u8>> {
        if len == 0 {
            return Some(Vec::new());
        }
        let len = len.min(self.limits.max_read_bytes.max(1));
        match self.reads.read_bytes(self.artifact, offset, len) {
            Ok(bytes) => {
                self.read_total = self.read_total.saturating_add(bytes.len() as u64);
                Some(bytes)
            }
            Err(error) => {
                if error.code == "budget_spent" {
                    self.budget_spent = true;
                } else {
                    push_note(
                        &mut self.notes,
                        &self.limits,
                        ParseNoteKind::Other,
                        format!(
                            "a read at offset {offset} failed ({}): {}",
                            error.code, error.message
                        ),
                    );
                }
                None
            }
        }
    }

    fn note(&mut self, kind: ParseNoteKind, message: impl Into<String>) {
        push_note(&mut self.notes, &self.limits, kind, message);
    }
}

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset + 4)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn u64_at(bytes: &[u8], offset: usize) -> Option<u64> {
    let slice = bytes.get(offset..offset + 8)?;
    Some(u64::from_le_bytes([
        slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
    ]))
}

fn checked(kind: &str) -> Coverage {
    Coverage {
        kind: kind.to_string(),
        state: CoverageState::Checked,
        why: None,
    }
}

fn absent(kind: &str, why: impl Into<String>) -> Coverage {
    Coverage {
        kind: kind.to_string(),
        state: CoverageState::Absent,
        why: Some(why.into()),
    }
}

fn exception_category(code: u32) -> &'static str {
    match code {
        EXCEPTION_ACCESS_VIOLATION => "access_violation",
        0xC000_0409 | 0xC000_0602 => "fast_fail",
        0xC000_0017 => "oom",
        0x8000_0003 => "assert",
        0xC000_0135 | 0xC000_0139 => "load_failure",
        _ => "unknown",
    }
}

fn access_kind(info: Option<u64>) -> Option<&'static str> {
    match info? {
        0 => Some("read"),
        1 => Some("write"),
        8 => Some("execute"),
        _ => None,
    }
}

pub(crate) fn run(
    config: &serde_json::Value,
    request: &ParserRequest,
    reads: &dyn ScriptReads,
) -> Result<ParserOutput, GenericError> {
    let config = load(config).map_err(GenericError::config)?;
    let producer = producer_with(&config.producer, "windows_minidump");
    let limits = request.limits;
    let mut dump = Dump {
        reads,
        artifact: &request.artifact.id,
        limits,
        size: request.artifact.size,
        read_total: 0,
        notes: Vec::new(),
        budget_spent: false,
    };

    let header = dump.read(0, HEADER_BYTES);
    let Some(header) = header else {
        return Ok(truncated_output(producer, dump.notes));
    };
    if header.len() < 4 || u32_at(&header, 0) != Some(SIGNATURE) {
        let mut notes = Vec::new();
        push_note(
            &mut notes,
            &limits,
            ParseNoteKind::WrongFormat,
            "this is not a Windows minidump",
        );
        return Ok(output(producer, Vec::new(), Vec::new(), Vec::new(), notes));
    }
    if header.len() < HEADER_BYTES as usize {
        dump.note(
            ParseNoteKind::Truncated,
            "the dump ends before its header is complete",
        );
        return Ok(truncated_output(producer, dump.notes));
    }

    let number_of_streams = u64::from(u32_at(&header, 8).unwrap_or(0));
    let directory_rva = u64::from(u32_at(&header, 12).unwrap_or(0));

    let mut exception_stream = None;
    let mut module_stream = None;
    let mut thread_stream = None;
    let mut memory64_stream = None;

    let directory_available = if directory_rva < dump.size {
        (dump.size - directory_rva) / DIRECTORY_BYTES
    } else {
        0
    };
    let stream_count = number_of_streams
        .min(directory_available)
        .min(MAX_DIRECTORY_ENTRIES);
    if number_of_streams > stream_count {
        dump.note(
            ParseNoteKind::Truncated,
            "the header names more streams than the dump holds; reading what is there",
        );
    }
    for index in 0..stream_count {
        let at = directory_rva + index * DIRECTORY_BYTES;
        let Some(directory) = dump.read(at, DIRECTORY_BYTES) else {
            break;
        };
        if directory.len() < DIRECTORY_BYTES as usize {
            if index == 0 {
                dump.note(
                    ParseNoteKind::Truncated,
                    "the stream directory is past the end of the dump",
                );
            }
            break;
        }
        let stream_type = u32_at(&directory, 0).unwrap_or(0);
        let stream = StreamRef {
            rva: u32_at(&directory, 8).unwrap_or(0),
            size: u32_at(&directory, 4).unwrap_or(0),
        };
        match stream_type {
            STREAM_THREAD_LIST if thread_stream.is_none() => thread_stream = Some(stream),
            STREAM_MODULE_LIST if module_stream.is_none() => module_stream = Some(stream),
            STREAM_EXCEPTION if exception_stream.is_none() => exception_stream = Some(stream),
            STREAM_MEMORY64_LIST if memory64_stream.is_none() => memory64_stream = Some(stream),
            _ => {}
        }
    }

    let mut facts: Vec<Fact> = Vec::new();
    let mut bucket: Vec<BucketPart> = Vec::new();
    let mut coverage: Vec<Coverage> = Vec::new();

    let (exception_fact, exception_bucket, exception_coverage) =
        parse_exception(&mut dump, exception_stream);
    if let Some(fact) = exception_fact {
        push_fact(&mut facts, &limits, fact)?;
    }
    if let Some(part) = exception_bucket {
        bucket.push(part);
    }
    coverage.push(exception_coverage);

    let (module_facts, modules, module_coverage) = parse_modules(&mut dump, module_stream)?;
    for fact in module_facts {
        push_fact(&mut facts, &limits, fact)?;
    }
    coverage.push(module_coverage);

    let memory_ranges = parse_memory64(&mut dump, memory64_stream);
    let frame_coverage = scan_frames(
        &mut dump,
        thread_stream,
        &modules,
        &memory_ranges,
        config.max_stack_bytes,
        &limits,
        &mut facts,
    )?;
    coverage.push(frame_coverage);

    if dump.budget_spent {
        push_note(
            &mut dump.notes,
            &limits,
            ParseNoteKind::BudgetSpent,
            "the dump was not read to the end: the byte budget ran out",
        );
    }

    Ok(output(
        producer,
        facts,
        bucket,
        coverage,
        std::mem::take(&mut dump.notes),
    ))
}

fn truncated_output(producer: Producer, notes: Vec<ParseNote>) -> ParserOutput {
    let coverage = vec![
        absent(
            "core.exception",
            "the dump is truncated before an exception stream could be read",
        ),
        absent(
            "core.module",
            "the dump is truncated before a module list could be read",
        ),
        absent(
            "core.frame",
            "the dump is truncated before a thread list could be read",
        ),
    ];
    output(producer, Vec::new(), Vec::new(), coverage, notes)
}

fn push_fact(
    facts: &mut Vec<Fact>,
    limits: &agora_game_api::crash::ParserLimits,
    fact: Fact,
) -> Result<(), GenericError> {
    if facts.len() >= limits.max_facts as usize {
        return Err(GenericError::too_many(format!(
            "{} facts, over the limit of {}",
            facts.len() + 1,
            limits.max_facts
        )));
    }
    facts.push(fact);
    Ok(())
}

fn parse_exception(
    dump: &mut Dump<'_>,
    stream: Option<StreamRef>,
) -> (Option<Fact>, Option<BucketPart>, Coverage) {
    let Some(stream) = stream else {
        return (
            None,
            None,
            absent("core.exception", "this dump has no exception stream"),
        );
    };
    if stream.rva == 0 || stream.size < EXCEPTION_STREAM_BYTES as u32 {
        return (
            None,
            None,
            absent(
                "core.exception",
                "the exception stream is missing or shorter than its header",
            ),
        );
    }
    let Some(bytes) = dump.read(u64::from(stream.rva), EXCEPTION_STREAM_BYTES) else {
        return (
            None,
            None,
            absent(
                "core.exception",
                "the exception stream is past the end of the dump",
            ),
        );
    };
    if bytes.len() < EXCEPTION_STREAM_BYTES as usize {
        return (
            None,
            None,
            absent(
                "core.exception",
                "the exception stream is past the end of the dump",
            ),
        );
    }

    let code = u32_at(&bytes, 8).unwrap_or(0);
    let address = u64_at(&bytes, 24).unwrap_or(0);
    let parameters = u32_at(&bytes, 32).unwrap_or(0).min(15);
    let info = |index: u32| -> Option<u64> {
        if index < parameters {
            u64_at(&bytes, 40 + index as usize * 8)
        } else {
            None
        }
    };

    let fault_address = if code == EXCEPTION_ACCESS_VIOLATION && parameters >= 2 {
        info(1).unwrap_or(address)
    } else {
        address
    };

    let mut attrs = BTreeMap::new();
    attrs.insert(
        "category".to_string(),
        AttrValue::Scalar(Scalar::Text(exception_category(code).to_string())),
    );
    attrs.insert(
        "class".to_string(),
        AttrValue::Scalar(Scalar::Text(format!("0x{code:08X}"))),
    );
    attrs.insert(
        "fault_address".to_string(),
        AttrValue::Scalar(Scalar::Text(format!("0x{fault_address:x}"))),
    );
    attrs.insert(
        "near_null".to_string(),
        AttrValue::Scalar(attr_scalar(
            "core.exception",
            "near_null",
            if fault_address < 0x10000 {
                "true"
            } else {
                "false"
            },
        )),
    );
    if code == EXCEPTION_ACCESS_VIOLATION {
        if let Some(access) = access_kind(info(0)) {
            attrs.insert(
                "access".to_string(),
                AttrValue::Scalar(Scalar::Text(access.to_string())),
            );
        }
    }

    let fact = Fact {
        kind: "core.exception".to_string(),
        attrs,
        refs: Vec::new(),
        span: None,
    };
    let bucket = BucketPart {
        scheme: "minidump.exception".to_string(),
        value: format!("0x{code:08X}"),
    };
    (Some(fact), Some(bucket), checked("core.exception"))
}

type ParsedModules = (Vec<Fact>, BTreeMap<u64, ModuleRange>, Coverage);

fn parse_modules(
    dump: &mut Dump<'_>,
    stream: Option<StreamRef>,
) -> Result<ParsedModules, GenericError> {
    let Some(stream) = stream else {
        return Ok((
            Vec::new(),
            BTreeMap::new(),
            absent("core.module", "this dump has no module list"),
        ));
    };
    let Some(count_bytes) = dump.read(u64::from(stream.rva), 4) else {
        return Ok((
            Vec::new(),
            BTreeMap::new(),
            absent("core.module", "the module list could not be read"),
        ));
    };
    if count_bytes.len() < 4 {
        dump.note(
            ParseNoteKind::Truncated,
            "the module list is past the end of the dump",
        );
        return Ok((
            Vec::new(),
            BTreeMap::new(),
            absent("core.module", "the module list is past the end of the dump"),
        ));
    }

    let count = u64::from(u32_at(&count_bytes, 0).unwrap_or(0));
    let available = stream.entries(4, MODULE_BYTES);
    let limit = count.min(available);
    if count > available {
        dump.note(
            ParseNoteKind::Truncated,
            "the module list names more modules than the stream holds",
        );
    }
    let mut facts = Vec::new();
    let mut ranges = BTreeMap::new();

    for index in 0..limit {
        if facts.len() >= dump.limits.max_facts as usize {
            return Err(GenericError::too_many(format!(
                "{} facts, over the limit of {}",
                facts.len() + 1,
                dump.limits.max_facts
            )));
        }
        let at = u64::from(stream.rva) + 4 + index * MODULE_BYTES;
        let Some(module) = dump.read(at, MODULE_BYTES) else {
            break;
        };
        if module.len() < MODULE_BYTES as usize {
            dump.note(
                ParseNoteKind::Truncated,
                "the module list ends before its last entry",
            );
            break;
        }

        let base = u64_at(&module, 0).unwrap_or(0);
        let size = u64::from(u32_at(&module, 8).unwrap_or(0));
        let name_rva = u32_at(&module, 20).unwrap_or(0);
        let signature = u32_at(&module, 24).unwrap_or(0);
        let version = if signature == 0xFEEF_04BD {
            match (u32_at(&module, 32), u32_at(&module, 36)) {
                (Some(ms), Some(ls)) => Some(format!(
                    "{}.{}.{}.{}",
                    ms >> 16,
                    ms & 0xffff,
                    ls >> 16,
                    ls & 0xffff
                )),
                _ => None,
            }
        } else {
            None
        };

        let name = read_name(dump, name_rva);
        if name.is_none() && name_rva != 0 {
            dump.note(
                ParseNoteKind::Other,
                format!("the name of the module at 0x{base:x} could not be read"),
            );
        }

        let mut attrs = BTreeMap::new();
        attrs.insert(
            "base_address".to_string(),
            AttrValue::Scalar(Scalar::Text(format!("0x{base:x}"))),
        );
        attrs.insert(
            "size".to_string(),
            AttrValue::Scalar(attr_scalar("core.module", "size", &size.to_string())),
        );
        if let Some(version) = version {
            attrs.insert(
                "version".to_string(),
                AttrValue::Scalar(Scalar::Text(version)),
            );
        }

        let refs = match &name {
            Some(name) if !name.is_empty() => vec![Ref {
                kind: "module".to_string(),
                value: name.clone(),
                strength: Strength::Mention,
            }],
            _ => Vec::new(),
        };

        facts.push(Fact {
            kind: "core.module".to_string(),
            attrs,
            refs,
            span: None,
        });
        ranges.insert(
            base,
            ModuleRange {
                end: base.saturating_add(size),
                name,
            },
        );
    }

    Ok((facts, ranges, checked("core.module")))
}

fn read_name(dump: &mut Dump<'_>, rva: u32) -> Option<String> {
    if rva == 0 {
        return None;
    }
    let length_bytes = dump.read(u64::from(rva), 4)?;
    if length_bytes.len() < 4 {
        return None;
    }
    let byte_length = u64::from(u32_at(&length_bytes, 0)?);
    let cap = (u64::from(dump.limits.max_attr_len) * 2).clamp(256, NAME_BYTES_CAP);
    let take = byte_length.min(cap);
    if take == 0 {
        return Some(String::new());
    }
    let raw = dump.read(u64::from(rva) + 4, take)?;
    if raw.is_empty() {
        return None;
    }
    let mut units: Vec<u16> = raw
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();
    if let Some(nul) = units.iter().position(|&unit| unit == 0) {
        units.truncate(nul);
    }
    let name: String = char::decode_utf16(units)
        .map(|unit| unit.unwrap_or('\u{FFFD}'))
        .collect();
    Some(truncate_to(name, dump.limits.max_attr_len as usize))
}

fn parse_memory64(dump: &mut Dump<'_>, stream: Option<StreamRef>) -> Vec<MemRange> {
    let Some(stream) = stream else {
        return Vec::new();
    };
    let Some(header) = dump.read(u64::from(stream.rva), 16) else {
        return Vec::new();
    };
    if header.len() < 16 {
        return Vec::new();
    }
    let count = u64_at(&header, 0).unwrap_or(0);
    let base_rva = u64_at(&header, 8).unwrap_or(0);
    let limit = count
        .min(stream.entries(16, MEMORY64_ENTRY_BYTES))
        .min(MAX_MEMORY64_RANGES);
    let mut ranges = Vec::new();
    let mut file_offset = base_rva;

    for index in 0..limit {
        let at = u64::from(stream.rva) + 16 + index * MEMORY64_ENTRY_BYTES;
        let Some(entry) = dump.read(at, MEMORY64_ENTRY_BYTES) else {
            break;
        };
        if entry.len() < MEMORY64_ENTRY_BYTES as usize {
            break;
        }
        let start = u64_at(&entry, 0).unwrap_or(0);
        let size = u64_at(&entry, 8).unwrap_or(0);
        ranges.push(MemRange {
            start,
            size,
            file_offset,
        });
        file_offset = file_offset.saturating_add(size);
    }
    ranges
}

fn module_for(address: u64, modules: &BTreeMap<u64, ModuleRange>) -> Option<(u64, &ModuleRange)> {
    let (&base, range) = modules.range(..=address).next_back()?;
    (address < range.end).then_some((base, range))
}

fn scan_frames(
    dump: &mut Dump<'_>,
    stream: Option<StreamRef>,
    modules: &BTreeMap<u64, ModuleRange>,
    memory_ranges: &[MemRange],
    max_stack_bytes: Option<u64>,
    limits: &agora_game_api::crash::ParserLimits,
    facts: &mut Vec<Fact>,
) -> Result<Coverage, GenericError> {
    let Some(stream) = stream else {
        return Ok(absent("core.frame", "this dump has no thread list"));
    };
    let Some(count_bytes) = dump.read(u64::from(stream.rva), 4) else {
        return Ok(absent("core.frame", "the thread list could not be read"));
    };
    if count_bytes.len() < 4 {
        dump.note(
            ParseNoteKind::Truncated,
            "the thread list is past the end of the dump",
        );
        return Ok(absent(
            "core.frame",
            "the thread list is past the end of the dump",
        ));
    }

    let count = u64::from(u32_at(&count_bytes, 0).unwrap_or(0));
    let available = stream.entries(4, THREAD_BYTES);
    let limit = count.min(available).min(MAX_THREADS);
    if count > limit {
        dump.note(
            ParseNoteKind::Truncated,
            "the thread list names more threads than the dump holds; scanning what is there",
        );
    }
    let mut frame_index = 0u64;
    let mut seen_addresses: BTreeSet<u64> = BTreeSet::new();

    for index in 0..limit {
        let at = u64::from(stream.rva) + 4 + index * THREAD_BYTES;
        let Some(thread) = dump.read(at, THREAD_BYTES) else {
            break;
        };
        if thread.len() < THREAD_BYTES as usize {
            dump.note(
                ParseNoteKind::Truncated,
                "the thread list ends before its last entry",
            );
            break;
        }

        let stack_start = u64_at(&thread, 24).unwrap_or(0);
        let data_size = u64::from(u32_at(&thread, 32).unwrap_or(0));
        let stack_rva = u32_at(&thread, 36).unwrap_or(0);
        if data_size == 0 {
            continue;
        }

        let (file_offset, available) = if stack_rva != 0 && u64::from(stack_rva) < dump.size {
            (u64::from(stack_rva), data_size)
        } else if let Some((offset, size)) = memory_ranges.iter().find_map(|range| {
            // A thread stack may start part-way into a larger captured range.
            let delta = stack_start.checked_sub(range.start)?;
            (delta < range.size)
                .then_some((range.file_offset.saturating_add(delta), range.size - delta))
        }) {
            (offset, data_size.min(size))
        } else {
            dump.note(
                ParseNoteKind::Truncated,
                "a thread's stack is not in this dump",
            );
            continue;
        };

        let file_remaining = dump.size.saturating_sub(file_offset);
        let budget_remaining = limits.max_total_read_bytes.saturating_sub(dump.read_total);
        let limited = available
            .min(limits.max_read_bytes)
            .min(max_stack_bytes.unwrap_or(u64::MAX))
            .min(file_remaining)
            .min(budget_remaining);
        if limited < data_size {
            dump.note(
                ParseNoteKind::Truncated,
                "a stack scan was cut short before the whole stack was read",
            );
        }
        if limited == 0 {
            continue;
        }

        let chunk = STACK_CHUNK_BYTES.min(limits.max_read_bytes.max(1));
        let mut consumed = 0u64;
        let mut carry: Vec<u8> = Vec::new();

        while consumed < limited {
            let want = chunk.min(limited - consumed);
            let offset = file_offset + consumed;
            let Some(bytes) = dump.read(offset, want) else {
                break;
            };
            if bytes.is_empty() {
                break;
            }
            let actually = bytes.len() as u64;
            let mut window = Vec::with_capacity(carry.len() + bytes.len());
            window.extend_from_slice(&carry);
            window.extend_from_slice(&bytes);

            if window.len() >= 8 {
                for start in 0..=window.len() - 8 {
                    let value = u64::from_le_bytes([
                        window[start],
                        window[start + 1],
                        window[start + 2],
                        window[start + 3],
                        window[start + 4],
                        window[start + 5],
                        window[start + 6],
                        window[start + 7],
                    ]);
                    let Some((base, range)) = module_for(value, modules) else {
                        continue;
                    };
                    if !seen_addresses.insert(value) {
                        continue;
                    }
                    if facts.len() >= limits.max_facts as usize {
                        return Err(GenericError::too_many(format!(
                            "{} facts, over the limit of {}",
                            facts.len() + 1,
                            limits.max_facts
                        )));
                    }
                    let mut attrs = BTreeMap::new();
                    attrs.insert(
                        "kind".to_string(),
                        AttrValue::Scalar(Scalar::Text("scan".to_string())),
                    );
                    attrs.insert(
                        "index".to_string(),
                        AttrValue::Scalar(attr_scalar(
                            "core.frame",
                            "index",
                            &frame_index.to_string(),
                        )),
                    );
                    attrs.insert(
                        "address".to_string(),
                        AttrValue::Scalar(Scalar::Text(format!("0x{value:x}"))),
                    );
                    attrs.insert(
                        "rva".to_string(),
                        AttrValue::Scalar(Scalar::Text(format!("0x{:x}", value - base))),
                    );
                    let refs = match &range.name {
                        Some(name) if !name.is_empty() => vec![Ref {
                            kind: "module".to_string(),
                            value: name.clone(),
                            strength: Strength::Mention,
                        }],
                        _ => Vec::new(),
                    };
                    facts.push(Fact {
                        kind: "core.frame".to_string(),
                        attrs,
                        refs,
                        span: Some(SourceSpan {
                            artifact: None,
                            section: None,
                            line_start: None,
                            line_end: None,
                        }),
                    });
                    frame_index += 1;
                }
            }

            let tail_start = window.len().saturating_sub(7);
            carry = window[tail_start..].to_vec();
            consumed += actually;
            if actually < want {
                break;
            }
        }
    }

    Ok(checked("core.frame"))
}
