//! Daemon bundle acquisition: fresh-download-with-cached-fallback for the
//! `.mjs` scripts, and a first-run/version-bumped copy of the bundled
//! native `node_modules` (node-pty, werift — see
//! `desktop/scripts/prepare-deps.mjs`) into `app_data`.
//!
//! Download base is overridable via the `AGILETASKER_BUNDLE_BASE` env var
//! (default `https://agiletasker.com/agent`) — a **dev-only** knob so
//! integration testing can point the app at a locally served copy of a
//! freshly built bundle before it ships in a real web deploy. See
//! DESIGN.md § "Dev knobs".

use crate::daemon::types::DaemonKind;
use std::path::Path;
use std::time::Duration;

const DEFAULT_BUNDLE_BASE: &str = "https://agiletasker.com/agent";
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    #[error("download failed ({0}) and no cached copy exists at {1}")]
    NoCacheAndDownloadFailed(String, std::path::PathBuf),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// `AGILETASKER_BUNDLE_BASE` if set (dev override), else the production
/// default. Read fresh on every call rather than cached once at startup —
/// it costs nothing and means a dev can flip the env var and hit
/// `restart_daemon` without relaunching the whole app.
pub fn bundle_base() -> String {
    std::env::var("AGILETASKER_BUNDLE_BASE").unwrap_or_else(|_| DEFAULT_BUNDLE_BASE.to_string())
}

/// Ensures `dest` holds a usable copy of `kind`'s bundle script: tries a
/// fresh download first (this is how `restart_daemon` rolls out daemon
/// updates — see the IPC contract), and falls back to whatever is already
/// cached on disk if the network call fails for any reason (offline, DNS,
/// timeout, non-2xx). Only errors if NEITHER a fresh download NOR a cached
/// copy is available.
pub async fn ensure_bundle(kind: DaemonKind, dest: &Path) -> Result<(), BundleError> {
    let url = format!("{}/{}", bundle_base(), kind.bundle_file());
    match download(&url).await {
        Ok(bytes) => {
            if let Some(parent) = dest.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(dest, bytes).await?;
            log::info!("daemon bundle downloaded: {url} -> {}", dest.display());
            Ok(())
        }
        Err(e) => {
            let cached = tokio::fs::try_exists(dest).await.unwrap_or(false);
            if cached {
                log::warn!(
                    "daemon bundle download failed ({e}), falling back to cached copy at {}",
                    dest.display()
                );
                Ok(())
            } else {
                Err(BundleError::NoCacheAndDownloadFailed(e.to_string(), dest.to_path_buf()))
            }
        }
    }
}

async fn download(url: &str) -> Result<Vec<u8>, reqwest::Error> {
    let client = reqwest::Client::builder().timeout(DOWNLOAD_TIMEOUT).build()?;
    let resp = client.get(url).send().await?.error_for_status()?;
    Ok(resp.bytes().await?.to_vec())
}

/// Copies the bundled native `node_modules` from the app's read-only
/// resource dir into `app_data/daemons/node_modules` on first run, or
/// whenever `app_version` differs from the last stamped copy (so a
/// node-pty/werift version bump in a future app update re-syncs
/// automatically). A no-op on every other startup.
pub async fn ensure_daemon_deps(
    resource_node_modules_dir: &Path,
    dest_node_modules_dir: &Path,
    stamp_file: &Path,
    app_version: &str,
) -> Result<(), BundleError> {
    let up_to_date = match tokio::fs::read_to_string(stamp_file).await {
        Ok(stamped) => stamped.trim() == app_version,
        Err(_) => false,
    };
    if up_to_date {
        return Ok(());
    }
    log::info!(
        "syncing daemon native deps {} -> {} (app version {app_version})",
        resource_node_modules_dir.display(),
        dest_node_modules_dir.display()
    );
    // Start from a clean destination so a stale file from a previous
    // version (e.g. a package removed between node-pty releases) can never
    // linger and shadow the fresh copy.
    if tokio::fs::try_exists(dest_node_modules_dir).await.unwrap_or(false) {
        tokio::fs::remove_dir_all(dest_node_modules_dir).await?;
    }
    copy_dir_recursive(resource_node_modules_dir, dest_node_modules_dir).await?;
    if let Some(parent) = stamp_file.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(stamp_file, app_version).await?;
    Ok(())
}

