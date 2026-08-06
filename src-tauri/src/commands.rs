//! The locked IPC surface: every `#[tauri::command]` the remote
//! `https://agiletasker.com` web app calls through the `remote-agiletasker`
//! capability (see `capabilities/remote.json`). Argument/return shapes are
//! camelCase on the wire via `serde(rename_all = "camelCase")`; command
//! names stay snake_case (Tauri's JS binding calls them verbatim — the web
//! side is being built against this exact contract in parallel).
//!
//! Every fallible path here maps its typed error to `String` at this
//! boundary (`.to_string()`), per repo convention — `daemon::DaemonError`
//! (thiserror) carries the structured detail up to this point; commands
//! only need `Result<T, String>` because that's what crosses IPC.

use crate::daemon::{DaemonManager, DaemonStatus, PairDaemonInput};
use serde::Serialize;
use std::sync::Arc;
use tauri::{AppHandle, State};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_updater::UpdaterExt;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopInfo {
    pub app_version: String,
    pub platform: String,
    pub arch: String,
    pub autostart: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellUpdateInfo {
    pub available: bool,
    pub version: Option<String>,
}

/// `std::env::consts::OS` already yields exactly `"windows"` / `"macos"` /
/// `"linux"` on the three desktop targets this app ships for — no mapping
/// needed. `ARCH` similarly yields the contract's expected `"x86_64"` /
/// `"aarch64"` etc.
#[tauri::command]
pub async fn desktop_info(app: AppHandle) -> Result<DesktopInfo, String> {
    let autostart = app.autolaunch().is_enabled().unwrap_or(false);
    Ok(DesktopInfo {
        app_version: app.package_info().version.to_string(),
        platform: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        autostart,
    })
}

/// Toggles autostart and returns the actually-resulting state (re-queried
/// after the attempt rather than echoing the request) so a silent failure
/// in the underlying platform call — logged, not surfaced as a hard error,
/// since the app is perfectly usable without autostart — still reports the
/// truth to the caller instead of a state that didn't actually take.
#[tauri::command]
pub async fn set_autostart(app: AppHandle, enabled: bool) -> Result<bool, String> {
    let manager = app.autolaunch();
    let toggle = if enabled { manager.enable() } else { manager.disable() };
    if let Err(e) = toggle {
        log::warn!("autostart {} failed: {e}", if enabled { "enable" } else { "disable" });
    }
    Ok(manager.is_enabled().map_err(|e| e.to_string())?)
}

#[tauri::command]
pub async fn pair_daemon(manager: State<'_, Arc<DaemonManager>>, config: PairDaemonInput) -> Result<DaemonStatus, String> {
    manager.pair(config).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn unpair_daemon(manager: State<'_, Arc<DaemonManager>>, id: String) -> Result<(), String> {
    manager.unpair(&id).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn list_daemons(manager: State<'_, Arc<DaemonManager>>) -> Result<Vec<DaemonStatus>, String> {
    Ok(manager.list())
}

#[tauri::command]
pub async fn restart_daemon(manager: State<'_, Arc<DaemonManager>>, id: String) -> Result<DaemonStatus, String> {
    manager.restart(&id).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn stop_daemon(manager: State<'_, Arc<DaemonManager>>, id: String) -> Result<DaemonStatus, String> {
    manager.stop(&id).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn start_daemon(manager: State<'_, Arc<DaemonManager>>, id: String) -> Result<DaemonStatus, String> {
    manager.start(&id).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn daemon_log_tail(
    manager: State<'_, Arc<DaemonManager>>,
    id: String,
    lines: Option<usize>,
) -> Result<String, String> {
    manager.log_tail(&id, lines).map_err(|e| e.to_string())
}

/// Native folder picker for choosing an agent's project `cwd`. Runs the
/// (synchronous, OS-blocking) dialog call on a blocking-pool thread via
/// `spawn_blocking` rather than tying up an async worker for however long
/// the user takes to pick a folder.
#[tauri::command]
pub async fn pick_directory(app: AppHandle) -> Result<Option<String>, String> {
    let picked = tauri::async_runtime::spawn_blocking(move || app.dialog().file().blocking_pick_folder())
        .await
        .map_err(|e| e.to_string())?;
    Ok(picked.and_then(|f| f.into_path().ok()).map(|p| p.to_string_lossy().to_string()))
}

#[tauri::command]
pub async fn check_for_shell_update(app: AppHandle) -> Result<ShellUpdateInfo, String> {
    let updater = app.updater().map_err(|e| e.to_string())?;
    match updater.check().await {
        Ok(Some(update)) => Ok(ShellUpdateInfo { available: true, version: Some(update.version.clone()) }),
        Ok(None) => Ok(ShellUpdateInfo { available: false, version: None }),
        Err(e) => Err(e.to_string()),
    }
}
