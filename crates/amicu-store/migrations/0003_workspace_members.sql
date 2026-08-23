-- Workspace membership: the join between a global user and a workspace, and the
-- authority for WHO may act WHERE, as WHAT. `role` is one of
-- owner/admin/member/viewer. This is the table the claim-bound gate (issue
-- andypalmi/amicu-planning#129) consults to confirm a user's membership before
-- minting a workspace-scoped token, and that privileged mutations re-check
-- in-transaction.
--
-- Composite primary key (workspace_id, user_id): a user has at most one role in
-- a workspace. Both columns are real foreign keys; ON DELETE CASCADE on the
-- user side means deleting a user tears down their memberships (the deletion
-- cleanup path #114 asked for).
--
-- invited_by is the user who added this member (NULL for the founding Owner
-- created at sign-up). It is a soft reference (no FK) so that removing the
-- inviter never blocks or rewrites an existing membership row.
--
-- joined_at is Unix seconds (BIGINT). No chrono.
CREATE TABLE workspace_members (
    workspace_id UUID   NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    user_id      UUID   NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role         TEXT   NOT NULL,
    invited_by   UUID,
    joined_at    BIGINT NOT NULL,
    PRIMARY KEY (workspace_id, user_id)
);

-- "List the workspaces a user belongs to" and cascade-on-user-delete both scan
-- by user_id; the PK covers the (workspace_id, ...) direction only.
CREATE INDEX workspace_members_user_idx ON workspace_members (user_id);

-- Row-level security. Membership rows are legitimately reachable from TWO
-- directions, so the policy admits a row under either bound context:
--
--   * app.current_workspace  - the workspace-scoped view: listing/adding/
--     changing/removing the members OF a workspace. Set by begin_scoped().
--   * app.current_user        - the user-scoped view: listing the workspaces a
--     user is IN (list-my-workspaces / workspace switching). A user may always
--     see their own memberships across workspaces. Set by begin_scoped_user().
--
-- Each store operation sets exactly ONE of the two GUCs; the other is unset, so
-- its NULLIF(current_setting(..., true), '') is NULL and contributes no match
-- (fail-closed). The NULLIF is load-bearing: a custom GUC that has ever been set
-- in a session reverts to the EMPTY STRING (not NULL) once its SET LOCAL ends,
-- so on a pooled connection the operand the current transaction did NOT set
-- reads '' - and a bare ''::uuid throws. NULLIF('' , '') coerces it back to NULL
-- so the clause simply fails to match instead of erroring.
--
-- WITH CHECK is intentionally the workspace clause ONLY: writes (add/change/
-- remove a member) are permitted solely within the workspace bound by
-- app.current_workspace. The user-scoped context is read-only, so a caller
-- cannot self-insert into an arbitrary workspace by binding app.current_user.
--
-- As everywhere in this schema, the explicit WHERE predicate on every statement
-- is the PRIMARY, always-on defense; RLS under the non-superuser amicu_app role
-- (see andypalmi/amicu-planning#114) is the backstop. FORCE applies the policy
-- to the table owner too.
ALTER TABLE workspace_members ENABLE ROW LEVEL SECURITY;
ALTER TABLE workspace_members FORCE ROW LEVEL SECURITY;
CREATE POLICY workspace_members_isolation ON workspace_members
    USING (
        workspace_id = NULLIF(current_setting('app.current_workspace', true), '')::uuid
        OR user_id = NULLIF(current_setting('app.current_user', true), '')::uuid
    )
    WITH CHECK (
        workspace_id = NULLIF(current_setting('app.current_workspace', true), '')::uuid
    );
