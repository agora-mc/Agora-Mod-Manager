use crate::java::{self, JavaInstallation};
use crate::runtime_catalog::RuntimeCatalog;
use crate::runtime_manager::{self, RuntimeProgress};
use agora_core::{
    ctx::Ctx,
    db,
    error::{LauncherError, LauncherResult},
    network::NetworkPolicy,
};
use std::sync::Arc;

#[derive(Clone)]
pub struct RuntimeService {
    ctx: Ctx,
}

impl RuntimeService {
    pub fn new(ctx: Ctx) -> Self {
        Self { ctx }
    }

    fn registry_conn(&self) -> Option<rusqlite::Connection> {
        self.ctx.registry_connection().ok()
    }

    pub fn effective_catalog(&self) -> RuntimeCatalog {
        let reg_conn = self.registry_conn();
        RuntimeCatalog::effective(reg_conn.as_ref())
    }

    pub fn list_candidates(&self) -> LauncherResult<Vec<JavaInstallation>> {
        let runtimes_root = self.ctx.paths.java_runtimes_root();
        let minecraft_dir = agora_core::paths::minecraft_dir();
        // Cache-first discovery: the persisted inventory skips the per-JRE
        // subprocess probes on warm starts; a stale inventory re-discovers
        // and refreshes it.
        Ok(java::java_candidates_cached(
            &self.ctx.paths.java_inventory(),
            &runtimes_root,
            minecraft_dir.as_deref(),
        ))
    }

    pub async fn ensure_runtime(
        &self,
        major: u32,
        policy: NetworkPolicy,
        progress: Arc<dyn RuntimeProgress>,
    ) -> LauncherResult<JavaInstallation> {
        let mode = self.get_java_runtime_mode()?;
        if mode != "automatic" {
            return Err(LauncherError::Generic {
                code: "ERR_JAVA_RUNTIME_MODE".into(),
                message: format!(
                    "Java runtime mode is '{mode}'. \
                     Set it to 'automatic' to allow managed provisioning, \
                     or use a system-installed JRE matching Java {major}."
                ),
            });
        }

        // No gate check here. Whether a runtime may be fetched is decided at the download, by
        // `runtime_manager::ensure_runtime` (the plan's policy) and by the checked request that
        // performs it (the live gate, once per request and per redirect hop). Asking beforehand
        // refused a runtime that is already installed, which needs no network at all.
        let runtimes_root = self.ctx.paths.java_runtimes_root();
        let catalog = self.effective_catalog();
        let lock_manager = self.ctx.lock_manager.clone();
        tokio::task::spawn_blocking(move || {
            runtime_manager::ensure_runtime(
                &runtimes_root,
                major,
                &catalog,
                &policy,
                Some(progress.as_ref()),
                Some(&lock_manager),
            )
        })
        .await
        .map_err(|error| LauncherError::Generic {
            code: "ERR_JAVA_PROVISION".into(),
            message: format!("Java provisioning task failed: {error}"),
        })?
    }

    pub fn remove_unused(&self) -> LauncherResult<usize> {
        let runtimes_root = self.ctx.paths.java_runtimes_root();
        let catalog = self.effective_catalog();
        runtime_manager::remove_unused(&runtimes_root, &catalog, &[])
    }

