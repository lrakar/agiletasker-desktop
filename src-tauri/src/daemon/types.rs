//! Shared data types for the daemon subsystem.
//!
//! This module is the wire format: `DaemonStatus` is exactly the JSON shape
//! the locked IPC contract specifies (camelCase, via `serde(rename_all)`),
//! and `DaemonConfig` is what gets persisted to `<app_data_dir>/daemons.json`
//! — deliberately WITHOUT the pairing key, which lives only in the OS
//! keychain (see `daemon::manager`).
//!
//! Kept free of `tauri`/`tokio` on purpose: `cargo test` exercises the
//! (de)serialization here without linking the whole application.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// Which daemon a `DaemonConfig` describes. Mirrors the two entry points in
/// scripts/agent/: `workspace-host.mjs` (Crew remote-PTY host) and
/// `agent-daemon.mjs` (Messenger agent bridge).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DaemonKind {
    Host,
    Agent,
}

impl DaemonKind {
    /// The `^host-[a-z0-9]{20}$` / `^agent-[a-z0-9]{20}$` prefix each
    /// daemon's own arg parser checks (see workspace-host.mjs L73,
    /// agent-daemon.mjs L155) — `commands::validate_pair_config` re-checks
    /// the same shape client-side so a typo fails fast with a clear error
    /// instead of a cryptic daemon-process exit(1).
    pub fn uid_prefix(self) -> &'static str {
        match self {
            DaemonKind::Host => "host-",
            DaemonKind::Agent => "agent-",
        }
    }

    /// Filename of the downloaded bundle, served at
    /// `{AGILETASKER_BUNDLE_BASE}/<this>` (default base
    /// `https://agiletasker.com/agent`).
    pub fn bundle_file(self) -> &'static str {
        match self {
            DaemonKind::Host => "agiletasker-host.mjs",
            DaemonKind::Agent => "agiletasker-agent.mjs",
        }
    }

    /// Only the host daemon needs `node_modules/{node-pty,werift}` physically
    /// adjacent to it (native deps, dynamic-imported relative to the file —
    /// see workspace-host.mjs L169). The agent bridge inlines the Firebase
    /// SDK and needs nothing extra (agent-daemon.mjs header comment).
    pub fn needs_native_deps(self) -> bool {
        matches!(self, DaemonKind::Host)
    }
}

/// Lifecycle state of a supervised daemon process, exactly the enum the IPC
/// contract specifies. `serde(rename_all = "kebab-case")` turns `AuthFailed`
/// into the wire value `"auth-failed"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DaemonState {
    Starting,
    Running,
    Backoff,
    AuthFailed,
    Conflict,
    Stopped,
}

/// Desired (as opposed to observed) run state — what `start_daemon` /
/// `stop_daemon` toggle and what gets persisted, independent of whatever the
/// live `DaemonState` currently is (e.g. desired=running while observed
/// state is transiently `backoff` mid-retry).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DesiredState {
    Running,
    Stopped,
}

/// The `DaemonStatus` JSON shape from the IPC contract, emitted as the
/// `daemon-status` event payload and returned by every daemon-control
/// command.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonStatus {
    pub id: String,
    pub kind: DaemonKind,
    pub uid: String,
    pub state: DaemonState,
    pub pid: Option<u32>,
    pub last_exit_code: Option<i32>,
    pub message: String,
    pub started_at: Option<i64>,
}

impl DaemonStatus {
    /// A fresh, not-yet-started status for a daemon that was just paired.
    pub fn initial(kind: DaemonKind, uid: &str) -> Self {
        DaemonStatus {
            id: uid.to_string(),
            kind,
            uid: uid.to_string(),
            state: DaemonState::Starting,
            pid: None,
            last_exit_code: None,
            message: "starting".to_string(),
            started_at: None,
        }
    }
}

