-- Password-reset tokens: single-use, expiring. Like refresh tokens, the server
-- stores only the SHA-256 hash of the opaque token (BYTEA, 32 bytes). There is
-- no rotation/family: a reset token is minted, consumed once, and dies.
--
-- Workspace-scoped with the same RLS backstop as refresh_tokens.
--
-- expires_at is Unix seconds (BIGINT). No chrono.
CREATE TABLE reset_tokens (
    workspace_id UUID    NOT NULL REFERENCES workspaces(id),
    token_hash   BYTEA   NOT NULL,
    user_subject TEXT    NOT NULL,
    expires_at   BIGINT  NOT NULL,
    consumed     BOOLEAN NOT NULL DEFAULT false,
    PRIMARY KEY (workspace_id, token_hash)
);

-- Row-level security backstop (see 0003 for the rationale on FORCE + the GUC).
ALTER TABLE reset_tokens ENABLE ROW LEVEL SECURITY;
ALTER TABLE reset_tokens FORCE ROW LEVEL SECURITY;
CREATE POLICY reset_tokens_workspace_isolation ON reset_tokens
    USING (workspace_id = current_setting('app.current_workspace', true)::uuid)
    WITH CHECK (workspace_id = current_setting('app.current_workspace', true)::uuid);
