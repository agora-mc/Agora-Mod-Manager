//! One typed registry for user settings.
//!
//! Settings used to be untyped JSON, and four different readers disagreed about
//! what a value meant: the Settings page read `0` as off, while the network
//! gate read anything that was not a boolean or a string as on. Each setting is
//! now declared here once, with its type, its default and its *fail direction* —
//! what an unreadable or invalid stored value means. Every network and privacy
//! switch fails closed: an unreadable value denies access.
//!
//! Writes are validated before they reach the database, and a value that cannot
//! be understood is replaced with its fail direction (and reported once) when
//! the database is opened.

use crate::error::{LauncherError, LauncherResult};
use rusqlite::Connection;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

/// The declared type of a setting, with its default and its fail direction.
///
/// "Missing" means the row is absent; "invalid" means the row is present but
/// holds something this type cannot use. A network or privacy switch must use
/// `false` as its invalid value, so a damaged database cannot turn it on.
#[derive(Debug, Clone, Copy)]
pub enum SettingKind {
    Bool {
        default: bool,
        invalid: bool,
    },
    Enum {
        values: &'static [&'static str],
        default: &'static str,
        invalid: &'static str,
    },
    /// The default is JSON text, so an integer default stays an integer.
    Number {
        default: &'static str,
    },
    String {
        default: Option<&'static str>,
    },
    /// Opaque presentation state (a list, an object, a journal). Stored as
    /// whatever valid JSON the caller provided.
    Json,
}

/// One setting. `key` is either an exact key or a prefix ending in `*`.
#[derive(Debug, Clone, Copy)]
pub struct SettingDef {
    pub key: &'static str,
    pub kind: SettingKind,
    /// A network or privacy switch. Its invalid value is always off/denied.
    pub protected: bool,
}

impl SettingDef {
    fn matches(&self, key: &str) -> bool {
        match self.key.strip_suffix('*') {
            Some(prefix) => key.starts_with(prefix),
            None => self.key == key,
        }
    }
}

macro_rules! bool_setting {
    ($key:literal, $default:expr, $invalid:expr) => {
        SettingDef {
            key: $key,
            kind: SettingKind::Bool {
                default: $default,
                invalid: $invalid,
            },
            protected: false,
        }
    };
}

macro_rules! network_setting {
    ($key:literal, $default:expr) => {
        SettingDef {
            key: $key,
            kind: SettingKind::Bool {
                default: $default,
                invalid: false,
            },
            protected: true,
        }
    };
}

const fn enum_setting(
    key: &'static str,
    values: &'static [&'static str],
    default: &'static str,
    invalid: &'static str,
) -> SettingDef {
    SettingDef {
        key,
        kind: SettingKind::Enum {
            values,
            default,
            invalid,
        },
        protected: false,
    }
}

const fn string_setting(key: &'static str) -> SettingDef {
    SettingDef {
        key,
        kind: SettingKind::String { default: None },
        protected: false,
    }
}

const fn number_setting(key: &'static str, default: &'static str) -> SettingDef {
    SettingDef {
        key,
        kind: SettingKind::Number { default },
        protected: false,
    }
}

const fn json_setting(key: &'static str) -> SettingDef {
    SettingDef {
        key,
        kind: SettingKind::Json,
        protected: false,
    }
}

