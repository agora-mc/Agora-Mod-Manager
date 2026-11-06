//! Hostile tests for attribution. Each is named for the failure it catches.

use std::time::Instant;

use agora_game_api::crash::{
    CrashPack, NameMatch, Owner, Ref, RefLookup, RefMap, Role, RoleRule, Strength,
};

use super::index::ArchiveIndex;
use super::*;

fn owner(key: &str) -> Owner {
    Owner::Item {
        key: key.to_string(),
        name: format!("{key} mod"),
    }
}

fn item(key: &str) -> LayerOwner {
    LayerOwner::Item {
        key: key.to_string(),
        name: format!("{key} mod"),
    }
}

fn layer(id: &str, enabled: bool, owner: LayerOwner, files: &[&str]) -> LayerView {
    LayerView::new(
        id,
        enabled,
        owner,
        files.iter().map(|file| file.to_string()).collect(),
    )
}

fn path_map(kind: &str, patterns: &[&str], matching: NameMatch, archives: bool) -> RefMap {
    RefMap {
        kind: kind.to_string(),
        lookup: RefLookup::Path {
            patterns: patterns.iter().map(|pattern| pattern.to_string()).collect(),
            archives,
        },
        matching,
    }
}

fn index_map(kind: &str, index: &str, matching: NameMatch) -> RefMap {
    RefMap {
        kind: kind.to_string(),
        lookup: RefLookup::Index(index.to_string()),
        matching,
    }
}

fn role_rule(kind: &str, value: &str, matching: NameMatch, role: Role) -> RoleRule {
    RoleRule {
        kind: kind.to_string(),
        value: value.to_string(),
        matching,
        role,
    }
}

fn reference(kind: &str, value: &str) -> Ref {
    Ref {
        kind: kind.to_string(),
        value: value.to_string(),
        strength: Strength::default(),
    }
}

fn resolver(view: InstanceView, maps: Vec<RefMap>, roles: Vec<RoleRule>) -> Resolver {
    Resolver::new(view, maps, roles)
}

#[test]
fn a_dotdot_value_is_never_a_file_key() {
    let view = InstanceView::new(vec![layer(
        "mod-a",
        true,
        item("mod-a"),
        &["Data/mods/a.dll"],
    )]);
    let resolver = resolver(
        view,
        vec![path_map("file", &["{value}"], NameMatch::Casefold, false)],
        Vec::new(),
    );
    let answer = resolver.attribute(&reference("file", r"..\..\windows\system32\x.dll"));
    assert_eq!(answer.owner, Owner::Unknown);
    assert!(answer.reason.unwrap().contains("not a path"));
}

#[test]
fn an_absolute_value_is_unknown() {
    let view = InstanceView::new(vec![layer(
        "mod-a",
        true,
        item("mod-a"),
        &["Data/mods/a.dll"],
    )]);
    let resolver = resolver(
        view,
        vec![path_map("file", &["{value}"], NameMatch::Casefold, false)],
        Vec::new(),
    );
    for value in [r"C:\Windows\System32\x.dll", "/etc/passwd"] {
        let answer = resolver.attribute(&reference("file", value));
        assert_eq!(answer.owner, Owner::Unknown, "{value}");
    }
}

#[test]
fn a_value_containing_the_placeholder_is_taken_literally() {
    let view = InstanceView::new(vec![layer("mod-a", true, item("mod-a"), &["Data/{value}"])]);
    let resolver = resolver(
        view,
        vec![path_map("file", &["Data/{value}"], NameMatch::Exact, false)],
        Vec::new(),
    );
    let answer = resolver.attribute(&reference("file", "{value}"));
    assert_eq!(
        answer.owner,
        Owner::Item {
            key: "mod-a".to_string(),
            name: "mod-a mod".to_string(),
        }
    );
}

