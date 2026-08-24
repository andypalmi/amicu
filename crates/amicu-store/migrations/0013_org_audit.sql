-- Append-only audit log for the Enterprise organization console
-- (andypalmi/amicu-planning#130). Every privileged membership mutation
-- (member added / role changed / member removed) writes exactly one row here, in
-- the SAME transaction as the `workspace_members` mutation it records, so a
-- mutation can never land without its audit record and vice versa (the #117
-- atomicity invariant, carried onto Postgres).
--
-- Workspace-scoped, like every other tenant table: the always-on
-- `WHERE workspace_id = $1` predicate is the primary filter and the RLS policy
-- below (fail-closed via NULLIF, see 0003/0012) is the backstop under the
-- non-superuser amicu_app role.
--
-- `id` is a global identity column used only for ordering ("most recent first" =
-- ORDER BY id DESC) and as an opaque wire id; it is never compared across
-- workspaces. `actor_id`/`target_id` are user ids (token `sub`, UUID). `detail`
-- is a short human string (e.g. "member -> admin"). recorded_at is Unix seconds
-- (BIGINT), matching the codebase convention. No chrono.
CREATE TABLE org_audit (
    id           BIGINT GENERATED ALWAYS AS IDENTITY,
    workspace_id UUID   NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    recorded_at  BIGINT NOT NULL,
    actor_id     UUID   NOT NULL,
    action       TEXT   NOT NULL,
    target_id    UUID,
    detail       TEXT,
    PRIMARY KEY (workspace_id, id)
);

-- "Most recent first" for one workspace scans by (workspace_id, id DESC); the
-- PRIMARY KEY (workspace_id, id) already covers that, so no extra index.

ALTER TABLE org_audit ENABLE ROW LEVEL SECURITY;
ALTER TABLE org_audit FORCE ROW LEVEL SECURITY;
CREATE POLICY org_audit_workspace_isolation ON org_audit
    USING (workspace_id = NULLIF(current_setting('app.current_workspace', true), '')::uuid)
    WITH CHECK (workspace_id = NULLIF(current_setting('app.current_workspace', true), '')::uuid);
