-- Refresh tokens: opaque, rotating, revocable. The server stores only the
-- SHA-256 hash of the opaque token (BYTEA, 32 bytes), never the token itself.
--
-- GLOBAL, not workspace-scoped. A refresh token is a session artifact of the
-- GLOBAL user identity, so it is keyed by user_id, not by workspace: like
-- `users` (and reset/verify tokens), it is control-plane and carries no
-- workspace RLS. The active-workspace binding the epic wants in the ACCESS token
-- is a signed `wsid` claim minted per issuance (andypalmi/amicu-planning#129),
-- not a property of the stored refresh row; if that issue needs the refresh row
-- pinned to a workspace it adds a `wsid` column then.
--
-- user_id is a REAL foreign key to users(id) with ON DELETE CASCADE: deleting a
-- user tears down their refresh tokens automatically. This resolves the soft-FK
-- note the old schema carried (andypalmi/amicu-planning#114) - account deletion
-- now exists in the model, so the cascade is defined rather than deferred.
--
-- expires_at is Unix seconds (BIGINT). No chrono.
--
-- family_id is the rotation-family: a sign-in opens a family and each rotation
-- mints its successor into the same family. Re-presenting an already-revoked
-- token is a reuse/theft signal that compromises the whole family (see
-- refresh_families).
CREATE TABLE refresh_tokens (
    token_hash BYTEA   NOT NULL PRIMARY KEY,
    user_id    UUID    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    family_id  TEXT    NOT NULL,
    expires_at BIGINT  NOT NULL,
    revoked    BOOLEAN NOT NULL DEFAULT false
);

-- Revocation sweeps by user (password reset) and by family (reuse detection).
CREATE INDEX refresh_tokens_user_idx ON refresh_tokens (user_id);
CREATE INDEX refresh_tokens_family_idx ON refresh_tokens (family_id);

-- Durable, cross-node record of families flagged compromised by reuse
-- detection. A row exists only once a family has been compromised; absence
-- means healthy. Global, matching refresh_tokens.
CREATE TABLE refresh_families (
    family_id   TEXT    NOT NULL PRIMARY KEY,
    compromised BOOLEAN NOT NULL DEFAULT true
);
