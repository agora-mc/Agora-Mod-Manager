//! CLI smoke test: the command tree, the desktop coverage map, and JSON output.
//!
//! - Every command the CLI has, found by walking `--help` from the top, prints
//!   its help and exits 0. Nothing here lists commands by hand, so a new
//!   command is covered the day it is added.
//! - Every CLI command `scripts/cli_coverage.json` names for a desktop command
//!   exists (`scripts/check_cli_coverage.py` checks the rest of that file).
//! - Read-only commands print JSON that parses with `--json`, against an empty
//!   data folder and with no network.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const AGORA_BIN: &str = env!("CARGO_BIN_EXE_agora");
const WORKERS: usize = 8;

fn agora(data_dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(AGORA_BIN)
        .arg("--data-dir")
        .arg(data_dir)
        .args(args)
        .env_remove("AGORA_TEST_MSA_CREDENTIALS_JSON")
        .stdin(Stdio::null())
        .output()
        .expect("run agora")
}

#[test]
fn plugin_restore_reaches_the_committed_journal_before_startup_recovery() {
    use agora_core::plugins::{install::InstallCrashPoint, service::PluginService};
    use std::io::Write;

    fn package(version: &str) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("agora-plugin.json", options).unwrap();
        let manifest = serde_json::json!({
            "manifest": 1, "id": "test.recovery", "name": "Recovery Test",
            "version": version, "license": "MIT", "apiRange": ">=0.1, <0.2",
            "entrypoint": "main.js"
        });
        zip.write_all(manifest.to_string().as_bytes()).unwrap();
        zip.start_file("main.js", options).unwrap();
        write!(zip, "// version {version}").unwrap();
        zip.finish().unwrap().into_inner()
    }

    let data = tempfile::tempdir().unwrap();
    let ctx = agora_core::ctx::CoreContext::for_testing(data.path().to_path_buf());
    ctx.paths.create_required_dirs().unwrap();
    agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
    let conn = agora_core::db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
    let service = PluginService::headless(
        ctx.clone(),
        std::sync::Arc::new(agora_plugin_host::QuickJsHost::new()),
    );
    service
        .install_package_bytes(&package("1.0.0"), true)
        .unwrap();
    ctx.test_seams
        .set_install_crash(Some(InstallCrashPoint::Committed));
    let crash = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        service
            .install_package_bytes(&package("2.0.0"), true)
            .unwrap();
    }));
    ctx.test_seams.set_install_crash(None);
    assert!(crash.is_err_and(|panic| panic
        .downcast_ref::<String>()
        .is_some_and(|message| message.contains("cut this plugin install short at Committed"))));
    conn.execute(
        "UPDATE plugin_install_journal SET document = json_set(document, '$.writer.pid', ?1)
         WHERE plugin_id = ?2",
        rusqlite::params![u32::MAX - 2, "test.recovery"],
    )
    .unwrap();
    let args = ["plugin", "recover", "test.recovery", "--restore"];
    assert!(!agora(data.path(), &args).status.success());
    let output = agora(data.path(), &[&args[..], &["--yes"]].concat());
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Recovered plugin test.recovery"));
    let id = agora_plugin_api::PluginId::parse("test.recovery").unwrap();
    let row = agora_core::plugins::store::get(&conn, &id)
        .unwrap()
        .unwrap();
    assert_eq!(row.manifest.version.to_string(), "1.0.0");
    assert_eq!(
        std::fs::read_to_string(row.install_dir.join("main.js")).unwrap(),
        "// version 1.0.0"
    );
}

/// The subcommands a `--help` page lists under `Commands:`, without `help`.
fn subcommands(help: &str) -> Vec<String> {
    let Some(start) = help.find("\nCommands:\n") else {
        return Vec::new();
    };
    help[start + "\nCommands:\n".len()..]
        .lines()
        .take_while(|line| !line.trim().is_empty())
        .filter_map(|line| line.strip_prefix("  "))
        .filter(|line| !line.starts_with(' '))
        .filter_map(|line| line.split_whitespace().next())
        .filter(|name| *name != "help")
        .map(str::to_string)
        .collect()
}

struct Walked {
    paths: BTreeSet<String>,
    failures: Vec<String>,
}

