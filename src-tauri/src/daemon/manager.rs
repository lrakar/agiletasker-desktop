//! The daemon supervisor: one actor-style Tokio task per paired daemon
//! (`run_supervisor`), owned and addressed through `DaemonManager`.
//!
//! ## Lifecycle model
//! Each supervisor task is a loop over two states — `desired: Stopped` (idle,
//! waiting for a command) and `desired: Running` (ensure bundle+deps, read
//! the key from the OS keychain, spawn the Node sidecar, then react to
//! either a supervisor command or a `CommandEvent` from the child via
//! `tokio::select!`). Every state transition updates the shared
//! `Arc<Mutex<DaemonStatus>>` and emits a `daemon-status` event.
//!
//! ## Stop sequence (per the 2026-08-06 orchestrator patch note)
//! Both daemon sources now treat stdin EOF as a graceful-shutdown request
//! when launched with `AGILETASKER_MANAGED=1` (always set below): they
//! `resume()` stdin and call their own `shutdown()` — which stamps
//! Firestore offline and exits 0 — on `'end'`/`'close'`. tauri-plugin-shell's
//! `CommandChild` has no explicit "close stdin without killing" method (its
//! only public methods are `write`, `kill(self)`, `pid` — verified against
//! the plugin's actual source, 2.3.5), so `graceful_stop` below gets the
//! same effect the only way the API allows: capture the pid, then **drop**
//! the whole `CommandChild`. Dropping closes its `stdin_writer` (the pipe
//! write end — the child sees EOF) and releases the wrapped
//! `Arc<shared_child::SharedChild>` handle, which — matching
//! `std::process::Child`'s documented drop semantics that `shared_child`
//! wraps without overriding — does NOT itself kill the process. The
//! `CommandEvent` receiver returned alongside `CommandChild` is a separate
//! object, so it keeps delivering the eventual `Terminated` event after the
//! drop. If the child hasn't exited within 5s (e.g. a still-live production
//! bundle that predates the stdin-close patch — see the 2026-08-06 note),
//! `hard_kill` escalates via the platform's own `taskkill`/`kill` utility,
//! addressed purely by pid (no lingering handle needed).
//!
//! ## Supervision policy
//! - Normal exit/crash → exponential backoff (`daemon::backoff`), attempt
//!   counter resets after 60s of clean uptime.
//! - Auth failure (`classify::ExitClass::AuthFailed`) → state `auth-failed`,
//!   desired flips to `Stopped`. No auto-restart: the user must call
//!   `pair_daemon` again with a fresh key, which unconditionally restarts.
//! - Twin conflict (`classify::ExitClass::Conflict`) → state `conflict`,
//!   retried on a fixed 120s cadence (not the exponential schedule — it
//!   isn't this process's fault, and doubling the wait doesn't help a race
//!   against another instance's heartbeat).

use crate::daemon::backoff;
use crate::daemon::bundles::{self, BundleError};
use crate::daemon::classify::{self, ExitClass};
use crate::daemon::paths;
use crate::daemon::types::{
    epoch_ms_now, DaemonConfig, DaemonKind, DaemonState, DaemonStatus, DaemonsFile, DesiredState,
    PairDaemonInput,
};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_shell::process::CommandEvent;
use tauri_plugin_shell::ShellExt;
use tauri::async_runtime::JoinHandle;
use tokio::sync::mpsc;

