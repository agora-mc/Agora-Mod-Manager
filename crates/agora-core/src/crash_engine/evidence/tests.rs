//! Hostile tests for collecting, copying, sweeping and retaining crash evidence. Each is named for
//! the failure it catches, so a regression fails a test instead of quietly losing a log.

use super::*;
use agora_game_api::crash::{CrashSource, MatchedBy, Retained};
use agora_game_api::GamePath;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

struct FixedRoots {
    root: Option<PathBuf>,
    resolved: Option<PathBuf>,
}

impl SourceRoots for FixedRoots {
    fn resolve(&self, location: &GamePath) -> Result<PathBuf, String> {
        if let Some(resolved) = &self.resolved {
            return Ok(resolved.clone());
        }
        let root = self.root.as_ref().ok_or("no test root")?;
        match location {
            GamePath::Instance { path } => Ok(root.join(path.as_str())),
            _ => Ok(root.clone()),
        }
    }
}

struct FakeFacts(Vec<(PathBuf, ProcessFacts)>);

impl ArtifactFacts for FakeFacts {
    fn process_facts(&self, artifact: &Path) -> Option<ProcessFacts> {
        self.0
            .iter()
            .find(|(path, _)| path == artifact)
            .map(|(_, facts)| facts.clone())
    }
}

fn source_at(id: &str, where_path: &str, glob: &str, bind: serde_json::Value) -> CrashSource {
    serde_json::from_value(json!({
        "id": id,
        "where": { "root": "instance", "path": where_path },
        "match": { "glob": glob },
        "bind": bind,
    }))
    .expect("a valid source")
}

fn time_bind(before: u32, after: u32) -> serde_json::Value {
    json!([{ "time": { "before_s": before, "after_s": after } }])
}

fn around_now() -> SessionWindow {
    let now = SystemTime::now();
    SessionWindow::new(
        now - Duration::from_secs(3600),
        now + Duration::from_secs(3600),
    )
}

fn session_with(window: SessionWindow, pid: Option<u32>, cwd: Option<&Path>) -> CrashSession {
    CrashSession {
        session_id: "session-1".to_string(),
        pid,
        working_dir: cwd.map(Path::to_path_buf),
        window,
    }
}

fn collect_all(root: &Path, sources: &[CrashSource], session: &CrashSession) -> SourceCollection {
    collect_with(root, sources, session, &NoArtifactFacts)
}

fn collect_with(
    root: &Path,
    sources: &[CrashSource],
    session: &CrashSession,
    facts: &dyn ArtifactFacts,
) -> SourceCollection {
    let roots = FixedRoots {
        root: Some(root.to_path_buf()),
        resolved: None,
    };
    let specs: Vec<SourceSpec<'_>> = sources
        .iter()
        .map(|source| SourceSpec {
            plugin_id: "test.plugin",
            source,
        })
        .collect();
    collect_sources(&specs, &roots, facts, session)
}

fn write(root: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = root.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&path, bytes).unwrap();
    path
}

fn copied(
    root: &Path,
    session: &CrashSession,
    collection: &SourceCollection,
    id: &str,
) -> CopiedSession {
    EvidenceStore::new(root)
        .copy(id, collection, CopyLimits::default())
        .unwrap_or_else(|error| panic!("copy failed: {error} (session {})", session.session_id))
}

#[test]
fn an_old_log_outside_the_window_is_not_bound() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "old.log", b"from a past session");
    let now = SystemTime::now();
    let window = SessionWindow::new(
        now + Duration::from_secs(3600),
        now + Duration::from_secs(7200),
    );
    let session = session_with(window, None, None);
    let source = source_at("old", "", "*.log", time_bind(0, 0));

    let collection = collect_all(tmp.path(), &[source], &session);

    assert!(collection.bound.is_empty(), "an old log must stay out");
    assert_eq!(collection.seen.len(), 1);
    assert_eq!(
        collection.seen[0].reason,
        RejectedReason::OutsideSessionWindow
    );
}

