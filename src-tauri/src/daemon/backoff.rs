//! Exponential backoff schedule for daemon restarts, per the supervision
//! policy in the IPC contract: 1s → 2s → 4s → ... capped at 60s, and the
//! attempt counter resets after 60s of clean uptime.
//!
//! Pure and dependency-free so `cargo test` can pin the exact sequence
//! without spinning up a supervisor task.

use std::time::Duration;

pub const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
pub const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// A restart is "clean" (attempt counter resets to 0) once the child has
/// stayed up at least this long.
pub const CLEAN_UPTIME_RESET: Duration = Duration::from_secs(60);
/// Conflict retries run on their own slower, fixed cadence rather than the
/// exponential schedule below — see `classify::ExitClass::Conflict`.
pub const CONFLICT_RETRY: Duration = Duration::from_secs(120);

/// Delay before the Nth consecutive restart attempt since the last clean
/// uptime window (attempt 1 = first restart after a crash, 2 = second
/// crash in a row with no 60s of clean uptime in between, ...).
/// `attempt == 0` is "no delay" (the very first spawn, not a restart).
///
/// Sequence: 0 → 1s → 2s → 4s → 8s → 16s → 32s → 60s → 60s → ...
pub fn delay_for_attempt(attempt: u32) -> Duration {
    if attempt == 0 {
        return Duration::ZERO;
    }
    // attempt=1 -> shift 0 (1s), attempt=2 -> shift 1 (2s), ... capped so the
    // shift itself can never overflow u32 before the duration-level cap
    // below kicks in.
    let shift = (attempt - 1).min(20);
    let ms = INITIAL_BACKOFF.as_millis().saturating_mul(1u128 << shift);
    let capped = ms.min(MAX_BACKOFF.as_millis());
    Duration::from_millis(capped as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attempt_zero_is_immediate() {
        assert_eq!(delay_for_attempt(0), Duration::ZERO);
    }

    #[test]
    fn doubles_from_one_second() {
        assert_eq!(delay_for_attempt(1), Duration::from_secs(1));
        assert_eq!(delay_for_attempt(2), Duration::from_secs(2));
        assert_eq!(delay_for_attempt(3), Duration::from_secs(4));
        assert_eq!(delay_for_attempt(4), Duration::from_secs(8));
        assert_eq!(delay_for_attempt(5), Duration::from_secs(16));
        assert_eq!(delay_for_attempt(6), Duration::from_secs(32));
    }

    #[test]
    fn caps_at_sixty_seconds() {
        assert_eq!(delay_for_attempt(7), Duration::from_secs(60));
        assert_eq!(delay_for_attempt(8), Duration::from_secs(60));
        assert_eq!(delay_for_attempt(100), Duration::from_secs(60));
    }

    #[test]
    fn never_exceeds_max_backoff_constant() {
        for attempt in 0..200 {
            assert!(delay_for_attempt(attempt) <= MAX_BACKOFF);
        }
    }

    #[test]
    fn is_monotonically_nondecreasing() {
        let mut prev = Duration::ZERO;
        for attempt in 0..30 {
            let d = delay_for_attempt(attempt);
            assert!(d >= prev, "attempt {attempt}: {d:?} < previous {prev:?}");
            prev = d;
        }
    }
}