/// Current wall-clock time as epoch milliseconds, for `DaemonStatus::started_at`.
/// `UNIX_EPOCH` is always in the past on any real clock, so the only failure
/// mode `duration_since` has (a clock set before 1970) is not worth a panic
/// path — fall back to 0 rather than `.unwrap()`.
pub fn epoch_ms_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Persisted (secret-free) daemon configuration — one entry per paired
/// daemon in `<app_data_dir>/daemons.json`. The pairing key never appears
/// here; it lives only in the OS keychain under (service="AgileTasker
/// Desktop", account=uid).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonConfig {
    /// == uid, kept as its own field to match the IPC/status shape 1:1.
    pub id: String,
    pub kind: DaemonKind,
    pub uid: String,
    pub cwd: Option<String>,
    pub desired: DesiredState,
}

/// The whole persisted file: `{ "daemons": [ ... ] }`. A struct (not a bare
/// `Vec`) so the file has room to grow a schema version or other top-level
/// field later without a breaking migration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonsFile {
    #[serde(default)]
    pub daemons: Vec<DaemonConfig>,
}

/// Input shape for the `pair_daemon` command — the only place the pairing
/// key ever appears in memory outside the keychain call itself.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairDaemonInput {
    pub kind: DaemonKind,
    pub uid: String,
    pub key: String,
    pub cwd: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_status_serializes_camel_case_and_kebab_state() {
        let status = DaemonStatus {
            id: "host-abcdefghijklmnopqrst".into(),
            kind: DaemonKind::Host,
            uid: "host-abcdefghijklmnopqrst".into(),
            state: DaemonState::AuthFailed,
            pid: Some(1234),
            last_exit_code: Some(1),
            message: "wrong or revoked key".into(),
            started_at: Some(1_700_000_000_000),
        };
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["lastExitCode"], 1);
        assert_eq!(json["startedAt"], 1_700_000_000_000i64);
        assert_eq!(json["state"], "auth-failed");
        assert_eq!(json["kind"], "host");
    }

    #[test]
    fn daemon_status_round_trips() {
        let status = DaemonStatus::initial(DaemonKind::Agent, "agent-abcdefghijklmnopqrst");
        let json = serde_json::to_string(&status).unwrap();
        let back: DaemonStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back.uid, status.uid);
        assert_eq!(back.state, DaemonState::Starting);
    }

    #[test]
    fn daemon_config_round_trips_without_key_field() {
        let cfg = DaemonConfig {
            id: "agent-abcdefghijklmnopqrst".into(),
            kind: DaemonKind::Agent,
            uid: "agent-abcdefghijklmnopqrst".into(),
            cwd: Some("C:/projects/foo".into()),
            desired: DesiredState::Running,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(!json.contains("key"), "config JSON must never carry the pairing key: {json}");
        let back: DaemonConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn daemons_file_defaults_to_empty_and_round_trips() {
        let empty: DaemonsFile = serde_json::from_str("{}").unwrap();
        assert!(empty.daemons.is_empty());

        let file = DaemonsFile {
            daemons: vec![DaemonConfig {
                id: "host-abcdefghijklmnopqrst".into(),
                kind: DaemonKind::Host,
                uid: "host-abcdefghijklmnopqrst".into(),
                cwd: None,
                desired: DesiredState::Stopped,
            }],
        };
        let json = serde_json::to_string_pretty(&file).unwrap();
        let back: DaemonsFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.daemons.len(), 1);
        assert_eq!(back.daemons[0].desired, DesiredState::Stopped);
    }

    #[test]
    fn pair_daemon_input_parses_locked_ipc_shape() {
        let raw = r#"{"kind":"host","uid":"host-abcdefghijklmnopqrst","key":"deadbeef","cwd":null}"#;
        let input: PairDaemonInput = serde_json::from_str(raw).unwrap();
        assert_eq!(input.kind, DaemonKind::Host);
        assert!(input.cwd.is_none());
    }

    #[test]
    fn uid_prefixes_match_daemon_source_regexes() {
        assert_eq!(DaemonKind::Host.uid_prefix(), "host-");
        assert_eq!(DaemonKind::Agent.uid_prefix(), "agent-");
    }
}