#[test]
fn pid_match_beats_time() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "crash-4242.log", b"this session");
    let session = session_with(around_now(), Some(4242), None);
    let source = source_at(
        "crash",
        "",
        "crash-*.log",
        json!([
            { "time": { "before_s": 3600, "after_s": 3600 } },
            { "pid_in_name": { "regex": "crash-(\\d+)\\.log" } }
        ]),
    );

    let collection = collect_all(tmp.path(), std::slice::from_ref(&source), &session);

    assert_eq!(collection.bound.len(), 1);
    assert_eq!(collection.bound[0].matched_by, MatchedBy::Pid);

    // The same file with a different process id falls back to the time, not to a pid match.
    write(tmp.path(), "crash-9999.log", b"another process");
    let collection = collect_all(tmp.path(), std::slice::from_ref(&source), &session);
    let other = collection
        .bound
        .iter()
        .find(|artifact| artifact.name == "crash-9999.log")
        .expect("the time still ties it to the session");
    assert_eq!(other.matched_by, MatchedBy::Time);
}

#[test]
fn process_facts_bind_a_log_whose_name_does_not() {
    let tmp = tempfile::tempdir().unwrap();
    let file = write(tmp.path(), "run.log", b"names no process");
    let session = session_with(around_now(), Some(555), Some(tmp.path()));
    let source = source_at(
        "facts",
        "",
        "*.log",
        json!(["pid_in_facts", "working_dir_in_facts"]),
    );

    let both = FakeFacts(vec![(
        file.clone(),
        ProcessFacts {
            pid: Some(555),
            working_dir: Some(tmp.path().to_path_buf()),
        },
    )]);
    let collection = collect_with(tmp.path(), std::slice::from_ref(&source), &session, &both);
    assert_eq!(collection.bound.len(), 1);
    assert_eq!(collection.bound[0].matched_by, MatchedBy::Pid);

    let only_folder = FakeFacts(vec![(
        file,
        ProcessFacts {
            pid: None,
            working_dir: Some(tmp.path().to_path_buf()),
        },
    )]);
    let collection = collect_with(tmp.path(), &[source], &session, &only_folder);
    assert_eq!(collection.bound.len(), 1);
    assert_eq!(collection.bound[0].matched_by, MatchedBy::WorkingDir);
}

#[test]
fn a_source_with_no_bind_that_works_rejects_everything() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "one.log", b"one");
    write(tmp.path(), "two.log", b"two");
    let session = session_with(around_now(), Some(7), None);

    let undeclared = source_at("undeclared", "", "*.log", json!([]));
    let collection = collect_all(tmp.path(), &[undeclared], &session);
    assert!(collection.bound.is_empty());
    assert_eq!(collection.seen.len(), 2);
    assert!(collection
        .seen
        .iter()
        .all(|seen| seen.reason == RejectedReason::NoBindDeclared));

    let mismatched = source_at(
        "mismatched",
        "",
        "*.log",
        json!([{ "pid_in_name": { "regex": "pid-(\\d+)\\.log" } }]),
    );
    let collection = collect_all(tmp.path(), &[mismatched], &session);
    assert!(collection.bound.is_empty());
    assert!(collection
        .seen
        .iter()
        .all(|seen| matches!(seen.reason, RejectedReason::NoBindMatched)));
}

#[test]
fn a_parent_component_in_a_resolved_path_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    write(&outside, "secret.log", b"not this instance");

    let roots = FixedRoots {
        root: None,
        resolved: Some(tmp.path().join("instance").join("..").join("outside")),
    };
    let source = source_at("escape", "", "*.log", time_bind(0, 0));
    let session = session_with(around_now(), None, None);
    let specs = [SourceSpec {
        plugin_id: "test.plugin",
        source: &source,
    }];

    let collection = collect_sources(&specs, &roots, &NoArtifactFacts, &session);

    assert!(collection.bound.is_empty());
    assert!(collection
        .problems
        .iter()
        .any(|problem| { problem.kind == SourceProblemKind::PathEscapes }));
}

