-- Email-verification tokens: single-use, expiring. Like reset tokens, the
-- server stores only the SHA-256 hash of the opaque token (BYTEA, 32 bytes),
-- never the token itself (mailed in a "confirm your email" link). There is no
-- rotation/family: a verify token is minted, consumed once, and dies.
--
-- Workspace-scoped with the same RLS backstop as reset_tokens.
--
-- expires_at is Unix seconds (BIGINT). No chrono.
--
-- user_subject is deliberately NOT a foreign key to accounts(subject), for the
-- same reason as refresh_tokens.user_subject: no account-deletion path exists
-- yet. Add the FK (and a deletion cleanup sweep) once it does - see
-- andypalmi/amicu-planning#114.
CREATE TABLE verify_tokens (
    workspace_id UUID    NOT NULL REFERENCES workspaces(id),
    token_hash   BYTEA   NOT NULL,
    user_subject TEXT    NOT NULL,
    expires_at   BIGINT  NOT NULL,
    consumed     BOOLEAN NOT NULL DEFAULT false,
    PRIMARY KEY (workspace_id, token_hash)
);

-- Row-level security backstop (see 0003 for the rationale on FORCE + the GUC).
ALTER TABLE verify_tokens ENABLE ROW LEVEL SECURITY;
ALTER TABLE verify_tokens FORCE ROW LEVEL SECURITY;
CREATE POLICY verify_tokens_workspace_isolation ON verify_tokens
    USING (workspace_id = current_setting('app.current_workspace', true)::uuid)
    WITH CHECK (workspace_id = current_setting('app.current_workspace', true)::uuid);
