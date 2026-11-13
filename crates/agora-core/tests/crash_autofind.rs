//! Sweeping older crash logs from a pack's `sources` and reading the pattern across them.
//! Every name here is invented; no real log is copied.

use std::path::Path;

use agora_core::crash_engine::attribution::{InstanceView, LayerOwner, LayerView, Resolver};
use agora_core::crash_engine::history::{
    summarize_recorded, CrashPlace, HistorySuspect, RecordedCrash,
};
use agora_core::crash_engine::{
    analyze, analyze_with_history, AnalyzeInput, CrashFile, CrashHistoryInput, Inspection,
    InspectionOutcome, InspectionRequest, Inspector, InstanceAnalysis,
};
use agora_core::crash_history::{
    load_recorded, prepare, sweep_instance_logs, HistorySetup, LogSweepLimits,
};
use agora_core::crash_packs::LoadedCrashPack;
use agora_core::ctx::Ctx;
use agora_game_api::crash::{lint_pack_json, Level, Owner, CRASH_CONTRACT_VERSION};
use agora_plugin_host::QuickJsHost;
use rusqlite::Connection;
use tempfile::TempDir;

const GAME: &str = "invented-game";
const PLUGIN: &str = "test.autofind";

fn db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    agora_core::db::run_migrations(&conn).unwrap();
    conn
}

fn test_ctx() -> (TempDir, Ctx) {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = Ctx::for_testing(tmp.path().to_path_buf());
    (tmp, ctx)
}

/// A pack with two file sources: the one a sweep expects, and a second a mod might add for its
/// own log folder. The rule names a module through the instance view.
fn test_pack() -> LoadedCrashPack {
    let json = r#"{
        "contract": 1,
        "sources": [
            {
                "id": "logs",
                "where": { "root": "instance", "path": "logs" },
                "match": { "glob": "*.txt" },
                "format": "test_log"
            },
            {
                "id": "mod_logs",
                "where": { "root": "instance", "path": "mod-logs" },
                "match": { "glob": "*.txt" },
                "format": "test_log"
            }
        ],
        "parsers": [
            {
                "id": "test_log",
                "detect": { "name_glob": "*.txt" },
                "entry": {
                    "generic": {
                        "name": "text_sections",
                        "config": {
                            "sections": [
                                {
                                    "id": "crash",
                                    "header": "^== CRASH ==$",
                                    "lines": [
                                        {
                                            "pattern": "^\\[(?P<index>\\d+)\\] (?P<mod>[A-Za-z0-9_.-]+)\\+0x(?P<rva>[0-9a-fA-F]+)$",
                                            "kind": "core.frame",
                                            "attrs": { "index": "index", "rva": "rva" },
                                            "refs": [
                                                { "kind": "module", "value": "mod", "strength": "exec" }
                                            ]
                                        }
                                    ]
                                }
                            ]
                        }
                    }
                }
            }
        ],
        "ref_maps": [
            {
                "kind": "module",
                "lookup": { "path": { "patterns": ["Mods/*/{value}"] } },
                "match": "casefold"
            }
        ],
        "rules": [
            {
                "id": "module_on_stack",
                "version": 1,
                "when": {
                    "fact": {
                        "kind": "core.frame",
                        "ref": { "kind": "module", "strength": "exec", "bind": "m" }
                    }
                },
                "effects": [
                    {
                        "signal": {
                            "family": "execution",
                            "candidate": { "owner_of": "m" },
                            "weight": 12
                        }
                    }
                ],
                "text": { "headline": { "en": "{owner:m} was on the stack." } }
            }
        ]
    }"#;
    let pack = lint_pack_json(json).unwrap_or_else(|problems| panic!("{problems:#?}"));
    assert_eq!(pack.contract, CRASH_CONTRACT_VERSION);
    LoadedCrashPack {
        plugin_id: PLUGIN.to_string(),
        plugin_name: "Autofind test pack".to_string(),
        plugin_version: "1.0.0".to_string(),
        dependencies: Vec::new(),
        official: false,
        file: "pack.json".to_string(),
        pack,
    }
}

fn view() -> InstanceView {
    InstanceView::new(vec![
        LayerView::new(
            "invented-one",
            true,
            LayerOwner::Item {
                key: "invented-one".to_string(),
                name: "Invented One".to_string(),
            },
            vec!["Mods/InventedOne/InventedMod.dll".to_string()],
        ),
        LayerView::new(
            "invented-two",
            true,
            LayerOwner::Item {
                key: "invented-two".to_string(),
                name: "Invented Two".to_string(),
            },
            vec!["Mods/InventedTwo/OtherMod.dll".to_string()],
        ),
    ])
}

struct NoInspector;

