//! Review proofs for plugin overlaps, load-order rules, caching, and sorting.

use std::sync::{Arc, Mutex};

use agora_core::ctx::CoreContext;
use agora_core::game_base::BaseMode;
use agora_core::game_discovery::{DiscoveredInstall, InstallCapabilities};
use agora_core::game_instance::{create, GameInstanceRecord};
use agora_core::game_load_order::{
    self, classify_overlaps, sort_entries, LoadAfterRule, LoadOrderEntry,
};
use agora_core::game_registry::{
    GameRegistry, IdentifiedInstall, PackageSource, RuntimeResolution,
};
use agora_core::session_ledger::{self, LaunchManifest, Owner};
use agora_game_api::{
    DeploymentStrategy, GameContext, GameDefinition, GameFuture, GameId, GamePackage, GamePath,
    InstallId, InstallKind, LoadOrderDefinition, LoadOrderFormat, LoadOrderRule, LoadOrderRules,
    LoadOrderRulesReply, OverlapItem, OverlapKey, OverlapReply, OverlapsRule, PackageDefinition,
    RelPath, RuntimeIdentity, SorterNote, SorterReply, SorterRequest, SorterRule, SorterStatus,
    StoreId, StoreIdentifier, UserDataLocation, UserFileMapping, UserFileStrategy,
};
use tempfile::TempDir;

fn entry(name: &str, masters: &[&str]) -> LoadOrderEntry {
    LoadOrderEntry {
        name: name.to_string(),
        implicit: false,
        active: true,
        present: true,
        managed: false,
        locked: false,
        badges: Vec::new(),
        masters: masters.iter().map(|m| m.to_string()).collect(),
    }
}

fn overlap_key(key: &str, label: &str, items: &[(&str, &str)]) -> OverlapKey {
    OverlapKey {
        key: key.to_string(),
        label: label.to_string(),
        items: items
            .iter()
            .map(|(name, digest)| OverlapItem {
                name: name.to_string(),
                digest: digest.to_string(),
            })
            .collect(),
    }
}

#[test]
fn transitive_load_after_rule_fails_to_mark_overlap_as_decided() {
    // User sets rules: C loads after B, and B loads after A.
    // By transitivity (which add_load_after_rule and sort enforce), C must load after A.
    let list = [
        entry("Base.esm", &[]),
        entry("A.esp", &[]),
        entry("B.esp", &[]),
        entry("C.esp", &[]),
    ];
    let rules = [
        LoadAfterRule {
            item: "B.esp".into(),
            after: "A.esp".into(),
        },
        LoadAfterRule {
            item: "C.esp".into(),
            after: "B.esp".into(),
        },
    ];
    let reply = OverlapReply {
        keys: vec![overlap_key(
            "Base.esm:000001",
            "GMST fJumpHeightMin",
            &[
                ("Base.esm", "orig"),
                ("A.esp", "change_a"),
                ("C.esp", "change_c"),
            ],
        )],
        notes: Vec::new(),
    };

    let report = classify_overlaps(&list, &reply, &rules).unwrap();
    assert_eq!(
        report.decided, 1,
        "C.esp > B.esp > A.esp already decides C.esp after A.esp, but classify_overlaps only checks direct rules: {report:?}"
    );
}

#[test]
fn winner_masters_traverses_inactive_intermediate_plugin() {
    let mut middle = entry("Middle.esp", &["A.esp"]);
    middle.active = false; // Middle.esp is disabled!
    let list = [
        entry("Base.esm", &[]),
        entry("A.esp", &[]),
        middle,
        entry("Winner.esp", &["Middle.esp"]),
    ];
    let reply = OverlapReply {
        keys: vec![overlap_key(
            "Base.esm:000001",
            "GMST fJumpHeightMin",
            &[
                ("Base.esm", "orig"),
                ("A.esp", "change_a"),
                ("Winner.esp", "change_w"),
            ],
        )],
        notes: Vec::new(),
    };

    let report = classify_overlaps(&list, &reply, &[]).unwrap();
    assert_eq!(
        report.patched, 0,
        "Middle.esp is inactive and not loaded, yet winner_masters traversed it to mark Winner.esp as patching A.esp"
    );
}