#[test]
fn a_file_deleted_between_listing_and_copy_is_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let file = write(tmp.path(), "gone.log", b"here for now");
    let session = session_with(around_now(), None, None);
    let source = source_at("gone", "", "*.log", time_bind(0, 0));
    let collection = collect_all(tmp.path(), &[source], &session);
    assert_eq!(collection.bound.len(), 1);

    std::fs::remove_file(&file).unwrap();
    let store = EvidenceStore::new(tmp.path().join("store"));
    let copied = store
        .copy("session-1", &collection, CopyLimits::default())
        .unwrap();

    assert_eq!(copied.artifacts.len(), 1);
    assert!(matches!(
        copied.artifacts[0].retained,
        Retained::NotRetained { .. }
    ));
    assert!(copied
        .problems
        .iter()
        .any(|problem| problem.kind == CopyProblemKind::Vanished));
}

#[cfg(windows)]
#[test]
fn a_locked_file_is_a_reported_problem() {
    use std::os::windows::fs::OpenOptionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let file = write(tmp.path(), "locked.log", b"held open");
    let _lock = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&file)
        .unwrap();

    let session = session_with(around_now(), None, None);
    let source = source_at("locked", "", "*.log", time_bind(0, 0));
    let collection = collect_all(tmp.path(), &[source], &session);
    let copied = EvidenceStore::new(tmp.path().join("store"))
        .copy("session-1", &collection, CopyLimits::default())
        .unwrap();

    assert!(matches!(
        copied.artifacts[0].retained,
        Retained::NotRetained { .. }
    ));
    assert!(copied
        .problems
        .iter()
        .any(|problem| problem.kind == CopyProblemKind::Unreadable));
}

#[test]
fn a_two_gigabyte_sparse_file_is_not_loaded_into_memory() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("huge.log");
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(2 * 1024 * 1024 * 1024).unwrap();
    drop(file);

    let session = session_with(around_now(), None, None);
    let source = source_at("huge", "", "*.log", time_bind(0, 0));
    let collection = collect_all(tmp.path(), &[source], &session);
    assert_eq!(collection.bound.len(), 1);

    let started = std::time::Instant::now();
    let copied = EvidenceStore::new(tmp.path().join("store"))
        .copy("session-1", &collection, CopyLimits::default())
        .unwrap();
    let elapsed = started.elapsed();

    let artifact = &copied.artifacts[0];
    match &artifact.retained {
        Retained::HeadTail { head, tail } => {
            assert_eq!(*head, DEFAULT_HEAD_BYTES);
            assert_eq!(*tail, DEFAULT_TAIL_BYTES);
        }
        other => panic!("a 2 GB log must be cut, not kept whole: {other:?}"),
    }
    assert_eq!(artifact.size, 2 * 1024 * 1024 * 1024);
    let stored = copied
        .objects
        .get(&artifact.id)
        .expect("the cut copy is stored");
    let stored_size = std::fs::metadata(stored).unwrap().len();
    assert!(
        stored_size <= DEFAULT_HEAD_BYTES + DEFAULT_TAIL_BYTES,
        "only the head and tail are copied, got {stored_size} bytes"
    );
    assert!(
        elapsed < Duration::from_secs(30),
        "reading 2 GB would take far longer: {elapsed:?}"
    );
}

#[test]
fn an_oversized_copy_keeps_head_and_tail_and_says_it_was_cut() {
    let tmp = tempfile::tempdir().unwrap();
    let body: Vec<u8> = (0..100u8).collect();
    write(tmp.path(), "big.log", &body);
    let session = session_with(around_now(), None, None);
    let source = source_at("big", "", "*.log", time_bind(0, 0));
    let collection = collect_all(tmp.path(), &[source], &session);

    let limits = CopyLimits {
        max_file_bytes: 50,
        head_bytes: 10,
        tail_bytes: 20,
    };
    let copied = EvidenceStore::new(tmp.path().join("store"))
        .copy("session-1", &collection, limits)
        .unwrap();

    assert_eq!(
        copied.artifacts[0].retained,
        Retained::HeadTail { head: 10, tail: 20 }
    );
    let stored = copied
        .objects
        .get(&copied.artifacts[0].id)
        .expect("stored copy");
    let bytes = std::fs::read(stored).unwrap();
    assert_eq!(bytes.len(), 30);
    assert_eq!(&bytes[..10], &body[..10]);
    assert_eq!(&bytes[10..], &body[80..]);
}

