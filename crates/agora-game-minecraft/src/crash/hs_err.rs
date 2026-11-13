//! The JVM's `hs_err_pid*.log`: a native fault, and the library it happened in.

use super::shared::{absent, checked, collect_lines, fact, notes_for, number, reference, text};
use agora_game_api::crash::{
    AttrValue, BucketPart, BuiltInParser, BuiltInParserError, Fact, ParserOutput, ParserReads,
    ParserRequest, Producer, Scalar, Strength, CRASH_CONTRACT_VERSION,
};

pub struct HsErrParser;

const FORMAT: &str = "minecraft_hs_err";
const HEADER: &str = "# A fatal error has been detected by the Java Runtime Environment:";

impl BuiltInParser for HsErrParser {
    fn parse(
        &self,
        request: &ParserRequest,
        reads: &dyn ParserReads,
    ) -> Result<ParserOutput, BuiltInParserError> {
        let collected = collect_lines(reads, &request.artifact.id, request.limits)?;
        if collected.lines.is_empty() {
            return Err(BuiltInParserError::new(
                "the crash file is empty; there is nothing to read",
            ));
        }
        if !collected.lines.iter().any(|line| line.trim() == HEADER) {
            return Ok(ParserOutput {
                contract: CRASH_CONTRACT_VERSION,
                producer: Producer {
                    format: FORMAT.to_string(),
                    version: None,
                },
                facts: Vec::new(),
                bucket: Vec::new(),
                coverage: Vec::new(),
                notes: vec![agora_game_api::crash::ParseNote {
                    kind: agora_game_api::crash::ParseNoteKind::WrongFormat,
                    message: Some("this file is not a JVM fatal error log".to_string()),
                }],
            });
        }

        let mut facts: Vec<Fact> = Vec::new();
        let mut signal: Option<String> = None;
        let mut code: Option<String> = None;
        let mut pid: Option<f64> = None;
        let mut java_version: Option<String> = None;
        let mut java_vm: Option<String> = None;
        let mut java_bits: Option<f64> = None;
        let mut module: Option<(String, String)> = None;
        let mut os: Option<String> = None;
        let mut ram_free_mb: Option<f64> = None;
        let mut fault: Option<String> = None;
        let mut access: Option<String> = None;
        let mut out_of_memory = false;
        let mut in_system = false;

        for line in &collected.lines {
            let trimmed = line.trim();

            if trimmed == "---------------  S Y S T E M  ---------------" {
                in_system = true;
                continue;
            }

            if let Some(rest) = trimmed.strip_prefix("#  ") {
                if let Some((name, rest)) = rest.split_once(' ') {
                    if name.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
                        signal.get_or_insert_with(|| name.to_string());
                        if let Some(start) = rest.find("(0x") {
                            if let Some(end) = rest[start..].find(')') {
                                code.get_or_insert_with(|| {
                                    rest[start + 1..start + end].to_string()
                                });
                            }
                        }
                        if let Some(pos) = rest.find(", pid=") {
                            let value = rest[pos + ", pid=".len()..]
                                .split(',')
                                .next()
                                .unwrap_or("")
                                .trim();
                            pid = value.parse().ok();
                        }
                    }
                }
            }
            if let Some(rest) = trimmed.strip_prefix("# JRE version: ") {
                java_version.get_or_insert_with(|| rest.to_string());
            }
            if let Some(rest) = trimmed.strip_prefix("# Java VM: ") {
                java_vm.get_or_insert_with(|| rest.to_string());
                if rest.contains("64-Bit") {
                    java_bits.get_or_insert(64.0);
                } else if rest.contains("32-Bit") || rest.contains("Client VM") {
                    java_bits.get_or_insert(32.0);
                }
            }
            if let Some(rest) = trimmed.strip_prefix("# ") {
                if let Some(inner) = rest.trim_start().split_once('[') {
                    let inside = inner.1.split(']').next().unwrap_or("");
                    if let Some((library, offset)) = inside.rsplit_once('+') {
                        if !library.trim().is_empty() {
                            let _ = module
                                .replace((library.trim().to_string(), offset.trim().to_string()));
                        }
                    }
                }
            }
            if let Some(rest) = trimmed.strip_prefix("siginfo:") {
                if let Some(start) = rest.find("reading address ") {
                    fault = Some(rest[start + "reading address ".len()..].trim().to_string());
                    access = Some("read".to_string());
                } else if let Some(start) = rest.find("writing address ") {
                    fault = Some(rest[start + "writing address ".len()..].trim().to_string());
                    access = Some("write".to_string());
                }
            }
            if in_system {
                if let Some(rest) = trimmed.strip_prefix("OS: ") {
                    os.get_or_insert_with(|| rest.trim().to_string());
                }
                if let Some(rest) = trimmed.strip_prefix("Memory: ") {
                    if let Some((_, after)) = rest.split_once('(') {
                        if let Some(free) = after.split_whitespace().next() {
                            let kb = free.trim_end_matches('k');
                            if let Ok(kb) = kb.parse::<f64>() {
                                ram_free_mb.get_or_insert(kb / 1024.0);
                            }
                        }
                    }
                }
            }
            if trimmed.contains("OutOfMemory") || trimmed.contains("Out of Memory") {
                out_of_memory = true;
            }
        }

