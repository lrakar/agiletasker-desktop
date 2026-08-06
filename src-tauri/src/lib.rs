//! AgileTasker desktop shell.
//!
//! A thin, mostly-native wrapper: the ONLY content ever shown is the live
//! `https://agiletasker.com` web app in a single webview (see
//! `navigation_allowed` for the narrow set of hosts navigation may leave
//! that origin for). Everything native this crate adds exists to serve one
//! goal — keep the user's `workspace-host`/`agent-bridge` Node daemons
//! (`daemon` module) alive for as long as the machine is on, supervised
//! from a tray icon rather than a terminal window the user has to keep
//! open. Closing the window hides to tray BY DEFAULT; only the tray's own
//! "Quit" item — after gracefully stopping every daemon — actually exits.
//! Workspaces v2 (C3) makes the window's own close behavior a persisted
//! per-machine setting (`settings` module): a user who never wants a
//! background tray presence can flip it to "quit", at which point the
//! window's close button takes the SAME graceful-stop-then-exit path as the
//! tray's "Quit" (see `tray::quit_gracefully`, now `pub` so this module's
//! `CloseRequested` handler can call it too).

mod commands;
mod daemon;
mod login_env;
mod oauth;
mod settings;
mod tray;

use daemon::DaemonManager;
use std::sync::Arc;
use std::time::Duration;
use tauri::{
    webview::NewWindowResponse, AppHandle, Listener, Manager, Url, WebviewUrl, WebviewWindowBuilder,
};
use tauri_plugin_autostart::MacosLauncher;
use tauri_plugin_opener::OpenerExt;

const MAIN_WINDOW_LABEL: &str = "main";
const START_URL: &str = "https://agiletasker.com";

/// Shows the main window (creating a fresh reference to it, focusing it),
/// and — on macOS — switches the app back to a Dock-visible "Regular"
/// activation policy. Shared by the tray's "Open AgileTasker", the
/// single-instance relaunch callback, and (implicitly, by NOT being called)
/// a `--hidden` autostart launch.
pub fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        let _ = window.show();
        let _ = window.set_focus();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
    }
}

/// Hides the main window and — on macOS — switches to the Dock-hidden
/// "Accessory" policy, so a tray-only app doesn't leave an icon bouncing in
/// the Dock with no window to show for it. Shared by the window's own
/// close-button handler and a `--hidden` autostart launch.
pub fn hide_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        let _ = window.hide();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);
    }
}

/// The exact host allowlist for navigation staying inside the webview
/// (`WebviewWindowBuilder::on_navigation`) — everything else is redirected
/// to the system browser instead (see `lib.rs` module docs and DESIGN.md's
/// security notes for what this chain is actually for: Firebase Auth's
/// `signInWithRedirect` against authDomain `agiletasker.com`, which bounces
/// through Google's own consent screen before returning).
fn navigation_allowed(url: &Url) -> bool {
    if url.scheme() != "https" {
        return false;
    }
    match url.host_str() {
        // The app itself, and Firebase Auth's `/__/auth/handler` — same
        // origin, so no extra path-based allowance is needed.
        Some("agiletasker.com") => true,
        // Google's OAuth consent/account-chooser screens, reached partway
        // through `signInWithRedirect`.
        Some("accounts.google.com") => true,
        // Account-chooser/consent pages sometimes relay through a
        // *.googleusercontent.com host as part of that same redirect
        // chain (also where Google profile photo assets live). Allowed
        // defensively per the product brief; not independently re-traced
        // against a live OAuth session — see DESIGN.md.
        Some(host) if host == "googleusercontent.com" || host.ends_with(".googleusercontent.com") => true,
        _ => false,
    }
}

