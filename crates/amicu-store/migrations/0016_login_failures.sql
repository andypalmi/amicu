-- Per-account sign-in backoff counters (andypalmi/amicu-planning#194).
--
-- One row per throttle key = a keyed PSEUDONYM of the normalized email (never
-- the plaintext address; see `amicu_auth::security::pseudonym`), created
-- whether or not an account with that email exists so backoff cannot be used
-- to enumerate accounts. Shared by every replica (Cloud Run is multi-instance).
--
-- Like `rate_limit_buckets` (0014) this is deliberately NOT workspace-scoped
-- and has no RLS: it is consulted before any authentication / tenancy context
-- exists. `locked_until` and `updated_at` are Unix seconds on Postgres's own
-- clock. See `amicu_store::login_throttle` for the atomic upsert.
CREATE TABLE login_failures (
    key          TEXT   PRIMARY KEY,
    failures     BIGINT NOT NULL,
    locked_until BIGINT NOT NULL,
    updated_at   BIGINT NOT NULL,
    -- Whether the most recent attempt was admitted (not refused by an active
    -- lock). Set by the same atomic upsert that decides it, so the caller can
    -- read the decision back via RETURNING without a racy second read.
    last_admitted BOOLEAN NOT NULL DEFAULT TRUE
);

-- Opportunistic cleanup sweeps `WHERE updated_at + <reset> <= now`.
CREATE INDEX login_failures_updated_at_idx ON login_failures (updated_at);
