//! Small persisted shell config: `<app_data_dir>/settings.json`.
//!
//! Mirrors `daemon::paths`'s load/save shape (read-missing-as-default,
//! write-temp-then-rename) for the ONE setting this shell persists outside
//! `daemons.json` today: window-close behavior (Workspaces v2, C3 —
//! `set_close_behavior` in `commands.rs`, read by `lib.rs`'s
//! `CloseRequested` handler). Kept in its own top-level module rather than
//! under `daemon/` because it isn't daemon-specific — it's whole-shell
//! config that just happens to reuse the same persistence shape.

use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager};

pub const CLOSE_BEHAVIOR_TRAY: &str = "tray";
pub const CLOSE_BEHAVIOR_QUIT: &str = "quit";

/// `true` for exactly the two wire values `set_close_behavior`/`desktop_info`
/// accept — anything else (a value from a newer shell this build doesn't
/// know yet, a hand-edited settings.json, ...) is invalid and must not be
/// silently accepted as if it meant something.
pub fn is_valid_close_behavior(s: &str) -> bool {
    s == CLOSE_BEHAVIOR_TRAY || s == CLOSE_BEHAVIOR_QUIT
}

fn default_close_behavior() -> String {
    CLOSE_BEHAVIOR_TRAY.to_string()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellSettings {
    #[serde(default = "default_close_behavior")]
    pub close_behavior: String,
}

impl Default for ShellSettings {
    fn default() -> Self {
        Self { close_behavior: default_close_behavior() }
    }
}

/// `<app_data_dir>/settings.json`.
pub fn settings_file(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join("settings.json")
}

/// Resolves `app_data_dir`, falling back to a clearly-named temp dir on the
/// (extremely unlikely) chance resolution fails — mirrors
/// `daemon::manager::DaemonManager::new`'s identical fallback so every
/// settings.json reader/writer agrees on where it lives even in that case.
pub fn resolve_app_data_dir(app: &AppHandle) -> PathBuf {
    app.path().app_data_dir().unwrap_or_else(|e| {
        log::warn!("could not resolve app_data_dir ({e}); falling back to a temp dir");
        std::env::temp_dir().join("agiletasker-desktop-fallback-data")
    })
}

/// Loads `settings.json`, treating "file doesn't exist" as an empty/default
/// config (first run) rather than an error — everything else (malformed
/// JSON, a permissions problem) is surfaced, same contract as
/// `daemon::paths::load_daemons_file`. Most call sites want
/// `load_settings_or_default` below instead; this one exists for tests and
/// for any future caller that genuinely needs to distinguish "never
/// written" from "broken".
pub fn load_settings(app_data_dir: &Path) -> io::Result<ShellSettings> {
    let path = settings_file(app_data_dir);
    match std::fs::read_to_string(&path) {
        Ok(contents) => serde_json::from_str(&contents).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(ShellSettings::default()),
        Err(e) => Err(e),
    }
}

/// `load_settings`, but never fails — any error (missing file is already
/// handled inside `load_settings` as a default; this covers corrupt JSON, a
/// permissions problem, ...) falls back to `ShellSettings::default()` with a
/// logged warning. Deliberately more forgiving than
/// `daemon::paths::load_daemons_file` (which surfaces a corrupt file loudly,
/// because silently discarding it would silently drop real paired daemons):
/// a broken settings.json has a safe, sensible fallback (today's only
/// behavior, hide-to-tray) with nothing to lose, so the app's window-close
/// behavior — and `desktop_info`'s response — must never fail or block on
/// it.
pub fn load_settings_or_default(app_data_dir: &Path) -> ShellSettings {
    load_settings(app_data_dir).unwrap_or_else(|e| {
        log::warn!("failed to load settings.json ({e}); using defaults");
        ShellSettings::default()
    })
}

/// Persists `settings.json`. Writes to a sibling temp file and renames over
/// the target so a crash mid-write never leaves a half-written, unparseable
/// file behind for the next launch's `load_settings` to choke on — same
/// pattern as `daemon::paths::save_daemons_file`.
pub fn save_settings(app_data_dir: &Path, settings: &ShellSettings) -> io::Result<()> {
    std::fs::create_dir_all(app_data_dir)?;
    let path = settings_file(app_data_dir);
    let tmp_path = path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(settings).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    std::fs::write(&tmp_path, json)?;
    std::fs::rename(&tmp_path, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("agiletasker-desktop-settings-test-{}-{}", std::process::id(), epoch_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn epoch_nanos() -> u128 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    }

    #[test]
    fn is_valid_close_behavior_accepts_exactly_tray_and_quit() {
        assert!(is_valid_close_behavior("tray"));
        assert!(is_valid_close_behavior("quit"));
        assert!(!is_valid_close_behavior("Tray"));
        assert!(!is_valid_close_behavior(""));
        assert!(!is_valid_close_behavior("quit "));
    }

    #[test]
    fn missing_settings_file_loads_as_default_tray() {
        let dir = tempdir();
        let loaded = load_settings(&dir).unwrap();
        assert_eq!(loaded.close_behavior, "tray");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempdir();
        let settings = ShellSettings { close_behavior: "quit".into() };
        save_settings(&dir, &settings).unwrap();
        let loaded = load_settings(&dir).unwrap();
        assert_eq!(loaded, settings);
        // No leftover temp file after a successful rename.
        assert!(!dir.join("settings.json.tmp").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_settings_file_surfaces_as_error_from_strict_loader() {
        let dir = tempdir();
        std::fs::write(settings_file(&dir), "{ not valid json").unwrap();
        assert!(load_settings(&dir).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_settings_or_default_falls_back_on_corrupt_file() {
        let dir = tempdir();
        std::fs::write(settings_file(&dir), "{ not valid json").unwrap();
        let loaded = load_settings_or_default(&dir);
        assert_eq!(loaded.close_behavior, "tray");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_close_behavior_field_defaults_to_tray() {
        let dir = tempdir();
        std::fs::write(settings_file(&dir), "{}").unwrap();
        let loaded = load_settings(&dir).unwrap();
        assert_eq!(loaded.close_behavior, "tray");
        std::fs::remove_dir_all(&dir).ok();
    }
}