/// Anything `navigation_allowed` rejects (or, unconditionally, any
/// `target=_blank`/`window.open` request — see `on_new_window` below) gets
/// handed to the OS's default browser instead of loading in-app.
fn open_externally(app: &AppHandle, url: &Url) {
    if let Err(e) = app.opener().open_url(url.to_string(), None::<&str>) {
        log::warn!("failed to open {url} in the system browser: {e}");
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // Must be first: a second launch attempt should detect the running
        // instance and exit before any other plugin/window/daemon spins up.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main_window(app);
        }))
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, Some(vec!["--hidden"])))
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(
            tauri_plugin_log::Builder::new()
                .targets([
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Stdout),
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::LogDir { file_name: None }),
                ])
                .build(),
        )
        .setup(|app| {
            // Must run before anything spawns a child process that might
            // shell out to `claude`/`codex` by bare name — see module docs.
            login_env::fix_macos_login_path();

            let handle = app.handle().clone();
            let launched_hidden = std::env::args().any(|a| a == "--hidden");

            let nav_handle = handle.clone();
            let new_window_handle = handle.clone();
            let start_url: Url = START_URL.parse().expect("START_URL is a valid literal https URL");
            WebviewWindowBuilder::new(app, MAIN_WINDOW_LABEL, WebviewUrl::External(start_url))
                .title("AgileTasker")
                .inner_size(1280.0, 800.0)
                .min_inner_size(980.0, 640.0)
                .visible(!launched_hidden)
                .on_navigation(move |url| {
                    if navigation_allowed(url) {
                        true
                    } else {
                        open_externally(&nav_handle, url);
                        false
                    }
                })
                .on_new_window(move |url, _features| {
                    // Per the product brief: target=_blank/window.open
                    // ALWAYS escapes to the system browser — this app only
                    // ever shows the one main window, even for links that
                    // would otherwise be same-origin-allowed.
                    open_externally(&new_window_handle, &url);
                    NewWindowResponse::Deny
                })
                .build()?;

            #[cfg(target_os = "macos")]
            if launched_hidden {
                let _ = app.handle().set_activation_policy(tauri::ActivationPolicy::Accessory);
            }

            let manager = DaemonManager::new(handle.clone());
            manager.load_and_spawn_all();
            app.manage(manager);

            tray::build(&handle)?;

            // Keep the tray's "Agents: N running" line live.
            let status_listener_handle = handle.clone();
            handle.listen("daemon-status", move |_event| {
                if let Some(manager) = status_listener_handle.try_state::<Arc<DaemonManager>>() {
                    tray::refresh_status_label(&status_listener_handle, &manager);
                }
            });

            // Background update checks: 10s after launch, then every 6h.
            // Interactive checks (tray "Check for updates…") are wired
            // separately in tray.rs and always show a dialog; these are
            // silent unless an update is actually found.
            let updates_handle = handle.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(Duration::from_secs(10)).await;
                loop {
                    tray::check_for_updates_background(&updates_handle).await;
                    tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
                }
            });

            Ok(())
        })
        .on_window_event(|window, event| {
            // Hide-to-tray is the DEFAULT (daemons must outlive the window),
            // but Workspaces v2 (C3) makes it a persisted per-machine
            // choice: `set_close_behavior`/settings.json. `prevent_close()`
            // always runs first — even the 'quit' path goes through
            // `quit_gracefully`'s own `app.exit(0)` rather than letting this
            // native close event tear the window down out from under a
            // still-running graceful-stop sequence.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let app = window.app_handle();
                let app_data_dir = settings::resolve_app_data_dir(app);
                let close_behavior = settings::load_settings_or_default(&app_data_dir).close_behavior;
                if close_behavior == settings::CLOSE_BEHAVIOR_QUIT {
                    let app = app.clone();
                    tauri::async_runtime::spawn(async move {
                        tray::quit_gracefully(&app).await;
                    });
                } else {
                    hide_main_window(app);
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::desktop_info,
            commands::set_autostart,
            commands::set_close_behavior,
            commands::pair_daemon,
            commands::unpair_daemon,
            commands::list_daemons,
            commands::restart_daemon,
            commands::stop_daemon,
            commands::start_daemon,
            commands::daemon_log_tail,
            commands::pick_directory,
            commands::check_for_shell_update,
            commands::google_sign_in,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the AgileTasker desktop shell");
}
