//! Steam's Proton prefix, and the launch options that run the Agora wrapper.
//!
//! A Windows game run through Proton lives inside a prefix at
//! `<library>/steamapps/compatdata/<appid>/pfx`. Its Windows user folders are the prefix's own
//! `drive_c/users/steamuser` folders, so a game package's `Documents` and `LocalAppData` sources
//! resolve there rather than in the Linux home folder.
//!
//! Steam keeps a game's launch options in `userdata/<account>/config/localconfig.vdf`. Agora only
//! reads that file: the player pastes the wrapper text into Steam themselves.

use std::path::{Path, PathBuf};

use agora_game_api::UserDataLocation;

use super::vdf_text::{get_object, get_str, parse_vdf_text, VdfValue};

/// The exact launch options text Agora asks the player to paste into Steam.
pub const WRAPPER_TEXT: &str = "agora games wrap %command%";

/// The Proton prefix for an install, when the install sits in a Steam library and the prefix
/// exists. `install_location` is `steamapps/common/<installdir>`.
pub fn proton_prefix(install_location: &Path, app_id: &str) -> Option<PathBuf> {
    let mut current = install_location;
    while let Some(parent) = current.parent() {
        if current
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("steamapps"))
        {
            let prefix = current.join("compatdata").join(app_id).join("pfx");
            return prefix.is_dir().then_some(prefix);
        }
        current = parent;
    }
    None
}

/// Like [`proton_prefix`], but a game Steam has already run through Proton whose prefix cannot
/// be used is an error naming why. Falling back to the Linux home would put a Windows game's
/// files in the wrong place, so an error is never "no prefix".
pub fn proton_prefix_checked(
    install_location: &Path,
    app_id: &str,
) -> Result<Option<PathBuf>, String> {
    let mut current = install_location;
    while let Some(parent) = current.parent() {
        if current
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("steamapps"))
        {
            let compatdata = current.join("compatdata").join(app_id);
            if !compatdata.exists() {
                // A native Linux game, or Proton has not run this app yet. There is no prefix.
                return Ok(None);
            }
            let prefix = compatdata.join("pfx");
            return match std::fs::metadata(&prefix) {
                Ok(metadata) if metadata.is_dir() => Ok(Some(prefix)),
                Ok(_) => Err(format!(
                    "the Proton prefix '{}' is not a folder",
                    prefix.display()
                )),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(format!(
                    "the Proton prefix '{}' is missing, though Steam has used it",
                    prefix.display()
                )),
                Err(error) => Err(format!(
                    "the Proton prefix '{}' could not be read: {error}",
                    prefix.display()
                )),
            };
        }
        current = parent;
    }
    Err(format!(
        "the game folder '{}' is not inside a Steam library, so its Proton prefix could not be found",
        install_location.display()
    ))
}

/// The Windows user folder a `UserDataLocation` names, inside a Proton prefix.
pub fn user_data_root(prefix: &Path, location: &UserDataLocation) -> PathBuf {
    crate::game_user_files::user_data_root_in_prefix(prefix, location)
}

/// Every `localconfig.vdf` under a Steam root, one per account that has signed in.
pub fn local_config_files(steam_root: &Path) -> Vec<PathBuf> {
    let userdata = steam_root.join("userdata");
    let Ok(entries) = std::fs::read_dir(&userdata) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path().join("config").join("localconfig.vdf"))
        .filter(|path| path.is_file())
        .collect();
    files.sort();
    files
}

/// What Steam has in a game's launch options, read only. `problem` says why a file could not be
/// read; it is never an empty result dressed up as success.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SteamLaunchStatus {
    pub app_id: String,
    pub expected: String,
    /// The launch options found for the app, from the first account that names it.
    pub configured: Option<String>,
    /// True when the configured options run the Agora wrapper.
    pub wrapper_present: bool,
    /// The `localconfig.vdf` files that were looked at.
    pub files: Vec<PathBuf>,
    /// The command Agora used to ask Steam to start the game, when a launch asked it.
    #[serde(default)]
    pub launcher: String,
    /// Unreadable or unparsable files, named with the reason. Empty means every file was read.
    pub problems: Vec<String>,
}

impl SteamLaunchStatus {
    /// The text to paste into Steam's launch options, when it is not already there.
    pub fn paste_text(&self) -> Option<&'static str> {
        (!self.wrapper_present).then_some(WRAPPER_TEXT)
    }
}