    pub fn get_java_runtime_mode(&self) -> LauncherResult<String> {
        let db_path = self.ctx.paths.local_state_db();
        if !db_path.exists() {
            return Ok("automatic".to_string());
        }
        let conn =
            db::local_state_connection(&db_path).map_err(|error| LauncherError::Generic {
                code: "ERR_LOCAL_STATE_FAILED".into(),
                message: error.to_string(),
            })?;
        Ok(db::get_setting(&conn, "java_runtime_mode")
            .ok()
            .flatten()
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| "automatic".to_string()))
    }

    pub fn inspect(&self, path: &std::path::Path) -> LauncherResult<JavaInstallation> {
        crate::java::inspect_java(path).ok_or_else(|| LauncherError::Generic {
            code: "ERR_JAVA_INSPECT".into(),
            message: format!("Could not inspect Java executable at '{}'", path.display()),
        })
    }

    pub fn network_policy(&self) -> LauncherResult<NetworkPolicy> {
        let db_path = self.ctx.paths.local_state_db();
        if !db_path.exists() {
            return Ok(NetworkPolicy::all_enabled());
        }
        let conn =
            db::local_state_connection(&db_path).map_err(|error| LauncherError::Generic {
                code: "ERR_LOCAL_STATE_FAILED".into(),
                message: error.to_string(),
            })?;
        Ok(NetworkPolicy::from_db(&conn))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_catalog::{RuntimeCatalog, RuntimeCatalogEntry};
    use crate::runtime_manager::{managed_entry_path, NoopProgress, RuntimeReceipt};
    use agora_core::network::NetworkPolicy;
    use agora_core::network_gate::{install, SettingsGate};
    use serde_json::json;
    use std::sync::Arc;

    /// The catalog entry the wrapper will look up: the embedded one for this platform.
    ///
    /// The receipt below carries its hash, exactly as an installation made from that archive
    /// would, and the java executable is a stand-in because the inspector is mocked.
    fn catalog_entry() -> RuntimeCatalogEntry {
        RuntimeCatalog::embedded()
            .lookup(21, std::env::consts::OS, std::env::consts::ARCH)
            .expect("the embedded catalog names a Java 21 runtime for this platform")
            .entry
    }

    /// A managed installation of `entry`, receipt and all, as a finished provisioning leaves it.
    fn install_stand_in_runtime(ctx: &Ctx, entry: &RuntimeCatalogEntry) {
        let java_bytes = b"a stand-in for the java executable; the inspector is mocked".to_vec();
        let relative = format!("bin/{}", if cfg!(windows) { "java.exe" } else { "java" });
        let entry_dir = managed_entry_path(&ctx.paths.java_runtimes_root(), entry);
        std::fs::create_dir_all(entry_dir.join("bin")).unwrap();
        std::fs::write(entry_dir.join(&relative), &java_bytes).unwrap();

        let mut receipt = RuntimeReceipt::from_entry(entry);
        receipt.java_relative_path = relative;
        receipt.java_sha256 = Some(agora_core::download::sha256_hex(&java_bytes));
        std::fs::write(
            entry_dir.join("receipt.json"),
            serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
    }

    /// The worker inspect mock and the outbound gate are process-wide, and each guard restores
    /// what it found: two of these tests overlapping under `cargo test` would leave the later one
    /// with no mock (it then tries to download its runtime). Held for each test's whole run.
    static PROCESS_STATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn mocked_inspect() -> crate::java::WorkerMockInspectGuard {
        crate::java::set_mock_inspect_for_workers(Some(|path| {
            Some(crate::java::JavaInstallation {
                path: path.to_path_buf(),
                version: 21,
                version_string: "21.0.2".into(),
                source: crate::java::JavaSource::Managed,
                arch: Some(std::env::consts::ARCH.to_string()),
            })
        }))
    }

    fn locked_down_context() -> (tempfile::TempDir, Ctx) {
        let root = tempfile::tempdir().unwrap();
        let ctx = crate::testing_context(root.path().to_owned());
        db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
        let conn = db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
        db::set_setting(&conn, "network_lockdown_enabled", &json!(true)).unwrap();
        install(Arc::new(SettingsGate::for_context(&ctx)));
        (root, ctx)
    }

    #[tokio::test]
    async fn c4_lockdown_lets_a_runtime_that_is_already_installed_through() {
        let _serial = PROCESS_STATE.lock().await;
        let (_root, ctx) = locked_down_context();
        let _inspect = mocked_inspect();
        let entry = catalog_entry();
        install_stand_in_runtime(&ctx, &entry);

        let service = RuntimeService::new(ctx.clone());
        let result = service
            .ensure_runtime(21, NetworkPolicy::all_enabled(), Arc::new(NoopProgress))
            .await;
        assert!(
            result.is_ok(),
            "Lockdown must not refuse a runtime that needs no network: {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn c4_lockdown_refuses_a_runtime_it_would_have_to_download() {
        let _serial = PROCESS_STATE.lock().await;
        let (_root, ctx) = locked_down_context();
        let _inspect = mocked_inspect();
        let _entry = catalog_entry();

        let service = RuntimeService::new(ctx.clone());
        let error = service
            .ensure_runtime(21, NetworkPolicy::all_enabled(), Arc::new(NoopProgress))
            .await
            .expect_err("a missing runtime has to be downloaded, and Lockdown forbids that");
        assert_eq!(error.code(), "ERR_NETWORK_LOCKDOWN", "{error}");
    }
}
