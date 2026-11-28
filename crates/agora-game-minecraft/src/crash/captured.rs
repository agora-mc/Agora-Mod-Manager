//! Agora's captured launch output: the sanitized tail of a direct launch's stdout and stderr.
//!
//! The launcher prefixes every line with `[stdout]` or `[stderr]` and writes a small `#` header.
//! That is Agora's own shape, not a game's, so this parser strips it and hands the cleaned lines to
//! the ordinary log or crash-report parser. The producer stays the one the inner parser states, so
//! a rule gated on `minecraft_latest_log` still sees a captured log.

use super::shared::collect_lines;
use super::{CrashReportParser, LatestLogParser};
use agora_game_api::crash::{
    BuiltInParser, BuiltInParserError, LinesReply, ParserOutput, ParserReadError, ParserReads,
    ParserRequest,
};

pub struct CapturedOutputParser;

const STREAMS: [&str; 2] = ["[stdout] ", "[stderr] "];

impl BuiltInParser for CapturedOutputParser {
    fn parse(
        &self,
        request: &ParserRequest,
        reads: &dyn ParserReads,
    ) -> Result<ParserOutput, BuiltInParserError> {
        let collected = collect_lines(reads, &request.artifact.id, request.limits)?;
        if collected.lines.is_empty() {
            return Err(BuiltInParserError::new(
                "the captured launch output is empty; there is nothing to read",
            ));
        }

        let mut cleaned: Vec<String> = Vec::with_capacity(collected.lines.len());
        for line in &collected.lines {
            if line.starts_with("# Agora captured Java output")
                || line.starts_with("# exit_code=")
                || line.starts_with("# runtime_ms=")
            {
                continue;
            }
            let line = STREAMS
                .iter()
                .find_map(|prefix| line.strip_prefix(prefix))
                .unwrap_or(line);
            cleaned.push(line.to_string());
        }
        if cleaned.iter().all(|line| line.trim().is_empty()) {
            return Err(BuiltInParserError::new(
                "the captured launch output has no lines to read",
            ));
        }

        // A captured crash report or log tail is the same text the game writes to its own files.
        // Pick the parser that owns that shape, and let it set the producer and coverage.
        let has_report = cleaned
            .iter()
            .any(|line| line.contains("---- Minecraft Crash Report ----"));
        let text = cleaned.join("\n");
        let reader = MemoryReads {
            bytes: text.into_bytes(),
        };
        let mut inner = request.clone();
        inner.artifact.name = if has_report {
            "captured-crash-report.txt".to_string()
        } else {
            "captured-latest.log".to_string()
        };
        inner.artifact.size = reader.bytes.len() as u64;
        if has_report {
            CrashReportParser.parse(&inner, &reader)
        } else {
            LatestLogParser.parse(&inner, &reader)
        }
    }
}

/// The cleaned text, behind the same two read operations a built-in parser gets.
struct MemoryReads {
    bytes: Vec<u8>,
}

impl MemoryReads {
    fn lines(&self) -> Vec<String> {
        let text = String::from_utf8_lossy(&self.bytes);
        let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
        if lines.last().is_some_and(|line| line.is_empty()) {
            lines.pop();
        }
        lines
    }
}

impl ParserReads for MemoryReads {
    fn read_bytes(
        &self,
        _artifact: &str,
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>, ParserReadError> {
        let start = (offset as usize).min(self.bytes.len());
        let end = start.saturating_add(len as usize).min(self.bytes.len());
        Ok(self.bytes[start..end].to_vec())
    }

    fn read_lines(
        &self,
        _artifact: &str,
        from_line: u64,
        max_lines: u32,
    ) -> Result<LinesReply, ParserReadError> {
        let lines = self.lines();
        let start = (from_line as usize).min(lines.len());
        let end = start.saturating_add(max_lines as usize).min(lines.len());
        Ok(LinesReply {
            lines: lines[start..end].to_vec(),
            next_line: (end < lines.len()).then_some(end as u64),
            cut: false,
            gap: None,
        })
    }
}