#[cfg(unix)]
#[test]
fn a_non_utf8_name_is_handled() {
    use std::os::unix::ffi::OsStringExt;

    let tmp = tempfile::tempdir().unwrap();
    let mut name = b"crash-".to_vec();
    name.push(0xFF);
    name.extend(b".log");
    let path = tmp.path().join(std::ffi::OsString::from_vec(name));
    std::fs::write(&path, b"bytes with a strange name").unwrap();

    let session = session_with(around_now(), None, None);
    let source = source_at("odd", "", "*.log", time_bind(0, 0));
    let collection = collect_all(tmp.path(), &[source], &session);
    assert_eq!(collection.bound.len(), 1);
}

#[cfg(windows)]
#[test]
fn a_windows_name_with_an_unpaired_surrogate_is_handled_if_the_volume_allows_it() {
    use std::os::windows::ffi::OsStringExt;

    let tmp = tempfile::tempdir().unwrap();
    let name = std::ffi::OsString::from_wide(&[
        0xD800,
        b'x' as u16,
        b'.' as u16,
        b'l' as u16,
        b'o' as u16,
        b'g' as u16,
    ]);
    let path = tmp.path().join(&name);
    if std::fs::write(&path, b"a name that is not valid unicode").is_err() {
        // NTFS on this machine refused the name; there is nothing to collect. Not a failure of
        // the collector.
        return;
    }

    let session = session_with(around_now(), None, None);
    let source = source_at("odd", "", "*.log", time_bind(0, 0));
    let collection = collect_all(tmp.path(), &[source], &session);
    assert_eq!(collection.bound.len(), 1);
    let _ = std::fs::remove_file(&path);
}

#[cfg(windows)]
#[test]
fn a_windows_name_with_unusual_unicode_is_handled() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "crash-\u{1F600}.log", b"emoji");
    let session = session_with(around_now(), None, None);
    let source = source_at("odd", "", "*.log", time_bind(0, 0));
    let collection = collect_all(tmp.path(), &[source], &session);
    assert_eq!(collection.bound.len(), 1);
}

#[test]
fn a_windows_device_name_is_never_an_artifact() {
    assert!(is_reserved_artifact_name("NUL.log"));
    assert!(is_reserved_artifact_name("com1.txt"));
    assert!(!is_reserved_artifact_name("nullified.log"));

    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "real.log", b"a real file");
    // On Windows this writes to the NUL device rather than creating an entry. Either way it must
    // never become an artifact.
    let _ = std::fs::write(tmp.path().join("NUL.log"), b"discard");

    let session = session_with(around_now(), None, None);
    let source = source_at("names", "", "*.log", time_bind(0, 0));
    let collection = collect_all(tmp.path(), &[source], &session);

    assert!(collection
        .bound
        .iter()
        .all(|artifact| artifact.name == "real.log"));
}

#[test]
fn empty_missing_and_unreadable_folders_are_distinct() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("empty")).unwrap();
    write(tmp.path(), "notdir", b"a file where a folder was expected");
    // `missing` is never created.

    let session = session_with(around_now(), None, None);
    let sources = [
        source_at("empty", "empty", "*.log", time_bind(0, 0)),
        source_at("missing", "missing", "*.log", time_bind(0, 0)),
        source_at("unreadable", "notdir", "*.log", time_bind(0, 0)),
    ];
    let collection = collect_all(tmp.path(), &sources, &session);

    let state = |id: &str| {
        collection
            .folders
            .iter()
            .find(|folder| folder.source_id == id)
            .map(|folder| folder.state)
            .expect("a folder observation")
    };
    assert_eq!(state("empty"), FolderState::Empty);
    assert_eq!(state("missing"), FolderState::Missing);
    assert_eq!(state("unreadable"), FolderState::Unreadable);
    assert_ne!(state("empty"), state("missing"));
    assert_ne!(state("missing"), state("unreadable"));
}

