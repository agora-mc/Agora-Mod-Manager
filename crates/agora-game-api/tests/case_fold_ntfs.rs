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
