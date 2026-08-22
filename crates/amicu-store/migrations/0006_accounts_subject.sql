-- ID-type reconciliation between the domain and the schema.
--
-- The domain user id (`User.id` in amicu-auth) is the token subject (`sub`): an
-- opaque String, and the key the refresh/reset tables reference. PR-1's
-- accounts table, however, keyed on a UUID surrogate `id`. Rather than force the
-- String subject into a UUID (it is not one) we keep the UUID as an internal
-- surrogate primary key and add a dedicated `subject TEXT` identity column that
-- carries the real domain id. Nothing is in production, so this corrective
-- migration is safe.
--
-- The store round-trips `User.id` through `subject`; the surrogate `id` is
-- filled by a database-side default and never surfaced to the domain.
ALTER TABLE accounts ADD COLUMN subject TEXT;

-- Backfill any pre-existing rows (none in practice) with the surrogate id text
-- so the NOT NULL tightening below cannot fail.
UPDATE accounts SET subject = id::text WHERE subject IS NULL;

ALTER TABLE accounts ALTER COLUMN subject SET NOT NULL;
ALTER TABLE accounts ADD CONSTRAINT accounts_workspace_subject_key UNIQUE (workspace_id, subject);

-- Let the database mint the surrogate id so inserts need only supply the domain
-- fields. gen_random_uuid() is built into Postgres 13+ (pgcrypto not required).
ALTER TABLE accounts ALTER COLUMN id SET DEFAULT gen_random_uuid();

-- Row-level security backstop for accounts, matching the token tables (see
-- 0003 for the full rationale, including the superuser/BYPASSRLS caveat). The
-- store sets app.current_workspace per transaction; when unset the policy
-- matches no rows (fails closed). Query-level workspace_id predicates remain the
-- primary, always-on defense.
ALTER TABLE accounts ENABLE ROW LEVEL SECURITY;
ALTER TABLE accounts FORCE ROW LEVEL SECURITY;
CREATE POLICY accounts_workspace_isolation ON accounts
    USING (workspace_id = current_setting('app.current_workspace', true)::uuid)
    WITH CHECK (workspace_id = current_setting('app.current_workspace', true)::uuid);
