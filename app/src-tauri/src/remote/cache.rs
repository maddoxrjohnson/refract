//! What was last fetched from a source, kept about 15 minutes. A failed fetch keeps
//! the last good data and reports the error with it.

use std::time::{Duration, Instant, SystemTime};

/// How long a good fetch is used before asking again.
pub const TTL: Duration = Duration::from_secs(15 * 60);
/// How long a failed fetch, or one with no credentials, waits before asking again,
/// unless refreshed.
pub const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(60);

/// One cached source: a list, or one repo's details.
#[derive(Debug, Clone)]
pub struct Slot<T> {
    good: Option<(T, SystemTime)>,
    error: Option<String>,
    attempted: Option<Instant>,
    /// The last attempt found no credentials.
    unconfigured: bool,
}

impl<T> Default for Slot<T> {
    fn default() -> Self {
        Slot {
            good: None,
            error: None,
            attempted: None,
            unconfigured: false,
        }
    }
}

impl<T> Slot<T> {
    /// Whether to fetch again: when asked to, never fetched, or the last attempt is
    /// older than [`TTL`] ([`RETRY_AFTER_FAILURE`] if it failed or found no
    /// credentials). An attempt at or after `now`, made while this caller waited for
    /// the slot, is fresh even when refreshing.
    pub fn needs_fetch(&self, now: Instant, refresh: bool) -> bool {
        let Some(at) = self.attempted else {
            return true;
        };
        if at >= now {
            return false;
        }
        let max_age = if self.error.is_some() || self.unconfigured {
            RETRY_AFTER_FAILURE
        } else {
            TTL
        };
        refresh || now.saturating_duration_since(at) >= max_age
    }

    /// Keeps a fetch's result. An error leaves the last good value in place.
    pub fn record(&mut self, result: Result<T, String>, now: Instant, wall: SystemTime) {
        self.attempted = Some(now);
        self.unconfigured = false;
        match result {
            Ok(value) => {
                self.good = Some((value, wall));
                self.error = None;
            }
            Err(err) => self.error = Some(err),
        }
    }

    /// The last good value and when it was fetched.
    pub fn good(&self) -> Option<(&T, SystemTime)> {
        self.good.as_ref().map(|(value, at)| (value, *at))
    }

    /// Why the last fetch failed, if it did.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

/// Brings `slot` up to date if it [needs it](Slot::needs_fetch): `token` finds the
/// credentials, then `fetch` uses them. Returns false when there are none, and then
/// empties the slot but for when it looked, so they are not looked for again
/// within [`RETRY_AFTER_FAILURE`]; a failure to read them is kept as the slot's
/// error.
pub fn update<T>(
    slot: &mut Slot<T>,
    refresh: bool,
    now: Instant,
    wall: SystemTime,
    token: impl FnOnce() -> Result<Option<String>, String>,
    fetch: impl FnOnce(&str) -> Result<T, String>,
) -> bool {
    if !slot.needs_fetch(now, refresh) {
        return !slot.unconfigured;
    }
    match token() {
        Ok(Some(token)) => slot.record(fetch(&token), now, wall),
        Ok(None) => {
            *slot = Slot {
                attempted: Some(now),
                unconfigured: true,
                ..Slot::default()
            };
            return false;
        }
        Err(err) => slot.record(Err(err), now, wall),
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token() -> Result<Option<String>, String> {
        Ok(Some("t".into()))
    }

    #[test]
    fn good_data_is_kept_for_the_ttl() {
        let start = Instant::now();
        let wall = SystemTime::UNIX_EPOCH;
        let mut slot = Slot::default();
        assert!(slot.needs_fetch(start, false));
        assert!(update(&mut slot, false, start, wall, token, |_| Ok(1)));
        assert_eq!(slot.good(), Some((&1, wall)));

        let mut fetched = false;
        let later = start + TTL - Duration::from_secs(1);
        update(&mut slot, false, later, wall, token, |_| {
            fetched = true;
            Ok(2)
        });
        assert!(!fetched);
        assert!(slot.needs_fetch(later, true));
        assert!(slot.needs_fetch(start + TTL, false));
    }

    #[test]
    fn failures_keep_the_last_good_data() {
        let start = Instant::now();
        let wall = SystemTime::UNIX_EPOCH;
        let mut slot = Slot::default();
        update(&mut slot, false, start, wall, token, |_| Ok(1));
        let later_wall = wall + Duration::from_secs(5);
        let failed = start + Duration::from_secs(1);
        update(&mut slot, true, failed, later_wall, token, |_| {
            Err::<i32, _>("down".into())
        });
        assert_eq!(slot.good(), Some((&1, wall)));
        assert_eq!(slot.error(), Some("down"));
        // A failure is retried sooner than good data is.
        assert!(!slot.needs_fetch(failed + RETRY_AFTER_FAILURE / 2, false));
        assert!(slot.needs_fetch(failed + RETRY_AFTER_FAILURE, false));

        let again = failed + Duration::from_secs(1);
        update(&mut slot, true, again, later_wall, token, |_| Ok(3));
        assert_eq!(slot.good(), Some((&3, later_wall)));
        assert_eq!(slot.error(), None);
    }

    #[test]
    fn missing_credentials_empty_the_slot() {
        let start = Instant::now();
        let wall = SystemTime::UNIX_EPOCH;
        let mut slot = Slot::default();
        update(&mut slot, false, start, wall, token, |_| Ok(1));
        let start = start + Duration::from_secs(1);
        let configured = update(
            &mut slot,
            true,
            start,
            wall,
            || Ok(None),
            |_| -> Result<i32, String> { panic!("no token, no fetch") },
        );
        assert!(!configured);
        assert_eq!(slot.good(), None);
        // Not looked for again until the retry time, or a refresh.
        let soon = start + RETRY_AFTER_FAILURE / 2;
        assert!(!slot.needs_fetch(soon, false));
        let configured = update(
            &mut slot,
            false,
            soon,
            wall,
            || panic!("not looked for again yet"),
            |_| -> Result<i32, String> { panic!("no token, no fetch") },
        );
        assert!(!configured);
        assert!(slot.needs_fetch(soon, true));
        assert!(slot.needs_fetch(start + RETRY_AFTER_FAILURE, false));
        assert!(update(
            &mut slot,
            false,
            start + RETRY_AFTER_FAILURE,
            wall,
            token,
            |_| Ok(2)
        ));
        assert_eq!(slot.good(), Some((&2, wall)));
    }

    #[test]
    fn a_fetch_made_while_waiting_is_fresh() {
        let start = Instant::now();
        let wall = SystemTime::UNIX_EPOCH;
        let mut slot = Slot::default();
        // Another caller fetched at `later` while this one, which asked at `start`,
        // waited for the slot.
        let later = start + Duration::from_secs(1);
        update(&mut slot, false, later, wall, token, |_| Ok(1));
        assert!(!slot.needs_fetch(start, true));
        assert!(slot.needs_fetch(later + Duration::from_millis(1), true));
    }

    #[test]
    fn credential_errors_are_reported() {
        let start = Instant::now();
        let mut slot = Slot::<i32>::default();
        let configured = update(
            &mut slot,
            false,
            start,
            SystemTime::UNIX_EPOCH,
            || Err("locked".into()),
            |_| panic!("no token, no fetch"),
        );
        assert!(configured);
        assert_eq!(slot.error(), Some("locked"));
        assert_eq!(slot.good(), None);
    }
}
