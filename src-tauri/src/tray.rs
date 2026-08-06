//! System tray: icon, menu, and the tray-driven half of the
//! hide-to-tray/quit-gracefully lifecycle (`lib.rs` owns the window
//! close-button half and the `show_main_window`/`hide_main_window` helpers
//! both this module and single-instance/autostart startup share).

use crate::daemon::DaemonManager;
use crate::show_main_window;
use std::sync::Arc;
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager,
};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_updater::UpdaterExt;

/// Wraps the dynamic "Agents: N running" `MenuItem` in Tauri managed state
/// so `refresh_status_label` can find and rewrite it later without holding
/// onto the whole `Menu` or rebuilding it on every daemon-status event.
struct StatusMenuItem(MenuItem<tauri::Wry>);

const ITEM_OPEN: &str = "open";
const ITEM_STATUS: &str = "agent-status";
const ITEM_CHECK_UPDATES: &str = "check-updates";
const ITEM_QUIT: &str = "quit";

pub fn build(app: &AppHandle) -> tauri::Result<()> {
    let open_item = MenuItem::with_id(app, ITEM_OPEN, "Open AgileTasker", true, None::<&str>)?;
    let status_item = MenuItem::with_id(app, ITEM_STATUS, "Agents: 0 running", false, None::<&str>)?;
    let check_updates_item = MenuItem::with_id(app, ITEM_CHECK_UPDATES, "Check for updates…", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, ITEM_QUIT, "Quit AgileTasker (stops agents)", true, None::<&str>)?;

    let menu = Menu::with_items(
        app,
        &[
            &open_item,
            &PredefinedMenuItem::separator(app)?,
            &status_item,
            &check_updates_item,
            &PredefinedMenuItem::separator(app)?,
            &quit_item,
        ],
    )?;

    app.manage(StatusMenuItem(status_item));

    TrayIconBuilder::with_id("main-tray")
        .icon(app.default_window_icon().cloned().expect("a window icon is configured in tauri.conf.json's bundle.icon"))
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            ITEM_OPEN => show_main_window(app),
            ITEM_CHECK_UPDATES => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move { check_for_updates_interactive(&app).await });
            }
            ITEM_QUIT => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move { quit_gracefully(&app).await });
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            // Left-click opens the window; every other click just opens the
            // menu (already wired via show_menu_on_left_click(false) meaning
            // right-click/whatever-is-native-for-this-platform shows it).
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                show_main_window(tray.app_handle());
            }
        })
        .build(app)?;

    Ok(())
}

/// Rewrites the tray's "Agents: N running" line — call after every
/// `daemon-status` transition (wired in `lib.rs`).
pub fn refresh_status_label(app: &AppHandle, manager: &DaemonManager) {
    let n = manager.running_count();
    if let Some(item) = app.try_state::<StatusMenuItem>() {
        if let Err(e) = item.0.set_text(format!("Agents: {n} running")) {
            log::warn!("failed to update tray status label: {e}");
        }
    }
}

/// Tray "Quit AgileTasker (stops agents)": gracefully stop every daemon
/// (closing their stdin so patched daemons self-exit cleanly, escalating to
/// a hard kill after 5s per daemon otherwise — see `daemon::manager`), THEN
/// actually exit the process. `pub` (Workspaces v2, C3) so `lib.rs`'s
/// `CloseRequested` handler can take this exact same path when the user's
/// persisted close-behavior setting is 'quit' — hide-to-tray remains the
/// default and only OTHER exit path (see that handler + `settings` module).
pub async fn quit_gracefully(app: &AppHandle) {
    if let Some(manager) = app.try_state::<Arc<DaemonManager>>() {
        manager.shutdown_all().await;
    }
    app.exit(0);
}

