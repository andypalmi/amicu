//! Rate limiting for the auth credential routes: a [`RateLimiter`] trait plus
//! two implementations, selected by `amicu-server`'s bootstrap
//! (andypalmi/amicu-planning#142).
//!
//! * [`InMemoryRateLimiter`] - the original process-local fixed-window
//!   counter. Single-node only: across N Cloud Run replicas each one grants
//!   its own full quota (an effective Nx limit), and every counter resets on
//!   every cold start. Kept for dev/tests and any DB-less deployment.
//! * [`PostgresRateLimiter`] - a fixed-window counter shared by every replica
//!   over the one Postgres pool each replica already holds
//!   (andypalmi/amicu-planning#139), backed by
//!   [`amicu_store::PostgresRateLimitStore`]'s single-round-trip atomic
//!   upsert. This is the one `AMICU_ENV=production` requires (consistent with
//!   the fail-fast posture andypalmi/amicu-planning#127 established for a
//!   missing `DATABASE_URL`) - see `amicu-server`'s `Config` docs. There is no
//!   Redis in this stack; Postgres is the only shared store available.
//!
//! # Fail-open on database error
//!
//! [`PostgresRateLimiter::check`] fails OPEN on a database error (allows the
//! request through, logs a `WARNING`, and bumps an in-process counter -
//! [`PostgresRateLimiter::fail_open_count`] - so an operator can alert on
//! sustained failures): a transient Postgres hiccup must not turn into a
//! total login/signup outage. This is a deliberate availability-over-strict-
//! limiting trade-off for exactly the credential routes this backs - see the
//! andypalmi/amicu-planning#142 PR description for the full rationale.
//!
//! Two hardening properties matter for both implementations (see
//! amicu-planning#94, extended by #142 for the Cloudflare edge case - see
//! [`crate::edge`]):
//!
//! * **Trustworthy client identity.** `X-Forwarded-For` is client-supplied and
//!   trivially spoofed, so it is only honored when the *peer* socket address is
//!   in a configured trusted-proxy allowlist ([`TrustedProxies`]), and then the
//!   chain is parsed right-to-left (peeling trusted hops) so an attacker cannot
//!   prepend a spoofed value. Otherwise the key is derived from the peer
//!   address, which an attacker cannot rotate at will. On Cloud Run behind
//!   Cloudflare, [`TrustedProxies::client_key_with_edge`] prefers
//!   `CF-Connecting-IP` instead, but ONLY once the caller has verified the
//!   `X-Amicu-Edge-Token` header (see [`crate::edge::EdgeTrust`]) - Google's LB
//!   peer address and `X-Forwarded-For` chain are not the real client on that
//!   topology (see the `edge` module docs). Keys are additionally scoped per
//!   route, so no single bucket can lock every client out of every credential
//!   endpoint.
//! * **Bounded memory** (in-memory impl only). The bucket map is
//!   capacity-limited and self-evicting (expired windows are swept, then the
//!   least-recently-seen bucket is dropped), so a key-rotating attacker cannot
//!   grow the map without bound. The Postgres impl has no such concern in
//!   process memory; its table is swept opportunistically instead (see
//!   [`amicu_store::ratelimit`]'s docs).

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
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
pub struct InMemoryRateLimiter {
    max: u32,
    window: Duration,
    capacity: usize,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl InMemoryRateLimiter {
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

    /// True if `key` has already used `max` hits in its live window (no hit
    /// is recorded).
    pub fn exhausted(&self, key: &str) -> bool {
        let now = Instant::now();
        let buckets = self.buckets.lock().expect("rate limiter poisoned");
        buckets.get(key).is_some_and(|b| {
            now.duration_since(b.window_start) <= self.window && b.count >= self.max
        })
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

impl Default for InMemoryRateLimiter {
    /// 30 requests per minute.
    fn default() -> Self {
        Self::new(30, Duration::from_secs(60))
    }
}

/// A rate-limit backend: record a hit for `key`, returning whether it is
/// within the configured limit. Implemented by [`InMemoryRateLimiter`]
/// (process-local, dev/tests/DB-less) and [`PostgresRateLimiter`] (shared
/// across every Cloud Run replica) - see the module docs for how
/// `amicu-server` selects between them.
///
/// `async` (via `#[async_trait]`, already a workspace dependency) even though
/// [`InMemoryRateLimiter`]'s own implementation never awaits: the trait must
/// accommodate [`PostgresRateLimiter`], which does a real database round
/// trip, and `amicu-api` has no `sqlx` dependency of its own (only
/// `amicu-store` talks to Postgres directly - see that crate's docs), so this
/// trait is the seam between them.
#[async_trait]
pub trait RateLimiter: Send + Sync {
    /// Record a hit for `key`; returns `true` if it is within the limit.
    async fn check(&self, key: &str) -> bool;

    /// True when `key`'s budget for the current window is already used up,
    /// WITHOUT counting a hit. Backends that cannot peek (and any backend
    /// error or timeout) report `false`: this is a fail-open advisory read.
    async fn is_exhausted(&self, _key: &str) -> bool {
        false
    }
}

#[async_trait]
impl RateLimiter for InMemoryRateLimiter {
    async fn check(&self, key: &str) -> bool {
        self.check(key)
    }

    async fn is_exhausted(&self, key: &str) -> bool {
        self.exhausted(key)
    }
}

/// Default per-`check` database budget, overridable via [`PostgresRateLimiter::with_timeout`]
/// (`amicu-server`'s `AMICU_RATE_LIMIT_TIMEOUT_MS`). Independent of the pool's
/// own (typically much longer) connection-acquire timeout
/// (`AMICU_DB_ACQUIRE_TIMEOUT_SECS`) - mirrors the dedicated-budget pattern
/// `AMICU_READYZ_TIMEOUT_MS` established for `GET /readyz`
/// (`crates/amicu-api/src/health.rs`): a saturated pool should fail OPEN fast,
/// not block a login/signup request up to the full acquire timeout.
const DEFAULT_TIMEOUT: Duration = Duration::from_millis(1_000);

/// Log every Nth fail-open (after the first) instead of every single one, so
/// a sustained outage doesn't flood stderr with one line per request -
/// mirrors [`amicu_store::PostgresRateLimitStore`]'s `cleanup_every` sampling
/// pattern. [`PostgresRateLimiter::fail_open_count`] stays exact regardless -
/// only the log line is sampled.
const WARN_SAMPLE_EVERY: u64 = 100;

/// Shared fixed-window counter backed by
/// [`amicu_store::PostgresRateLimitStore`] (andypalmi/amicu-planning#142) -
/// see the module docs for the production-selection and fail-open rules.
pub struct PostgresRateLimiter {
    store: amicu_store::PostgresRateLimitStore,
    max: u32,
    window_secs: i64,
    /// Upper bound `check` waits for the Postgres hit before failing OPEN;
    /// see [`DEFAULT_TIMEOUT`] and [`Self::with_timeout`].
    timeout: Duration,
    /// Count of `check` calls that failed open due to a database error or
    /// timeout. Exposed via [`Self::fail_open_count`] as the "metric" the
    /// module docs' fail-open section promises - this crate has no
    /// metrics/telemetry sink wired to a rate limiter today, so an operator
    /// watching logs (a sampled `WARNING` line - see [`WARN_SAMPLE_EVERY`])
    /// or polling this counter through their own instrumentation are the two
    /// ways to notice sustained failures. Always incremented exactly once per
    /// fail-open, independent of log sampling.
    fail_open_hits: AtomicU64,
}

impl PostgresRateLimiter {
    /// A limiter allowing `max` requests per `window`, sharing state with
    /// every other replica through `store`'s pool. Uses [`DEFAULT_TIMEOUT`]
    /// for the per-`check` database budget; override with
    /// [`Self::with_timeout`].
    #[must_use]
    pub fn new(store: amicu_store::PostgresRateLimitStore, max: u32, window: Duration) -> Self {
        Self {
            store,
            max,
            window_secs: i64::try_from(window.as_secs()).unwrap_or(i64::MAX).max(1),
            timeout: DEFAULT_TIMEOUT,
            fail_open_hits: AtomicU64::new(0),
        }
    }

    /// Overrides the per-`check` database timeout (default [`DEFAULT_TIMEOUT`],
    /// `AMICU_RATE_LIMIT_TIMEOUT_MS` in `amicu-server`). A `check` that
    /// doesn't hear back from Postgres within `timeout` fails OPEN exactly
    /// like a database error - see the module docs' "Fail-open on database
    /// error" section.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Number of `check` calls that fell back to fail-open because the
    /// database hit errored or timed out. Monotonic for the lifetime of this
    /// instance; primarily for tests/observability.
    #[must_use]
    pub fn fail_open_count(&self) -> u64 {
        self.fail_open_hits.load(Ordering::Relaxed)
    }

    /// Records one fail-open (bumping the exact counter) and logs a sampled
    /// `WARNING` - the first failure, then every [`WARN_SAMPLE_EVERY`]th
    /// after that, so a sustained outage doesn't flood stderr with one line
    /// per request.
    fn note_fail_open(&self, detail: impl std::fmt::Display) {
        let n = self.fail_open_hits.fetch_add(1, Ordering::Relaxed) + 1;
        if n == 1 || n.is_multiple_of(WARN_SAMPLE_EVERY) {
            eprintln!(
                "amicu-api: WARNING - Postgres rate limiter check failed for key \
                 (key content withheld from this log line; keys embed the request path and \
                 may embed a client IP or email), failing OPEN (request allowed): {detail}. \
                 cumulative fail-open count for this limiter: {n} (this line is sampled: \
                 logged on the 1st failure and then every {WARN_SAMPLE_EVERY}th thereafter).",
            );
        }
    }
}

#[async_trait]
impl RateLimiter for PostgresRateLimiter {
    async fn check(&self, key: &str) -> bool {
        match tokio::time::timeout(self.timeout, self.store.hit(key, self.window_secs)).await {
            Ok(Ok(count)) => count <= i64::from(self.max),
            // Fail OPEN: a transient database error must not turn into a
            // total login/signup outage for the credential routes this backs
            // (see the module docs' "Fail-open on database error" section for
            // the full rationale). Never fail closed here.
            Ok(Err(err)) => {
                self.note_fail_open(err);
                true
            }
            // The hit didn't finish within `self.timeout` - a saturated pool
            // or a slow/hung database must not block (or eventually lock out)
            // every credential request either; a timeout is just another
            // shape of database unavailability, so it fails OPEN the same way.
            Err(_) => {
                self.note_fail_open(format_args!(
                    "database hit did not complete within the configured {:?} rate-limit \
                     timeout (AMICU_RATE_LIMIT_TIMEOUT_MS)",
                    self.timeout
                ));
                true
            }
        }
    }

    async fn is_exhausted(&self, key: &str) -> bool {
        match tokio::time::timeout(self.timeout, self.store.peek(key)).await {
            Ok(Ok(count)) => count >= i64::from(self.max),
            // Fail open, like `check`.
            Ok(Err(err)) => {
                self.note_fail_open(err);
                false
            }
            Err(_) => {
                self.note_fail_open("peek timed out");
                false
            }
        }
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

    /// True if any entry is a catch-all (`0.0.0.0/0` or `::/0`), i.e. every
    /// peer is "trusted" and `X-Forwarded-For` is believed from anyone.
    #[must_use]
    pub fn has_catch_all(&self) -> bool {
        self.nets.iter().any(|n| n.prefix_len() == 0)
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
    /// * the peer socket IP when the peer is **not** a trusted proxy - its
    ///   `X-Forwarded-For` is not believed at all; otherwise
    /// * the first untrusted address found walking the `X-Forwarded-For` chain
    ///   **right-to-left**, peeling off trusted-proxy hops - i.e. the real
    ///   client as seen by the outermost trusted hop. Parsing right-to-left is
    ///   what makes this unspoofable: with append-style proxies the header is
    ///   `"<attacker-supplied>, <real-client>"`, and only the right-hand,
    ///   proxy-appended addresses can be trusted. Falls back to the peer IP when
    ///   the chain holds only trusted hops or an unparseable entry (so a proxy
    ///   forwarding junk can never mint an unbounded set of keys).
    ///
    /// Returns `None` when there is no peer address - connect-info was not wired
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

    /// Like [`Self::client_key`], but prefers `edge_ip` - the
    /// already-verified `CF-Connecting-IP` client (see
    /// [`crate::edge::EdgeTrust::trusted_client_ip`]) - as the client identity
    /// when present, instead of the peer/`X-Forwarded-For` path.
    ///
    /// This function does no header parsing or trust decision itself: `Some`
    /// is only ever passed once the caller has independently verified
    /// `X-Amicu-Edge-Token`, so it is safe to trust unconditionally here.
    /// Falls back to [`Self::client_key`] (and its `None`-on-no-peer
    /// fail-open behavior) when `edge_ip` is `None` - the non-Cloudflare or
    /// no-valid-token path, unchanged from before andypalmi/amicu-planning#142.
    #[must_use]
    pub fn client_key_with_edge(
        &self,
        route: &str,
        edge_ip: Option<IpAddr>,
        peer: Option<SocketAddr>,
        xff: Option<&str>,
    ) -> Option<String> {
        let ident = self.client_ident_with_edge(edge_ip, peer, xff)?;
        Some(format!("{route}|{ident}"))
    }

    /// The client identity (textual IP) [`Self::client_key_with_edge`] keys on,
    /// without the route prefix; `None` when there is no trustworthy identity.
    #[must_use]
    pub fn client_ident_with_edge(
        &self,
        edge_ip: Option<IpAddr>,
        peer: Option<SocketAddr>,
        xff: Option<&str>,
    ) -> Option<String> {
        if let Some(ip) = edge_ip {
            return Some(ip.to_string());
        }
        Some(self.client_ident(peer?, xff))
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
    /// entirely trusted, or hits an unparseable entry - in every such case the
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
        let limiter = InMemoryRateLimiter::new(3, Duration::from_secs(60));
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
        let limiter = InMemoryRateLimiter::new(1, Duration::from_millis(20));
        assert!(limiter.check("a"));
        assert!(!limiter.check("a"));
        std::thread::sleep(Duration::from_millis(40));
        assert!(limiter.check("a"), "window elapsed, budget refreshed");
    }

    #[test]
    fn map_is_bounded_and_evicts() {
        let limiter = InMemoryRateLimiter::with_capacity(5, Duration::from_secs(60), 4);
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
        let limiter = InMemoryRateLimiter::with_capacity(5, Duration::from_millis(10), 2);
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

    /// [`RateLimiter::check`] (the async trait) agrees with the concrete
    /// type's own sync `check` for [`InMemoryRateLimiter`] - proves the
    /// trait-object path (`Arc<dyn RateLimiter>`, what `AuthApiState` actually
    /// holds) behaves identically to calling the inherent method directly.
    #[tokio::test]
    async fn in_memory_limiter_trait_object_matches_inherent_method() {
        let limiter: Arc<dyn RateLimiter> =
            Arc::new(InMemoryRateLimiter::new(2, Duration::from_secs(60)));
        assert!(limiter.check("a").await);
        assert!(limiter.check("a").await);
        assert!(!limiter.check("a").await, "3rd request over budget");
    }

    #[test]
    fn client_key_with_edge_prefers_edge_ip_over_peer_and_xff() {
        let tp = TrustedProxies::default();
        // No trusted proxies configured at all, and a peer/XFF that would
        // normally key on the peer address - edge_ip still wins.
        let key = tp.client_key_with_edge(
            "/signin",
            Some("203.0.113.7".parse().unwrap()),
            Some(sock("10.0.0.1")),
            Some("1.1.1.1"),
        );
        assert_eq!(key.as_deref(), Some("/signin|203.0.113.7"));
    }

    #[test]
    fn client_key_with_edge_falls_back_when_no_edge_ip() {
        let tp = TrustedProxies::default();
        let key = tp.client_key_with_edge("/signin", None, Some(sock("203.0.113.9")), None);
        assert_eq!(key.as_deref(), Some("/signin|203.0.113.9"));
    }

    #[test]
    fn client_key_with_edge_scopes_per_route_like_client_key() {
        let tp = TrustedProxies::default();
        let ip = Some("203.0.113.7".parse().unwrap());
        let signin = tp.client_key_with_edge("/signin", ip, None, None);
        let refresh = tp.client_key_with_edge("/token/refresh", ip, None, None);
        assert_ne!(signin, refresh);
    }
}
