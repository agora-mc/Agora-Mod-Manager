//! Process-wide authorization for outbound requests.
//!
//! # Why this is not per-module
//!
//! Lockdown Mode is documented as a global override that disables every network
//! endpoint, but it used to be enforced by each feature module calling
//! `db::is_network_enabled` at its own entry point. That is an optional check,
//! and the modules that forgot it were exactly the ones that mattered: GitHub
//! device-flow sign-in reached `github.com` with lockdown on, the desktop
//! governance client POSTed to `api.github.com/graphql` through a raw
//! `reqwest::Client` that skipped the entire URL policy layer as well, and the
//! install pipeline's "standalone" download helpers built their own client sets
//! and never consulted the setting.
//!
//! A global promise cannot depend on every future feature author remembering an
//! optional call. The gate is therefore consulted inside the checked-request
//! chokepoints in [`crate::http_client`], which every production request already
//! funnels through.
//!
//! # Fail closed
//!
//! With no gate installed, the answer is deny. There is deliberately no
//! production constructor that yields an unrestricted gate: an adapter that
//! forgets to install one loses network access, which is a bug that surfaces
//! immediately, rather than losing lockdown, which is a bug nobody sees.
//!
//! The gate reads the settings version before every request and reloads the values only when
//! it changed, so toggling lockdown takes effect on the next request — including in a second
//! process sharing the same settings database.
//!
//! A settings read that fails denies: an unreadable store cannot establish that the user
//! permits the request, and the gate does not fall back to the last snapshot it read.

use crate::error::{LauncherError, LauncherResult};
use crate::http_client::ClientCategory;
use crate::settings::{SettingsCache, SettingsReads};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};

/// Decides whether an outbound request in a given category may proceed.
pub trait NetworkGate: Send + Sync {
    /// `Err` refuses the request. Called before any DNS resolution or socket
    /// work, so a refusal produces no network activity at all.
    fn check(&self, category: ClientCategory) -> LauncherResult<()>;
}

/// The default until an adapter installs a real gate.
pub struct DenyAll;

impl NetworkGate for DenyAll {
    fn check(&self, category: ClientCategory) -> LauncherResult<()> {
        Err(LauncherError::Generic {
            code: "ERR_NETWORK_GATE_MISSING".into(),
            message: format!(
                "Refusing a {category:?} request: no network policy has been installed for this \
                 process, so it cannot be established that the user permits outbound requests."
            ),
        })
    }
}

/// Allows everything.
///
/// **Test support only.** Always compiled because `Ctx::for_testing` is, and
/// that is consumed by integration tests in the adapter crates. No production
/// path constructs it — the same convention as `HttpClients::for_testing`.
pub struct AllowAll;

impl NetworkGate for AllowAll {
    fn check(&self, _category: ClientCategory) -> LauncherResult<()> {
        Ok(())
    }
}

/// The production gate: Lockdown Mode plus the per-endpoint toggles, read through the
/// context's settings cache (§32.9).
///
/// The cache reads the settings version (one single-row read) before each request and reloads
/// the values only when it changed, so a request costs one read instead of a connection and a
/// full settings load. A refresh that fails denies: the gate never answers from the last
/// snapshot it managed to read.
pub struct SettingsGate {
    reads: SettingsReads,
    cache: Arc<SettingsCache>,
}

impl SettingsGate {
    /// A gate for a database path alone, with no seams and no shared cache.
    ///
    /// Test support, and the shape a caller that has nothing but a path needs; a context's
    /// gate comes from [`Self::for_context`].
    pub fn new(db_path: PathBuf) -> Self {
        Self {
            reads: SettingsReads::standalone(db_path),
            cache: Arc::new(SettingsCache::new()),
        }
    }

    /// The gate of a context: its database, its clock, its seams and its settings cache.
    pub fn for_context(ctx: &crate::ctx::Ctx) -> Self {
        Self {
            reads: SettingsReads::of(ctx),
            cache: Arc::clone(&ctx.settings_cache),
        }
    }
}

/// The per-endpoint setting a category is governed by.
///
/// `None` means the category has no individual toggle and is controlled by
/// Lockdown Mode alone.
fn endpoint_setting(category: ClientCategory) -> Option<&'static str> {
    match category {
        ClientCategory::MojangMetadata => Some("network_mojang_metadata_enabled"),
        ClientCategory::MojangContent => Some("network_mojang_content_enabled"),
        ClientCategory::Loader => Some("network_loader_enabled"),
        ClientCategory::Modrinth | ClientCategory::Modpack => Some("network_modrinth_enabled"),
        ClientCategory::Microsoft => Some("network_msa_enabled"),
        ClientCategory::JavaRuntime => Some("network_adoptium_enabled"),
        ClientCategory::Registry => Some("network_registry_sync_enabled"),
        ClientCategory::GitHub => Some("network_github_oauth_enabled"),
        // `network_plugins_enabled` is standing permission for a plugin's own
        // JavaScript to reach the network. Fetching an update is the launcher
        // acting on the user's behalf, for a plugin that may hold no network
        // capability at all. Welding the two together would mean someone who
        // never wants plugins touching the network also could not update them.
        //
        // `None` means Lockdown alone governs update fetches, which is right
        // because a button press is not standing permission. The automatic
        // sweep consults `plugin_updates_enabled` at the service layer.
        // Plugin JavaScript itself still requires the explicit opt-in below.
        ClientCategory::Plugin => Some("network_plugins_enabled"),
        ClientCategory::PluginUpdate | ClientCategory::PluginPackage => None,
        // These carry content the user has separately consented to; the
        // consent check lives at the call site. Lockdown still applies.
        ClientCategory::PinnedArtifact | ClientCategory::ConsentedContent => None,
        // Decoration on a page the user opened; Lockdown alone governs it.
        ClientCategory::CommunityImage => None,
        ClientCategory::LogUpload => Some("network_log_upload_enabled"),
        ClientCategory::GameServer => Some("network_server_ping_enabled"),
    }
}

