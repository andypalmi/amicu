//! In-memory fixed-window rate limiter for the auth credential routes.
//!
//! Single-node only; a distributed deployment should back this with a shared
//! store. Two hardening properties matter here (see amicu-planning#94):
//!
//! * **Trustworthy client identity.** `X-Forwarded-For` is client-supplied and
//!   trivially spoofed, so it is only honored when the *peer* socket address is
//!   in a configured trusted-proxy allowlist ([`TrustedProxies`]), and then the
//!   chain is parsed right-to-left (peeling trusted hops) so an attacker cannot
//!   prepend a spoofed value. Otherwise the key is derived from the peer
//!   address, which an attacker cannot rotate at will. Keys are additionally
//!   scoped per route, so no single bucket can lock every client out of every
//!   credential endpoint.
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
    ///
    /// Eviction discards a bucket's counter, so an attacker able to mint many
    /// distinct identities could, in principle, evict-and-reset their own
    /// at-limit bucket. That requires forging identities, which the peer-address
    /// / right-to-left-XFF keying prevents for any single untrusted source; the
    /// generous default `capacity` keeps eviction off the hot path in practice.
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

    /// Derive the per-route rate-limit key for a request, or `None` when no
    /// trustworthy client identity is available and the request must be let
    /// through rather than share a bucket.
    ///
    /// The key is `"<route>|<client>"`. The client identity is:
    /// * the peer socket IP when the peer is **not** a trusted proxy — its
    ///   `X-Forwarded-For` is not believed at all; otherwise
    /// * the first untrusted address found walking the `X-Forwarded-For` chain
    ///   **right-to-left**, peeling off trusted-proxy hops — i.e. the real
    ///   client as seen by the outermost trusted hop. Parsing right-to-left is
    ///   what makes this unspoofable: with append-style proxies the header is
    ///   `"<attacker-supplied>, <real-client>"`, and only the right-hand,
    ///   proxy-appended addresses can be trusted. Falls back to the peer IP when
    ///   the chain holds only trusted hops or an unparseable entry (so a proxy
    ///   forwarding junk can never mint an unbounded set of keys).
    ///
    /// Returns `None` when there is no peer address — connect-info was not wired
    /// on the serving socket. The caller must fail open on `None`; collapsing
    /// such requests into one shared bucket would be a lockout vector.
    #[must_use]
    pub fn client_key(
        &self,
        route: &str,
        peer: Option<SocketAddr>,
        xff: Option<&str>,
    ) -> Option<String> {
        let ident = self.client_ident(peer?, xff);
        Some(format!("{route}|{ident}"))
    }

    fn client_ident(&self, peer: SocketAddr, xff: Option<&str>) -> String {
        if !self.is_trusted(peer.ip()) {
            return peer.ip().to_string();
        }
        if let Some(client) = xff.and_then(|header| self.real_client_from_chain(header)) {
            return client.to_string();
        }
        peer.ip().to_string()
    }

    /// Walk the `X-Forwarded-For` chain right-to-left, peeling off trusted-proxy
    /// hops, and return the first untrusted address (the real client as seen by
    /// the outermost trusted hop). Returns `None` when the chain is empty,
    /// entirely trusted, or hits an unparseable entry — in every such case the
    /// caller falls back to the peer IP rather than key on attacker-supplied
    /// junk. Called only when the peer itself is a trusted proxy.
    fn real_client_from_chain(&self, header: &str) -> Option<IpAddr> {
        for entry in header.rsplit(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            // A non-IP entry is where a trusted proxy's clean chain ends; stop
            // and fall back to the peer instead of keying on the raw value.
            let ip = entry.parse::<IpAddr>().ok()?;
            if self.is_trusted(ip) {
                continue; // peel a trusted proxy hop and keep walking left
            }
            return Some(ip); // first untrusted from the right = real client
        }
        None
    }
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
        assert_eq!(spoof_a.as_deref(), Some("/signin|203.0.113.9"));
    }

    #[test]
    fn trusted_peer_uses_rightmost_untrusted_as_client() {
        let tp = TrustedProxies::from_strs(["10.0.0.0/8"]).unwrap();
        // Chain is `client, proxy`; the proxy hop (10.1.2.3) is peeled from the
        // right, leaving the real client.
        let key = tp.client_key(
            "/signin",
            Some(sock("10.1.2.3")),
            Some("198.51.100.7, 10.1.2.3"),
        );
        assert_eq!(key.as_deref(), Some("/signin|198.51.100.7"));
        // Bare-IP allowlist entries also work.
        let tp = TrustedProxies::from_strs(["10.1.2.3"]).unwrap();
        let key = tp.client_key("/signin", Some(sock("10.1.2.3")), Some("198.51.100.7"));
        assert_eq!(key.as_deref(), Some("/signin|198.51.100.7"));
    }

    #[test]
    fn append_style_spoof_is_ignored() {
        // Append-style proxy (e.g. nginx $proxy_add_x_forwarded_for): the header
        // is `"<attacker-supplied>, <real-client>"`. Right-to-left parsing keys
        // on the proxy-appended real client, never the attacker's left value, so
        // rotating the spoofed prefix cannot mint a fresh bucket per request.
        let tp = TrustedProxies::from_strs(["10.0.0.0/8"]).unwrap();
        let proxy = Some(sock("10.9.9.9"));
        let a = tp.client_key("/signin", proxy, Some("1.1.1.1, 203.0.113.7"));
        let b = tp.client_key("/signin", proxy, Some("2.2.2.2, 203.0.113.7"));
        assert_eq!(a, b, "spoofed prefix must not change the key");
        assert_eq!(a.as_deref(), Some("/signin|203.0.113.7"));
    }

    #[test]
    fn multiple_trusted_hops_are_peeled() {
        // client, then two trusted proxy hops in the chain, peer is a third.
        let tp = TrustedProxies::from_strs(["10.0.0.0/8"]).unwrap();
        let key = tp.client_key(
            "/signin",
            Some(sock("10.0.0.1")),
            Some("203.0.113.5, 10.0.0.2, 10.0.0.3"),
        );
        assert_eq!(key.as_deref(), Some("/signin|203.0.113.5"));
    }

    #[test]
    fn trusted_peer_with_junk_forwarded_for_falls_back_to_peer() {
        let tp = TrustedProxies::from_strs(["10.0.0.0/8"]).unwrap();
        let key = tp.client_key("/signin", Some(sock("10.1.2.3")), Some("not-an-ip"));
        assert_eq!(key.as_deref(), Some("/signin|10.1.2.3"));
    }

    #[test]
    fn all_trusted_chain_falls_back_to_peer() {
        let tp = TrustedProxies::from_strs(["10.0.0.0/8"]).unwrap();
        let key = tp.client_key(
            "/signin",
            Some(sock("10.0.0.1")),
            Some("10.0.0.2, 10.0.0.3"),
        );
        assert_eq!(key.as_deref(), Some("/signin|10.0.0.1"));
    }

    #[test]
    fn missing_peer_yields_no_key() {
        // No connect-info: no trustworthy identity, so the caller fails open
        // instead of sharing one bucket.
        let tp = TrustedProxies::default();
        assert_eq!(tp.client_key("/signin", None, Some("1.1.1.1")), None);
        assert_eq!(tp.client_key("/signin", None, None), None);
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
