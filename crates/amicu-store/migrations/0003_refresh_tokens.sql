-- Refresh tokens: opaque, rotating, revocable. The server stores only the
-- SHA-256 hash of the opaque token (BYTEA, 32 bytes), never the token itself.
--
-- Workspace-scoped: every row carries workspace_id and every query filters on
-- it (tenant isolation is mandatory from day one). Row-level security is a
-- backstop; see the policy below.
--
-- expires_at is Unix seconds (BIGINT), matching the codebase's i64 timestamp
-- convention. No chrono.
--
-- family_id is the rotation-family: a sign-in opens a family and each rotation
-- mints its successor into the same family. Re-presenting an already-revoked
-- token is a reuse/theft signal that compromises the whole family (see
-- refresh_families).
--
-- user_subject is deliberately NOT a foreign key to accounts(subject). There
-- is no account-deletion path yet, so a real FK (and the cascade/cleanup
-- policy it would force a decision on) is premature; the workspace_id
-- predicate + RLS already give tenant isolation. Add the FK (and a deletion
-- cleanup sweep for the referencing token tables) once account deletion
-- exists - see andypalmi/amicu-planning#114.
CREATE TABLE refresh_tokens (
    workspace_id UUID   NOT NULL REFERENCES workspaces(id),
    token_hash   BYTEA  NOT NULL,
    user_subject TEXT   NOT NULL,
    family_id    TEXT   NOT NULL,
    expires_at   BIGINT NOT NULL,
    revoked      BOOLEAN NOT NULL DEFAULT false,
    PRIMARY KEY (workspace_id, token_hash)
);

-- Revocation sweeps by user (password reset) and by family (reuse detection).
CREATE INDEX refresh_tokens_user_idx ON refresh_tokens (workspace_id, user_subject);
CREATE INDEX refresh_tokens_family_idx ON refresh_tokens (workspace_id, family_id);

-- Durable, cross-node record of families flagged compromised by reuse
-- detection (the persisted flag the in-memory store's TODO asked for). A row
-- exists only once a family has been compromised; absence means healthy.
CREATE TABLE refresh_families (
    workspace_id UUID    NOT NULL REFERENCES workspaces(id),
    family_id    TEXT    NOT NULL,
    compromised  BOOLEAN NOT NULL DEFAULT true,
    PRIMARY KEY (workspace_id, family_id)
);

-- Row-level security backstop. The policy keys on a per-transaction GUC the
-- store sets via set_config('app.current_workspace', <wsid>, true); when unset,
-- current_setting(..., true) is NULL and no rows match, so it fails closed.
-- FORCE makes the policy apply to the table owner too (owners otherwise bypass
-- RLS).
--
-- IMPORTANT: a SUPERUSER or a role with BYPASSRLS bypasses RLS entirely, even
-- with FORCE. `POSTGRES_USER` in docker-compose.yml is the Postgres bootstrap
-- superuser and must NOT be the role the application connects as; the
-- dev/self-host compose instead provisions a dedicated non-superuser,
-- non-BYPASSRLS `amicu_app` role (see docker/postgres-init/01-app-role.sql)
-- for the app to connect as, so this backstop actually engages. A production
-- deployment must do the same. Because a misconfigured connection can still
-- silently disable this backstop, the query-level workspace_id predicate on
-- every statement remains the PRIMARY, always-on defense; RLS is defense in
-- depth.
ALTER TABLE refresh_tokens ENABLE ROW LEVEL SECURITY;
ALTER TABLE refresh_tokens FORCE ROW LEVEL SECURITY;
CREATE POLICY refresh_tokens_workspace_isolation ON refresh_tokens
    USING (workspace_id = current_setting('app.current_workspace', true)::uuid)
    WITH CHECK (workspace_id = current_setting('app.current_workspace', true)::uuid);

ALTER TABLE refresh_families ENABLE ROW LEVEL SECURITY;
ALTER TABLE refresh_families FORCE ROW LEVEL SECURITY;
CREATE POLICY refresh_families_workspace_isolation ON refresh_families
    USING (workspace_id = current_setting('app.current_workspace', true)::uuid)
    WITH CHECK (workspace_id = current_setting('app.current_workspace', true)::uuid);
