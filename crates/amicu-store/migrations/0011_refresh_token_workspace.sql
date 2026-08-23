-- Bind each refresh token to the workspace its session is scoped to
-- (andypalmi/amicu-planning#129). This is the column 0005 explicitly anticipated:
-- the active-workspace context lives in the signed ACCESS token, but the refresh
-- row must REMEMBER which workspace it was minted for so rotation can re-validate
-- membership in exactly that workspace and re-mint the access token scoped to it.
-- That re-validation on rotation is what makes revocation (removal / role change)
-- land within the short access TTL.
--
-- NOT NULL with no default and no backfill: per the epic (#127) this is a hard
-- replacement with no production data to preserve, and every token is minted
-- through the issuer, which always supplies a real wsid - there is no
-- default-workspace path. A pre-existing dev row (there is no prod) would have no
-- meaningful workspace to migrate to; such tokens are intentionally invalidated,
-- so the table is expected empty when this applies.
--
-- ON DELETE CASCADE: deleting a workspace tears down the refresh sessions scoped
-- to it (a workspace teardown should not leave rotatable sessions behind),
-- mirroring the user_id cascade already on this table.
ALTER TABLE refresh_tokens
    ADD COLUMN workspace_id UUID NOT NULL
        REFERENCES workspaces(id) ON DELETE CASCADE;

-- Workspace teardown sweeps refresh rows by workspace_id; index it like the
-- existing user/family sweeps.
CREATE INDEX refresh_tokens_workspace_idx ON refresh_tokens (workspace_id);
