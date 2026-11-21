//! Case-equivalence of game paths must match NTFS on the user's volume (checked on D: 2026-10-31:
//! `i.txt` and `ı.txt` are two files, `s.txt` and `ſ.txt` are two files, `Ü.txt` and `ü.txt` are one).

use agora_game_api::fold_path_case;

#[test]
fn umlaut_names_that_ntfs_treats_as_one_fold_together() {
    // Positive control: this pair is one file on NTFS.
    assert_eq!(fold_path_case("Ü.txt"), fold_path_case("ü.txt"));
}

#[test]
fn dotless_i_and_capital_i_stay_different_names_as_ntfs_keeps_them() {
    // fold_path_case maps U+0131 to `I`, so the two names collide although NTFS keeps two files.
    assert_ne!(fold_path_case("ı.txt"), fold_path_case("i.txt"));
}

#[test]
fn long_s_and_s_stay_different_names_as_ntfs_keeps_them() {
    // fold_path_case maps U+017F to `S`, so the two names collide although NTFS keeps two files.
    assert_ne!(fold_path_case("ſ.txt"), fold_path_case("s.txt"));
}

#[test]
#[ignore = "measures this machine's NTFS upcase table; run with --ignored --nocapture"]
fn measure_ntfs_upcase_exceptions() {
    use std::path::{Path, PathBuf};

    fn simple_upper(ch: char) -> char {
        let mut upper = ch.to_uppercase();
        match (upper.next(), upper.next()) {
            (Some(single), None) => single,
            _ => ch,
        }
    }

    fn valid_in_name(ch: char) -> bool {
        !ch.is_control() && !matches!(ch, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
    }

    // The worktree's target folder is on D:; the probe never leaves it and removes only its own.
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target");
    let dir: PathBuf = base.join(format!("ntfs-case-probe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut measured = 0usize;
    let mut exceptions: Vec<(char, char)> = Vec::new();
    for code in 0..=0xFFFFu32 {
        let Some(ch) = char::from_u32(code) else {
            continue;
        };
        let upper = simple_upper(ch);
        if upper == ch || !valid_in_name(ch) || !valid_in_name(upper) {
            continue;
        }
        // `_x` keeps a trailing dot or space from being stripped by Windows.
        let name = format!("p{code:04x}_{}_x", ch);
        let probe = format!("p{code:04x}_{}_x", upper);
        std::fs::write(dir.join(&name), b"x").unwrap();
        measured += 1;
        match std::fs::metadata(dir.join(&probe)) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                exceptions.push((ch, upper));
            }
            Err(e) => panic!("cannot probe U+{code:04X}: {e}"),
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
    println!(
        "measured {measured} BMP characters; NTFS did not fold {}:",
        exceptions.len()
    );
    for (ch, upper) in &exceptions {
        println!(
            "    ({}, {}), // U+{:04X} -> U+{:04X}",
            ch.escape_default(),
            upper.escape_default(),
            *ch as u32,
            *upper as u32
        );
    }
    assert!(!Path::new(&dir).exists(), "probe folder removed");
}
