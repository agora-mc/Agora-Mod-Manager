//! Noticing mod downloads: thin desktop adapters over `agora_core::download_watch`, plus the
//! background poll that runs only while the player has download watching on.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agora_core::ctx::Ctx;
use agora_core::download_watch::{self, AddDownloadOutcome, DownloadRow, WatchState};
use agora_core::error::{LauncherError, LauncherResult};
use tauri::Emitter;

/// The watcher's in-memory baseline and pending sizes. It is replaced when the folder changes or
/// when the player turns watching off.
#[derive(Clone, Default)]
pub struct DownloadsWatcher(Arc<Mutex<Option<WatchState>>>);

impl DownloadsWatcher {
    fn with_state<T>(
        &self,
        work: impl FnOnce(&mut Option<WatchState>) -> LauncherResult<T>,
    ) -> LauncherResult<T> {
        let mut guard = self.0.lock().map_err(|_| LauncherError::Generic {
            code: "ERR_DOWNLOADS_LOCK".into(),
            message: "The download watcher is unavailable.".into(),
        })?;
        work(&mut guard)
    }

    /// One poll, starting the baseline first if needed. Returns how many new notices were added.
    fn scan(&self, ctx: &Ctx) -> LauncherResult<usize> {
        self.with_state(|state| {
            if state.is_none() {
                *state = Some(WatchState::start(ctx)?);
            }
            let added = state.as_mut().expect("started above").scan(ctx)?;
            Ok(added.len())
        })
    }

    fn reset(&self) {
        if let Ok(mut guard) = self.0.lock() {
            *guard = None;
        }
    }
}

/// Every new download Agora recognised, with files that no longer exist pruned.
#[tauri::command]
pub async fn downloads_list(app: tauri::AppHandle) -> LauncherResult<Vec<DownloadRow>> {
    tokio::task::spawn_blocking(move || {
        let ctx = crate::core_context(&app)?;
        Ok(download_watch::list(&ctx)?)
    })
    .await
    .map_err(|_| LauncherError::Generic {
        code: "ERR_DOWNLOADS_TASK".into(),
        message: "Reading new downloads did not finish.".into(),
    })?
}

/// Run one poll now, so the Library can refresh without waiting for the timer. Returns how many
/// new notices this poll added.
#[tauri::command]
pub async fn downloads_scan(
    app: tauri::AppHandle,
    watcher: tauri::State<'_, DownloadsWatcher>,
) -> LauncherResult<usize> {
    let ctx = crate::core_context(&app)?;
    let watcher = watcher.inner().clone();
    let added = tokio::task::spawn_blocking(move || watcher.scan(&ctx))
        .await
        .map_err(|_| LauncherError::Generic {
            code: "ERR_DOWNLOADS_TASK".into(),
            message: "Scanning the downloads folder did not finish.".into(),
        })??;
    if added > 0 {
        let _ = app.emit("downloads-changed", added);
    }
    Ok(added)
}

/// Remember a download by hash and stop offering it.
#[tauri::command]
pub async fn downloads_ignore(app: tauri::AppHandle, sha256: String) -> LauncherResult<()> {
    let handle = app.clone();
    let result: LauncherResult<()> = tokio::task::spawn_blocking(move || {
        let ctx = crate::core_context(&handle)?;
        download_watch::ignore(&ctx, &sha256)?;
        Ok(())
    })
    .await
    .map_err(|_| LauncherError::Generic {
        code: "ERR_DOWNLOADS_TASK".into(),
        message: "Ignoring the download did not finish.".into(),
    })?;
    result?;
    let _ = app.emit("downloads-changed", 0);
    Ok(())
}

/// Add a recognised download to the library through its game's own flow. The downloaded file is
/// read and never moved, changed or deleted.
#[tauri::command]
pub async fn downloads_add(
    app: tauri::AppHandle,
    sha256: String,
) -> LauncherResult<AddDownloadOutcome> {
    let handle = app.clone();
    let outcome = tokio::task::spawn_blocking(move || -> LauncherResult<AddDownloadOutcome> {
        let ctx = crate::core_context(&handle)?;
        Ok(download_watch::add(&ctx, &sha256)?)
    })
    .await
    .map_err(|_| LauncherError::Generic {
        code: "ERR_DOWNLOADS_TASK".into(),
        message: "Adding the download to the library did not finish.".into(),
    })??;
    let _ = app.emit("downloads-changed", 0);
    Ok(outcome)
}

/// The platform Downloads folder, for showing the default when the player has not chosen one.
#[tauri::command]
pub fn downloads_default_folder() -> Option<String> {
    download_watch::default_folder().map(|folder| folder.to_string_lossy().to_string())
}

/// Poll the folder every five seconds, but only while the setting is on. Turning it off drops the
/// baseline, so files downloaded while it was off are not offered when it is turned back on.
pub fn watch_downloads(app: tauri::AppHandle, ctx: Ctx, watcher: DownloadsWatcher) {
    const POLL_INTERVAL: Duration = Duration::from_secs(5);
    let spawned = std::thread::Builder::new()
        .name("downloads-watch".into())
        .spawn(move || loop {
            std::thread::sleep(POLL_INTERVAL);
            match download_watch::enabled(&ctx) {
                Ok(true) => {}
                Ok(false) => {
                    watcher.reset();
                    continue;
                }
                Err(error) => {
                    eprintln!("[downloads] could not read the setting: {error}");
                    continue;
                }
            }
            match watcher.scan(&ctx) {
                Ok(0) => {}
                Ok(added) => {
                    let _ = app.emit("downloads-changed", added);
                }
                Err(error) => eprintln!("[downloads] scan failed: {error}"),
            }
        });
    if let Err(error) = spawned {
        eprintln!("[downloads] watcher unavailable: {error}");
    }
}
