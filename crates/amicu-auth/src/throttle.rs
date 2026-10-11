//! Per-account failed-sign-in backoff (andypalmi/amicu-planning#194).
//!
//! The throttle counts every sign-in ATTEMPT per key (the pseudonym of the
//! normalized email - tracked identically whether or not the account exists,
//! so backoff reveals nothing about which accounts exist). Counting at
//! attempt start rather than on failure closes the "N parallel guesses before
//! any failure is recorded" race; a successful sign-in [`LoginThrottle::clear`]s
//! the key.
//!
//! After [`FREE_ATTEMPTS`] attempts, each further attempt locks the key for
//! `min(MAX_BACKOFF_SECS, 2^(n - FREE_ATTEMPTS))` seconds (2s, 4s, 8s, ... 15
//! min). While locked, attempts are refused WITHOUT incrementing (so hammering
//! cannot extend the lock). A key idle for [`RESET_AFTER_SECS`] starts over.
//!
//! Implementations: [`InMemoryLoginThrottle`] (process-local; dev/tests) and
//! `amicu_store::PostgresLoginThrottle` (shared by every replica).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::error::Result;

/// Attempts allowed before any lock applies.
pub const FREE_ATTEMPTS: u32 = 3;
/// Upper bound of a single lock, seconds.
pub const MAX_BACKOFF_SECS: u64 = 900;
/// Idle time after which a key's counter starts over, seconds.
pub const RESET_AFTER_SECS: u64 = 3600;

/// Lock applied after the `attempts`-th attempt, seconds.
#[must_use]
pub fn backoff_secs(attempts: u32) -> u64 {
    if attempts <= FREE_ATTEMPTS {
        0
    } else {
        MAX_BACKOFF_SECS.min(1u64 << (attempts - FREE_ATTEMPTS).min(20))
    }
}

/// Result of [`LoginThrottle::begin_attempt`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// The attempt may proceed (and has been counted).
    Allowed,
    /// The key is locked; refuse generically.
    Locked {
        /// Seconds until the lock lapses.
        retry_after_secs: u64,
    },
}

/// Backing store for the sign-in backoff counters.
#[async_trait]
pub trait LoginThrottle: Send + Sync {
    /// Atomically check the lock and, if not locked, count this attempt.
    async fn begin_attempt(&self, key: &str) -> Result<Admission>;
    /// Forget `key` (successful sign-in).
    async fn clear(&self, key: &str) -> Result<()>;
}

type NowFn = Arc<dyn Fn() -> i64 + Send + Sync>;

struct Entry {
    attempts: u32,
    locked_until: i64,
    updated_at: i64,
}

/// Process-local [`LoginThrottle`]. NOT multi-replica safe.
pub struct InMemoryLoginThrottle {
    entries: Mutex<HashMap<String, Entry>>,
    now: NowFn,
    capacity: usize,
}

impl Default for InMemoryLoginThrottle {
    fn default() -> Self {
        Self::with_clock(Arc::new(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        }))
    }
}

impl InMemoryLoginThrottle {
    /// A throttle reading time (unix seconds) from `now` - for tests.
    #[must_use]
    pub fn with_clock(now: NowFn) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            now,
            capacity: 16_384,
        }
    }
}

#[async_trait]
impl LoginThrottle for InMemoryLoginThrottle {
    async fn begin_attempt(&self, key: &str) -> Result<Admission> {
        let now = (self.now)();
        let mut map = self.entries.lock().expect("throttle poisoned");
        if map.len() >= self.capacity && !map.contains_key(key) {
            let reset = i64::try_from(RESET_AFTER_SECS).unwrap_or(i64::MAX);
            map.retain(|_, e| e.updated_at + reset > now);
            if map.len() >= self.capacity
                && let Some(oldest) = map
                    .iter()
                    .min_by_key(|(_, e)| e.updated_at)
                    .map(|(k, _)| k.clone())
            {
                map.remove(&oldest);
            }
        }
        let e = map.entry(key.to_owned()).or_insert(Entry {
            attempts: 0,
            locked_until: 0,
            updated_at: now,
        });
        if e.locked_until > now {
            return Ok(Admission::Locked {
                retry_after_secs: u64::try_from(e.locked_until - now).unwrap_or(0),
            });
        }
        let reset = i64::try_from(RESET_AFTER_SECS).unwrap_or(i64::MAX);
        if e.updated_at + reset <= now {
            e.attempts = 0;
        }
        e.attempts = e.attempts.saturating_add(1);
        e.updated_at = now;
        e.locked_until = now + i64::try_from(backoff_secs(e.attempts)).unwrap_or(0);
        Ok(Admission::Allowed)
    }

    async fn clear(&self, key: &str) -> Result<()> {
        self.entries.lock().expect("throttle poisoned").remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicI64, Ordering};

    use super::*;

    #[test]
    fn backoff_schedule() {
        assert_eq!(backoff_secs(1), 0);
        assert_eq!(backoff_secs(3), 0);
        assert_eq!(backoff_secs(4), 2);
        assert_eq!(backoff_secs(5), 4);
        assert_eq!(backoff_secs(6), 8);
        assert_eq!(backoff_secs(40), MAX_BACKOFF_SECS);
    }

    #[tokio::test]
    async fn locks_then_expires_then_clears_and_is_per_key() {
        let t = Arc::new(AtomicI64::new(1_000));
        let t2 = t.clone();
        let th = InMemoryLoginThrottle::with_clock(Arc::new(move || t2.load(Ordering::SeqCst)));
        for _ in 0..3 {
            assert_eq!(th.begin_attempt("a").await.unwrap(), Admission::Allowed);
        }
        // 4th attempt is admitted but arms a 2s lock.
        assert_eq!(th.begin_attempt("a").await.unwrap(), Admission::Allowed);
        assert_eq!(
            th.begin_attempt("a").await.unwrap(),
            Admission::Locked {
                retry_after_secs: 2
            }
        );
        // Other keys are unaffected.
        assert_eq!(th.begin_attempt("b").await.unwrap(), Admission::Allowed);
        // Hammering while locked does not extend the lock.
        t.fetch_add(2, Ordering::SeqCst);
        assert_eq!(th.begin_attempt("a").await.unwrap(), Admission::Allowed);
        // 5th attempt armed a 4s lock.
        assert_eq!(
            th.begin_attempt("a").await.unwrap(),
            Admission::Locked {
                retry_after_secs: 4
            }
        );
        th.clear("a").await.unwrap();
        assert_eq!(th.begin_attempt("a").await.unwrap(), Admission::Allowed);
    }

    #[tokio::test]
    async fn idle_key_starts_over() {
        let t = Arc::new(AtomicI64::new(0));
        let t2 = t.clone();
        let th = InMemoryLoginThrottle::with_clock(Arc::new(move || t2.load(Ordering::SeqCst)));
        for _ in 0..4 {
            th.begin_attempt("a").await.unwrap();
        }
        t.store(10_000, Ordering::SeqCst);
        for _ in 0..3 {
            assert_eq!(th.begin_attempt("a").await.unwrap(), Admission::Allowed);
        }
    }
}
