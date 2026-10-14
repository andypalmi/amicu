-- Fail-closed backport for the config/state row-level-security policies.
--
-- The 0008/0009 policies compared `workspace_id` against
-- `current_setting('app.current_workspace', true)::uuid` DIRECTLY. That is a
-- latent fault on a pooled connection: a custom GUC that has ever been SET LOCAL
-- in a session reverts to the EMPTY STRING '' (not NULL) once its transaction
-- ends, so a later transaction that does NOT bind it reads '' - and a bare
-- ''::uuid throws `invalid input syntax for type uuid: ""`. Instead of failing
-- CLOSED (no rows), the policy would ERROR, turning a missing-context bug into a
-- 500 and masking whether isolation held.
--
-- This migration redefines every config/state policy to wrap the GUC in
-- NULLIF(current_setting('app.current_workspace', true), '')::uuid, exactly the
-- pattern 0003 already uses for workspace_members: an unset (or reverted-to-'')
-- GUC coerces to NULL, and `workspace_id = NULL` is UNKNOWN, so the policy simply
-- matches nothing (fail-closed) rather than erroring. The always-on per-statement
-- `WHERE workspace_id = $1` predicate remains the primary defense; this is the
-- backstop under the non-superuser amicu_app role.
--
-- Migrations are immutable once shipped, so 0008/0009 are left untouched: this is
-- a forward DROP POLICY / CREATE POLICY pass over the same five tables.

DROP POLICY configs_workspace_isolation ON configs;
CREATE POLICY configs_workspace_isolation ON configs
    USING (workspace_id = NULLIF(current_setting('app.current_workspace', true), '')::uuid)
    WITH CHECK (workspace_id = NULLIF(current_setting('app.current_workspace', true), '')::uuid);

DROP POLICY config_versions_workspace_isolation ON config_versions;
CREATE POLICY config_versions_workspace_isolation ON config_versions
    USING (workspace_id = NULLIF(current_setting('app.current_workspace', true), '')::uuid)
    WITH CHECK (workspace_id = NULLIF(current_setting('app.current_workspace', true), '')::uuid);

DROP POLICY state_snapshots_workspace_isolation ON state_snapshots;
CREATE POLICY state_snapshots_workspace_isolation ON state_snapshots
    USING (workspace_id = NULLIF(current_setting('app.current_workspace', true), '')::uuid)
    WITH CHECK (workspace_id = NULLIF(current_setting('app.current_workspace', true), '')::uuid);

DROP POLICY state_current_workspace_isolation ON state_current;
CREATE POLICY state_current_workspace_isolation ON state_current
    USING (workspace_id = NULLIF(current_setting('app.current_workspace', true), '')::uuid)
    WITH CHECK (workspace_id = NULLIF(current_setting('app.current_workspace', true), '')::uuid);

DROP POLICY state_locks_workspace_isolation ON state_locks;
CREATE POLICY state_locks_workspace_isolation ON state_locks
    USING (workspace_id = NULLIF(current_setting('app.current_workspace', true), '')::uuid)
    WITH CHECK (workspace_id = NULLIF(current_setting('app.current_workspace', true), '')::uuid);
