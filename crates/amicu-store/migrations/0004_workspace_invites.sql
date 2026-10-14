-- Workspace invites: a pending, tokenized offer to join a workspace at a given
-- role. The server stores only the SHA-256 hash of the opaque invite token
-- (BYTEA, 32 bytes), never the token itself (it is mailed in the invite link).
-- Redeeming the token is what turns an invite into a workspace_members row
-- (that acceptance flow lands later; this migration
-- is the data layer it builds on).
--
-- id is a server-generated UUID. workspace_id FKs to the inviting workspace
-- (cascade so deleting a workspace clears its pending invites). email is the
-- invitee (a global identity may or may not exist yet). token_hash is globally
-- UNIQUE so redemption can find the one invite a presented token maps to.
--
-- expires_at / accepted_at are Unix seconds (BIGINT); accepted_at NULL means
-- still pending. No chrono.
CREATE TABLE workspace_invites (
    id           UUID   PRIMARY KEY,
    workspace_id UUID   NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    email        TEXT   NOT NULL,
    role         TEXT   NOT NULL,
    token_hash   BYTEA  NOT NULL UNIQUE,
    expires_at   BIGINT NOT NULL,
    accepted_at  BIGINT
);

-- Row-level security. Like workspace_members, an invite is reachable from two
-- directions, so the policy admits a row under either bound context:
--
--   * app.current_workspace    - the workspace-scoped view: creating, listing,
--     accepting, or expiring the invites OF a workspace. Set by begin_scoped().
--   * app.current_invite_token - the redemption view: finding the single invite
--     a presented token maps to, WITHOUT knowing its workspace yet (that is the
--     whole point of redemption). The unguessable 32-byte token IS the
--     capability, passed as the lowercase-hex of its hash. Set by
--     begin_scoped_invite().
--
-- Each store operation sets exactly ONE GUC; the other reads as NULL (via
-- NULLIF, since a once-set custom GUC reverts to '' - not NULL - after its SET
-- LOCAL ends on a pooled connection, and a bare ''::uuid would throw) and
-- matches nothing (fail-closed). WITH CHECK is the workspace clause ONLY: invites are
-- created/mutated solely within the workspace bound by app.current_workspace;
-- the token context is read-only (redemption reads the invite, then the
-- resulting membership write happens under the workspace context).
--
-- The explicit WHERE predicate on every statement is the primary defense; RLS
-- under the non-superuser amicu_app role is the backstop. FORCE applies it to
-- the owner too.
ALTER TABLE workspace_invites ENABLE ROW LEVEL SECURITY;
ALTER TABLE workspace_invites FORCE ROW LEVEL SECURITY;
CREATE POLICY workspace_invites_isolation ON workspace_invites
    USING (
        workspace_id = NULLIF(current_setting('app.current_workspace', true), '')::uuid
        OR token_hash = decode(NULLIF(current_setting('app.current_invite_token', true), ''), 'hex')
    )
    WITH CHECK (
        workspace_id = NULLIF(current_setting('app.current_workspace', true), '')::uuid
    );