const KEYCHAIN_SERVICE: &str = "AgileTasker Desktop";
const LOG_CAP: usize = 1000;
const GRACEFUL_STOP_GRACE: Duration = Duration::from_secs(5);
const SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("invalid uid: expected \"{expected_prefix}\" followed by 20 lowercase letters/digits")]
    InvalidUid { expected_prefix: &'static str },
    #[error("invalid key: expected 64 lowercase hex characters")]
    InvalidKey,
    #[error("no daemon with id {0}")]
    NotFound(String),
    #[error("keychain error: {0}")]
    Keychain(#[from] keyring::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Bundle(#[from] BundleError),
    #[error("tauri error: {0}")]
    Tauri(#[from] tauri::Error),
}

/// Commands a `DaemonManager` method sends into a running supervisor task.
#[derive(Debug, Clone, Copy)]
enum SupervisorCmd {
    Start,
    Stop,
    /// Force a graceful stop-then-respawn with a fresh bundle download —
    /// this is how `restart_daemon` rolls out daemon updates, and how
    /// `pair_daemon` applies a replaced key to an already-running daemon.
    Restart,
    /// App is quitting: stop and end the task for good (no further
    /// `desired`-driven respawn).
    Shutdown,
}

/// Fixed-capacity FIFO of log lines backing `daemon_log_tail`.
struct RingBuffer {
    lines: VecDeque<String>,
    cap: usize,
}

impl RingBuffer {
    fn new(cap: usize) -> Self {
        Self { lines: VecDeque::with_capacity(cap.min(64)), cap }
    }

    fn push(&mut self, line: String) {
        if self.lines.len() >= self.cap {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    fn tail(&self, n: usize) -> String {
        let skip = self.lines.len().saturating_sub(n);
        self.lines.iter().skip(skip).cloned().collect::<Vec<_>>().join("\n")
    }
}

struct Entry {
    config: DaemonConfig,
    status: Arc<Mutex<DaemonStatus>>,
    logs: Arc<Mutex<RingBuffer>>,
    cmd_tx: mpsc::UnboundedSender<SupervisorCmd>,
    task: JoinHandle<()>,
}

/// Owns every paired daemon's supervisor task. One instance lives in Tauri's
/// managed state (`app.manage(...)`) for the app's whole lifetime.
pub struct DaemonManager {
    app: AppHandle,
    app_data_dir: PathBuf,
    entries: Mutex<HashMap<String, Entry>>,
}

impl DaemonManager {
    /// `app_data_dir` is resolved once, synchronously (path resolution
    /// doesn't need the async runtime) — every other method reuses the
    /// cached value instead of re-resolving and re-handling that error at
    /// every call site. On the (extremely unlikely) chance resolution
    /// fails, falls back to a clearly-named temp dir rather than making the
    /// whole app unconstructable over a directory the OS should always be
    /// able to name.
    pub fn new(app: AppHandle) -> Arc<Self> {
        let app_data_dir = app.path().app_data_dir().unwrap_or_else(|e| {
            log::error!("could not resolve app_data_dir ({e}); falling back to a temp dir");
            std::env::temp_dir().join("agiletasker-desktop-fallback-data")
        });
        Arc::new(Self { app, app_data_dir, entries: Mutex::new(HashMap::new()) })
    }

    /// The resolved app-data dir — shared with `daemon::reap`, which scans
    /// for stray processes running bundles out of this same base path.
    pub fn app_data_dir(&self) -> &std::path::Path {
        &self.app_data_dir
    }

    /// Loads `daemons.json` and spawns a supervisor task per persisted
    /// entry, honoring its persisted `desired` state. Called at startup
    /// (after `daemon::reap` clears strays), and again by the update flow
    /// to respawn everything `shutdown_all` stopped when an update install
    /// fails (see `tray::install_update_stopping_agents`) — safe to re-call
    /// only because `shutdown_all` drained `entries` first.
    pub fn load_and_spawn_all(&self) {
        let file = match paths::load_daemons_file(&self.app_data_dir) {
            Ok(f) => f,
            Err(e) => {
                log::error!("failed to load daemons.json ({e}) — starting with no paired daemons");
                return;
            }
        };
        for cfg in file.daemons {
            self.spawn_entry(cfg);
        }
    }

    fn spawn_entry(&self, config: DaemonConfig) {
        let status = Arc::new(Mutex::new(DaemonStatus::initial(config.kind, &config.uid)));
        let logs = Arc::new(Mutex::new(RingBuffer::new(LOG_CAP)));
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        // `tauri::async_runtime::spawn`, NOT `tokio::spawn`: this is also
        // called from `setup()` (startup restore of persisted daemons), which
        // runs OUTSIDE any Tokio runtime context — bare `tokio::spawn` there
        // panics ("there is no reactor running") and takes the whole app down
        // on every launch once a daemon is paired. Tauri's wrapper holds a
        // handle to the app's runtime and works from either context.
        let task = tauri::async_runtime::spawn(run_supervisor(
            self.app.clone(),
            self.app_data_dir.clone(),
            config.id.clone(),
            config.kind,
            config.uid.clone(),
            config.cwd.clone(),
            status.clone(),
            logs.clone(),
            cmd_rx,
            config.desired,
        ));
        let mut entries = self.lock_entries();
        entries.insert(config.id.clone(), Entry { config, status, logs, cmd_tx, task });
    }

    fn lock_entries(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Validates + pairs a daemon: stores the key in the OS keychain
    /// (replacing any existing entry for this uid), upserts + persists the
    /// (secret-free) config, and (re)starts its supervisor — a fresh entry
    /// spawns one, an existing one is told to `Restart` so the replaced key
    /// takes effect immediately. Enables autostart the first time a daemon
    /// is ever paired against an installation with none currently paired
    /// (see module docs — "default-on" per the IPC contract).
    pub fn pair(&self, input: PairDaemonInput) -> Result<DaemonStatus, DaemonError> {
        validate_pair_input(&input)?;
        let PairDaemonInput { kind, uid, key, cwd } = input;

        // Land the key before anything else can observe this uid as
        // "paired" — the supervisor always reads the key fresh from the
        // keychain on every spawn attempt, never caching it in memory.
        let keychain = keyring::Entry::new(KEYCHAIN_SERVICE, &uid)?;
        keychain.set_password(&key)?;

        let config = DaemonConfig { id: uid.clone(), kind, uid: uid.clone(), cwd, desired: DesiredState::Running };

        let was_empty = self.lock_entries().is_empty();
        let existed = {
            let mut entries = self.lock_entries();
            if let Some(entry) = entries.get_mut(&uid) {
                entry.config = config.clone();
                let _ = entry.cmd_tx.send(SupervisorCmd::Restart);
                true
            } else {
                false
            }
        };
        if !existed {
            self.spawn_entry(config);
        }

        self.persist_all()?;

        if was_empty {
            if let Err(e) = self.app.autolaunch().enable() {
                log::warn!("could not enable autostart after first pair: {e}");
            }
        }

        self.emit_full_refresh();
        self.status_of(&uid).ok_or(DaemonError::NotFound(uid))
    }

    /// Stops and tears down a daemon: signals its task to shut down, waits
    /// briefly for that (best-effort — `unpair` itself never hangs on a
    /// wedged child; the task's own hard-kill escalation is what actually
    /// bounds the wait), then removes the keychain entry and persisted
    /// config.
    pub async fn unpair(&self, id: &str) -> Result<(), DaemonError> {
        let entry = {
            let mut entries = self.lock_entries();
            entries.remove(id)
        }
        .ok_or_else(|| DaemonError::NotFound(id.to_string()))?;

        let _ = entry.cmd_tx.send(SupervisorCmd::Shutdown);
        let uid = entry.config.uid.clone();
        if tokio::time::timeout(SHUTDOWN_JOIN_TIMEOUT, entry.task).await.is_err() {
            log::warn!("daemon {id} did not finish shutting down within {SHUTDOWN_JOIN_TIMEOUT:?} of unpair (its own hard-kill escalation is still in flight)");
        }

        match keyring::Entry::new(KEYCHAIN_SERVICE, &uid) {
            Ok(k) => {
                if let Err(e) = k.delete_credential() {
                    if !matches!(e, keyring::Error::NoEntry) {
                        log::warn!("failed to delete keychain entry for {uid}: {e}");
                    }
                }
            }
            Err(e) => log::warn!("failed to open keychain entry for {uid} to delete it: {e}"),
        }

        self.persist_all()?;
        self.emit_full_refresh();
        Ok(())
    }

    pub fn list(&self) -> Vec<DaemonStatus> {
        self.lock_entries().values().map(|e| e.snapshot_status()).collect()
    }

    fn status_of(&self, id: &str) -> Option<DaemonStatus> {
        self.lock_entries().get(id).map(|e| e.snapshot_status())
    }

    pub fn restart(&self, id: &str) -> Result<DaemonStatus, DaemonError> {
        self.send_cmd(id, SupervisorCmd::Restart)?;
        self.status_of(id).ok_or_else(|| DaemonError::NotFound(id.to_string()))
    }

    pub fn stop(&self, id: &str) -> Result<DaemonStatus, DaemonError> {
        self.set_desired(id, DesiredState::Stopped)?;
        self.send_cmd(id, SupervisorCmd::Stop)?;
        self.status_of(id).ok_or_else(|| DaemonError::NotFound(id.to_string()))
    }

    pub fn start(&self, id: &str) -> Result<DaemonStatus, DaemonError> {
        self.set_desired(id, DesiredState::Running)?;
        self.send_cmd(id, SupervisorCmd::Start)?;
        self.status_of(id).ok_or_else(|| DaemonError::NotFound(id.to_string()))
    }

    pub fn log_tail(&self, id: &str, lines: Option<usize>) -> Result<String, DaemonError> {
        let n = lines.unwrap_or(200).min(LOG_CAP);
        let entries = self.lock_entries();
        let entry = entries.get(id).ok_or_else(|| DaemonError::NotFound(id.to_string()))?;
        let tail = entry.logs.lock().unwrap_or_else(|e| e.into_inner()).tail(n);
        Ok(tail)
    }

    /// Gracefully stops every daemon concurrently (bounded by each task's
    /// own grace + hard-kill escalation) — called from the tray's "Quit
    /// AgileTasker" handler before the process exits, and from the update
    /// flow before an install (running daemons hold the sidecar `node.exe`
    /// open, which fails the installer AND orphans them across the
    /// restart — see `tray::install_update_stopping_agents`). Persisted
    /// `desired` states are deliberately untouched, so a later
    /// `load_and_spawn_all` restores exactly what was running.
    pub async fn shutdown_all(&self) {
        let drained: Vec<Entry> = self.lock_entries().drain().map(|(_, v)| v).collect();
        let mut waiters = Vec::with_capacity(drained.len());
        for entry in drained {
            let _ = entry.cmd_tx.send(SupervisorCmd::Shutdown);
            let id = entry.config.id.clone();
            waiters.push(tauri::async_runtime::spawn(async move {
                if tokio::time::timeout(SHUTDOWN_JOIN_TIMEOUT, entry.task).await.is_err() {
                    log::warn!("daemon {id} still shutting down after {SHUTDOWN_JOIN_TIMEOUT:?} — quitting anyway");
                }
            }));
        }
        for w in waiters {
            let _ = w.await;
        }
    }

    /// How many daemons currently report `state: running` — feeds the
    /// tray's "Agents: N running" label.
    pub fn running_count(&self) -> usize {
        self.lock_entries()
            .values()
            .filter(|e| e.snapshot_status().state == DaemonState::Running)
            .count()
    }

    fn set_desired(&self, id: &str, desired: DesiredState) -> Result<(), DaemonError> {
        {
            let mut entries = self.lock_entries();
            let entry = entries.get_mut(id).ok_or_else(|| DaemonError::NotFound(id.to_string()))?;
            entry.config.desired = desired;
        }
        self.persist_all()
    }

    fn send_cmd(&self, id: &str, cmd: SupervisorCmd) -> Result<(), DaemonError> {
        let entries = self.lock_entries();
        let entry = entries.get(id).ok_or_else(|| DaemonError::NotFound(id.to_string()))?;
        // A send failure here means the task already ended on its own — a
        // benign race (e.g. it was mid-Shutdown from another path), not
        // something worth surfacing to the IPC caller.
        let _ = entry.cmd_tx.send(cmd);
        Ok(())
    }

    fn persist_all(&self) -> Result<(), DaemonError> {
        let daemons: Vec<DaemonConfig> = self.lock_entries().values().map(|e| e.config.clone()).collect();
        paths::save_daemons_file(&self.app_data_dir, &DaemonsFile { daemons }).map_err(DaemonError::Io)
    }

    fn emit_full_refresh(&self) {
        for status in self.list() {
            let _ = self.app.emit("daemon-status", &status);
        }
    }
}

impl Entry {
    fn snapshot_status(&self) -> DaemonStatus {
        self.status.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// `^<prefix>[a-z0-9]{20}$` / `^[0-9a-f]{64}$`, the same shapes both daemon
/// sources check themselves — validated here too so a typo surfaces as a
/// clear `pair_daemon` error instead of a spawned process's cryptic exit(1).
fn validate_pair_input(input: &PairDaemonInput) -> Result<(), DaemonError> {
    let prefix = input.kind.uid_prefix();
    let uid_ok = input
        .uid
        .strip_prefix(prefix)
        .map(|rest| rest.len() == 20 && rest.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()))
        .unwrap_or(false);
    if !uid_ok {
        return Err(DaemonError::InvalidUid { expected_prefix: prefix });
    }
    let key_ok = input.key.len() == 64 && input.key.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c));
    if !key_ok {
        return Err(DaemonError::InvalidKey);
    }
    Ok(())
}

fn push_log(logs: &Arc<Mutex<RingBuffer>>, prefix: &str, bytes: &[u8]) {
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.trim_end_matches(['\r', '\n']);
    if trimmed.is_empty() {
        return;
    }
    logs.lock().unwrap_or_else(|e| e.into_inner()).push(format!("[{prefix}] {trimmed}"));
}

fn set_status(app: &AppHandle, status: &Arc<Mutex<DaemonStatus>>, mutate: impl FnOnce(&mut DaemonStatus)) {
    let snapshot = {
        let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
        mutate(&mut s);
        s.clone()
    };
    let _ = app.emit("daemon-status", &snapshot);
}

/// Drops `child` to close its stdin (see module docs for why that's the
/// only lever the shell plugin's API exposes), then waits up to
/// `GRACEFUL_STOP_GRACE` for the paired `CommandEvent` receiver to report
/// `Terminated` — draining any trailing stdout/stderr into `logs` while it
/// waits — escalating to `hard_kill` if the deadline passes first.
async fn graceful_stop(
    child: tauri_plugin_shell::process::CommandChild,
    rx: &mut mpsc::Receiver<CommandEvent>,
    logs: &Arc<Mutex<RingBuffer>>,
) {
    let pid = child.pid();
    drop(child);

    let sleep = tokio::time::sleep(GRACEFUL_STOP_GRACE);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            _ = &mut sleep => {
                log::warn!("daemon pid {pid} did not exit within {GRACEFUL_STOP_GRACE:?} of stdin close — escalating to a hard kill");
                hard_kill(pid).await;
                return;
            }
            event = rx.recv() => {
                match event {
                    Some(CommandEvent::Terminated(_)) | None => return,
                    Some(CommandEvent::Stdout(b)) => push_log(logs, "out", &b),
                    Some(CommandEvent::Stderr(b)) => push_log(logs, "err", &b),
                    _ => {}
                }
            }
        }
    }
}

/// Last-resort termination by pid, used when a child ignores stdin closing
/// for the full grace period — and by `daemon::reap` for stray daemons from
/// a previous shell instance (where no stdin handle ever existed to close).
/// Shells out to the platform's own process-kill utility rather than a
/// native syscall crate: by this point we no longer hold a `CommandChild`
/// (it was dropped to close stdin — see `graceful_stop`), so there is no
/// in-process kill() handle left to call, and addressing by bare pid is
/// exactly what these utilities are for.
pub(crate) async fn hard_kill(pid: u32) {
    let result = if cfg!(windows) {
        let mut cmd = tokio::process::Command::new("taskkill");
        cmd.args(["/PID", &pid.to_string(), "/T", "/F"]);
        // CREATE_NO_WINDOW — a GUI-subsystem parent would otherwise flash a
        // console window for the utility.
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000);
        cmd.output().await
    } else {
        tokio::process::Command::new("kill").args(["-TERM", &pid.to_string()]).output().await
    };
    if let Err(e) = result {
        log::error!("hard kill of pid {pid} failed to launch: {e}");
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_supervisor(
    app: AppHandle,
    app_data_dir: PathBuf,
    id: String,
    kind: DaemonKind,
    uid: String,
    cwd: Option<String>,
    status: Arc<Mutex<DaemonStatus>>,
    logs: Arc<Mutex<RingBuffer>>,
    mut cmd_rx: mpsc::UnboundedReceiver<SupervisorCmd>,
    initial_desired: DesiredState,
) {
    let mut desired = initial_desired;
    let mut attempt: u32 = 0;
    log::debug!("supervisor task started for {id} ({kind:?})");

    'outer: loop {
        if matches!(desired, DesiredState::Stopped) {
            set_status(&app, &status, |s| {
                s.state = DaemonState::Stopped;
                s.pid = None;
                s.message = "stopped".to_string();
            });
            match cmd_rx.recv().await {
                Some(SupervisorCmd::Start) | Some(SupervisorCmd::Restart) => {
                    desired = DesiredState::Running;
                    attempt = 0;
                    continue 'outer;
                }
                Some(SupervisorCmd::Stop) => continue 'outer,
                Some(SupervisorCmd::Shutdown) | None => return,
            }
        }

        // desired == Running from here down.
        if attempt > 0 {
            let delay = backoff::delay_for_attempt(attempt);
            set_status(&app, &status, |s| {
                s.state = DaemonState::Backoff;
                s.message = format!("retrying in {}s", delay.as_secs());
            });
            let sleep = tokio::time::sleep(delay);
            tokio::pin!(sleep);
            tokio::select! {
                _ = &mut sleep => {}
                cmd = cmd_rx.recv() => match cmd {
                    Some(SupervisorCmd::Stop) => { desired = DesiredState::Stopped; continue 'outer; }
                    Some(SupervisorCmd::Shutdown) | None => return,
                    Some(SupervisorCmd::Restart) => { attempt = 0; }
                    Some(SupervisorCmd::Start) => {}
                },
            }
        }

        set_status(&app, &status, |s| {
            s.state = DaemonState::Starting;
            s.message = "resolving daemon bundle".to_string();
        });

        let bundle_dest = paths::bundle_path(&app_data_dir, kind);
        if let Err(e) = bundles::ensure_bundle(kind, &bundle_dest).await {
            attempt += 1;
            set_status(&app, &status, |s| {
                s.state = DaemonState::Backoff;
                s.message = format!("bundle unavailable: {e}");
            });
            continue 'outer;
        }

        if kind.needs_native_deps() {
            let resource_dir = app.path().resource_dir().unwrap_or_else(|e| {
                log::error!("could not resolve resource_dir ({e}); native-deps sync will fail");
                PathBuf::from(".")
            });
            let resource_node_modules = paths::resource_daemon_deps_node_modules(&resource_dir);
            let dest_node_modules = paths::node_modules_dir(&app_data_dir);
            let stamp = paths::deps_stamp_file(&app_data_dir);
            let app_version = app.package_info().version.to_string();
            if let Err(e) =
                bundles::ensure_daemon_deps(&resource_node_modules, &dest_node_modules, &stamp, &app_version).await
            {
                attempt += 1;
                set_status(&app, &status, |s| {
                    s.state = DaemonState::Backoff;
                    s.message = format!("native deps sync failed: {e}");
                });
                continue 'outer;
            }
        }

        let key = match keyring::Entry::new(KEYCHAIN_SERVICE, &uid).and_then(|e| e.get_password()) {
            Ok(k) => k,
            Err(e) => {
                // Shouldn't happen in normal operation — `pair()` always
                // writes the key before a supervisor can observe this uid —
                // but an operator-level keychain wipe is possible, and this
                // is not the same failure class as a wrong/revoked key
                // (auth-failed comes from the DAEMON rejecting the key, not
                // from us being unable to find one at all).
                attempt += 1;
                set_status(&app, &status, |s| {
                    s.state = DaemonState::Backoff;
                    s.message = format!("could not read pairing key from the OS keychain: {e}");
                });
                continue 'outer;
            }
        };

        let mut args = vec![bundle_dest.to_string_lossy().to_string(), "--pair".to_string(), format!("{uid}:{key}")];
        if let (DaemonKind::Agent, Some(c)) = (kind, &cwd) {
            args.push("--cwd".to_string());
            args.push(c.clone());
        }

        let sidecar = match app.shell().sidecar("node") {
            Ok(s) => s,
            Err(e) => {
                attempt += 1;
                set_status(&app, &status, |s| {
                    s.state = DaemonState::Backoff;
                    s.message = format!("could not resolve the node sidecar: {e}");
                });
                continue 'outer;
            }
        };

        let spawn_result = sidecar.args(&args).current_dir(&app_data_dir).env("AGILETASKER_MANAGED", "1").spawn();
        let (mut rx, child) = match spawn_result {
            Ok(pair) => pair,
            Err(e) => {
                attempt += 1;
                set_status(&app, &status, |s| {
                    s.state = DaemonState::Backoff;
                    s.message = format!("spawn failed: {e}");
                });
                continue 'outer;
            }
        };

        let pid = child.pid();
        let spawned_at = tokio::time::Instant::now();
        set_status(&app, &status, |s| {
            s.state = DaemonState::Running;
            s.pid = Some(pid);
            s.started_at = Some(epoch_ms_now());
            s.last_exit_code = None;
            s.message = "running".to_string();
        });

        let mut child = Some(child);
        let mut stderr_tail = String::new();
        let exit_class: ExitClass;
        let exit_code: Option<i32>;

        loop {
            tokio::select! {
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(SupervisorCmd::Stop) => {
                            graceful_stop(child.take().expect("child present while running"), &mut rx, &logs).await;
                            desired = DesiredState::Stopped;
                            continue 'outer;
                        }
                        Some(SupervisorCmd::Shutdown) | None => {
                            graceful_stop(child.take().expect("child present while running"), &mut rx, &logs).await;
                            set_status(&app, &status, |s| { s.state = DaemonState::Stopped; s.pid = None; });
                            return;
                        }
                        Some(SupervisorCmd::Restart) => {
                            graceful_stop(child.take().expect("child present while running"), &mut rx, &logs).await;
                            attempt = 0;
                            continue 'outer;
                        }
                        Some(SupervisorCmd::Start) => { /* already running */ }
                    }
                }
                event = rx.recv() => {
                    match event {
                        Some(CommandEvent::Stdout(b)) => push_log(&logs, "out", &b),
                        Some(CommandEvent::Stderr(b)) => {
                            push_log(&logs, "err", &b);
                            stderr_tail.push_str(&String::from_utf8_lossy(&b));
                            stderr_tail.push('\n');
                            const STDERR_TAIL_CAP: usize = 8000;
                            if stderr_tail.len() > STDERR_TAIL_CAP {
                                let cut = stderr_tail.len() - STDERR_TAIL_CAP;
                                stderr_tail.drain(..cut);
                            }
                        }
                        Some(CommandEvent::Error(e)) => push_log(&logs, "err", e.as_bytes()),
                        Some(CommandEvent::Terminated(payload)) => {
                            exit_code = payload.code;
                            exit_class = classify::classify_stderr(&stderr_tail);
                            child = None;
                            break;
                        }
                        None => {
                            exit_code = None;
                            exit_class = classify::classify_stderr(&stderr_tail);
                            child = None;
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }
        let _ = child; // already None; keeps the binding's intent obvious at the break sites above

        if spawned_at.elapsed() >= backoff::CLEAN_UPTIME_RESET {
            attempt = 0;
        }

        match exit_class {
            ExitClass::AuthFailed => {
                set_status(&app, &status, |s| {
                    s.state = DaemonState::AuthFailed;
                    s.last_exit_code = exit_code;
                    s.pid = None;
                    s.message = "wrong or revoked key — re-pair to retry".to_string();
                });
                desired = DesiredState::Stopped;
                continue 'outer;
            }
            ExitClass::Conflict => {
                set_status(&app, &status, |s| {
                    s.state = DaemonState::Conflict;
                    s.last_exit_code = exit_code;
                    s.pid = None;
                    s.message = "another instance is already online — retrying in 120s".to_string();
                });
                let sleep = tokio::time::sleep(backoff::CONFLICT_RETRY);
                tokio::pin!(sleep);
                tokio::select! {
                    _ = &mut sleep => {}
                    cmd = cmd_rx.recv() => match cmd {
                        Some(SupervisorCmd::Stop) => { desired = DesiredState::Stopped; continue 'outer; }
                        Some(SupervisorCmd::Shutdown) | None => return,
                        _ => {}
                    },
                }
                continue 'outer;
            }
            ExitClass::Other => {
                attempt += 1;
                set_status(&app, &status, |s| {
                    s.state = DaemonState::Backoff;
                    s.last_exit_code = exit_code;
                    s.pid = None;
                    s.message = format!("exited (code {exit_code:?}) — restarting");
                });
                continue 'outer;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::types::DaemonKind;

    fn input(kind: DaemonKind, uid: &str, key: &str) -> PairDaemonInput {
        PairDaemonInput { kind, uid: uid.to_string(), key: key.to_string(), cwd: None }
    }

    // Exactly 64 lowercase hex chars (`^[0-9a-f]{64}$`) — built as four
    // concatenated 16-char blocks rather than hand-typed (an earlier
    // hand-typed version silently drifted to 62 chars and made these tests
    // fail for the wrong reason), with a compile-time length assertion so
    // it can never drift again unnoticed.
    const VALID_KEY: &str = concat!("0123456789abcdef", "0123456789abcdef", "0123456789abcdef", "0123456789abcdef");
    const _: () = assert!(VALID_KEY.len() == 64);

    #[test]
    fn accepts_valid_host_pair() {
        assert!(validate_pair_input(&input(DaemonKind::Host, "host-abcdefghijklmnopqrst", VALID_KEY)).is_ok());
    }

    #[test]
    fn accepts_valid_agent_pair() {
        assert!(validate_pair_input(&input(DaemonKind::Agent, "agent-abcdefghijklmnopqrst", VALID_KEY)).is_ok());
    }

    #[test]
    fn rejects_wrong_prefix_for_kind() {
        // A host-shaped uid submitted as kind Agent (or vice versa) must fail.
        assert!(validate_pair_input(&input(DaemonKind::Agent, "host-abcdefghijklmnopqrst", VALID_KEY)).is_err());
        assert!(validate_pair_input(&input(DaemonKind::Host, "agent-abcdefghijklmnopqrst", VALID_KEY)).is_err());
    }

    #[test]
    fn rejects_wrong_length_uid_suffix() {
        assert!(validate_pair_input(&input(DaemonKind::Host, "host-tooshort", VALID_KEY)).is_err());
        assert!(validate_pair_input(&input(DaemonKind::Host, "host-abcdefghijklmnopqrstEXTRA", VALID_KEY)).is_err());
    }

    #[test]
    fn rejects_uppercase_in_uid() {
        assert!(validate_pair_input(&input(DaemonKind::Host, "host-ABCDEFGHIJKLMNOPQRST", VALID_KEY)).is_err());
    }

    #[test]
    fn rejects_short_or_uppercase_key() {
        assert!(validate_pair_input(&input(DaemonKind::Host, "host-abcdefghijklmnopqrst", "deadbeef")).is_err());
        let upper_key = VALID_KEY.to_uppercase();
        assert!(validate_pair_input(&input(DaemonKind::Host, "host-abcdefghijklmnopqrst", &upper_key)).is_err());
    }

    #[test]
    fn ring_buffer_caps_and_tails() {
        let mut rb = RingBuffer::new(3);
        rb.push("a".into());
        rb.push("b".into());
        rb.push("c".into());
        rb.push("d".into()); // evicts "a"
        assert_eq!(rb.tail(10), "b\nc\nd");
        assert_eq!(rb.tail(2), "c\nd");
        assert_eq!(rb.tail(0), "");
    }
}