/// Walk the whole tree, one level at a time, running each level's `--help`
/// pages on a few threads.
fn walk(data_dir: &Path) -> Walked {
    let mut walked = Walked {
        paths: BTreeSet::new(),
        failures: Vec::new(),
    };
    let mut level: Vec<Vec<String>> = vec![Vec::new()];
    while !level.is_empty() {
        let chunk = level.len().div_ceil(WORKERS).max(1);
        let results: Vec<(Vec<String>, std::process::Output)> = std::thread::scope(|scope| {
            let handles: Vec<_> = level
                .chunks(chunk)
                .map(|paths| {
                    scope.spawn(move || {
                        paths
                            .iter()
                            .map(|path| {
                                let mut args: Vec<&str> = path.iter().map(String::as_str).collect();
                                args.push("--help");
                                (path.clone(), agora(data_dir, &args))
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|handle| handle.join().expect("help worker"))
                .collect()
        });
        let mut next = Vec::new();
        for (path, output) in results {
            let help = String::from_utf8_lossy(&output.stdout);
            if !output.status.success() || !help.contains("Usage:") {
                walked.failures.push(format!(
                    "`agora {} --help` exited {:?}: {}",
                    path.join(" "),
                    output.status.code(),
                    String::from_utf8_lossy(&output.stderr)
                ));
                continue;
            }
            for name in subcommands(&help) {
                let mut child = path.clone();
                child.push(name);
                walked.paths.insert(child.join(" "));
                next.push(child);
            }
        }
        level = next;
    }
    walked
}

fn temp_data_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("temp data dir")
}

#[test]
fn every_command_prints_help_and_every_mapped_command_exists() {
    let tmp = temp_data_dir();
    let walked = walk(tmp.path());
    assert!(walked.failures.is_empty(), "{}", walked.failures.join("\n"));
    // A floor, so a parse that silently finds nothing cannot pass.
    assert!(
        walked.paths.len() > 200,
        "found only {} CLI commands; did the help format change?",
        walked.paths.len()
    );

    let coverage_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/cli_coverage.json");
    let coverage: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&coverage_path).expect("read coverage"))
            .expect("coverage is JSON");
    let missing: Vec<String> = coverage["commands"]
        .as_object()
        .expect("commands object")
        .iter()
        .filter_map(|(desktop, entry)| {
            let path = entry.get("cli")?.as_str()?;
            (!walked.paths.contains(path))
                .then(|| format!("{desktop} -> `agora {path}` is not a CLI command"))
        })
        .collect();
    assert!(
        missing.is_empty(),
        "scripts/cli_coverage.json names CLI commands that do not exist:\n{}",
        missing.join("\n")
    );
}

/// Read-only commands that need no instance, no account and no network.
const JSON_COMMANDS: &[&[&str]] = &[
    &["list-instances"],
    &["paths"],
    &["doctor"],
    &["settings", "list"],
    &["plugin", "list"],
    &["plugin", "official", "list"],
    &["provider", "list"],
    &["template", "list"],
    &["instance", "deleted"],
    &["games", "list"],
    &["games", "folder", "list"],
    &["games", "base", "list"],
    &["games", "instance", "list"],
    &["games", "content", "list"],
    &["registry", "status"],
    &["loader", "list"],
    &["governance", "config"],
    &["proxy", "show"],
];

#[test]
fn read_only_commands_print_json() {
    let tmp = temp_data_dir();
    let mut failures = Vec::new();
    for command in JSON_COMMANDS {
        let mut args = vec!["--json"];
        args.extend_from_slice(command);
        let output = agora(tmp.path(), &args);
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !output.status.success() {
            failures.push(format!(
                "`agora --json {}` exited {:?}: {}",
                command.join(" "),
                output.status.code(),
                String::from_utf8_lossy(&output.stderr)
            ));
        } else if let Err(error) = serde_json::from_str::<serde_json::Value>(&stdout) {
            failures.push(format!(
                "`agora --json {}` printed something that is not JSON ({error}): {}",
                command.join(" "),
                stdout.chars().take(300).collect::<String>()
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn help_parser_reads_the_commands_section() {
    let help = "About\n\nUsage: agora [OPTIONS] <COMMAND>\n\nCommands:\n  list   List things\n  \
                show   Show one\n           continued description\n  help   Print help\n\nOptions:\n  \
                --json\n";
    assert_eq!(
        subcommands(help),
        vec!["list".to_string(), "show".to_string()]
    );
    assert!(subcommands("Usage: agora paths\n\nOptions:\n  --json\n").is_empty());
}
