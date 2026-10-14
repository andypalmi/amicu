-- Shared, cross-replica fixed-window rate limiter backing store.
-- Cloud Run autoscales N stateless replicas;
-- the process-local in-memory limiter (crates/amicu-api/src/ratelimit.rs)
-- gives each replica its own full quota, so the effective limit is N times
-- `AMICU_RATE_LIMIT_MAX` and resets on every cold start. This table lets every
-- replica share ONE counter per (route, client) key over the one shared
-- Postgres pool every replica already holds.
--
-- Deliberately NOT workspace/tenant-scoped, unlike every RLS-protected table
-- elsewhere in this schema: a rate-limit key is a route+client-IP (or
-- route+email) string with no workspace concept, checked BEFORE any
-- authentication/tenancy context exists (the credential routes this backs are
-- exactly the routes a caller hits before proving who they are). No RLS
-- policy, no FORCE ROW LEVEL SECURITY - so it does not need to be owned by a
-- non-superuser/non-BYPASSRLS role for `probe_rls_bypass_risk` to consider it
-- safe; ownership by the migrator role (see the two-role production model in
-- crates/amicu-server/src/lib.rs) is fine.
--
-- One row per key. `expires_at` is both the fixed window's end (Unix seconds)
-- and the sweep predicate for opportunistic cleanup
-- (`amicu_store::ratelimit::PostgresRateLimitStore::cleanup_expired`). A hit
-- is one atomic `INSERT ... ON CONFLICT (key) DO UPDATE ... RETURNING count`
-- (see that module): the UPDATE branch resets to a fresh window when the
-- existing row's window has already expired, or increments the counter
-- in-window otherwise - one round trip, no read-then-write race, which is
-- what makes concurrent hits from different replicas count correctly against
-- a single shared budget.
CREATE TABLE rate_limit_buckets (
    key          TEXT   PRIMARY KEY,
    window_start BIGINT NOT NULL,
    count        BIGINT NOT NULL,
    expires_at   BIGINT NOT NULL
);

-- Opportunistic cleanup sweeps `WHERE expires_at <= $1`; every live key is
-- also looked up by primary key on the hot path, which the PRIMARY KEY index
-- already covers.
CREATE INDEX rate_limit_buckets_expires_at_idx ON rate_limit_buckets (expires_at);