#[test]
fn sort_entries_can_pull_dependent_above_its_own_master() {
    // Initial order:
    // 1. A.esp (masters: ["B.esp"]) -- currently broken because B.esp is at 3.
    // 2. M.esm (masters: [])
    // 3. B.esp (masters: ["M.esm"]) -- currently valid because M.esm is at 2 < 3!
    //
    // In step 1 of sort_entries:
    // Dependent A.esp (at=0) needs B.esp (from=2).
    // place_free moves B.esp from index 2 to index 0 (above M.esm!).
    // Order after move 1: [B.esp, A.esp, M.esm].
    // Now B.esp is at 0, which is ABOVE its master M.esm at 2!
    // In step 2 of sort_entries:
    // Dependent B.esp (at=0) needs M.esm (from=2).
    // place_free moves M.esm from index 2 to index 0.
    // Final order: [M.esm, B.esp, A.esp].
    // Wait: what if M.esm is LOCKED at position 2?!
    // If M.esm is locked at position 2:
    // Step 1 moves B.esp to position 1 (above locked M.esm at 2)!
    // Then in step 2, B.esp (at 1) needs M.esm (at 2), which is LOCKED and cannot move!
    // So sort_entries BROKE B.esp's satisfied master requirement on M.esm by pulling B.esp above locked M.esm!
    let mut m_locked = entry("M.esm", &[]);
    m_locked.locked = true;
    let mut list = vec![
        entry("A.esp", &["B.esp"]),
        m_locked,
        entry("B.esp", &["M.esm"]),
    ];

    let (_moves, blocked) = sort_entries(&mut list).unwrap();
    let b_pos = list.iter().position(|e| e.name == "B.esp").unwrap();
    let m_pos = list.iter().position(|e| e.name == "M.esm").unwrap();
    assert!(
        m_pos < b_pos,
        "sort_entries moved B.esp (pos {b_pos}) above its locked master M.esm (pos {m_pos}), breaking a valid master order! list={list:?}, blocked={blocked:?}"
    );
}

// ---------------------------------------------------------------------------
// Integration harness for rules, caching, and LOOT findings
// ---------------------------------------------------------------------------

static LOCK: Mutex<()> = Mutex::new(());

struct ReviewPackage {
    def: PackageDefinition,
    rules: Vec<LoadOrderRule>,
    overlaps_calls: Arc<Mutex<usize>>,
    notes_calls: Arc<Mutex<usize>>,
}

impl GamePackage for ReviewPackage {
    fn definition(&self) -> &PackageDefinition {
        &self.def
    }

    fn load_order_rules<'a>(
        &'a self,
        _context: GameContext,
        _list_id: &'a str,
    ) -> GameFuture<'a, Option<LoadOrderRulesReply>> {
        let rules = self.rules.clone();
        Box::pin(async move {
            Ok(Some(LoadOrderRulesReply {
                rules,
                ..Default::default()
            }))
        })
    }

    fn sort_load_order<'a>(
        &'a self,
        request: SorterRequest,
    ) -> GameFuture<'a, Option<SorterReply>> {
        let calls = self.notes_calls.clone();
        Box::pin(async move {
            *calls.lock().unwrap() += 1;
            Ok(Some(SorterReply {
                order: request
                    .entries
                    .iter()
                    .filter(|e| e.present)
                    .map(|e| e.name.clone())
                    .collect(),
                notes: vec![SorterNote {
                    item: "A.esp".into(),
                    level: "warn".into(),
                    text: format!("note for {}", request.context.instance),
                }],
                general: Vec::new(),
                checks: Vec::new(),
            }))
        })
    }

    fn update_sorter<'a>(
        &'a self,
        _request: SorterRequest,
        _update: bool,
    ) -> GameFuture<'a, Option<SorterStatus>> {
        Box::pin(async move {
            Ok(Some(SorterStatus {
                fetched_at: "2026-11-16T00:00:00Z".into(),
                source: "test".into(),
                data_version: "v1".into(),
            }))
        })
    }

    fn find_overlaps<'a>(
        &'a self,
        _request: SorterRequest,
    ) -> GameFuture<'a, Option<OverlapReply>> {
        let calls = self.overlaps_calls.clone();
        Box::pin(async move {
            *calls.lock().unwrap() += 1;
            Ok(Some(OverlapReply {
                keys: vec![overlap_key(
                    "Base:1",
                    "GMST fTest",
                    &[("Base.esm", "0"), ("A.esp", "1"), ("B.esp", "2")],
                )],
                notes: Vec::new(),
            }))
        })
    }
}

struct ReviewHarness {
    _guard: std::sync::MutexGuard<'static, ()>,
    _tmp: TempDir,
    _user: TempDir,
    ctx: CoreContext,
    def: GameDefinition,
    install: IdentifiedInstall,
}

impl Drop for ReviewHarness {
    fn drop(&mut self) {
        std::env::remove_var("AGORA_TEST_USER_DATA_ROOT");
    }
}

