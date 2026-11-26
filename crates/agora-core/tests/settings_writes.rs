use agora_core::{db, settings};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

type WriteSetting = fn(&Connection, &str, bool) -> anyhow::Result<()>;

#[test]
fn every_settings_writer_commits_value_and_version_together() {
    assert_all_sql_writers_are_covered();

    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("local_state.db");
    db::init_local_state_db(&path).unwrap();
    let writer = db::local_state_connection(&path).unwrap();
    let reader = db::local_state_connection(&path).unwrap();
    let writers: [(&str, WriteSetting); 2] = [
        ("db::set_setting", |conn, key, value| {
            db::set_setting(conn, key, &json!(value))
        }),
        ("settings::set", |conn, key, value| {
            settings::set(conn, key, if value { "true" } else { "false" }).map(drop)
        }),
    ];
    let mut failures = Vec::new();
    for (name, write) in writers {
        let key = format!("test.versioned.{name}");
        for value in [false, true] {
            let before = db::settings_version(&reader);
            let commits = Arc::new(AtomicUsize::new(0));
            writer.commit_hook(Some({
                let commits = Arc::clone(&commits);
                move || {
                    commits.fetch_add(1, Ordering::SeqCst);
                    false
                }
            }));
            write(&writer, &key, value).unwrap();
            writer.commit_hook(None::<fn() -> bool>);
            assert_eq!(
                commits.load(Ordering::SeqCst),
                1,
                "{name} must publish each setting with one SQLite commit"
            );
            assert_eq!(
                db::get_setting(&reader, &key).unwrap(),
                Some(Value::Bool(value))
            );

            let after = db::settings_version(&reader);
            match (before, after) {
                (Ok(before), Ok(after)) if after > before => {}
                (before, after) => failures.push(format!("{name} writing {value} failed to advance a readable version: {before:?} -> {after:?}")),
            }
        }

        let before = db::settings_version(&reader);
        let vetoes = Arc::new(AtomicUsize::new(0));
        writer.commit_hook(Some({
            let vetoes = Arc::clone(&vetoes);
            move || {
                vetoes.fetch_add(1, Ordering::SeqCst);
                true
            }
        }));
        let result = write(&writer, &key, false);
        writer.commit_hook(None::<fn() -> bool>);
        assert_eq!(
            vetoes.load(Ordering::SeqCst),
            1,
            "the injected commit veto did not reach {name}"
        );
        assert!(result.is_err(), "{name} must report a failed commit");
        assert_eq!(
            db::get_setting(&reader, &key).unwrap(),
            Some(json!(true)),
            "{name} left a value behind after its commit was vetoed"
        );
        if let Ok(before) = before {
            assert_eq!(
                db::settings_version(&reader).unwrap(),
                before,
                "{name} left a version bump behind after its commit was vetoed"
            );
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// Force a new SQL writer to join the transaction cases above, or delegate to a writer already
// exercised there. The pre-version migration is intentionally exempt.
fn assert_all_sql_writers_are_covered() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let pattern = regex::Regex::new(
        r#"(?i)\b(?:insert(?:\s+or\s+\w+)?\s+into|replace\s+into|delete\s+from|update(?:\s+or\s+\w+)?)\s+(?:main\.)?["`\[]?user_settings\b"#,
    ).unwrap();
    let test_module =
        regex::Regex::new(r"(?m)^#\[cfg\(test\)\]\r?\nmod \w+ \{\r?\n(?s:.*?)^\}").unwrap();
    let crate_sources = root
        .join("crates")
        .read_dir()
        .unwrap()
        .map(|entry| entry.unwrap().path().join("src"));
    let mut pending: Vec<_> = crate_sources
        .chain([root.join("desktop/src-tauri/src")])
        .filter(|path| path.is_dir())
        .collect();
    while let Some(dir) = pending.pop() {
        for entry in dir.read_dir().unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                pending.push(path);
                continue;
            }
            if path.extension().and_then(|extension| extension.to_str()) != Some("rs") {
                continue;
            }
            let source = fs::read_to_string(&path).unwrap();
            for hit in pattern.find_iter(&source) {
                if test_module
                    .find_iter(&source)
                    .any(|module| module.range().contains(&hit.start()))
                {
                    continue;
                }
                let owner = source[..hit.start()].lines().rev().find_map(|line| {
                    let (prefix, name) = line.split_once("fn ")?;
                    (!prefix.trim_start().starts_with('/')).then(|| name.split('(').next().unwrap())
                });
                let relative = path
                    .strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                assert!(
                    matches!((relative.as_str(), owner),
                        ("crates/agora-core/src/db.rs", Some("set_setting" | "migrate_v10"))
                        | ("crates/agora-core/src/settings.rs", Some("set"))
                    ),
                    "uncovered settings SQL writer at {relative}:{} in {owner:?}; delegate to a tested writer or add a transaction case",
                    source[..hit.start()].lines().count(),
                );
            }
        }
    }
}
