//! Daemon supervision subsystem: pairing, keychain-backed secrets, bundle
//! acquisition, and the crash/backoff/auth-failed/conflict state machine
//! behind the `*_daemon` IPC commands. See `manager` for the lifecycle
//! model and `classify` for the exact stderr strings driving auth-failed
//! vs. twin-conflict detection.

pub mod backoff;
pub mod bundles;
pub mod classify;
pub mod manager;
pub mod paths;
pub mod reap;
pub mod types;

pub use manager::DaemonManager;
pub use types::{DaemonStatus, PairDaemonInput};
