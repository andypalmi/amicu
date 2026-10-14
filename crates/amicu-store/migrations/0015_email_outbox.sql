-- Transactional email outbox.
--
-- WHY: on Cloud Run the CPU is throttled to near-zero the moment a response is
-- written, so the previous pattern - `tokio::spawn`-ing the SMTP send after the
-- HTTP response returned - stalls or dies outright and the mail is silently
-- dropped. Instead every transactional email is PERSISTED at request time (the
-- same request-time transaction that mints the reset/verify token), and a
-- separate, authenticated `POST /internal/outbox/drain` request - driven by
-- Cloud Scheduler once a minute - does the actual SMTP work INSIDE a request, so
-- CPU is allocated for it.
--
-- GLOBAL, not workspace-scoped, and therefore deliberately carries NO row-level
-- security - exactly like `users`, `refresh_tokens`, `reset_tokens` and
-- `verify_tokens` (see 0002/0005/0006/0007). A queued email belongs to the
-- control plane, not to a tenant: the drain runs as the service itself with no
-- workspace GUC bound, so an RLS policy keyed on `app.current_workspace` would
-- fail closed on every row and the queue would never drain. The production
-- RLS-bypass startup guard (`amicu_store::probe_rls_bypass_risk`) only flags
-- FORCE-RLS tables the connecting role OWNS, so adding an RLS-free table here
-- trips nothing; no allowlist or escape hatch is needed.
--
-- Timestamps are Unix seconds (BIGINT), matching the codebase convention. No
-- chrono.
CREATE TABLE email_outbox (
    -- Client-side generated UUID: the enqueue runs inside the caller's
    -- transaction and never needs a RETURNING round-trip to learn the id.
    id              UUID    NOT NULL PRIMARY KEY,
    created_at      BIGINT  NOT NULL,

    -- Mirrors `amicu_auth::notify::Notification` exactly - the input the mailer
    -- seam already takes - so the drain can rebuild one verbatim and hand it to
    -- the SAME `Notifier` the old inline send used. `kind` is the snake_case
    -- discriminant of `NotificationKind` ('password_reset', 'verify_email',
    -- 'account_exists'); `recipient` is the normalized lowercase email.
    kind            TEXT    NOT NULL,
    recipient       TEXT    NOT NULL,

    -- The opaque one-shot secret the link embeds (reset / verify token), or NULL
    -- for a kind that carries none ('account_exists'). This is the ONE place the
    -- raw token exists at rest - `reset_tokens`/`verify_tokens` store only its
    -- SHA-256 - so it is cleared (set to NULL) the moment the row is marked sent
    -- or dead-lettered, bounding the window to "queued but not yet delivered".
    token           TEXT,

    -- Delivery state machine. `attempts` is bumped at CLAIM time, not at failure
    -- time, so a drain that crashes mid-send still burns an attempt and a poison
    -- message can never be retried forever.
    attempts        INT     NOT NULL DEFAULT 0,
    next_attempt_at BIGINT  NOT NULL,
    -- Lease held by the claimer for the duration of one send. Combined with
    -- `FOR UPDATE SKIP LOCKED` this is what makes concurrent replicas (and
    -- overlapping Scheduler invocations) safe: the row-level lock excludes two
    -- claimers inside the claiming statement, and the lease excludes them for the
    -- much longer SMTP send that follows.
    locked_until    BIGINT,
    last_error      TEXT,
    sent_at         BIGINT,
    dead_at         BIGINT
);

-- The claim query's exact shape: due, undelivered, unleased rows in
-- next_attempt_at order. Partial, so delivered/dead rows (the overwhelming
-- majority over time) cost nothing to keep around.
CREATE INDEX email_outbox_due
    ON email_outbox (next_attempt_at)
    WHERE sent_at IS NULL AND dead_at IS NULL;