impl ReviewHarness {
    fn new(rules: Vec<LoadOrderRule>) -> Self {
        let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let user = TempDir::new().unwrap();
        std::env::set_var("AGORA_TEST_USER_DATA_ROOT", user.path());

        let install_dir = tmp.path().join("install");
        std::fs::create_dir_all(install_dir.join("Data")).unwrap();
        std::fs::write(install_dir.join("Game.exe"), b"exe").unwrap();
        for name in ["Base.esm", "A.esp", "B.esp", "C.esp"] {
            std::fs::write(install_dir.join("Data").join(name), name.as_bytes()).unwrap();
        }

        let rule = LoadOrderDefinition {
            id: "plugins".into(),
            name: "Plugins".into(),
            file: RelPath::new("user/Plugins.txt").unwrap(),
            destination: Default::default(),
            items: agora_game_api::LoadOrderItems {
                folder: RelPath::new("Data").unwrap(),
                patterns: vec!["*.esm".into(), "*.esp".into()],
                folders: false,
            },
            format: LoadOrderFormat::Lines {
                active_prefix: "*".into(),
                header: Vec::new(),
            },
            rules: LoadOrderRules {
                kind: "script".into(),
                entry: Some("rules".into()),
                implicit: vec!["Base.esm".into()],
                implicit_list_file: None,
                sorter: Some(SorterRule {
                    hook: "sort".into(),
                    update: Some("update".into()),
                    update_label: None,
                    options: serde_json::Value::Null,
                }),
                overlaps: Some(OverlapsRule {
                    hook: "overlaps".into(),
                    options: serde_json::Value::Null,
                }),
            },
            intro: None,
            item_heading: None,
        };

        let def = GameDefinition {
            mo2_game_name: None,
            import_ids: Default::default(),
            session: Default::default(),
            id: GameId::new("test-game").unwrap(),
            extends: None,
            name: "Test Game".into(),
            stores: vec![StoreIdentifier {
                store: StoreId::steam(),
                product: "12345".into(),
            }],
            version_sources: vec![],
            deployment: DeploymentStrategy::VirtualFileSystem,
            content_rules: vec![],
            content_layout: None,
            framework_ids: vec![],
            tool_ids: vec![],
            launch: Some(agora_game_api::LaunchRecipe {
                executable: GamePath::Runtime {
                    path: RelPath::new("Game.exe").unwrap(),
                },
                arguments: vec![],
                environment: Default::default(),
                working_directory: GamePath::Runtime {
                    path: RelPath::default(),
                },
            }),
            user_files: vec![UserFileMapping::new(
                GamePath::UserData {
                    location: UserDataLocation::LocalAppData,
                    path: RelPath::new("Test Game/Plugins.txt").unwrap(),
                },
                RelPath::new("user/Plugins.txt").unwrap(),
                UserFileStrategy::JournaledSwap,
            )
            .with_stores(vec![StoreId::steam()])],
            save_paths: vec![],
            linked_archive_patterns: vec![],
            declared_writes: vec![],
            excluded_paths: vec![],
            load_orders: vec![rule],
            runtime_files: Vec::new(),
            save_location: Vec::new(),
            ini_checks: Vec::new(),
            launch_alternatives: Vec::new(),
            copy_patterns: Vec::new(),
        };

        let overlaps_calls = Arc::new(Mutex::new(0));
        let notes_calls = Arc::new(Mutex::new(0));
        let pkg = PackageDefinition {
            id: "test.test-game".into(),
            version: semver::Version::new(0, 1, 0),
            api_range: semver::VersionReq::parse(">=0.1, <0.2").unwrap(),
            parents: vec![],
            game_templates: std::collections::BTreeMap::new(),
            behaviours: None,
            games: vec![def.clone()],
            frameworks: vec![],
            tools: vec![],
        };

        let ctx = CoreContext::for_testing(tmp.path().join("data"));
        agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
        let mut builder = GameRegistry::builder();
        builder
            .add(
                PackageSource::Compiled {
                    crate_name: "test".into(),
                },
                Arc::new(ReviewPackage {
                    def: pkg,
                    rules,
                    overlaps_calls,
                    notes_calls,
                }),
            )
            .unwrap();
        let ctx = ctx.with_games(Arc::new(builder.build()));

        let runtime = RuntimeIdentity {
            game: def.id.clone(),
            store: StoreId::steam(),
            version: "1.0.0".into(),
            build: None,
        };
        let volume =
            agora_core::game_discovery::volume::VolumeDetector::new().get_volume_info(&install_dir);
        let install = IdentifiedInstall {
            game: def.id.clone(),
            install_id: InstallId::new("steam:12345").unwrap(),
            discovered: DiscoveredInstall {
                store: StoreId::steam(),
                product: "12345".into(),
                name: "Test Game".into(),
                kind: InstallKind::BaseGame,
                parent_product: None,
                location: install_dir,
                store_version: Some("1.0.0".into()),
                store_build: None,
                executables: vec!["Game.exe".into()],
                capabilities: InstallCapabilities {
                    executables_readable: true,
                    accepts_new_files: true,
                    relocatable: true,
                },
                volume,
            },
            add_ons: vec![],
            runtime: RuntimeResolution::Identified {
                runtime,
                source: "executable".into(),
            },
        };

        Self {
            _guard: guard,
            _tmp: tmp,
            _user: user,
            ctx,
            def,
            install,
        }
    }

