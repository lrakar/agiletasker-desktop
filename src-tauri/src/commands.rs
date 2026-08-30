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
use crate::oauth::{self, GoogleSignInResult};
use crate::settings::{self, ShellSettings};
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
    /// Machine hostname (Workspaces v2, C3/C1) — surfaced so the web app can
    /// label "this computer" in device lists (`gethostname`, lossy: any
    /// non-UTF8 bytes in an exotic hostname become the replacement
    /// character rather than failing the whole command).
    pub hostname: String,
    /// Persisted window-close behavior — 'tray' (default, hide) or 'quit'
    /// (same graceful stop-then-exit as the tray's own Quit). See
    /// `set_close_behavior` and the `settings` module.
    pub close_behavior: String,
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
    let app_data_dir = settings::resolve_app_data_dir(&app);
    let close_behavior = settings::load_settings_or_default(&app_data_dir).close_behavior;
    Ok(DesktopInfo {
        app_version: app.package_info().version.to_string(),
        platform: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        autostart,
        hostname: gethostname::gethostname().to_string_lossy().into_owned(),
        close_behavior,
    })
}

/// Sets the persisted window-close behavior ('tray' hides, matching today's
/// only behavior; 'quit' makes the window's close button take the same
/// graceful-stop-then-exit path as the tray's own "Quit" — see `lib.rs`'s
/// `CloseRequested` handler). Validates before writing anything (an invalid
/// value is a caller bug, not a state to silently coerce), then — per the
/// same "re-queried after the attempt rather than echoing the request"
/// philosophy `set_autostart` documents above — reads the value back from
/// disk rather than just returning what was requested, so a caller can
/// trust the response reflects what's actually persisted.
#[tauri::command]
pub async fn set_close_behavior(app: AppHandle, behavior: String) -> Result<String, String> {
    if !settings::is_valid_close_behavior(&behavior) {
        return Err(format!("invalid close behavior \"{behavior}\" (expected \"tray\" or \"quit\")"));
    }
    let app_data_dir = settings::resolve_app_data_dir(&app);
    settings::save_settings(&app_data_dir, &ShellSettings { close_behavior: behavior }).map_err(|e| e.to_string())?;
    Ok(settings::load_settings_or_default(&app_data_dir).close_behavior)
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

/// The web UI's gift-banner Install (src/lib/desktop/shellUpdates.ts):
/// re-checks the updater fresh (never trusts a handle from an earlier
/// `check_for_shell_update` round-trip) and runs the one sanctioned install
/// path — stop every agent gracefully, download + install, restart the app.
/// On success the process restarts, so the webview usually never sees the
/// `Ok`; a failure respawns the stopped agents Rust-side (see
/// `tray::install_update_stopping_agents`) and rejects with the reason.
#[tauri::command]
pub async fn install_shell_update(app: AppHandle) -> Result<(), String> {
    let updater = app.updater().map_err(|e| e.to_string())?;
    match updater.check().await {
        Ok(Some(update)) => crate::tray::install_update_stopping_agents(&app, update).await.map_err(|e| e.to_string()),
        Ok(None) => Err("no shell update is available".to_string()),
        Err(e) => Err(e.to_string()),
    }
}

/// System-browser Google sign-in (see `oauth` module docs for the full
/// loopback + PKCE flow this wraps). Errors surface Rust's user-facing
/// message text as-is (`OAuthError`'s `thiserror` `Display` impls, e.g.
/// "sign-in was cancelled") — the web side shows/logs them verbatim rather
/// than re-deriving copy from an error code.
#[tauri::command]
pub async fn google_sign_in(app: AppHandle) -> Result<GoogleSignInResult, String> {
    oauth::google_sign_in(&app).await.map_err(|e| e.to_string())
}