        let module = module.filter(|(library, _)| !library.is_empty());
        let category = if out_of_memory {
            "oom"
        } else if signal.as_deref() == Some("EXCEPTION_ACCESS_VIOLATION") {
            "access_violation"
        } else if signal.is_some() {
            "fast_fail"
        } else {
            "unknown"
        };

        if let Some(signal) = &signal {
            let near_null = fault
                .as_deref()
                .and_then(parse_hex)
                .is_some_and(|address| address < 0x1_0000);
            let mut attrs = vec![
                ("class", text(signal.clone())),
                ("category", text(category)),
                (
                    "message",
                    text(
                        code.clone()
                            .map(|code| format!("signal {signal} ({code})"))
                            .unwrap_or_else(|| format!("signal {signal}")),
                    ),
                ),
                ("depth", number(0.0)),
            ];
            if let Some(access) = &access {
                attrs.push(("access", text(access.clone())));
            }
            if let Some(fault) = &fault {
                attrs.push(("fault_address", text(fault.clone())));
            }
            attrs.push(("near_null", AttrValue::Scalar(Scalar::Bool(near_null))));
            facts.push(fact("core.exception", attrs, Vec::new(), 0, "fatal"));
        }
        if let Some((library, offset)) = &module {
            facts.push(fact(
                "core.frame",
                vec![
                    ("index", number(0.0)),
                    ("kind", text("probable")),
                    ("symbol_scheme", text("native")),
                    ("symbol", text(format!("{library}+{offset}"))),
                ],
                vec![reference("module", library.clone(), Strength::ExecTop)],
                0,
                "fatal",
            ));
            facts.push(fact(
                "core.module",
                Vec::new(),
                vec![reference("module", library.clone(), Strength::Mention)],
                0,
                "fatal",
            ));
        }
        if let Some(pid) = pid {
            facts.push(fact(
                "core.process",
                vec![("pid", number(pid))],
                Vec::new(),
                0,
                "fatal",
            ));
        }
        if let Some(os) = &os {
            facts.push(fact(
                "core.system",
                vec![("os", text(os.clone()))],
                Vec::new(),
                0,
                "system",
            ));
        }
        if let Some(free) = ram_free_mb {
            facts.push(fact(
                "core.system",
                vec![("ram_free_mb", number(free))],
                Vec::new(),
                0,
                "system",
            ));
        }
        let mut system_attrs = Vec::new();
        if let Some(version) = &java_version {
            system_attrs.push(("java_version", text(version.clone())));
        }
        if let Some(vm) = &java_vm {
            system_attrs.push(("java_vm", text(vm.clone())));
        }
        if let Some(bits) = java_bits {
            system_attrs.push(("java_bits", number(bits)));
        }
        let has_system = !system_attrs.is_empty();
        if has_system {
            facts.push(fact(
                "minecraft.system",
                system_attrs,
                Vec::new(),
                0,
                "system",
            ));
        }

        let mut bucket = Vec::new();
        if let Some(signal) = &signal {
            let value = match &module {
                Some((library, _)) => format!("{signal}|{library}"),
                None => signal.clone(),
            };
            bucket.push(BucketPart {
                scheme: "hs_err".to_string(),
                value,
            });
        }

        let coverage = vec![
            checked("core.exception"),
            if module.is_some() {
                checked("core.frame")
            } else {
                absent("core.frame", "the file names no problematic frame")
            },
            if module.is_some() {
                checked("core.module")
            } else {
                absent("core.module", "the file names no problematic frame")
            },
            checked("core.system"),
            if pid.is_some() {
                checked("core.process")
            } else {
                absent("core.process", "the file does not name a process id")
            },
            if has_system {
                checked("minecraft.system")
            } else {
                absent(
                    "minecraft.system",
                    "the file does not state its Java version",
                )
            },
        ];

        Ok(ParserOutput {
            contract: CRASH_CONTRACT_VERSION,
            producer: Producer {
                format: FORMAT.to_string(),
                version: None,
            },
            facts,
            bucket,
            coverage,
            notes: notes_for(&collected, request.limits),
        })
    }
}

fn parse_hex(text: &str) -> Option<u64> {
    u64::from_str_radix(text.trim().trim_start_matches("0x"), 16).ok()
}
