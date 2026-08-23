-- Password-reset tokens: single-use, expiring. Like refresh tokens, the server
-- stores only the SHA-256 hash of the opaque token (BYTEA, 32 bytes). There is
-- no rotation/family: a reset token is minted, consumed once, and dies.
--
-- GLOBAL, not workspace-scoped: a password reset acts on the GLOBAL user
-- identity (the password lives on `users`), so the token is keyed by user_id.
-- Control-plane, no workspace RLS - same rationale as refresh_tokens and users.
--
-- user_id is a REAL foreign key to users(id) with ON DELETE CASCADE (see
-- refresh_tokens; resolves andypalmi/amicu-planning#114).
--
-- expires_at is Unix seconds (BIGINT). No chrono.
CREATE TABLE reset_tokens (
    token_hash BYTEA   NOT NULL PRIMARY KEY,
    user_id    UUID    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    expires_at BIGINT  NOT NULL,
    consumed   BOOLEAN NOT NULL DEFAULT false
);
