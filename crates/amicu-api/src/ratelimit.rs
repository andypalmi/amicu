//! In-memory fixed-window rate limiter for the auth credential routes.
//!
//! Single-node only; a distributed deployment should back this with a shared
//! store. Two hardening properties matter here (see amicu-planning#94):
//!
//! * **Trustworthy client identity.** `X-Forwarded-For` is client-supplied and
//!   trivially spoofed, so it is only honored when the *peer* socket address is
//!   in a configured trusted-proxy allowlist ([`TrustedProxies`]). Otherwise the
//!   key is derived from the peer address, which an attacker cannot rotate at
//!   will. Keys are additionally scoped per route, so no single bucket can lock
//!   every client out of every credential endpoint.
//! * **Bounded memory.** The bucket map is capacity-limited and self-evicting
//!   (expired windows are swept, then the least-recently-seen bucket is dropped),
//!   so a key-rotating attacker cannot grow the map without bound.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ipnet::IpNet;

/// Default cap on the number of distinct buckets held at once.
const DEFAULT_CAPACITY: usize = 16_384;

/// One fixed window's state for a single key.
struct Bucket {
    /// Start of the current counting window.
    window_start: Instant,
    /// Requests seen in the current window.
    count: u32,
    /// Last time this key was touched, for LRU eviction.
    last_seen: Instant,
}

/// Fixed-window counter: at most `max` requests per `window` per key, with a
/// bounded, self-evicting key map.
pub struct RateLimiter {
    max: u32,
    window: Duration,
    capacity: usize,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl RateLimiter {
    /// A limiter allowing `max` requests per `window`, with the default bucket
    /// capacity.
    #[must_use]
    pub fn new(max: u32, window: Duration) -> Self {
        Self::with_capacity(max, window, DEFAULT_CAPACITY)
    }

    /// A limiter with an explicit bucket `capacity`. Once the map reaches
    /// `capacity`, admitting a new key first sweeps expired windows and, if
    /// still full, evicts the least-recently-seen bucket. `capacity` is clamped
    /// to at least 1.
    #[must_use]
    pub fn with_capacity(max: u32, window: Duration, capacity: usize) -> Self {
        Self {
            max,
            window,
            capacity: capacity.max(1),
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Record a hit for `key`; returns `true` if it is within the limit.
    pub fn check(&self, key: &str) -> bool {
        let now = Instant::now();
        let mut buckets = self.buckets.lock().expect("rate limiter poisoned");

        if let Some(bucket) = buckets.get_mut(key) {
            if now.duration_since(bucket.window_start) > self.window {
                bucket.window_start = now;
                bucket.count = 0;
            }
            bucket.last_seen = now;
            if bucket.count >= self.max {
                return false;
            }
            bucket.count += 1;
            return true;
        }

        // New key: keep the map bounded before inserting.
        if buckets.len() >= self.capacity {
            Self::evict(&mut buckets, now, self.window, self.capacity);
        }
        buckets.insert(
            key.to_string(),
            Bucket {
                window_start: now,
                count: 1,
                last_seen: now,
            },
        );
        // A pathological `max == 0` blocks everything, matching the counter.
        self.max > 0
    }

    /// Number of buckets currently tracked. Primarily for tests/observability.
    #[must_use]
    pub fn tracked(&self) -> usize {
        self.buckets.lock().expect("rate limiter poisoned").len()
    }

    /// Make room for one new key: drop expired windows first, then, while still
    /// at `capacity`, evict the least-recently-seen bucket.
    fn evict(
        buckets: &mut HashMap<String, Bucket>,
        now: Instant,
        window: Duration,
        capacity: usize,
    ) {
        buckets.retain(|_, b| now.duration_since(b.last_seen) <= window);
        while buckets.len() >= capacity {
            let Some(oldest) = buckets
                .iter()
                .min_by_key(|(_, b)| b.last_seen)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            buckets.remove(&oldest);
        }
    }
}

impl Default for RateLimiter {
    /// 30 requests per minute.
    fn default() -> Self {
        Self::new(30, Duration::from_secs(60))
    }
}

/// Configured allowlist of trusted reverse-proxy addresses, plus the logic for
/// deriving a rate-limit key from a request's peer address and headers.
///
/// `X-Forwarded-For` is only believed when the connecting peer is one of these
/// networks; from any other peer the header is ignored and the peer address is
/// used directly. An empty allowlist (the default) trusts no proxy, so every
/// key is derived from the peer address.
#[derive(Clone, Debug, Default)]
pub struct TrustedProxies {
    nets: Arc<[IpNet]>,
}

impl TrustedProxies {
    /// Build from parsed networks.
    #[must_use]
    pub fn new(nets: impl IntoIterator<Item = IpNet>) -> Self {
        Self {
            nets: nets.into_iter().collect(),
        }
    }

    /// Build from string entries, each either a CIDR (`10.0.0.0/8`) or a bare
    /// IP address (`192.0.2.1`, treated as a single host). Returns the first
    /// entry that fails to parse.
    ///
    /// # Errors
    /// Returns [`ProxyParseError`] carrying the offending entry when a string is
    /// neither a valid CIDR nor a bare IP address.
    pub fn from_strs<I, S>(entries: I) -> Result<Self, ProxyParseError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut nets = Vec::new();
        for entry in entries {
            let entry = entry.as_ref().trim();
            if entry.is_empty() {
                continue;
            }
            let net = entry
                .parse::<IpNet>()
                .or_else(|_| entry.parse::<IpAddr>().map(IpNet::from))
                .map_err(|_| ProxyParseError(entry.to_string()))?;
            nets.push(net);
        }
        Ok(Self::new(nets))
    }

    /// True if `ip` belongs to a trusted-proxy network.
    #[must_use]
    pub fn is_trusted(&self, ip: IpAddr) -> bool {
        self.nets.iter().any(|net| net.contains(&ip))
    }

    /// Derive the per-route rate-limit key for a request.
    ///
    /// The key is `"<route>|<client>"` where `<client>` is:
    /// * the left-most valid `X-Forwarded-For` entry, if the peer is a trusted
    ///   proxy and supplied one; otherwise
    /// * the peer IP address; otherwise (no peer address available, i.e. the
    ///   server was not wired with connection info)
    /// * a per-route `"unknown"` sentinel.
    #[must_use]
    pub fn client_key(&self, route: &str, peer: Option<SocketAddr>, xff: Option<&str>) -> String {
        format!("{route}|{}", self.client_ident(peer, xff))
    }

    fn client_ident(&self, peer: Option<SocketAddr>, xff: Option<&str>) -> String {
        match peer {
            Some(peer) => {
                if self.is_trusted(peer.ip())
                    && let Some(client) = xff.and_then(first_forwarded_for)
                {
                    return client.to_string();
                }
                peer.ip().to_string()
            }
            None => "unknown".to_string(),
        }
    }
}

/// Left-most `X-Forwarded-For` entry, if it parses as an IP address. Rejecting
/// unparseable values keeps a trusted proxy from being tricked into minting an
/// unbounded set of junk keys.
fn first_forwarded_for(header: &str) -> Option<IpAddr> {
    header
        .split(',')
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<IpAddr>().ok())
}

/// A trusted-proxy allowlist entry that is neither a valid CIDR nor IP address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyParseError(pub String);

impl std::fmt::Display for ProxyParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid trusted-proxy entry: {}", self.0)
    }
}

