//! A stand-in for a game, so the session ledger can be tested with no game (formats B4, layer 4).
//!
//! It ends the way the test asks: with a chosen exit code, with an access violation, or the way
//! `skse64_loader.exe` does, by starting the real "game" and exiting at once. It is a test
//! program: nothing ships it and nothing but tests starts it.
//!
//! Usage: `agora-session-crasher [--ready <file>] [--hold-until <file>] <mode> [args]`
//!
//!   --ready <file>       create this file once running (after a `spawn` child has started)
//!   --hold-until <file>  before ending, wait until this file exists (60 s at most)
//!
//!   exit <code>          exit with this code
//!   av                   write through a null pointer: an access violation, `0xC0000005`
//!   abort                `std::process::abort()`: `0xC0000409` on Windows, SIGABRT elsewhere
//!   spawn <exe> [args]   start <exe> with <args> as a child and exit 0 at once, like a loader
//!
//! Windows error dialogs are switched off, so a crash ends the process silently.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[cfg(windows)]
fn quiet_crashes() {
    extern "system" {
        fn SetErrorMode(mode: u32) -> u32;
    }
    // SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX: no Windows Error Reporting dialog.
    // SAFETY: a plain Win32 call with an integer argument.
    unsafe {
        SetErrorMode(0x0001 | 0x0002);
    }
}

#[cfg(not(windows))]
fn quiet_crashes() {}

fn touch(path: &Path) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, b"");
}

fn hold(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn main() {
    quiet_crashes();
    let mut args = std::env::args().skip(1).peekable();
    let mut ready: Option<PathBuf> = None;
    let mut hold_until: Option<PathBuf> = None;
    loop {
        match args.peek().map(String::as_str) {
            Some("--ready") => {
                args.next();
                ready = args.next().map(PathBuf::from);
            }
            Some("--hold-until") => {
                args.next();
                hold_until = args.next().map(PathBuf::from);
            }
            _ => break,
        }
    }
    let Some(mode) = args.next() else {
        eprintln!("agora-session-crasher: a mode is needed (exit, av, abort, spawn)");
        std::process::exit(2);
    };

    match mode.as_str() {
        "spawn" => {
            let Some(exe) = args.next() else {
                eprintln!("agora-session-crasher: spawn needs an executable");
                std::process::exit(2);
            };
            let child = std::process::Command::new(&exe).args(args).spawn();
            if let Err(e) = child {
                eprintln!("agora-session-crasher: could not start {exe}: {e}");
                std::process::exit(3);
            }
            if let Some(ready) = &ready {
                touch(ready);
            }
            // The loader's work is done: exit without waiting for the child.
            if let Some(until) = &hold_until {
                hold(until);
            }
            std::process::exit(0);
        }
        "exit" | "av" | "abort" => {
            let code: i32 = match mode.as_str() {
                "exit" => args.next().and_then(|c| c.parse().ok()).unwrap_or(0),
                _ => 0,
            };
            if let Some(ready) = &ready {
                touch(ready);
            }
            if let Some(until) = &hold_until {
                hold(until);
            }
            match mode.as_str() {
                "av" => {
                    // SAFETY: deliberately invalid: this is the access violation under test.
                    unsafe { std::ptr::null_mut::<u32>().write_volatile(1) };
                    // Not reached on any platform that faults; do not look like a normal exit.
                    std::process::exit(99);
                }
                "abort" => std::process::abort(),
                _ => std::process::exit(code),
            }
        }
        other => {
            eprintln!("agora-session-crasher: unknown mode '{other}'");
            std::process::exit(2);
        }
    }
}