#[test]
fn empty_and_whitespace_values_are_unknown() {
    let view = InstanceView::new(vec![layer(
        "mod-a",
        true,
        item("mod-a"),
        &["Data/mods/a.dll"],
    )]);
    let resolver = resolver(
        view,
        vec![path_map(
            "file",
            &["Data/{value}"],
            NameMatch::Casefold,
            false,
        )],
        Vec::new(),
    );
    for value in ["", "   ", "\t\n"] {
        let answer = resolver.attribute(&reference("file", value));
        assert_eq!(answer.owner, Owner::Unknown, "{value:?}");
    }
}

#[test]
fn wrong_case_name_matches_by_casefold() {
    let view = InstanceView::new(vec![layer(
        "mod-a",
        true,
        item("mod-a"),
        &["Data/Meshes/Example/Hair.NIF"],
    )]);
    let resolver = resolver(
        view,
        vec![path_map(
            "file",
            &["Data/{value}"],
            NameMatch::Casefold,
            false,
        )],
        Vec::new(),
    );
    let answer = resolver.attribute(&reference("file", "meshes/example/hair.nif"));
    assert_eq!(answer.owner, owner("mod-a"));
    assert_eq!(answer.via.as_deref(), Some("Data/meshes/example/hair.nif"));
}

#[test]
fn forward_and_back_slashes_match() {
    let view = InstanceView::new(vec![layer(
        "mod-a",
        true,
        item("mod-a"),
        &["Data/meshes/hair.nif"],
    )]);
    let resolver = resolver(
        view,
        vec![path_map(
            "file",
            &["Data/{value}"],
            NameMatch::Casefold,
            false,
        )],
        Vec::new(),
    );
    let answer = resolver.attribute(&reference("file", r"meshes\hair.nif"));
    assert_eq!(answer.owner, owner("mod-a"));
}

#[test]
fn a_trailing_dot_matches_the_windows_name() {
    let view = InstanceView::new(vec![layer(
        "mod-a",
        true,
        item("mod-a"),
        &["Data/mods/a.dll"],
    )]);
    let resolver = resolver(
        view,
        vec![path_map(
            "file",
            &["Data/{value}"],
            NameMatch::Casefold,
            false,
        )],
        Vec::new(),
    );
    let answer = resolver.attribute(&reference("file", "mods/a.dll."));
    assert_eq!(answer.owner, owner("mod-a"));
}

#[test]
fn unicode_case_pairs_that_ntfs_does_not_fold_stay_distinct() {
    let view = InstanceView::new(vec![layer(
        "mod-a",
        true,
        item("mod-a"),
        &["Data/straße.txt"],
    )]);
    let resolver = resolver(
        view,
        vec![path_map(
            "file",
            &["Data/{value}"],
            NameMatch::Casefold,
            false,
        )],
        Vec::new(),
    );
    for value in ["strasse.txt", "STRASSE.txt"] {
        let answer = resolver.attribute(&reference("file", value));
        assert_eq!(answer.owner, Owner::NotInInstance, "{value}");
    }
    let answer = resolver.attribute(&reference("file", "STRAßE.txt"));
    assert_eq!(answer.owner, owner("mod-a"));
}

#[test]
fn three_mods_later_position_wins_and_listing_order_does_not_matter() {
    let make = |files_a: &[&str], files_b: &[&str], files_c: &[&str]| {
        InstanceView::new(vec![
            layer("mod-a", true, item("mod-a"), files_a),
            layer("mod-b", true, item("mod-b"), files_b),
            layer("mod-c", true, item("mod-c"), files_c),
        ])
    };
    let maps = vec![path_map(
        "file",
        &["Data/{value}"],
        NameMatch::Casefold,
        false,
    )];
    let first = resolver(
        make(
            &["Data/shared.esp", "Data/only-a.esp"],
            &["Data/only-b.esp", "Data/shared.esp"],
            &["Data/shared.esp", "Data/only-c.esp"],
        ),
        maps.clone(),
        Vec::new(),
    );
    let second = resolver(
        make(
            &["Data/only-a.esp", "Data/shared.esp"],
            &["Data/shared.esp", "Data/only-b.esp"],
            &["Data/only-c.esp", "Data/shared.esp"],
        ),
        maps,
        Vec::new(),
    );
    let value = reference("file", "shared.esp");
    let first_answer = first.attribute(&value);
    let second_answer = second.attribute(&value);
    assert_eq!(first_answer.owner, owner("mod-c"));
    assert_eq!(first_answer.also_provided_by, ["mod-a", "mod-b"]);
    assert_eq!(first_answer, second_answer);
}