impl std::error::Error for ProxyParseError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn sock(ip: &str) -> SocketAddr {
        SocketAddr::new(ip.parse().unwrap(), 12345)
    }

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

    #[test]
    fn window_resets_after_expiry() {
        let limiter = RateLimiter::new(1, Duration::from_millis(20));
        assert!(limiter.check("a"));
        assert!(!limiter.check("a"));
        std::thread::sleep(Duration::from_millis(40));
        assert!(limiter.check("a"), "window elapsed, budget refreshed");
    }

    #[test]
    fn map_is_bounded_and_evicts() {
        let limiter = RateLimiter::with_capacity(5, Duration::from_secs(60), 4);
        for i in 0..50 {
            assert!(limiter.check(&format!("k{i}")));
        }
        assert!(
            limiter.tracked() <= 4,
            "bucket map stays within capacity, got {}",
            limiter.tracked()
        );
    }

    #[test]
    fn expired_buckets_are_swept_on_pressure() {
        let limiter = RateLimiter::with_capacity(5, Duration::from_millis(10), 2);
        assert!(limiter.check("old-1"));
        assert!(limiter.check("old-2"));
        std::thread::sleep(Duration::from_millis(25));
        // Admitting a new key sweeps the two expired windows first.
        assert!(limiter.check("fresh"));
        assert!(limiter.tracked() <= 2);
    }

    #[test]
    fn untrusted_peer_ignores_forwarded_for() {
        // No trusted proxies: XFF is never believed; key follows the peer.
        let tp = TrustedProxies::default();
        let spoof_a = tp.client_key("/signin", Some(sock("203.0.113.9")), Some("1.1.1.1"));
        let spoof_b = tp.client_key("/signin", Some(sock("203.0.113.9")), Some("2.2.2.2"));
        assert_eq!(spoof_a, spoof_b, "rotating XFF must not change the key");
        assert_eq!(spoof_a, "/signin|203.0.113.9");
    }

    #[test]
    fn trusted_peer_honors_forwarded_for() {
        let tp = TrustedProxies::from_strs(["10.0.0.0/8"]).unwrap();
        let key = tp.client_key(
            "/signin",
            Some(sock("10.1.2.3")),
            Some("198.51.100.7, 10.1.2.3"),
        );
        assert_eq!(key, "/signin|198.51.100.7");
        // Bare-IP allowlist entries also work.
        let tp = TrustedProxies::from_strs(["10.1.2.3"]).unwrap();
        let key = tp.client_key("/signin", Some(sock("10.1.2.3")), Some("198.51.100.7"));
        assert_eq!(key, "/signin|198.51.100.7");
    }

    #[test]
    fn trusted_peer_with_junk_forwarded_for_falls_back_to_peer() {
        let tp = TrustedProxies::from_strs(["10.0.0.0/8"]).unwrap();
        let key = tp.client_key("/signin", Some(sock("10.1.2.3")), Some("not-an-ip"));
        assert_eq!(key, "/signin|10.1.2.3");
    }

    #[test]
    fn keys_are_scoped_per_route() {
        let tp = TrustedProxies::default();
        let signin = tp.client_key("/signin", Some(sock("203.0.113.9")), None);
        let refresh = tp.client_key("/token/refresh", Some(sock("203.0.113.9")), None);
        assert_ne!(signin, refresh, "no shared cross-route bucket");
    }

    #[test]
    fn from_strs_rejects_garbage() {
        assert_eq!(
            TrustedProxies::from_strs(["nonsense"]).unwrap_err(),
            ProxyParseError("nonsense".to_string())
        );
    }
}
