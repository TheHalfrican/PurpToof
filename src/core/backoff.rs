//! The backoff ladder.
//!
//! Reconnecting in a tight loop is worse than the bug being fixed, so every
//! recovery attempt is spaced by this. Re-arms deliberately do **not** use it:
//! a benign reopen is not a failure and must not push the ladder down.

use std::time::Duration;

/// 1s, 2s, 5s, 10s, 30s, then 60s forever.
const RUNGS: [u64; 6] = [1, 2, 5, 10, 30, 60];

/// How long flow must stay healthy before the ladder resets.
///
/// Deliberately long, and deliberately a *duration* rather than a single good
/// sample: one non-silent reading during a flapping connection is not
/// evidence of health, and resetting on it would let the app thrash forever
/// at the bottom rung.
pub const HEALTHY_RESET: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    rung: usize,
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

impl Backoff {
    pub const fn new() -> Self {
        Self { rung: 0 }
    }

    /// The delay to apply before the *next* attempt.
    pub fn current(self) -> Duration {
        Duration::from_secs(RUNGS[self.rung.min(RUNGS.len() - 1)])
    }

    /// Move one rung down, saturating at the cap rather than wrapping or
    /// panicking.
    pub fn advance(&mut self) {
        if self.rung + 1 < RUNGS.len() {
            self.rung += 1;
        }
    }

    pub fn reset(&mut self) {
        self.rung = 0;
    }

    pub fn is_reset(self) -> bool {
        self.rung == 0
    }

    /// 1-based, for the UI: "Reconnecting (attempt 3)".
    pub fn attempt(self) -> u32 {
        self.rung as u32 + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_at_one_second() {
        let b = Backoff::new();
        assert_eq!(b.current(), Duration::from_secs(1));
        assert!(b.is_reset());
        assert_eq!(b.attempt(), 1);
    }

    #[test]
    fn walks_the_specified_ladder() {
        let mut b = Backoff::new();
        let expected = [1u64, 2, 5, 10, 30, 60];
        for (i, secs) in expected.iter().enumerate() {
            assert_eq!(
                b.current(),
                Duration::from_secs(*secs),
                "rung {i} should be {secs}s"
            );
            b.advance();
        }
    }

    #[test]
    fn cap_holds_at_sixty() {
        let mut b = Backoff::new();
        // Far more advances than there are rungs.
        for _ in 0..100 {
            b.advance();
        }
        assert_eq!(b.current(), Duration::from_secs(60));
    }

    #[test]
    fn attempt_number_tracks_the_rung() {
        let mut b = Backoff::new();
        assert_eq!(b.attempt(), 1);
        b.advance();
        assert_eq!(b.attempt(), 2);
        b.advance();
        assert_eq!(b.attempt(), 3);
    }

    #[test]
    fn attempt_number_saturates_with_the_cap() {
        let mut b = Backoff::new();
        for _ in 0..100 {
            b.advance();
        }
        assert_eq!(b.attempt(), RUNGS.len() as u32);
    }

    #[test]
    fn reset_returns_to_the_top() {
        let mut b = Backoff::new();
        b.advance();
        b.advance();
        b.advance();
        assert!(!b.is_reset());
        b.reset();
        assert!(b.is_reset());
        assert_eq!(b.current(), Duration::from_secs(1));
        assert_eq!(b.attempt(), 1);
    }

    #[test]
    fn advance_is_monotonic() {
        let mut b = Backoff::new();
        let mut previous = Duration::ZERO;
        for _ in 0..20 {
            let current = b.current();
            assert!(
                current >= previous,
                "ladder must never go back up on advance"
            );
            previous = current;
            b.advance();
        }
    }
}
