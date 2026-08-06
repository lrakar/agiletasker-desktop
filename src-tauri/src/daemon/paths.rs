//! Filesystem layout under `<app_data_dir>` and the app's bundled resource
//! dir, plus the small load/save helpers for `daemons.json`.
//!
//! Every function here takes an already-resolved base `Path` rather than an
//! `AppHandle`, so the layout and the persistence round trip are both
//! testable with a plain tempdir — no Tauri runtime required.

use crate::daemon::types::{DaemonKind, DaemonsFile};
use std::io;
use std::path::{Path, PathBuf};

/// `<app_data_dir>/daemons/` — bundle .mjs files + the copied native
/// `node_modules/` live here (see `daemon::bundles`).
pub fn daemons_dir(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join("daemons")
}

/// `<app_data_dir>/daemons.json` — persisted config, secret-free (see
/// `DaemonConfig`'s doc comment for where the key actually lives).
pub fn daemons_config_file(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join("daemons.json")
}

/// Where a given daemon kind's downloaded/cached bundle script lives.
pub fn bundle_path(app_data_dir: &Path, kind: DaemonKind) -> PathBuf {
    daemons_dir(app_data_dir).join(kind.bundle_file())
}

/// The copied-from-resources native `node_modules/` (node-pty, werift) that
/// the host daemon needs adjacent to it.
pub fn node_modules_dir(app_data_dir: &Path) -> PathBuf {
    daemons_dir(app_data_dir).join("node_modules")
}

/// Stamp file recording which app version last copied `node_modules` from
/// resources into `app_data` — see `daemon::bundles::ensure_daemon_deps`.
pub fn deps_stamp_file(app_data_dir: &Path) -> PathBuf {
    daemons_dir(app_data_dir).join(".deps-version")
}

/// `resources/daemon-deps/node_modules` inside the app's bundled resource
/// directory (`bundle.resources` in tauri.conf.json maps
/// `resources/daemon-deps/**/*` in, see `scripts/prepare-deps.mjs`).
pub fn resource_daemon_deps_node_modules(resource_dir: &Path) -> PathBuf {
    resource_dir.join("resources").join("daemon-deps").join("node_modules")
}

/// Loads `daemons.json`, treating "file doesn't exist" as an empty config
/// (first run) rather than an error — everything else (malformed JSON, a
/// permissions problem) is surfaced so the caller can log it instead of
/// silently discarding a corrupt file's worth of pairings.
pub fn load_daemons_file(app_data_dir: &Path) -> io::Result<DaemonsFile> {
    let path = daemons_config_file(app_data_dir);
    match std::fs::read_to_string(&path) {
        Ok(contents) => serde_json::from_str(&contents)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(DaemonsFile::default()),
        Err(e) => Err(e),
    }
}

/// Persists `daemons.json`. Writes to a sibling temp file and renames over
/// the target so a crash mid-write (or two rapid pair/unpair calls racing —
/// callers serialize through `DaemonManager`'s mutex, but this is cheap
/// insurance regardless) never leaves a half-written, unparseable file
/// behind for the next launch's `load_daemons_file` to choke on.
pub fn save_daemons_file(app_data_dir: &Path, file: &DaemonsFile) -> io::Result<()> {
    std::fs::create_dir_all(app_data_dir)?;
    let path = daemons_config_file(app_data_dir);
    let tmp_path = path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(file)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    std::fs::write(&tmp_path, json)?;
    std::fs::rename(&tmp_path, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::types::{DaemonConfig, DesiredState};

    fn tempdir() -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "agiletasker-desktop-paths-test-{}-{}",
            std::process::id(),
            epoch_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn epoch_nanos() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    #[test]
    fn layout_paths_join_as_expected() {
        let base = Path::new("/data");
        assert_eq!(daemons_dir(base), Path::new("/data/daemons"));
        assert_eq!(daemons_config_file(base), Path::new("/data/daemons.json"));
        assert_eq!(
            bundle_path(base, DaemonKind::Host),
            Path::new("/data/daemons/agiletasker-host.mjs")
        );
        assert_eq!(
            bundle_path(base, DaemonKind::Agent),
            Path::new("/data/daemons/agiletasker-agent.mjs")
        );
        assert_eq!(node_modules_dir(base), Path::new("/data/daemons/node_modules"));
        assert_eq!(deps_stamp_file(base), Path::new("/data/daemons/.deps-version"));
        assert_eq!(
            resource_daemon_deps_node_modules(base),
            Path::new("/data/resources/daemon-deps/node_modules")
        );
    }

    #[test]
    fn missing_daemons_file_loads_as_empty() {
        let dir = tempdir();
        let loaded = load_daemons_file(&dir).unwrap();
        assert!(loaded.daemons.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempdir();
        let file = DaemonsFile {
            daemons: vec![DaemonConfig {
                id: "agent-abcdefghijklmnopqrst".into(),
                kind: DaemonKind::Agent,
                uid: "agent-abcdefghijklmnopqrst".into(),
                cwd: Some("C:/projects/foo".into()),
                desired: DesiredState::Running,
            }],
        };
        save_daemons_file(&dir, &file).unwrap();
        let loaded = load_daemons_file(&dir).unwrap();
        assert_eq!(loaded.daemons, file.daemons);
        // No leftover temp file after a successful rename.
        assert!(!dir.join("daemons.json.tmp").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_daemons_file_surfaces_as_error_not_silently_emptied() {
        let dir = tempdir();
        std::fs::write(daemons_config_file(&dir), "{ not valid json").unwrap();
        let result = load_daemons_file(&dir);
        assert!(result.is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