/// Read the launch options for one app across every account under the roots. A file that cannot be
/// read is reported in `problems`; it never hides another account's answer.
pub fn launch_status(steam_roots: &[PathBuf], app_id: &str) -> SteamLaunchStatus {
    let mut files = Vec::new();
    let mut problems = Vec::new();
    let mut configured = None;
    for root in steam_roots {
        for file in local_config_files(root) {
            files.push(file.clone());
            match std::fs::read_to_string(&file) {
                Ok(text) => match parse_vdf_text(&text) {
                    Ok(parsed) => {
                        if configured.is_none() {
                            if let Some(options) = app_launch_options(&parsed, app_id) {
                                configured = Some(options);
                            }
                        }
                    }
                    Err(error) => problems.push(format!("{}: {error}", file.display())),
                },
                Err(error) => problems.push(format!("{}: {error}", file.display())),
            }
        }
    }
    let wrapper_present = configured.as_deref().is_some_and(runs_wrapper);
    SteamLaunchStatus {
        app_id: app_id.to_string(),
        expected: WRAPPER_TEXT.to_string(),
        configured,
        wrapper_present,
        files,
        problems,
        launcher: String::new(),
    }
}

/// Whether launch options contain the wrapper and pass Steam's command through it.
pub fn runs_wrapper(options: &str) -> bool {
    let words: Vec<&str> = options.split_whitespace().collect();
    words.windows(2).any(|pair| pair == ["games", "wrap"]) && words.contains(&"%command%")
}

/// Find `"apps" -> "<appid>" -> "LaunchOptions"` at any depth in a parsed VDF file.
fn app_launch_options(entries: &[(String, VdfValue)], app_id: &str) -> Option<String> {
    for (key, value) in entries {
        let VdfValue::Object(object) = value else {
            continue;
        };
        if key.eq_ignore_ascii_case("apps") {
            if let Some(app) = get_object(object, app_id) {
                if let Some(options) = get_str(app, "LaunchOptions") {
                    return Some(options.to_string());
                }
            }
        }
        if let Some(found) = app_launch_options(object, app_id) {
            return Some(found);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn steam_tree(root: &Path, app_id: &str, options: &str) {
        let config = root.join("userdata/123/config");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            config.join("localconfig.vdf"),
            format!(
                "\"UserLocalConfigStore\"\n{{\n\t\"Software\"\n\t{{\n\t\t\"Valve\"\n\t\t{{\n\t\t\t\"Steam\"\n\t\t\t{{\n\t\t\t\t\"apps\"\n\t\t\t\t{{\n\t\t\t\t\t\"{app_id}\"\n\t\t\t\t\t{{\n\t\t\t\t\t\t\"LaunchOptions\"\t\t\"{options}\"\n\t\t\t\t\t}}\n\t\t\t\t}}\n\t\t\t}}\n\t\t}}\n\t}}\n}}\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn an_invented_steam_tree_reports_the_launch_options() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("Steam");
        steam_tree(&root, "4242", "agora games wrap %command% -skipintro");
        let status = launch_status(std::slice::from_ref(&root), "4242");
        assert!(status.wrapper_present, "{status:?}");
        assert_eq!(
            status.configured.as_deref(),
            Some("agora games wrap %command% -skipintro")
        );
        assert!(status.problems.is_empty(), "{status:?}");
    }

    #[test]
    fn a_missing_wrapper_names_the_text_to_paste() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("Steam");
        steam_tree(&root, "4242", "-windowed");
        let status = launch_status(std::slice::from_ref(&root), "4242");
        assert!(!status.wrapper_present);
        assert_eq!(status.paste_text(), Some(WRAPPER_TEXT));
    }

    #[test]
    fn an_unreadable_file_is_a_problem_not_a_clean_answer() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("Steam");
        let file = root.join("userdata/123/config/localconfig.vdf");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"\"unterminated").unwrap();
        let status = launch_status(std::slice::from_ref(&root), "4242");
        assert!(!status.problems.is_empty(), "{status:?}");
        assert!(!status.wrapper_present);
    }

    #[test]
    fn the_prefix_is_found_from_the_install_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = tmp
            .path()
            .join("SteamLibrary/steamapps/compatdata/4242/pfx");
        std::fs::create_dir_all(&prefix).unwrap();
        let install = tmp.path().join("SteamLibrary/steamapps/common/A Game");
        std::fs::create_dir_all(&install).unwrap();
        assert_eq!(proton_prefix(&install, "4242"), Some(prefix));
        assert_eq!(proton_prefix(&install, "9999"), None);
    }
}