impl Inspector for NoInspector {
    fn inspect(&self, _request: &InspectionRequest<'_>) -> InspectionOutcome {
        Inspection::Missing.into()
    }
}

fn write_log(root: &Path, relative: &str, rva: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!("== CRASH ==\n[0] InventedMod.dll+0x{rva}\n")).unwrap();
}

fn log_bytes(rva: &str) -> Vec<u8> {
    format!("== CRASH ==\n[0] InventedMod.dll+0x{rva}\n").into_bytes()
}

struct Fixture {
    _tmp: TempDir,
    ctx: Ctx,
    conn: Connection,
    instance_dir: std::path::PathBuf,
    pack: LoadedCrashPack,
    resolver: Resolver,
    inspector: NoInspector,
    host: QuickJsHost,
}

impl Fixture {
    fn new() -> Self {
        let (tmp, ctx) = test_ctx();
        let pack = test_pack();
        let resolver = Resolver::new(view(), pack.pack.ref_maps.clone(), pack.pack.roles.clone());
        let instance_dir = ctx.paths.instance_dir("inst").unwrap();
        std::fs::create_dir_all(&instance_dir).unwrap();
        Self {
            _tmp: tmp,
            ctx,
            conn: db(),
            instance_dir,
            pack,
            resolver,
            inspector: NoInspector,
            host: QuickJsHost::new(),
        }
    }

    fn setup(&self) -> HistorySetup<'_> {
        HistorySetup {
            instance_id: "inst",
            game_id: GAME,
            runtime_version: Some("1.0"),
            packs: std::slice::from_ref(&self.pack),
            built_in: &[],
            host: &self.host,
            resolver: &self.resolver,
            inspector: &self.inspector,
            could_not_check: &[],
            setup: None,
            exclude_hashes: &[],
        }
    }

    fn sweep(&self) -> agora_core::crash_history::LogSweepReport {
        sweep_instance_logs(
            &self.ctx,
            &self.conn,
            &self.setup(),
            LogSweepLimits::default(),
        )
        .unwrap()
    }
}

#[test]
fn an_empty_source_folder_is_explicitly_nothing_to_read_not_a_clean_result() {
    let fixture = Fixture::new();
    std::fs::create_dir_all(fixture.instance_dir.join("logs")).unwrap();
    std::fs::create_dir_all(fixture.instance_dir.join("mod-logs")).unwrap();
    let report = fixture.sweep();
    assert_eq!(report.found, 0);
    assert_eq!(report.analyzed, 0);
    assert!(report.problems.is_empty(), "{:#?}", report.problems);
    assert!(load_recorded(&fixture.conn, "inst", &[])
        .unwrap()
        .is_empty());
}

#[test]
fn a_missing_source_folder_is_reported_never_read_as_clean() {
    let fixture = Fixture::new();
    let report = fixture.sweep();
    assert_eq!(report.found, 0);
    assert_eq!(
        report.problems.len(),
        2,
        "both declared source folders are missing: {:#?}",
        report.problems
    );
    assert!(report
        .problems
        .iter()
        .all(|problem| !problem.detail.is_empty()));
}

#[test]
fn both_declared_sources_are_swept_and_one_log_is_analysed_once() {
    let fixture = Fixture::new();
    write_log(&fixture.instance_dir, "logs/first.txt", "1000");
    write_log(&fixture.instance_dir, "mod-logs/second.txt", "2000");

    let report = fixture.sweep();
    assert_eq!(report.sources, 2);
    assert_eq!(report.found, 2);
    assert_eq!(report.analyzed, 2, "{report:#?}");
    assert_eq!(report.reused, 0);

    let stored = load_recorded(&fixture.conn, "inst", &[]).unwrap();
    assert_eq!(stored.len(), 2);
    assert!(stored.iter().all(|crash| crash.record.readable));
    for crash in &stored {
        assert_eq!(
            crash.record.candidates[0].owner,
            Owner::Item {
                key: "invented-one".to_string(),
                name: "Invented One".to_string(),
            }
        );
    }
    let summary = summarize_recorded(&stored);
    assert_eq!(summary.crashes, 2);
    assert_eq!(summary.suspects[0].crashes_named, 2);
    assert_eq!(summary.suspects[0].distinct_places, 2);

    // The same bytes are never analysed again.
    let again = fixture.sweep();
    assert_eq!(again.found, 2);
    assert_eq!(again.analyzed, 0);
    assert_eq!(again.reused, 2);
    assert_eq!(load_recorded(&fixture.conn, "inst", &[]).unwrap().len(), 2);

    // Changed bytes are a new crash, analysed once.
    write_log(&fixture.instance_dir, "logs/first.txt", "3000");
    let changed = fixture.sweep();
    assert_eq!(changed.analyzed, 1);
    assert_eq!(changed.reused, 1);
    assert_eq!(load_recorded(&fixture.conn, "inst", &[]).unwrap().len(), 3);
}

