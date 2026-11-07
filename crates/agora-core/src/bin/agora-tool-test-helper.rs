//! A tiny file-operation program for tests that used to copy the system `cmd.exe`.
//!
//! A copied `cmd.exe` depends on the side files of the machine it came from and behaves
//! differently on other Windows versions, which is not something Agora controls. This program is
//! built from this workspace and does the few file operations those tests need.
//!
//! Usage (operations run in the order given):
//!   --write <path>        create the file and its folders
//!   --content <text>      what --write writes (default: empty)
//!   --content-env <name>  what --write writes, taken from an environment variable
//!   --delete <path>       delete the file
//!   --stdout <text>       print a line to stdout
//!   --stderr <text>       print a line to stderr
//!   --sleep-ms <ms>       sleep before exiting
//!   --exit <code>         exit with this code (default: 0)
//!
//! It is also a stand-in for `redMod.exe`: when the first argument is `deploy` it records the
//! arguments in `redmod-deploy-args.txt` and writes the staged script blob
//! `r6/cache/modded/final.redscripts` beside its working folder, which is what a run captured
//! into a staging folder needs. A real REDmod deploy does the same under `-root=`.

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("agora-tool-test-helper: {error}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<u8, String> {
    let all: Vec<String> = std::env::args().skip(1).collect();
    if all.first().map(String::as_str) == Some("deploy") {
        return fake_redmod(&all);
    }
    let mut args = all.into_iter();
    let mut writes: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    let mut deletes: Vec<PathBuf> = Vec::new();
    let mut sleep_ms = 0u64;
    let mut code = 0u8;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--write" => {
                let path = args.next().ok_or("--write needs a path")?;
                let content = match args.next() {
                    Some(flag) if flag == "--content" => {
                        args.next().ok_or("--content needs text")?.into_bytes()
                    }
                    Some(flag) if flag == "--content-env" => {
                        let name = args.next().ok_or("--content-env needs a name")?;
                        std::env::var(&name).unwrap_or_default().into_bytes()
                    }
                    Some(other) => {
                        return Err(format!("unexpected argument after --write: {other}"))
                    }
                    None => Vec::new(),
                };
                writes.push((PathBuf::from(path), content));
            }
            "--delete" => {
                deletes.push(PathBuf::from(args.next().ok_or("--delete needs a path")?));
            }
            "--stdout" => {
                println!("{}", args.next().ok_or("--stdout needs text")?);
            }
            "--stderr" => {
                eprintln!("{}", args.next().ok_or("--stderr needs text")?);
            }
            "--sleep-ms" => {
                sleep_ms = args
                    .next()
                    .ok_or("--sleep-ms needs a number")?
                    .parse()
                    .map_err(|_| "--sleep-ms needs a number".to_string())?;
            }
            "--exit" => {
                code = args
                    .next()
                    .ok_or("--exit needs a number")?
                    .parse()
                    .map_err(|_| "--exit needs a code between 0 and 255".to_string())?;
            }
            other => return Err(format!("unknown argument '{other}'")),
        }
    }

    for (path, content) in writes {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
        }
        std::fs::write(&path, &content)
            .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    }
    for path in deletes {
        std::fs::remove_file(&path)
            .map_err(|e| format!("could not delete {}: {e}", path.display()))?;
    }
    if sleep_ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(sleep_ms));
    }
    Ok(code)
}

/// The test double for `redmod deploy -root=<game>`: keep the arguments for the test to read, and
/// write the staged script blob the game would read.
fn fake_redmod(args: &[String]) -> Result<u8, String> {
    std::fs::write(
        "redmod-deploy-args.txt",
        args.join(
            "
",
        ),
    )
    .map_err(|e| format!("could not record the deploy arguments: {e}"))?;
    let blob = PathBuf::from("r6/cache/modded/final.redscripts");
    if let Some(parent) = blob.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    }
    std::fs::write(&blob, b"fake deployed scripts")
        .map_err(|e| format!("could not write {}: {e}", blob.display()))?;
    Ok(0)
}
