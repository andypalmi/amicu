-- Accounts, scoped to a workspace. The composite primary key
-- (workspace_id, id) makes the workspace the isolation boundary at the row
-- level, and email is unique per workspace (not globally).
--
-- created_at is Unix seconds (BIGINT), matching the codebase's i64 timestamp
-- convention. No chrono.
--
-- NOTE: Row-level-security policies and the real store implementations land in
-- PR-2. RLS is deliberately NOT enabled here: enabling it without the store
-- first setting the session workspace GUC would block all access. That wiring
-- (the per-request `SET LOCAL` of the workspace id plus the CREATE POLICY
-- statements) is coordinated in PR-2.
CREATE TABLE accounts (
    workspace_id           UUID NOT NULL REFERENCES workspaces(id),
    id                     UUID NOT NULL,
    email                  TEXT NOT NULL,
    password_hash          TEXT NOT NULL,
    tier                   TEXT NOT NULL,
    roles                  TEXT[] NOT NULL DEFAULT '{}',
    stripe_customer_id     TEXT,
    stripe_subscription_id TEXT,
    email_verified         BOOLEAN NOT NULL DEFAULT false,
    created_at             BIGINT NOT NULL,
    PRIMARY KEY (workspace_id, id),
    UNIQUE (workspace_id, email)
);