#[test]
fn a_turned_off_winner_lets_the_next_one_own() {
    let view = InstanceView::new(vec![
        layer("mod-a", true, item("mod-a"), &["Data/shared.esp"]),
        layer("mod-b", true, item("mod-b"), &["Data/shared.esp"]),
        layer("mod-c", false, item("mod-c"), &["Data/shared.esp"]),
    ]);
    let resolver = resolver(
        view,
        vec![path_map(
            "file",
            &["Data/{value}"],
            NameMatch::Casefold,
            false,
        )],
        Vec::new(),
    );
    let answer = resolver.attribute(&reference("file", "shared.esp"));
    assert_eq!(answer.owner, owner("mod-b"));
    assert_eq!(answer.also_provided_by, ["mod-a"]);
}

#[test]
fn only_a_turned_off_provider_is_informative_not_unknown() {
    let view = InstanceView::new(vec![layer(
        "mod-a",
        false,
        item("mod-a"),
        &["Data/shared.esp"],
    )]);
    let resolver = resolver(
        view,
        vec![path_map(
            "file",
            &["Data/{value}"],
            NameMatch::Casefold,
            false,
        )],
        Vec::new(),
    );
    let answer = resolver.attribute(&reference("file", "shared.esp"));
    assert_eq!(answer.owner, owner("mod-a"));
    assert!(answer.reason.unwrap().contains("turned off"));
}

#[test]
fn an_unavailable_listing_is_unknown_not_not_in_instance() {
    let maps = vec![path_map(
        "file",
        &["Data/{value}"],
        NameMatch::Casefold,
        false,
    )];
    let unavailable = resolver(
        InstanceView::unavailable("the disk went away"),
        maps.clone(),
        Vec::new(),
    );
    let answer = unavailable.attribute(&reference("file", "shared.esp"));
    assert_eq!(answer.owner, Owner::Unknown);
    assert!(answer.reason.unwrap().contains("disk went away"));

    let not_given = resolver(InstanceView::not_given(), maps, Vec::new());
    assert_eq!(
        not_given.attribute(&reference("file", "shared.esp")).owner,
        Owner::Unknown
    );
}

#[test]
fn a_complete_listing_that_lacks_the_file_is_not_in_instance() {
    let view = InstanceView::new(vec![layer(
        "mod-a",
        true,
        item("mod-a"),
        &["Data/other.esp"],
    )]);
    let resolver = resolver(
        view,
        vec![path_map(
            "file",
            &["Data/{value}"],
            NameMatch::Casefold,
            false,
        )],
        Vec::new(),
    );
    assert_eq!(
        resolver.attribute(&reference("file", "shared.esp")).owner,
        Owner::NotInInstance
    );
}

struct FakeArchives {
    available: bool,
    entries: Vec<(String, Vec<String>)>,
}

impl ArchiveIndex for FakeArchives {
    fn archives_containing(&self, path: &str) -> Option<Vec<String>> {
        if !self.available {
            return None;
        }
        Some(
            self.entries
                .iter()
                .find(|(name, _)| name == path)
                .map(|(_, archives)| archives.clone())
                .unwrap_or_default(),
        )
    }
}