#[test]
fn a_sweep_budget_stop_is_reported() {
    let tmp = tempfile::tempdir().unwrap();
    for index in 0..10 {
        write(tmp.path(), &format!("log-{index}.txt"), b"changed now");
    }
    let baseline = Baseline::default();
    let result = sweep(
        &[SweepRoot {
            name: "instance".to_string(),
            path: tmp.path().to_path_buf(),
        }],
        around_now(),
        Some(&baseline),
        SweepLimits {
            max_files: 3,
            ..SweepLimits::default()
        },
    );

    assert_eq!(result.stopped_early, Some(SweepStop::FileCount));
    assert!(!result.baseline_complete);
    assert!(result.facts.len() <= 3);
}

#[test]
fn a_missing_baseline_says_it_cannot_compare() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "new.log", b"from this session");
    let result = sweep(
        &[SweepRoot {
            name: "instance".to_string(),
            path: tmp.path().to_path_buf(),
        }],
        around_now(),
        None,
        SweepLimits::default(),
    );

    assert!(matches!(
        result.comparison,
        BaselineComparison::Missing { .. }
    ));
    assert!(
        result.facts.is_empty(),
        "without a baseline nothing can be called new"
    );
    assert_eq!(result.files_seen, 1);
}

#[test]
fn retention_deletes_oldest_sessions_first() {
    let tmp = tempfile::tempdir().unwrap();
    let store_root = tmp.path().join("store");
    for name in ["a", "b", "c"] {
        let folder = store_root.join(name);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("log.txt"), vec![b'x'; 10]).unwrap();
        std::thread::sleep(Duration::from_millis(30));
    }

    let store = EvidenceStore::new(&store_root);
    let report = store
        .retention(
            &[],
            RetentionLimits {
                max_sessions: 1,
                max_bytes: u64::MAX,
            },
        )
        .unwrap();

    assert_eq!(report.removed, vec!["a".to_string(), "b".to_string()]);
    assert!(!store_root.join("a").exists());
    assert!(!store_root.join("b").exists());
    assert!(store_root.join("c").exists());
    assert!(report.bytes_after <= report.bytes_before);
}

#[test]
fn retention_keeps_a_session_the_user_kept() {
    let tmp = tempfile::tempdir().unwrap();
    let store_root = tmp.path().join("store");
    for name in ["kept", "dropped"] {
        let folder = store_root.join(name);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("log.txt"), b"evidence").unwrap();
    }

    let store = EvidenceStore::new(&store_root);
    store
        .retention(
            &["kept".to_string()],
            RetentionLimits {
                max_sessions: 0,
                max_bytes: 0,
            },
        )
        .unwrap();

    assert!(store_root.join("kept").exists());
    assert!(!store_root.join("dropped").exists());
}

#[cfg(windows)]
#[test]
fn retention_does_not_follow_a_junction_inside_the_store() {
    use std::process::Command;

    let tmp = tempfile::tempdir().unwrap();
    let store_root = tmp.path().join("store");
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("marker.txt"), b"do not touch").unwrap();

    let session_a = store_root.join("a");
    std::fs::create_dir_all(&session_a).unwrap();
    std::fs::write(session_a.join("log.txt"), b"old").unwrap();
    let link = session_a.join("link");
    let status = Command::new("cmd")
        .arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(&link)
        .arg(&outside)
        .status()
        .unwrap();
    assert!(status.success(), "the junction was not created");
    assert!(link.exists(), "the junction was not created");

    let session_b = store_root.join("b");
    std::fs::create_dir_all(&session_b).unwrap();
    std::fs::write(session_b.join("log.txt"), b"new").unwrap();

    let store = EvidenceStore::new(&store_root);
    let report = store
        .retention(
            &[],
            RetentionLimits {
                max_sessions: 0,
                max_bytes: 0,
            },
        )
        .unwrap();

    assert!(
        outside.join("marker.txt").exists(),
        "retention followed a junction out of the store"
    );
    assert!(!session_b.exists(), "ordinary sessions are still deleted");
    assert!(report.skipped.iter().any(|skip| skip.path == link));

    // Remove the junction itself so the temporary folder can be cleaned up.
    let _ = std::fs::remove_dir(&link);
}