    fn instance(&self, name: &str, lines: &str) -> GameInstanceRecord {
        let inst = create(
            &self.ctx,
            &self.install,
            &self.def,
            name,
            None,
            BaseMode::Linked,
            &|_| {},
        )
        .unwrap();
        let path = self
            .ctx
            .paths
            .instance_dir(&inst.instance_id)
            .unwrap()
            .join("user/Plugins.txt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, lines.as_bytes()).unwrap();
        inst
    }
}

#[test]
fn add_load_after_rule_allows_rule_violating_implicit_plugin_position() {
    let h = ReviewHarness::new(Vec::new());
    // Base.esm is implicit (always loaded first, cannot move).
    let inst = h.instance("implicit-rule", "*A.esp\r\n*B.esp\r\n");

    // Asking Base.esm (implicit, always first) to load after A.esp can NEVER hold!
    let result = game_load_order::add_load_after_rule(
        &h.ctx,
        &inst.instance_id,
        &h.def,
        Some("plugins"),
        "Base.esm",
        "A.esp",
    );
    assert!(
        result.is_err(),
        "add_load_after_rule allowed setting 'Base.esm loads after A.esp' even though Base.esm is always loaded first: {result:?}"
    );
}

#[test]
fn replace_load_after_rules_accepts_cycles_and_master_contradictions() {
    let h = ReviewHarness::new(vec![LoadOrderRule::Requires {
        item: "B.esp".into(),
        master: "A.esp".into(),
        optional: false,
    }]);
    let inst = h.instance("replace-cycle", "*A.esp\r\n*B.esp\r\n");

    // A.esp is a master of B.esp, and A->B + B->A is a 2-cycle.
    game_load_order::replace_load_after_rules(
        &h.ctx,
        &inst.instance_id,
        "plugins",
        &[
            LoadAfterRule {
                item: "A.esp".into(),
                after: "B.esp".into(),
            },
            LoadAfterRule {
                item: "B.esp".into(),
                after: "A.esp".into(),
            },
        ],
    )
    .unwrap();

    let stored = game_load_order::load_after_rules(&h.ctx, &inst.instance_id, "plugins").unwrap();
    assert!(
        stored.is_empty(),
        "replace_load_after_rules stored a master contradiction and 2-cycle without validation: {stored:?}"
    );
}

#[test]
fn replace_load_after_rules_keeps_a_rule_that_can_hold() {
    let h = ReviewHarness::new(Vec::new());
    let inst = h.instance("replace-valid", "*A.esp\r\n*B.esp\r\n");

    game_load_order::replace_load_after_rules(
        &h.ctx,
        &inst.instance_id,
        "plugins",
        &[LoadAfterRule {
            item: "A.esp".into(),
            after: "B.esp".into(),
        }],
    )
    .unwrap();

    let stored = game_load_order::load_after_rules(&h.ctx, &inst.instance_id, "plugins").unwrap();
    assert_eq!(
        stored,
        [LoadAfterRule {
            item: "A.esp".into(),
            after: "B.esp".into(),
        }],
        "a valid replacement rule must still be stored: {stored:?}"
    );
}

#[test]
fn replace_load_after_rules_skips_a_rule_that_puts_an_implicit_plugin_after_a_normal_one() {
    let h = ReviewHarness::new(Vec::new());
    let inst = h.instance("replace-implicit", "*A.esp\r\n*B.esp\r\n");

    game_load_order::replace_load_after_rules(
        &h.ctx,
        &inst.instance_id,
        "plugins",
        &[LoadAfterRule {
            item: "Base.esm".into(),
            after: "A.esp".into(),
        }],
    )
    .unwrap();

    let stored = game_load_order::load_after_rules(&h.ctx, &inst.instance_id, "plugins").unwrap();
    assert!(
        stored.is_empty(),
        "replace stored a rule that would make the always-loaded Base.esm load after a normal plugin: {stored:?}"
    );
}

#[test]
fn add_load_after_rule_names_the_implicit_plugin_as_the_reason() {
    let h = ReviewHarness::new(Vec::new());
    let inst = h.instance("implicit-reason", "*A.esp\r\n*B.esp\r\n");

    let error = game_load_order::add_load_after_rule(
        &h.ctx,
        &inst.instance_id,
        &h.def,
        Some("plugins"),
        "Base.esm",
        "A.esp",
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("always loaded first"),
        "the refusal must name why: {error}"
    );
}

#[test]
fn overlaps_and_notes_cache_collide_across_instances_of_same_game() {
    let h = ReviewHarness::new(Vec::new());
    let inst1 = h.instance("inst-1", "*A.esp\r\n*B.esp\r\n");
    let inst2 = h.instance("inst-2", "*A.esp\r\n*B.esp\r\n");

    // Refresh sorter notes on inst1 (stores "note for inst-1" under key (game_id, list_id)).
    let notes1 =
        game_load_order::refresh_sorter_notes(&h.ctx, &inst1.instance_id, &h.def, Some("plugins"))
            .unwrap();
    assert_eq!(notes1[0].message, format!("note for {}", inst1.instance_id));

    // Plain read on inst2 gets inst1's cached notes because SorterNotesCache is keyed by (game, list) without instance_id!
    let order2 =
        game_load_order::order_in(&h.ctx, &inst2.instance_id, &h.def, Some("plugins")).unwrap();
    assert!(
        order2.notes_out_of_date || order2.notes[0].message.contains(&inst2.instance_id),
        "inst2 was served inst1's cached sorter notes: {:?}",
        order2.notes
    );
}

#[test]
fn overlaps_cache_does_not_share_a_report_across_instances_of_same_game() {
    let h = ReviewHarness::new(Vec::new());
    let inst1 = h.instance("overlaps-1", "*A.esp\r\n*B.esp\r\n");
    let inst2 = h.instance("overlaps-2", "*A.esp\r\n*B.esp\r\n");

    let report =
        game_load_order::refresh_overlaps(&h.ctx, &inst1.instance_id, &h.def, Some("plugins"))
            .unwrap();
    assert_eq!(report.pairs.len(), 1);

    // The two instances have the same files and rules, so only the instance key separates them.
    let order2 =
        game_load_order::order_in(&h.ctx, &inst2.instance_id, &h.def, Some("plugins")).unwrap();
    assert!(
        order2.overlaps.is_none(),
        "inst2 was served inst1's cached overlaps report: {:?}",
        order2.overlaps
    );
    assert!(
        order2.overlaps_out_of_date,
        "inst2 must ask its own overlaps reader"
    );
}

#[test]
fn new_since_last_launch_ignores_inactive_plugin_moving_in_previous_manifest() {
    let h = ReviewHarness::new(Vec::new());
    // Suppose in the last launch, plugins were: Inactive.esp (off), A.esp (on), B.esp (on).
    // Now Inactive.esp was removed (or moved below B.esp), while A.esp and B.esp are still active in the same relative order (A then B)!
    let inst = h.instance("false-moved", "*A.esp\r\n*B.esp\r\n");

    let conn = agora_core::db::local_state_connection(&h.ctx.paths.local_state_db()).unwrap();
    let prev_manifest = LaunchManifest {
        runtime: "steam:1.0.0".into(),
        load_order: vec![
            session_ledger::LoadOrderEntry {
                list: "plugins".into(),
                name: "Inactive.esp".into(),
                active: false,
            },
            session_ledger::LoadOrderEntry {
                list: "plugins".into(),
                name: "A.esp".into(),
                active: true,
            },
            session_ledger::LoadOrderEntry {
                list: "plugins".into(),
                name: "B.esp".into(),
                active: true,
            },
        ],
        owner_layers: Vec::new(),
        ..LaunchManifest::default()
    };
    session_ledger::begin(
        &conn,
        &inst.instance_id,
        "2026-01-01T00:00:00Z",
        &prev_manifest,
        Owner::current(),
    )
    .unwrap();
    drop(conn);

    // Neither A.esp nor B.esp was added, turned on, or moved relative to active plugins:
    // only an inactive plugin above them was removed!
    let report =
        game_load_order::refresh_overlaps(&h.ctx, &inst.instance_id, &h.def, Some("plugins"))
            .unwrap();
    assert_eq!(report.pairs.len(), 1);
    assert!(
        !report.pairs[0].new_since_last_launch,
        "removing an inactive plugin at index 0 shifted raw indices of A.esp and B.esp from (1,2) to (0,1), falsely flagging their conflict as new since last launch!"
    );
}