/// Recursive async directory copy. Boxed because async fns can't recurse
/// directly (the future's size would be infinite). Resolves symlinks to
/// their target file rather than propagating a link that may not survive
/// the copy (some npm layouts stage optional-dependency shims as symlinks;
/// a dangling one in `app_data` would break the require() it backs).
fn copy_dir_recursive<'a>(
    src: &'a Path,
    dest: &'a Path,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), BundleError>> + Send + 'a>> {
    Box::pin(async move {
        tokio::fs::create_dir_all(dest).await?;
        let mut entries = tokio::fs::read_dir(src).await?;
        while let Some(entry) = entries.next_entry().await? {
            let file_type = entry.file_type().await?;
            let dest_path = dest.join(entry.file_name());
            if file_type.is_dir() {
                copy_dir_recursive(&entry.path(), &dest_path).await?;
            } else if file_type.is_symlink() {
                if let Ok(target) = tokio::fs::read_link(entry.path()).await {
                    let resolved = if target.is_absolute() { target } else { src.join(target) };
                    if tokio::fs::metadata(&resolved).await.map(|m| m.is_file()).unwrap_or(false) {
                        tokio::fs::copy(&resolved, &dest_path).await?;
                    }
                }
            } else {
                tokio::fs::copy(entry.path(), &dest_path).await?;
            }
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // `AGILETASKER_BUNDLE_BASE` is process-global state; `cargo test` runs
    // tests in this file on multiple threads by default, so every test that
    // touches the var takes this lock first to avoid one test's env change
    // leaking into another's assertion mid-flight. `unwrap_or_else` shrugs
    // off poisoning from an earlier panicking test rather than cascading
    // failures across the whole module.
    static ENV_LOCK: Mutex<()> = Mutex::new(());
    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    // `env::set_var`/`remove_var` are `unsafe` (process-global mutation
    // isn't thread-safe in general) — these tests already serialize access
    // via `ENV_LOCK`, so the safety condition rustc can't see is genuinely
    // upheld here. Centralized so the `unsafe` blocks don't have to repeat.
    fn set_env(key: &str, val: &str) {
        unsafe { std::env::set_var(key, val) }
    }
    fn remove_env(key: &str) {
        unsafe { std::env::remove_var(key) }
    }

    #[test]
    fn bundle_base_defaults_to_production() {
        let _guard = lock_env();
        remove_env("AGILETASKER_BUNDLE_BASE");
        assert_eq!(bundle_base(), "https://agiletasker.com/agent");
    }

    #[test]
    fn bundle_base_honors_dev_override() {
        let _guard = lock_env();
        set_env("AGILETASKER_BUNDLE_BASE", "http://127.0.0.1:8787/agent");
        assert_eq!(bundle_base(), "http://127.0.0.1:8787/agent");
        remove_env("AGILETASKER_BUNDLE_BASE");
    }

    #[tokio::test]
    async fn ensure_bundle_falls_back_to_cache_when_download_fails() {
        let _guard = lock_env();
        let dir = std::env::temp_dir().join(format!("agiletasker-bundle-test-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let dest = dir.join("agiletasker-host.mjs");
        tokio::fs::write(&dest, b"cached copy").await.unwrap();
        // Bogus base guarantees the download attempt fails fast (invalid
        // scheme/host) without needing a real network round trip in CI.
        set_env("AGILETASKER_BUNDLE_BASE", "http://127.0.0.1:1/agent");
        let result = ensure_bundle(DaemonKind::Host, &dest).await;
        remove_env("AGILETASKER_BUNDLE_BASE");
        assert!(result.is_ok(), "{result:?}");
        let contents = tokio::fs::read_to_string(&dest).await.unwrap();
        assert_eq!(contents, "cached copy");
        tokio::fs::remove_dir_all(&dir).await.ok();
    }

    #[tokio::test]
    async fn ensure_bundle_errors_when_no_cache_and_download_fails() {
        let _guard = lock_env();
        let dir = std::env::temp_dir().join(format!("agiletasker-bundle-test-nocache-{}", std::process::id()));
        let dest = dir.join("agiletasker-agent.mjs");
        set_env("AGILETASKER_BUNDLE_BASE", "http://127.0.0.1:1/agent");
        let result = ensure_bundle(DaemonKind::Agent, &dest).await;
        remove_env("AGILETASKER_BUNDLE_BASE");
        assert!(result.is_err());
        tokio::fs::remove_dir_all(&dir).await.ok();
    }

    #[tokio::test]
    async fn ensure_daemon_deps_copies_then_skips_when_stamped() {
        let base = std::env::temp_dir().join(format!("agiletasker-deps-test-{}", std::process::id()));
        let resource_dir = base.join("resource-node_modules");
        let dest_dir = base.join("app-data-node_modules");
        let stamp = base.join(".deps-version");
        tokio::fs::create_dir_all(resource_dir.join("node-pty")).await.unwrap();
        tokio::fs::write(resource_dir.join("node-pty").join("index.js"), b"module.exports = {}").await.unwrap();

        ensure_daemon_deps(&resource_dir, &dest_dir, &stamp, "0.1.0").await.unwrap();
        assert!(dest_dir.join("node-pty").join("index.js").exists());
        assert_eq!(tokio::fs::read_to_string(&stamp).await.unwrap(), "0.1.0");

        // Simulate the resource dir changing after the copy — a second call
        // with the SAME app_version must be a no-op (stamp already current).
        tokio::fs::write(resource_dir.join("node-pty").join("index.js"), b"changed").await.unwrap();
        ensure_daemon_deps(&resource_dir, &dest_dir, &stamp, "0.1.0").await.unwrap();
        assert_eq!(
            tokio::fs::read_to_string(dest_dir.join("node-pty").join("index.js")).await.unwrap(),
            "module.exports = {}",
            "same app_version must not re-copy"
        );

        // A version bump DOES re-copy.
        ensure_daemon_deps(&resource_dir, &dest_dir, &stamp, "0.2.0").await.unwrap();
        assert_eq!(
            tokio::fs::read_to_string(dest_dir.join("node-pty").join("index.js")).await.unwrap(),
            "changed"
        );

        tokio::fs::remove_dir_all(&base).await.ok();
    }
}