/// Every setting Agora reads or writes. Patterns end in `*` and cover keys that
/// carry a game, instance or provider id.
const REGISTRY: &[SettingDef] = &[
    // ------------------------------------------------------------------
    // Network and privacy switches. Every one fails closed.
    // ------------------------------------------------------------------
    network_setting!("network_modrinth_enabled", true),
    network_setting!("network_modrinth_cdn_enabled", true),
    network_setting!("network_registry_sync_enabled", true),
    network_setting!("network_github_oauth_enabled", true),
    network_setting!("network_msa_enabled", true),
    network_setting!("network_adoptium_enabled", true),
    network_setting!("network_server_ping_enabled", true),
    network_setting!("network_log_upload_enabled", true),
    network_setting!("network_mojang_metadata_enabled", true),
    network_setting!("network_mojang_content_enabled", true),
    network_setting!("network_loader_enabled", true),
    // Opt-in: a missing row means plugins have no network access.
    network_setting!("network_plugins_enabled", false),
    // Lockdown is fail-closed in the other direction: a damaged value keeps it
    // engaged rather than releasing the kill switch.
    SettingDef {
        key: "network_lockdown_enabled",
        kind: SettingKind::Bool {
            default: false,
            invalid: true,
        },
        protected: true,
    },
    network_setting!("reduced_security_mode", false),
    network_setting!("allow_unverified_packs", false),
    network_setting!("technic_enabled", false),
    network_setting!("modrinth_enabled", false),
    network_setting!("ai_mcp_enabled", false),
    SettingDef {
        key: "plugins_emergency_stop",
        kind: SettingKind::Bool {
            default: false,
            invalid: true,
        },
        protected: true,
    },
    json_setting("network_proxy"),
    // ------------------------------------------------------------------
    // Application behaviour.
    // ------------------------------------------------------------------
    bool_setting!("always_pre_touch", true, true),
    bool_setting!("install_auto_confirm_clean", false, false),
    bool_setting!("install_always_auto_confirm", false, false),
    bool_setting!("plugin_updates_enabled", false, false),
    bool_setting!("onboarding_complete", false, false),
    bool_setting!("version_sort_by_date", false, false),
    bool_setting!("show_update_changelogs", true, true),
    bool_setting!("notice_mod_downloads", false, false),
    bool_setting!("mod_downloads_toast", true, true),
    bool_setting!("share_content_between_instances", true, false),
    bool_setting!("ambience.enabled", true, true),
    bool_setting!("ambience.sound", false, false),
    bool_setting!("ambience.music-on", false, false),
    bool_setting!("ambience.clear-background", false, false),
    bool_setting!("more_games_dismissed", false, false),
    enum_setting(
        "launch_mode",
        &["direct", "delegation"],
        "delegation",
        "delegation",
    ),
    enum_setting(
        "java_runtime_mode",
        &["automatic", "prompt", "manual"],
        "automatic",
        "automatic",
    ),
    enum_setting(
        "launcher_on_game_start",
        &["keep", "minimize", "hide"],
        "keep",
        "keep",
    ),
    // ------------------------------------------------------------------
    // Numbers.
    // ------------------------------------------------------------------
    number_setting("update_sweep_interval_hours", "12"),
    number_setting("games_recently_deleted_days", "30"),
    number_setting("ambience.music-volume", "0.35"),
    number_setting("ambience.sound-volume", "0.5"),
    number_setting("onboarding_step", "0"),
    // ------------------------------------------------------------------
    // Strings and opaque state written by the UI, the CLI and core.
    // ------------------------------------------------------------------
    string_setting("active_game"),
    string_setting("advanced_mode"),
    string_setting("default_instance_template"),
    string_setting("java_path"),
    string_setting("last_home_visit"),
    string_setting("mcp_bearer_token"),
    string_setting("mod_downloads_watch_folder"),
    string_setting("mojang_launcher_path"),
    string_setting("plugin_theme"),
    string_setting("agora_pre_launch_command"),
    string_setting("agora_post_exit_command"),
    json_setting("agora_env_vars"),
    json_setting("agora_notes"),
    json_setting("agora_share_options"),
    json_setting("agora_share_servers"),
    json_setting("agora_window_size"),
    json_setting("agora-world-journal"),
    json_setting("cached_registry_asset_id"),
    json_setting("cached_registry_tag"),
    json_setting("controlify_offer_declined"),
    json_setting("datapack_world_scopes"),
    json_setting("disabled_games"),
    json_setting("failed_registry_update_tag"),
    json_setting("health_preferences"),
    json_setting("ignored_mod_download_hashes"),
    json_setting("instance_groups"),
    json_setting("last_registry_check"),
    json_setting("last_registry_update_failure"),
    json_setting("last_registry_update_success"),
    json_setting("mod_download_notices"),
    json_setting("plugin_surface_selections"),
    json_setting("recent_quick_play_servers"),
    SettingDef {
        key: "curated_source_*",
        kind: SettingKind::Bool {
            default: true,
            invalid: false,
        },
        protected: false,
    },
    string_setting("game_instance_default.*"),
    json_setting("game.*"),
    bool_setting!("health_silenced_*", false, false),
    string_setting("crash_answer_dismissed_*"),
];

/// Look a key up in the registry. Exact keys win over patterns.
pub fn lookup(key: &str) -> Option<&'static SettingDef> {
    REGISTRY
        .iter()
        .find(|definition| !definition.key.ends_with('*') && definition.key == key)
        .or_else(|| {
            REGISTRY
                .iter()
                .find(|definition| definition.key.ends_with('*') && definition.matches(key))
        })
}

