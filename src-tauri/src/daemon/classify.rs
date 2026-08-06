//! Classifies a daemon child's stderr output into the outcomes the
//! supervisor (`daemon::manager`) treats specially, using substrings
//! extracted verbatim from the two daemon sources in the main repo
//! (`scripts/agent/workspace-host.mjs`, `scripts/agent/agent-daemon.mjs`,
//! read 2026-08-06 for this task).
//!
//! Both daemons print a DIFFERENT full sentence per identity kind for the
//! "same" failure, but each converges on a shared substring that is a
//! stable classification anchor:
//!
//! - **auth failure** — every wrong/revoked-key rejection contains the
//!   literal substring `"Pairing failed"`:
//!   - workspace-host.mjs: `Pairing failed (${e.code || e.message}) — wrong
//!     or revoked key? Re-create the host in the app.`
//!   - agent-daemon.mjs (paired mode): `Pairing failed (${e.code ||
//!     e.message}) — wrong or revoked key? Re-create the agent in
//!     Messenger → Add agent.`
//! - **twin conflict** — every singleton-guard rejection contains the
//!   literal substring `"refusing to start a twin"`:
//!   - workspace-host.mjs: `Another host daemon (instance ...) heartbeat
//!     ...s ago — refusing to start a twin.\nStop it first, or wait ...s if
//!     it crashed.`
//!   - agent-daemon.mjs: `Another daemon (...) heartbeat ...s ago on this
//!     conversation — refusing to start a twin.\nStop the other one
//!     (Ctrl+C in its terminal) or wait ...s if it crashed.`
//!
//! Everything else — missing native deps (host only), a malformed `--pair`
//! value (shouldn't happen; `commands::validate_pair_config` checks the
//! same regex before we ever spawn), a Firestore hiccup, an ordinary crash —
//! is deliberately NOT special-cased: the IPC contract only names
//! `auth-failed` and `conflict` as distinct states, so anything else is a
//! plain backoff-and-restart per the supervision policy.

/// Outcome of classifying a daemon's accumulated stderr at process exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitClass {
    /// Wrong or revoked pairing key. No auto-restart — the user must
    /// re-pair (`pair_daemon` with a fresh key), which replaces the
    /// keychain entry and restarts unconditionally.
    AuthFailed,
    /// Another live instance of this identity is already online elsewhere.
    /// Retried on a slow cadence (every 120s) rather than backed off,
    /// since it isn't this process's fault and may resolve on its own
    /// (the other instance exits, its heartbeat goes stale, etc).
    Conflict,
    /// Anything else: normal exponential-backoff restart.
    Other,
}

/// Classify a daemon's stderr (accumulated across its run, or just the tail
/// — either works since both anchor substrings appear on the single line
/// that triggers `process.exit(1)` in the daemon).
pub fn classify_stderr(combined_stderr: &str) -> ExitClass {
    // Twin-conflict is checked first: it's the more specific, more
    // actionable-without-the-user case. Auth-failed disables auto-restart
    // entirely, which would be the wrong call if wording ever changed to
    // mention both in one message.
    if combined_stderr.contains("refusing to start a twin") {
        ExitClass::Conflict
    } else if combined_stderr.contains("Pairing failed") {
        ExitClass::AuthFailed
    } else {
        ExitClass::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Copied verbatim (modulo the interpolated instance id / elapsed
    // seconds, which are irrelevant to classification) from the two daemon
    // sources.
    const HOST_AUTH_FAIL: &str =
        "Pairing failed (auth/wrong-password) — wrong or revoked key? Re-create the host in the app.";
    const AGENT_AUTH_FAIL: &str =
        "Pairing failed (auth/wrong-password) — wrong or revoked key? Re-create the agent in Messenger → Add agent.";
    const HOST_TWIN: &str = "Another host daemon (instance h_m1a2b3c4_x9y8z7) heartbeat 12s ago — refusing to start a twin.\nStop it first, or wait 90s if it crashed.";
    const AGENT_TWIN: &str = "Another daemon (d_m1a2b3c4_x9y8z7) heartbeat 5s ago on this conversation — refusing to start a twin.\nStop the other one (Ctrl+C in its terminal) or wait 90s if it crashed.";
    // Also exercised: the daemons' other exit(1) paths, which must NOT
    // classify as auth-failed or conflict (they're plain crashes to the
    // supervisor — e.g. missing native deps is host-only and recoverable by
    // an operator running `npm install`, which a backoff retry can pick up
    // once fixed, unlike auth-failed which explicitly never retries).
    const HOST_MISSING_DEPS: &str =
        "Missing native deps — run: npm install node-pty werift (in the folder next to agiletasker-host.mjs)";
    const BAD_PAIR_ARG: &str = "Bad --pair value. Expected <hostUid>:<key> exactly as shown in the app.";

    #[test]
    fn classifies_host_auth_failure() {
        assert_eq!(classify_stderr(HOST_AUTH_FAIL), ExitClass::AuthFailed);
    }

    #[test]
    fn classifies_agent_auth_failure() {
        assert_eq!(classify_stderr(AGENT_AUTH_FAIL), ExitClass::AuthFailed);
    }

    #[test]
    fn classifies_host_twin_conflict() {
        assert_eq!(classify_stderr(HOST_TWIN), ExitClass::Conflict);
    }

    #[test]
    fn classifies_agent_twin_conflict() {
        assert_eq!(classify_stderr(AGENT_TWIN), ExitClass::Conflict);
    }

    #[test]
    fn classifies_missing_native_deps_as_other_not_auth_failed() {
        assert_eq!(classify_stderr(HOST_MISSING_DEPS), ExitClass::Other);
    }

    #[test]
    fn classifies_bad_pair_arg_as_other() {
        assert_eq!(classify_stderr(BAD_PAIR_ARG), ExitClass::Other);
    }

    #[test]
    fn classifies_unrelated_crash_as_other() {
        assert_eq!(
            classify_stderr("TypeError: cannot read properties of undefined (reading 'foo')"),
            ExitClass::Other
        );
    }

    #[test]
    fn classifies_empty_stderr_as_other() {
        assert_eq!(classify_stderr(""), ExitClass::Other);
    }

    #[test]
    fn twin_takes_priority_if_both_substrings_somehow_present() {
        let s = format!("Pairing failed nonsense {HOST_TWIN}");
        assert_eq!(classify_stderr(&s), ExitClass::Conflict);
    }
}
