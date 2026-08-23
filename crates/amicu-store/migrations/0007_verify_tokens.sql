-- Email-verification tokens: single-use, expiring. Like reset tokens, the
-- server stores only the SHA-256 hash of the opaque token (BYTEA, 32 bytes),
-- never the token itself (mailed in a "confirm your email" link). There is no
-- rotation/family: a verify token is minted, consumed once, and dies.
--
-- GLOBAL, not workspace-scoped: email verification confirms the GLOBAL user's
-- email (email_verified_at lives on `users`), so the token is keyed by user_id.
-- Control-plane, no workspace RLS - same rationale as refresh_tokens and users.
--
-- user_id is a REAL foreign key to users(id) with ON DELETE CASCADE (see
-- refresh_tokens; resolves andypalmi/amicu-planning#114).
--
-- expires_at is Unix seconds (BIGINT). No chrono.
CREATE TABLE verify_tokens (
    token_hash BYTEA   NOT NULL PRIMARY KEY,
    user_id    UUID    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    expires_at BIGINT  NOT NULL,
    consumed   BOOLEAN NOT NULL DEFAULT false
);