#[test]
fn older_logs_over_the_sweep_bound_are_reported_as_skipped() {
    let fixture = Fixture::new();
    for index in 0..4 {
        write_log(
            &fixture.instance_dir,
            &format!("logs/crash-{index}.txt"),
            &format!("{:x}", 0x1000 + index),
        );
    }
    let limits = LogSweepLimits {
        max_logs: 2,
        ..LogSweepLimits::default()
    };
    let report =
        sweep_instance_logs(&fixture.ctx, &fixture.conn, &fixture.setup(), limits).unwrap();
    assert_eq!(report.found, 4);
    assert_eq!(report.analyzed, 2);
    assert_eq!(report.skipped_older, 2);
    let prepared = prepare(&fixture.ctx, &fixture.conn, fixture.setup(), limits, true).unwrap();
    assert!(prepared.skipped_note(limits).is_some());
}

#[test]
fn a_recurring_suspect_boosts_the_answer_and_gets_one_counting_finding() {
    let fixture = Fixture::new();
    let file = CrashFile::new("current.txt", log_bytes("1000"));
    let instance = InstanceAnalysis {
        game_id: GAME,
        resolver: &fixture.resolver,
        inspector: &fixture.inspector,
        could_not_check: &[],
    };
    let without = analyze(&AnalyzeInput {
        packs: std::slice::from_ref(&fixture.pack),
        files: std::slice::from_ref(&file),
        runtime_version: Some("1.0"),
        host: &fixture.host,
        built_in: &[],
        instance: Some(instance),
    })
    .unwrap();
    let baseline = without.answer.candidates[0].score;

    let prior: Vec<RecordedCrash> = ["2000", "3000"]
        .iter()
        .enumerate()
        .map(|(index, rva)| RecordedCrash {
            record: agora_core::crash_engine::history::CrashRecord {
                artifact: format!("old-{index}.txt"),
                readable: true,
                candidates: vec![HistorySuspect {
                    owner: Owner::Item {
                        key: "invented-one".to_string(),
                        name: "Invented One".to_string(),
                    },
                    role: None,
                }],
                place: Some(CrashPlace {
                    kind: "module".to_string(),
                    value: "InventedMod.dll".to_string(),
                    location: Some(format!("0x{rva}")),
                }),
            },
            bucket: Vec::new(),
            at_unix_ms: Some(1_000 + index as i64),
        })
        .collect();
    let sessions = std::collections::BTreeMap::new();
    let with = analyze_with_history(
        &AnalyzeInput {
            packs: std::slice::from_ref(&fixture.pack),
            files: std::slice::from_ref(&file),
            runtime_version: Some("1.0"),
            host: &fixture.host,
            built_in: &[],
            instance: Some(instance),
        },
        &[],
        &[],
        Some(&CrashHistoryInput {
            prior: &prior,
            sessions_since_first: &sessions,
            setup_since_unix_ms: None,
        }),
    )
    .unwrap();
    assert!(
        with.answer.candidates[0].score > baseline,
        "a repeat is a boost: {} vs {baseline}",
        with.answer.candidates[0].score
    );
    let finding = with
        .answer
        .findings
        .iter()
        .find(|finding| {
            finding.source.rule_id.as_deref()
                == Some(agora_core::crash_engine::history::RECURRING_SUSPECT_RULE)
        })
        .expect("a recurrence at different places gets a finding");
    assert_eq!(finding.level, Level::Possible);
    assert_eq!(
        finding.headline.get("en").map(String::as_str),
        Some("Invented One was involved in 2 of your last 2 crashes.")
    );
    assert!(finding.candidate.is_some());
}

#[test]
fn a_framework_owner_is_never_boosted_by_history() {
    let prior = vec![RecordedCrash {
        record: agora_core::crash_engine::history::CrashRecord {
            artifact: "old.txt".to_string(),
            readable: true,
            candidates: vec![HistorySuspect {
                owner: Owner::Framework {
                    name: "Invented Engine".to_string(),
                },
                role: Some(agora_game_api::crash::Role::Framework),
            }],
            place: Some(CrashPlace {
                kind: "module".to_string(),
                value: "engine.dll".to_string(),
                location: Some("0x1".to_string()),
            }),
        },
        bucket: Vec::new(),
        at_unix_ms: None,
    }];
    let summary = summarize_recorded(&prior);
    assert_eq!(summary.suspects.len(), 0);
    assert_eq!(summary.always_running.len(), 1);
    assert!(agora_core::crash_engine::history::repeat_signals(&summary).is_empty());
}
