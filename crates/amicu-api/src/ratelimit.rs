//! Minimal in-memory fixed-window rate limiter for auth endpoints.
//!
//! Single-node only; a distributed deployment should back this with a shared
//! store. Keyed by client identity (proxy `X-Forwarded-For`).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Fixed-window counter: at most `max` requests per `window` per key.
pub struct RateLimiter {
    max: u32,
    window: Duration,
    buckets: Mutex<HashMap<String, (Instant, u32)>>,
}

impl RateLimiter {
    /// A limiter allowing `max` requests per `window`.
    #[must_use]
    pub fn new(max: u32, window: Duration) -> Self {
        Self {
            max,
            window,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Record a hit for `key`; returns `true` if it is within the limit.
    pub fn check(&self, key: &str) -> bool {
        let now = Instant::now();
        let mut buckets = self.buckets.lock().expect("rate limiter poisoned");
        let entry = buckets.entry(key.to_string()).or_insert((now, 0));
        if now.duration_since(entry.0) > self.window {
            *entry = (now, 0);
        }
        if entry.1 >= self.max {
            return false;
        }
        entry.1 += 1;
        true
    }
}

impl Default for RateLimiter {
    /// 30 requests per minute.
    fn default() -> Self {
        Self::new(30, Duration::from_secs(60))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_up_to_max_then_blocks() {
        let limiter = RateLimiter::new(3, Duration::from_secs(60));
        assert!(limiter.check("a"));
        assert!(limiter.check("a"));
        assert!(limiter.check("a"));
        assert!(
            !limiter.check("a"),
            "4th request over the window is blocked"
        );
        // A different key has its own budget.
        assert!(limiter.check("b"));
    }
}
