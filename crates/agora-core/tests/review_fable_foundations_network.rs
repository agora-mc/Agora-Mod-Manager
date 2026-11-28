//! Foundations review (Fable): network policy findings, written as failing tests.
//!
//! These tests describe behaviour the code does not have yet. They are expected to FAIL until
//! the findings are fixed; nothing here changes production code.

use agora_core::http_client::{self, ClientCategory, HostPolicy, HttpClients, ProxySetting};
use agora_core::network_gate::{self, NetworkGate, SettingsGate};
use std::net::TcpListener;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn fresh_db() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = tmp.path().join("local_state.db");
    agora_core::db::init_local_state_db(&db).unwrap();
    (tmp, db)
}

/// `http_client.rs:1456` and `:1582` build a fresh `reqwest::blocking::Client` that never calls
/// `apply_proxy`. A user who set a manual proxy (a corporate proxy, or Tor for privacy) has every
/// blocking request (plugin `net.fetchJson`, Minecraft metadata, runtime downloads) go out
/// directly instead.
///
/// The proxy here is a local listener. With the proxy honoured, the request connects to it. The
/// target host cannot resolve, so a direct request never reaches the listener.
#[test]
fn a_blocking_request_goes_through_the_configured_proxy() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();

    let _gate = network_gate::install_for_test(Arc::new(network_gate::AllowAll));
    http_client::configure_proxy(ProxySetting::Manual {
        url: format!("http://127.0.0.1:{port}"),
        username: None,
    });

    let clients = HttpClients::new().unwrap();
    let hosts = vec!["agora-review.invalid".to_string()];
    let request = std::thread::spawn(move || {
        let _ = http_client::blocking_checked_request_with_policy(
            &clients,
            ClientCategory::Plugin,
            "https://agora-review.invalid/data.json",
            HostPolicy::PluginDeclared(&hosts),
        );
    });

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut proxy_was_used = false;
    while Instant::now() < deadline {
        if listener.accept().is_ok() {
            proxy_was_used = true;
            break;
        }
        if request.is_finished() {
            // One last look: the connection may have arrived just before the request ended.
            proxy_was_used = listener.accept().is_ok();
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        proxy_was_used,
        "a manual proxy is configured, but the blocking request never connected to it: \
         blocking requests bypass the proxy the user chose"
    );
}

/// Settings are untyped JSON. `agora settings set network_modrinth_enabled 0` stores the number
/// 0, which `SettingsService::get_bool` (and so the Settings page) reads as off, while
/// `db::is_value_enabled` (`db.rs:1178`) reads anything that is not a bool or a string as ON.
/// The switch shows off and the gate allows the request.
#[test]
fn a_network_switch_stored_as_zero_denies_the_request() {
    let (_tmp, db) = fresh_db();
    {
        let conn = agora_core::db::local_state_connection(&db).unwrap();
        agora_core::settings::set(&conn, "network_modrinth_enabled", "0").unwrap();
    }
    let gate = SettingsGate::new(db);
    assert!(
        gate.check(ClientCategory::Modrinth).is_err(),
        "network_modrinth_enabled is 0 (off), yet the network gate allowed a Modrinth request"
    );
}

/// The same for a value that cannot be parsed at all: `db::get_setting` (`db.rs:1089`) turns
/// unparseable JSON into `Null`, and `Null` reads as enabled.
#[test]
fn a_network_switch_whose_value_is_unreadable_denies_the_request() {
    let (_tmp, db) = fresh_db();
    {
        let conn = agora_core::db::local_state_connection(&db).unwrap();
        conn.execute(
            "UPDATE user_settings SET value_json = ?1 WHERE key = ?2",
            rusqlite::params!["fal", "network_modrinth_enabled"],
        )
        .unwrap();
    }
    let gate = SettingsGate::new(db);
    assert!(
        gate.check(ClientCategory::Modrinth).is_err(),
        "the switch holds a truncated value, yet the network gate allowed the request"
    );
}

/// `network_plugins_enabled` is opt-in (seeded `false`), but the gate reads it with the
/// opt-out rule "missing means enabled" (`db.rs:1105`). If the row is absent (a settings reset, a
/// restored older database, a hand edit), plugin scripts get network access nobody granted.
#[test]
fn plugin_network_access_stays_off_when_its_opt_in_setting_is_missing() {
    let (_tmp, db) = fresh_db();
    {
        let conn = agora_core::db::local_state_connection(&db).unwrap();
        conn.execute(
            "DELETE FROM user_settings WHERE key = ?1",
            rusqlite::params!["network_plugins_enabled"],
        )
        .unwrap();
    }
    let gate = SettingsGate::new(db);
    assert!(
        gate.check(ClientCategory::Plugin).is_err(),
        "the opt-in for plugin network access is absent, yet the gate allowed a plugin request"
    );
}