#[test]
fn an_unavailable_archive_index_is_unknown() {
    let view = InstanceView::new(vec![layer(
        "mod-a",
        true,
        item("mod-a"),
        &["Data/Skyrim - Meshes.bsa"],
    )]);
    let maps = vec![path_map(
        "file",
        &["Data/{value}"],
        NameMatch::Casefold,
        true,
    )];
    for resolver in [
        resolver(view.clone(), maps.clone(), Vec::new()),
        resolver(view.clone(), maps.clone(), Vec::new()).with_archive_index(FakeArchives {
            available: false,
            entries: Vec::new(),
        }),
    ] {
        let answer = resolver.attribute(&reference("file", "meshes/hair.nif"));
        assert_eq!(answer.owner, Owner::Unknown);
        assert!(answer.reason.unwrap().contains("archive index"));
    }
}

#[test]
fn an_available_archive_index_names_the_archive_owner() {
    let view = InstanceView::new(vec![layer(
        "mod-bsa",
        true,
        item("mod-bsa"),
        &["Data/Skyrim - Meshes.bsa"],
    )]);
    let resolver = resolver(
        view,
        vec![path_map(
            "file",
            &["Data/{value}"],
            NameMatch::Casefold,
            true,
        )],
        Vec::new(),
    )
    .with_archive_index(FakeArchives {
        available: true,
        entries: vec![(
            "Data/meshes/hair.nif".to_string(),
            vec!["Data/Skyrim - Meshes.bsa".to_string()],
        )],
    });
    let answer = resolver.attribute(&reference("file", "meshes/hair.nif"));
    assert_eq!(answer.owner, owner("mod-bsa"));
    assert_eq!(answer.via.as_deref(), Some("Data/Skyrim - Meshes.bsa"));
}

#[test]
fn a_very_long_value_resolves_without_panicking() {
    let view = InstanceView::new(vec![layer(
        "mod-a",
        true,
        item("mod-a"),
        &["Data/mods/a.dll"],
    )]);
    let resolver = resolver(
        view,
        vec![path_map("file", &["{value}"], NameMatch::Casefold, false)],
        Vec::new(),
    );
    let answer = resolver.attribute(&reference("file", &"a".repeat(200_000)));
    assert_eq!(answer.owner, Owner::NotInInstance);
}

#[test]
fn a_role_on_a_kind_with_no_map_still_reports_the_role() {
    let view = InstanceView::new(Vec::new());
    let resolver = resolver(
        view,
        Vec::new(),
        vec![role_rule(
            "class",
            "net.minecraft",
            NameMatch::PrefixDot,
            Role::Platform,
        )],
    );
    let answer = resolver.attribute(&reference("class", "net.minecraft.client.Main"));
    assert_eq!(answer.owner, Owner::Unknown);
    assert_eq!(answer.role, Some(Role::Platform));
    assert!(answer.reason.unwrap().contains("no ref map"));
}

#[test]
fn an_index_lookup_finds_the_item_that_declares_the_name() {
    let view = InstanceView::new(vec![
        layer("mod-a", true, item("mod-a"), &["mods/a.jar"])
            .with_index("mod_id", vec!["examplemod".to_string()]),
        layer("mod-b", true, item("mod-b"), &["mods/b.jar"])
            .with_index("java_package", vec!["com.example".to_string()]),
    ]);
    let resolver = resolver(
        view,
        vec![
            index_map("mod_id", "mod_id", NameMatch::Exact),
            index_map("class", "java_package", NameMatch::PrefixDot),
        ],
        Vec::new(),
    );
    assert_eq!(
        resolver.attribute(&reference("mod_id", "examplemod")).owner,
        owner("mod-a")
    );
    assert_eq!(
        resolver
            .attribute(&reference("class", "com.example.Widget"))
            .owner,
        owner("mod-b")
    );
}