/// Whether the key is declared. The CLI refuses to write an undeclared key.
pub fn is_registered(key: &str) -> bool {
    lookup(key).is_some()
}

fn invalid_error(key: &str, expected: &str, got: &Value) -> LauncherError {
    LauncherError::Generic {
        code: "ERR_SETTING_INVALID".into(),
        message: format!(
            "The setting `{key}` expects {expected}; {got} is not. The value was not stored."
        ),
    }
}

fn unknown_error(key: &str) -> LauncherError {
    LauncherError::Generic {
        code: "ERR_SETTING_UNKNOWN".into(),
        message: format!(
            "No setting named `{key}` is declared. Run `agora settings list` to see the keys \
             Agora knows."
        ),
    }
}

/// Parse the legacy boolean encodings Agora has accepted: JSON booleans, the
/// strings `true`/`false`/`1`/`0` and the numbers `1`/`0`. Anything else is not
/// a boolean.
pub fn parse_legacy_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(value) => Some(*value),
        Value::String(text) => match text.trim().to_ascii_lowercase().as_str() {
            "true" | "1" => Some(true),
            "false" | "0" => Some(false),
            _ => None,
        },
        Value::Number(number) => match number.as_i64() {
            Some(1) => Some(true),
            Some(0) => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn default_value(kind: SettingKind) -> Option<Value> {
    match kind {
        SettingKind::Bool { default, .. } => Some(Value::Bool(default)),
        SettingKind::Enum { default, .. } => Some(Value::String(default.to_string())),
        SettingKind::Number { default } => serde_json::from_str(default).ok(),
        SettingKind::String { default } => default.map(|text| Value::String(text.to_string())),
        SettingKind::Json => None,
    }
}

/// Report an unusable stored value once per process, then keep going. The read
/// still returns the fail direction; this only makes the damage visible.
fn report_once(key: &str, stored: &Value, used: &Value) {
    static REPORTED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let reported = REPORTED.get_or_init(|| Mutex::new(HashSet::new()));
    let first = reported
        .lock()
        .map(|mut seen| seen.insert(key.to_string()))
        .unwrap_or(false);
    if first {
        eprintln!(
            "[settings] {key} holds {stored}, which is not a value Agora understands; \
             using {used} instead"
        );
    }
}

/// Read a declared setting through its type. Missing rows take the default;
/// invalid rows take the fail direction and are reported once.
fn read_typed(definition: &SettingDef, raw: Option<Value>) -> Option<Value> {
    match definition.kind {
        SettingKind::Bool { default, invalid } => {
            let value = match raw {
                None => default,
                Some(value) => match parse_legacy_bool(&value) {
                    Some(value) => value,
                    None => {
                        report_once(definition.key, &value, &Value::Bool(invalid));
                        invalid
                    }
                },
            };
            Some(Value::Bool(value))
        }
        SettingKind::Enum {
            values,
            default,
            invalid,
        } => {
            let value = match raw {
                None => default.to_string(),
                Some(Value::String(text)) if values.contains(&text.as_str()) => text,
                Some(other) => {
                    report_once(definition.key, &other, &Value::String(invalid.to_string()));
                    invalid.to_string()
                }
            };
            Some(Value::String(value))
        }
        SettingKind::Number { default } => {
            let fallback = serde_json::from_str(default).unwrap_or(Value::Null);
            match raw {
                None => Some(fallback),
                Some(value) if value.is_number() => Some(value),
                Some(value) => {
                    report_once(definition.key, &value, &fallback);
                    Some(fallback)
                }
            }
        }
        SettingKind::String { default } => match raw {
            None => default.map(|text| Value::String(text.to_string())),
            Some(Value::Null) => None,
            Some(Value::String(text)) => Some(Value::String(text)),
            Some(Value::Number(number)) => Some(Value::String(number.to_string())),
            Some(Value::Bool(value)) => Some(Value::String(value.to_string())),
            Some(other) => {
                let fallback = default
                    .map(|text| Value::String(text.to_string()))
                    .unwrap_or(Value::Null);
                report_once(definition.key, &other, &fallback);
                default.map(|text| Value::String(text.to_string()))
            }
        },
        SettingKind::Json => raw,
    }
}

/// Report a key that the network gate named but the registry does not declare
/// as a boolean switch. This is a code bug; the read still denies, and the
/// message is printed once per process so a hot path is not flooded.
pub fn report_undeclared_switch(key: &str) {
    static REPORTED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let reported = REPORTED.get_or_init(|| Mutex::new(HashSet::new()));
    let first = reported
        .lock()
        .map(|mut seen| seen.insert(key.to_string()))
        .unwrap_or(false);
    if first {
        eprintln!(
            "[settings] {key} is named as a network switch but is not a declared boolean;              treating it as off"
        );
    }
}

/// Read a declared setting from the database. `Ok(None)` means the key is not
/// declared or is an opaque setting with no row.
pub fn read(conn: &Connection, key: &str) -> LauncherResult<Option<Value>> {
    let Some(definition) = lookup(key) else {
        return Ok(None);
    };
    let raw = crate::db::get_setting(conn, key).map_err(storage_error)?;
    Ok(read_typed(definition, raw))
}

/// Read a boolean switch, failing closed on an unreadable database.
///
/// `Ok(None)` means the key is not a declared boolean (the caller may fall back
/// to the legacy raw read). A present-but-invalid value is read as the setting's
/// fail direction.
pub fn read_bool(conn: &Connection, key: &str) -> LauncherResult<Option<bool>> {
    let Some(definition) = lookup(key) else {
        return Ok(None);
    };
    if !matches!(definition.kind, SettingKind::Bool { .. }) {
        return Ok(None);
    }
    let raw = crate::db::get_setting(conn, key).map_err(storage_error)?;
    match read_typed(definition, raw) {
        Some(Value::Bool(value)) => Ok(Some(value)),
        _ => Ok(None),
    }
}

/// Validate a value for a declared setting, returning the value to store.
///
/// Legacy boolean encodings are accepted and stored as genuine JSON booleans.
/// A bad value is refused with the reason; it is never written.
pub fn validate(key: &str, value: &Value) -> LauncherResult<Value> {
    let Some(definition) = lookup(key) else {
        return Err(unknown_error(key));
    };
    match definition.kind {
        SettingKind::Bool { .. } => match parse_legacy_bool(value) {
            Some(value) => Ok(Value::Bool(value)),
            None => Err(invalid_error(key, "true or false", value)),
        },
        SettingKind::Enum { values, .. } => match value {
            Value::String(text) if values.contains(&text.as_str()) => Ok(value.clone()),
            _ => Err(invalid_error(
                key,
                &format!("one of {}", values.join(", ")),
                value,
            )),
        },
        SettingKind::Number { .. } => {
            if value.is_number() {
                Ok(value.clone())
            } else {
                Err(invalid_error(key, "a number", value))
            }
        }
        SettingKind::String { .. } => match value {
            Value::String(_) | Value::Null => Ok(value.clone()),
            _ => Err(invalid_error(key, "a string", value)),
        },
        SettingKind::Json => Ok(value.clone()),
    }
}

fn storage_error(error: anyhow::Error) -> LauncherError {
    LauncherError::Generic {
        code: "ERR_LOCAL_STATE_FAILED".into(),
        message: error.to_string(),
    }
}

/// Bring stored legacy encodings up to their declared type.
///
/// Runs once when the database is opened, inside the same transaction as the
/// migrations. A readable legacy value (`"true"`, `1`) becomes a genuine JSON
/// value; a value that cannot be understood becomes its fail direction and is
/// reported once. A cleanly absent row is left alone so its default applies.
pub fn migrate(conn: &Connection) -> anyhow::Result<()> {
    let rows = crate::settings::list(conn)?;
    for (key, text) in rows {
        let Some(definition) = lookup(&key) else {
            continue;
        };
        let raw: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let Some(readable) = readable_for_write(definition.kind, &raw) else {
            let Some(fallback) = fail_value(definition.kind) else {
                continue;
            };
            report_once(&key, &raw, &fallback);
            crate::db::set_setting(conn, &key, &fallback)?;
            continue;
        };
        if readable != raw {
            crate::db::set_setting(conn, &key, &readable)?;
        }
    }
    Ok(())
}

/// The value a declared type would accept from storage, in the shape it should
/// be stored in. `None` means the stored value is not understood.
fn readable_for_write(kind: SettingKind, raw: &Value) -> Option<Value> {
    match kind {
        SettingKind::Bool { .. } => parse_legacy_bool(raw).map(Value::Bool),
        SettingKind::Enum { values, .. } => match raw {
            Value::String(text) if values.contains(&text.as_str()) => Some(raw.clone()),
            _ => None,
        },
        SettingKind::Number { .. } => {
            if raw.is_number() {
                Some(raw.clone())
            } else {
                None
            }
        }
        SettingKind::String { .. } => match raw {
            Value::String(_) | Value::Null => Some(raw.clone()),
            Value::Number(number) => Some(Value::String(number.to_string())),
            Value::Bool(value) => Some(Value::String(value.to_string())),
            _ => None,
        },
        SettingKind::Json => Some(raw.clone()),
    }
}

fn fail_value(kind: SettingKind) -> Option<Value> {
    match kind {
        SettingKind::Bool { invalid, .. } => Some(Value::Bool(invalid)),
        SettingKind::Enum { invalid, .. } => Some(Value::String(invalid.to_string())),
        SettingKind::Number { .. } => default_value(kind),
        SettingKind::String { .. } => None,
        SettingKind::Json => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE user_settings (key TEXT PRIMARY KEY, value_json TEXT NOT NULL);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn a_declared_switch_is_recognised() {
        assert!(is_registered("network_modrinth_enabled"));
        assert!(is_registered("curated_source_anything_enabled"));
        assert!(is_registered("game.skyrim-se.deployment"));
        assert!(is_registered("game_instance_default.skyrim-se"));
        assert!(is_registered("crash_answer_dismissed_instance-1"));
        assert!(!is_registered("a_key_agora_never_declared"));
    }

    #[test]
    fn zero_string_and_number_are_off() {
        for stored in [
            Value::Number(0.into()),
            Value::String("0".into()),
            Value::String("false".into()),
            Value::Bool(false),
        ] {
            let conn = conn();
            crate::db::set_setting(&conn, "network_modrinth_enabled", &stored).unwrap();
            assert_eq!(
                read_bool(&conn, "network_modrinth_enabled").unwrap(),
                Some(false),
                "{stored}"
            );
        }
    }

    #[test]
    fn an_unreadable_switch_reads_as_off() {
        for stored in [
            Value::String("fal".into()),
            Value::Null,
            Value::Array(vec![]),
        ] {
            let conn = conn();
            crate::db::set_setting(&conn, "network_modrinth_enabled", &stored).unwrap();
            assert_eq!(
                read_bool(&conn, "network_modrinth_enabled").unwrap(),
                Some(false),
                "{stored}"
            );
        }
    }

    #[test]
    fn a_missing_opt_in_is_off_but_a_missing_opt_out_is_on() {
        let conn = conn();
        assert_eq!(
            read_bool(&conn, "network_plugins_enabled").unwrap(),
            Some(false)
        );
        assert_eq!(
            read_bool(&conn, "network_modrinth_enabled").unwrap(),
            Some(true)
        );
    }

    #[test]
    fn an_invalid_lockdown_stays_engaged() {
        let conn = conn();
        crate::db::set_setting(
            &conn,
            "network_lockdown_enabled",
            &Value::String("yes".into()),
        )
        .unwrap();
        assert_eq!(
            read_bool(&conn, "network_lockdown_enabled").unwrap(),
            Some(true)
        );
    }

    #[test]
    fn writes_are_validated_and_normalised() {
        assert_eq!(
            validate("network_plugins_enabled", &Value::String("1".into())).unwrap(),
            Value::Bool(true)
        );
        let wrong =
            validate("network_plugins_enabled", &Value::String("perhaps".into())).unwrap_err();
        assert_eq!(wrong.code(), "ERR_SETTING_INVALID");
        let unknown = validate("a_key_agora_never_declared", &Value::Bool(true)).unwrap_err();
        assert_eq!(unknown.code(), "ERR_SETTING_UNKNOWN");
        assert_eq!(
            validate("launch_mode", &Value::String("direct".into())).unwrap(),
            Value::String("direct".into())
        );
        assert!(validate("launch_mode", &Value::String("sideways".into())).is_err());
        assert!(validate("update_sweep_interval_hours", &Value::String("12".into())).is_err());
    }

    #[test]
    fn migration_rewrites_legacy_values_and_damaged_switches() {
        let conn = conn();
        crate::db::set_setting(
            &conn,
            "network_modrinth_enabled",
            &Value::String("true".into()),
        )
        .unwrap();
        crate::db::set_setting(
            &conn,
            "network_plugins_enabled",
            &Value::String("fal".into()),
        )
        .unwrap();
        migrate(&conn).unwrap();
        assert_eq!(
            crate::db::get_setting(&conn, "network_modrinth_enabled").unwrap(),
            Some(Value::Bool(true))
        );
        assert_eq!(
            crate::db::get_setting(&conn, "network_plugins_enabled").unwrap(),
            Some(Value::Bool(false))
        );
    }
}