#[test]
fn two_sessions_with_identical_files_each_get_a_copy() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "same.log", b"identical bytes");
    let session = session_with(around_now(), None, None);
    let source = source_at("same", "", "*.log", time_bind(0, 0));
    let collection = collect_all(tmp.path(), &[source], &session);

    let store = EvidenceStore::new(tmp.path().join("store"));
    let first = store
        .copy("session-one", &collection, CopyLimits::default())
        .unwrap();
    let second = store
        .copy("session-two", &collection, CopyLimits::default())
        .unwrap();

    assert_eq!(first.artifacts[0].id, second.artifacts[0].id);
    let first_path = first.objects.get(&first.artifacts[0].id).unwrap();
    let second_path = second.objects.get(&second.artifacts[0].id).unwrap();
    assert_ne!(first_path, second_path);
    assert!(first_path.exists());
    assert!(second_path.exists());
}

#[test]
fn a_store_write_failure_is_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    let store_root = tmp.path().join("store");
    std::fs::write(&store_root, b"not a folder").unwrap();

    let store = EvidenceStore::new(&store_root);
    let result = store.copy(
        "session-1",
        &SourceCollection::default(),
        CopyLimits::default(),
    );

    assert!(
        result.is_err(),
        "a store that cannot be written is an error"
    );
}

#[cfg(windows)]
#[test]
fn a_junction_pointing_outside_the_root_is_not_followed() {
    use std::process::Command;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("instance");
    std::fs::create_dir_all(&root).unwrap();
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    // The junction is named as the source's folder artifact, so an implementation that follows it
    // would copy `secret.log` and present it as this instance's evidence.
    std::fs::write(outside.join("secret.log"), b"not this instance").unwrap();
    std::fs::write(outside.join("marker.txt"), b"leave me").unwrap();

    let link = root.join("linked.log");
    let status = Command::new("cmd")
        .arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(&link)
        .arg(&outside)
        .status()
        .unwrap();
    assert!(status.success(), "the junction was not created");
    assert!(link.exists(), "the junction was not created");

    let session = session_with(around_now(), None, None);
    let source: CrashSource = serde_json::from_value(json!({
        "id": "logs",
        "where": { "root": "instance", "path": "" },
        "match": { "glob": "linked.log" },
        "unit": "folder",
        "bind": [{ "time": { "before_s": 3600, "after_s": 3600 } }],
    }))
    .unwrap();
    let collection = collect_all(&root, &[source], &session);

    assert!(
        collection.bound.is_empty(),
        "a junction must not become a folder artifact"
    );
    assert!(collection
        .seen
        .iter()
        .any(|seen| seen.reason == RejectedReason::ReparsePoint));

    let copied = copied(tmp.path(), &session, &collection, "session-1");
    assert!(copied.artifacts.is_empty());
    assert!(
        outside.join("marker.txt").exists() && outside.join("secret.log").exists(),
        "nothing outside the root may be read or copied"
    );

    let _ = std::fs::remove_dir(&link);
}

#[test]
fn a_fifty_thousand_file_instance_is_swept() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("instance");
    for folder in 0..50 {
        let folder_path = root.join(format!("mod-{folder}"));
        std::fs::create_dir_all(&folder_path).unwrap();
        for file in 0..1000 {
            std::fs::File::create(folder_path.join(format!("log-{file}.txt"))).unwrap();
        }
    }

    let result = sweep(
        &[SweepRoot {
            name: "instance".to_string(),
            path: root,
        }],
        around_now(),
        Some(&Baseline::default()),
        SweepLimits::default(),
    );

    assert_eq!(result.stopped_early, None, "50,000 files are within limits");
    assert!(result.files_seen >= 50_000);
    assert!(result.facts.len() >= 50_000);
    assert!(result.baseline_complete);
}
