//! How long to wait before restarting the helper.

use std::time::Duration;

/// Delays between restarts. The first restart waits `initial`, and each failure in a
/// row doubles the wait, up to `max`. A helper that ran for at least `reset_after`
/// before exiting counts as healthy, so the next wait is `initial` again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    pub initial: Duration,
    pub max: Duration,
    pub reset_after: Duration,
}

impl Default for Backoff {
    fn default() -> Backoff {
        Backoff {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(30),
            reset_after: Duration::from_secs(60),
        }
    }
}

impl Backoff {
    /// The attempt number after one more failure. `ran_for` is how long the helper ran,
    /// or `None` when it failed to start.
    pub(crate) fn next_attempt(&self, attempt: u32, ran_for: Option<Duration>) -> u32 {
        match ran_for {
            Some(ran_for) if ran_for >= self.reset_after => 1,
            _ => attempt.saturating_add(1),
        }
    }

    /// The wait before restart `attempt`, counting from 1.
    pub(crate) fn delay(&self, attempt: u32) -> Duration {
        let factor = 1u32
            .checked_shl(attempt.saturating_sub(1))
            .unwrap_or(u32::MAX);
        self.initial
            .checked_mul(factor)
            .unwrap_or(self.max)
            .min(self.max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backoff() -> Backoff {
        Backoff {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(30),
            reset_after: Duration::from_secs(60),
        }
    }

    #[test]
    fn delay_doubles_up_to_the_cap() {
        let delays: Vec<u64> = (1..=8).map(|a| backoff().delay(a).as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 30, 30, 30]);
        assert_eq!(backoff().delay(40), Duration::from_secs(30));
        assert_eq!(backoff().delay(u32::MAX), Duration::from_secs(30));
    }

    #[test]
    fn attempts_count_failures_in_a_row() {
        let b = backoff();
        let short = Some(Duration::from_secs(5));
        assert_eq!(b.next_attempt(0, short), 1);
        assert_eq!(b.next_attempt(1, short), 2);
        assert_eq!(b.next_attempt(2, None), 3);
        assert_eq!(b.next_attempt(u32::MAX, None), u32::MAX);
    }

    #[test]
    fn a_long_healthy_run_resets_the_delay() {
        let b = backoff();
        assert_eq!(b.next_attempt(5, Some(Duration::from_secs(60))), 1);
        assert_eq!(b.next_attempt(5, Some(Duration::from_secs(3600))), 1);
        assert_eq!(
            b.delay(b.next_attempt(5, Some(Duration::from_secs(61)))),
            b.initial
        );
    }
}