#[test]
fn the_worked_skyrim_example_resolves_as_expected() {
    let pack: CrashPack = serde_json::from_str(include_str!(
        "../../../../agora-game-api/tests/crash_examples/skyrim.pack.json"
    ))
    .expect("the Skyrim example parses");
    let view = InstanceView::new(vec![
        layer("base", true, LayerOwner::Base, &["SkyrimSE.exe"]),
        layer(
            "example-physics",
            true,
            item("example-physics"),
            &["Data/SKSE/Plugins/ExamplePhysics.dll"],
        ),
        layer(
            "example-hair",
            true,
            item("example-hair"),
            &["Data/meshes/example/hair.nif"],
        ),
    ]);
    let resolver = Resolver::from_pack(view, &pack);

    let game = resolver.attribute(&reference("module", "SkyrimSE.exe"));
    assert_eq!(game.owner, Owner::Base);
    assert_eq!(game.role, Some(Role::Platform));

    let physics = resolver.attribute(&reference("module", "ExamplePhysics.dll"));
    assert_eq!(physics.owner, owner("example-physics"));
    assert_eq!(physics.role, Some(Role::HookFramework));
    assert_eq!(
        physics.via.as_deref(),
        Some("Data/SKSE/Plugins/ExamplePhysics.dll")
    );

    let mesh = resolver.attribute(&reference("file", "meshes/example/hair.nif"));
    assert_eq!(mesh.owner, owner("example-hair"));
    assert_eq!(mesh.role, None);
    assert_eq!(mesh.via.as_deref(), Some("Data/meshes/example/hair.nif"));
}

#[test]
fn resolution_is_deterministic_in_any_input_order() {
    let maps = vec![path_map(
        "file",
        &["Data/{value}"],
        NameMatch::Casefold,
        false,
    )];
    let first = resolver(
        InstanceView::new(vec![
            layer("mod-a", true, item("mod-a"), &["Data/x.esp", "Data/y.esp"]),
            layer("mod-b", true, item("mod-b"), &["Data/y.esp", "Data/x.esp"]),
        ]),
        maps.clone(),
        Vec::new(),
    );
    let second = resolver(
        InstanceView::new(vec![
            layer("mod-a", true, item("mod-a"), &["Data/y.esp", "Data/x.esp"]),
            layer("mod-b", true, item("mod-b"), &["Data/x.esp", "Data/y.esp"]),
        ]),
        maps,
        Vec::new(),
    );
    for value in ["x.esp", "y.esp", "missing.esp"] {
        let reference = reference("file", value);
        assert_eq!(
            first.attribute(&reference),
            second.attribute(&reference),
            "{value}"
        );
    }
}

#[test]
fn the_index_scales_to_a_large_instance() {
    const MODS: usize = 368;
    const FILES_PER_MOD: usize = 1_360;
    const REFS: usize = 10_000;

    let mut layers = Vec::with_capacity(MODS);
    for module in 0..MODS {
        let files: Vec<String> = (0..FILES_PER_MOD)
            .map(|file| format!("Data/mod{module:03}/f{file:04}.nif"))
            .collect();
        layers.push(LayerView::new(
            format!("mod-{module:03}"),
            true,
            item(&format!("mod-{module:03}")),
            files,
        ));
    }
    let started = Instant::now();
    let view = InstanceView::new(layers);
    let build = started.elapsed();

    let resolver = resolver(
        view,
        vec![path_map(
            "file",
            &["Data/{value}"],
            NameMatch::Casefold,
            false,
        )],
        Vec::new(),
    );
    let started = Instant::now();
    for index in 0..REFS {
        let module = index % MODS;
        let file = (index * 7) % FILES_PER_MOD;
        let answer = resolver.attribute(&reference(
            "file",
            &format!("mod{module:03}/f{file:04}.nif"),
        ));
        assert_eq!(answer.owner, owner(&format!("mod-{module:03}")));
    }
    let resolving = started.elapsed();
    eprintln!(
        "attribution scale: {} files, {} refs: build {:?}, resolve {:?}",
        MODS * FILES_PER_MOD,
        REFS,
        build,
        resolving
    );
    assert!(
        resolving.as_secs() < 30,
        "resolving {REFS} refs took {resolving:?}"
    );
}
