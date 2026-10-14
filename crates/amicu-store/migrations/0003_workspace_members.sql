-- Workspace membership: the join between a global user and a workspace, and the
-- authority for WHO may act WHERE, as WHAT. `role` is one of
-- owner/admin/member/viewer. This is the table the claim-bound gate
-- consults to confirm a user's membership before
-- minting a workspace-scoped token, and that privileged mutations re-check
-- in-transaction.
--
-- Composite primary key (workspace_id, user_id): a user has at most one role in
-- a workspace. Both columns are real foreign keys, but with DELIBERATELY
-- DIFFERENT ON DELETE actions:
--
--   * user_id ON DELETE CASCADE - deleting a user tears down their own
--     memberships (the deletion cleanup path #114 asked for). This is safe
--     because the memberships being removed are the deleted user's own rows.
--
--   * workspace_id ON DELETE RESTRICT - deleting a WORKSPACE is BLOCKED while it
--     still has members. A workspace is never dropped out from under the people
--     in it; a real teardown must remove the members first (explicitly and
--     audibly), never silently cascade away other users' memberships. This is
--     load-bearing for the fail-closed `UserStore::delete` guard: it deletes a
--     user's personal workspace only when that workspace has no OTHER members,
--     and RESTRICT enforces exactly that even when those other members' rows are
--     invisible under the deleter's single RLS context (a foreign-key violation
--     surfaces instead of a silent cross-tenant cascade). See UserStore::delete.
--
-- invited_by is the user who added this member (NULL for the founding Owner
-- created at sign-up). It is a soft reference (no FK) so that removing the
-- inviter never blocks or rewrites an existing membership row.
--
-- joined_at is Unix seconds (BIGINT). No chrono.
CREATE TABLE workspace_members (
    workspace_id UUID   NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
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
-- CONTRACT - AT MOST ONE context GUC may be set per transaction. This is an OR
-- policy: if BOTH app.current_workspace and app.current_user were set in the
-- SAME transaction, a bare `SELECT * FROM workspace_members` would union the
-- workspace's members AND the user's other-workspace memberships - i.e. leak the
-- other tenant's rows. So this policy is STRICTLY WEAKER than the single-GUC
-- config/state policies (0008/0009), and its isolation rests on two invariants
-- the store upholds:
--   1. Each begin_scoped_* helper binds EXACTLY ONE context GUC and no other
--      (begin_scoped -> app.current_workspace; begin_scoped_user ->
--      app.current_user; begin_scoped_invite -> app.current_invite_token). This
--      is asserted by a regression test (see amicu-store's lib.rs tests:
--      scoped_helpers_bind_exactly_one_context_guc).
--   2. Every query carries its own explicit WHERE predicate
--      (WHERE workspace_id = $1 / WHERE user_id = $1) as the PRIMARY filter; the
--      RLS OR-policy is only the backstop.
--
-- Given (1), the OTHER GUC is unset, so its NULLIF(current_setting(..., true),
-- '') is NULL and contributes no match (fail-closed). The NULLIF is load-bearing:
-- a custom GUC that has ever been set in a session reverts to the EMPTY STRING
-- (not NULL) once its SET LOCAL ends, so on a pooled connection the operand the
-- current transaction did NOT set reads '' - and a bare ''::uuid throws.
-- NULLIF('' , '') coerces it back to NULL so the clause simply fails to match
-- instead of erroring.
--
-- WITH CHECK is intentionally the workspace clause ONLY: writes (add/change/
-- remove a member) are permitted solely within the workspace bound by
-- app.current_workspace. The user-scoped context is read-only, so a caller
-- cannot self-insert into an arbitrary workspace by binding app.current_user.
--
-- As everywhere in this schema, the explicit WHERE predicate on every statement
-- is the PRIMARY, always-on defense; RLS under the non-superuser amicu_app role
-- is the backstop. FORCE applies the policy
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