impl NetworkGate for SettingsGate {
    fn check(&self, category: ClientCategory) -> LauncherResult<()> {
        // A failed read is a refusal, and names what could not be read (§32.9, §32.13).
        let settings = self
            .cache
            .load(&self.reads)
            .map_err(|error| LauncherError::Generic {
                code: "ERR_NETWORK_SETTINGS_UNREADABLE".into(),
                message: format!("Refusing a {category:?} request: {error}"),
            })?;

        if settings.lockdown_denies_network()? {
            return Err(LauncherError::Generic {
                code: "ERR_NETWORK_LOCKDOWN".into(),
                message: format!(
                    "Lockdown Mode is on, so Agora is not making any network requests \
                     (attempted: {category:?})."
                ),
            });
        }

        if let Some(key) = endpoint_setting(category) {
            if !settings.network_enabled(key) {
                return Err(LauncherError::Generic {
                    code: "ERR_NETWORK_ENDPOINT_DISABLED".into(),
                    message: format!(
                        "This kind of request is turned off in Settings (attempted: {category:?})."
                    ),
                });
            }
        }

        Ok(())
    }
}

/// A gate, shared: a context holds one and its HTTP clients hold the same one, so a clone of
/// either answers from the same settings.
pub type SharedGate = Arc<dyn NetworkGate>;

fn slot() -> &'static RwLock<Option<SharedGate>> {
    static SLOT: OnceLock<RwLock<Option<SharedGate>>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(None))
}

/// Install the process-wide gate. Called once at adapter startup.
pub fn install(gate: SharedGate) {
    if let Ok(mut slot) = slot().write() {
        *slot = Some(gate);
    }
}

/// The installed gate, or a denying one if no adapter installed anything.
pub fn current() -> SharedGate {
    match slot().read() {
        Ok(slot) => slot.clone().unwrap_or_else(|| Arc::new(DenyAll)),
        // A poisoned lock is not evidence of permission.
        Err(_) => Arc::new(DenyAll),
    }
}

/// Authorize an outbound request, or explain why not.
pub fn authorize(category: ClientCategory) -> LauncherResult<()> {
    current().check(category)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_installed_denies() {
        assert!(DenyAll.check(ClientCategory::GitHub).is_err());
    }

    #[test]
    fn lockdown_denies_every_category_including_ungated_ones() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = tmp.path().join("local_state.db");
        crate::db::init_local_state_db(&db).unwrap();
        {
            let conn = crate::db::local_state_connection(&db).unwrap();
            crate::db::set_setting(&conn, "network_lockdown_enabled", &serde_json::json!(true))
                .unwrap();
        }
        let gate = SettingsGate::new(db);
        // Including the consent-gated categories, which have no toggle of
        // their own and were previously reachable under lockdown.
        for category in [
            ClientCategory::GitHub,
            ClientCategory::Microsoft,
            ClientCategory::PinnedArtifact,
            ClientCategory::ConsentedContent,
            ClientCategory::Modpack,
        ] {
            let error = gate.check(category).unwrap_err();
            assert!(
                format!("{error:?}").contains("ERR_NETWORK_LOCKDOWN"),
                "{category:?}"
            );
        }
    }

    #[test]
    fn an_unreadable_settings_store_denies() {
        let gate = SettingsGate::new(PathBuf::from("/definitely/not/a/database"));
        assert!(gate.check(ClientCategory::GitHub).is_err());
    }

    #[test]
    fn a_disabled_endpoint_denies_only_its_own_category() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = tmp.path().join("local_state.db");
        crate::db::init_local_state_db(&db).unwrap();
        {
            let conn = crate::db::local_state_connection(&db).unwrap();
            crate::db::set_setting(&conn, "network_modrinth_enabled", &serde_json::json!(false))
                .unwrap();
        }
        let gate = SettingsGate::new(db);
        assert!(gate.check(ClientCategory::Modrinth).is_err());
        // Modpack downloads are Modrinth content, so they follow the same
        // toggle rather than slipping past it.
        assert!(gate.check(ClientCategory::Modpack).is_err());
        assert!(gate.check(ClientCategory::MojangContent).is_ok());
    }

    #[test]
    fn every_switch_the_gate_can_name_is_a_declared_fail_closed_boolean() {
        use crate::settings_registry::{lookup, SettingKind};

        let mut keys: Vec<&str> = ClientCategory::ALL
            .iter()
            .filter_map(|category| endpoint_setting(*category))
            .collect();
        keys.push("network_plugins_enabled");
        keys.push("plugin_updates_enabled");
        keys.sort_unstable();
        keys.dedup();

        for key in keys {
            let definition = lookup(key)
                .unwrap_or_else(|| panic!("{key} is named by the network gate but is undeclared"));
            match definition.kind {
                SettingKind::Bool { invalid: false, .. } => {}
                other => {
                    panic!("{key} must be a boolean whose fail direction is off, got {other:?}")
                }
            }
        }
    }

    #[test]
    fn plugin_updates_use_lockdown_without_reusing_plugin_javascript_permission() {
        assert_eq!(
            endpoint_setting(ClientCategory::Plugin),
            Some("network_plugins_enabled")
        );
        assert_eq!(endpoint_setting(ClientCategory::PluginUpdate), None);
        assert_eq!(endpoint_setting(ClientCategory::PluginPackage), None);
    }
}