/// Tray "Check for updates…": always shows a native dialog, whether an
/// update was found, none was, or the check itself failed — this is the
/// explicit, user-initiated path, so silence on any outcome would just
/// read as the click not having done anything.
async fn check_for_updates_interactive(app: &AppHandle) {
    let updater = match app.updater() {
        Ok(u) => u,
        Err(e) => {
            log::error!("updater plugin unavailable: {e}");
            show_message(app, "Check for updates", &format!("Updater is unavailable: {e}"), MessageDialogKind::Error);
            return;
        }
    };
    match updater.check().await {
        Ok(Some(update)) => {
            let version = update.version.clone();
            let confirmed = ask(
                app,
                "Update AgileTasker shell",
                &format!("A new version of the AgileTasker shell ({version}) is available. Update now?"),
            )
            .await;
            if confirmed {
                if let Err(e) = update.download_and_install(|_chunk, _total| {}, || {}).await {
                    log::error!("update download/install failed: {e}");
                    show_message(app, "Update failed", &format!("Could not install the update: {e}"), MessageDialogKind::Error);
                } else {
                    app.restart();
                }
            }
        }
        Ok(None) => show_message(app, "Check for updates", "AgileTasker is already up to date.", MessageDialogKind::Info),
        Err(e) => {
            log::error!("update check failed: {e}");
            show_message(app, "Check for updates", &format!("Could not check for updates: {e}"), MessageDialogKind::Error);
        }
    }
}

/// Startup (10s after launch) and every-6h background update check (wired
/// in `lib.rs`'s `setup()`).
///
/// Fully silent by design — no dialogs on any outcome. The app's UI lives on
/// agiletasker.com, so a web deploy already reaches users without any of
/// this; shell updates are infrequent, small, and of no interest to the
/// person using the app. Asking permission to install one is pure
/// interruption.
///
/// The one thing that genuinely must not be interrupted is a running agent:
/// installing replaces the binary and restarts the process, which would kill
/// every supervised daemon mid-work. So the install is gated on the app
/// being IDLE (no daemons running). While agents are working the update is
/// simply left on the server and re-checked on the next 6h tick — it lands
/// the first time the machine is quiet, or on the next launch. Deferring an
/// update costs nothing; killing someone's agent run costs them real work.
pub async fn check_for_updates_background(app: &AppHandle) {
    let updater = match app.updater() {
        Ok(u) => u,
        Err(e) => {
            log::warn!("background update check: updater plugin unavailable: {e}");
            return;
        }
    };
    match updater.check().await {
        Ok(Some(update)) => {
            let version = update.version.clone();
            let busy = app
                .try_state::<Arc<DaemonManager>>()
                .map(|m| m.running_count() > 0)
                .unwrap_or(false);
            if busy {
                log::info!("shell update {version} available; deferring — agents are running");
                return;
            }
            log::info!("shell update {version} available; installing silently (app idle)");
            match update.download_and_install(|_chunk, _total| {}, || {}).await {
                Ok(()) => app.restart(),
                Err(e) => log::error!("silent update install failed (will retry next check): {e}"),
            }
        }
        Ok(None) => {}
        Err(e) => log::warn!("background update check failed: {e}"),
    }
}

/// `MessageDialogBuilder::show`/`blocking_show` block the calling thread
/// (native dialog event loop), so both dialog helpers below hop to the
/// blocking pool rather than stall whatever async task called them.
fn show_message(app: &AppHandle, title: &str, body: &str, kind: MessageDialogKind) {
    let app = app.clone();
    let title = title.to_string();
    let body = body.to_string();
    tauri::async_runtime::spawn_blocking(move || {
        app.dialog().message(body).title(title).kind(kind).blocking_show();
    });
}

async fn ask(app: &AppHandle, title: &str, body: &str) -> bool {
    let app = app.clone();
    let title = title.to_string();
    let body = body.to_string();
    tauri::async_runtime::spawn_blocking(move || {
        app.dialog().message(body).title(title).buttons(MessageDialogButtons::OkCancel).kind(MessageDialogKind::Info).blocking_show()
    })
    .await
    .unwrap_or(false)
}
