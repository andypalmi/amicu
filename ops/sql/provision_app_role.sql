-- Two-role production provisioning for amicu-store's Postgres schema
--
-- Shape: a MIGRATOR/OWNER role (`amicu_owner` below - name it however your
-- infra prefers) creates and owns every table `amicu-server migrate` applies
-- (run this script once, then `amicu-server migrate`, connected AS the owner
-- role); the RUNNING SERVICE connects as a SEPARATE, DML-only role
-- (`amicu_app`) that owns nothing.
--
-- `amicu-store`'s migrations set `FORCE ROW LEVEL SECURITY` on every
-- workspace-scoped table specifically so RLS still binds even to a role that
-- somehow ends up owning the tables - but amicu-server's production startup
-- guard (`amicu_store::probe_rls_bypass_risk`, gated on `AMICU_ENV=production`)
-- refuses to boot the SERVICE as the owner anyway, because an owning role can
-- trivially `ALTER TABLE ... NO FORCE ROW LEVEL SECURITY` and remove that
-- backstop. Keeping the owner and the runtime role distinct is what makes the
-- guard meaningful, not merely decorative.
--
-- Managed-Postgres note: Cloud SQL's `cloudsqlsuperuser` and Supabase's
-- `postgres`/`supabase_admin` roles carry `BYPASSRLS`. Supabase's `postgres`
-- role in particular is also the role you would be tempted to reach for as
-- the migrator - that is fine for running `amicu-server migrate` (a migrator
-- SHOULD be privileged), but never run the SERVICE itself as it, or as any
-- role that inherits from it. The startup guard's role-membership check
-- (`pg_has_role(current_user, r.oid, 'member')` over every role with
-- `rolsuper OR rolbypassrls`) catches inherited membership in either
-- automatically, with no role name hardcoded on either side - it will refuse
-- a `postgres`/`supabase_admin`-derived runtime role exactly the same way it
-- refuses a bare Postgres superuser.
--
-- Usage:
--   1. As a bootstrap superuser (or an existing schema owner), run this
--      script once per database, substituting real passwords for the two
--      :'...'_password psql variables (or drop the PASSWORD clauses entirely
--      and manage credentials through your platform's IAM/secrets integration
--      instead - Cloud SQL IAM auth and Supabase both support that).
--   2. Apply migrations as the owner:
--        DATABASE_URL=<amicu_owner DSN> amicu-server migrate
--   3. Run the service as the app role:
--        DATABASE_URL=<amicu_app DSN> AMICU_ENV=production amicu-server
--
-- Roles are CLUSTER-GLOBAL in Postgres (not scoped to one database), so the
-- `CREATE ROLE` statements below are guarded with an `IF NOT EXISTS` check
-- (the same idiom `docker/postgres-init/01-app-role.sql` uses) and this
-- script is safe to run again - under `psql -v ON_ERROR_STOP=1` included -
-- against a SECOND database on a cluster where `amicu_owner`/`amicu_app`
-- already exist from provisioning a first one: it will not error, it will
-- just skip straight to the per-database `ALTER SCHEMA` / `GRANT` /
-- `ALTER DEFAULT PRIVILEGES` statements for the new database.
--
-- Re-run step 1's GRANT/ALTER DEFAULT PRIVILEGES statements (steps 2-3 below)
-- any time you provision a NEW database this way; you do NOT need to re-run
-- them after a later `amicu-server migrate` adds more tables - the ALTER
-- DEFAULT PRIVILEGES clauses make that automatic.

-- psql's `:'var'` substitution does NOT reach inside a dollar-quoted (`$$
-- ... $$`) function body - it is purely a client-side text substitution over
-- the OUTER script, and a `DO $$ ... $$` block's body is opaque server-side
-- PL/pgSQL text. So the two passwords are staged into session-local GUCs
-- HERE, in plain (non-dollar-quoted) SQL where `:'var'` substitution does
-- apply, and each `DO` block below reads its GUC back via `current_setting`
-- and builds the `CREATE ROLE` dynamically with `EXECUTE format(...)`
-- (`%L` safely quotes the password as a literal).
SELECT set_config('provision.owner_password', :'amicu_owner_password', false);
SELECT set_config('provision.app_password', :'amicu_app_password', false);

-- 1. The migrator/owner role. LOGIN so `amicu-server migrate` can connect
--    directly as it. CREATEDB/CREATEROLE are NOT needed and are withheld;
--    BYPASSRLS is NEVER granted here (it is unnecessary - the owner already
--    bypasses RLS on its own tables unless FORCE is set, and granting
--    BYPASSRLS besides would make this role a bypass risk on every OTHER
--    table too).
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'amicu_owner') THEN
        EXECUTE format(
            'CREATE ROLE amicu_owner LOGIN PASSWORD %L '
            'NOSUPERUSER NOCREATEDB NOCREATEROLE NOBYPASSRLS',
            current_setting('provision.owner_password')
        );
    END IF;
END
$$;

-- Since PG16 a CREATEROLE non-superuser (e.g. Supabase's `postgres`) that
-- creates a role gets ADMIN OPTION on it but NOT SET/INHERIT membership
-- (unless createrole_self_grant is set), so `ALTER SCHEMA ... OWNER TO` and
-- `ALTER DEFAULT PRIVILEGES FOR ROLE amicu_owner` below would fail with
-- "must be able to SET ROLE". Grant the invoking admin membership explicitly.
-- The admin being a member of amicu_owner is harmless: the invariant is that
-- amicu_app has NO privileged memberships (verified at the end, unchanged).
-- Idempotent; for a true superuser it is a no-op (at most a NOTICE).
GRANT amicu_owner TO CURRENT_USER;

-- amicu_owner owns `public`, so it (and only it) can CREATE the
-- tables/indexes `amicu-server migrate` applies, and therefore owns every
-- object those migrations create.
ALTER SCHEMA public OWNER TO amicu_owner;

-- 2. The runtime, DML-only role the SERVICE connects as.
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'amicu_app') THEN
        EXECUTE format(
            'CREATE ROLE amicu_app LOGIN PASSWORD %L '
            'NOSUPERUSER NOCREATEDB NOCREATEROLE NOBYPASSRLS',
            current_setting('provision.app_password')
        );
    END IF;
END
$$;

-- CONNECT is granted to PUBLIC by default; USAGE on the schema is required
-- explicitly since amicu_app does not own it (without USAGE, amicu_app could
-- not even resolve unqualified table names, let alone query them).
GRANT USAGE ON SCHEMA public TO amicu_app;

-- 3. Grant DML (never DDL - amicu_app must never create/alter objects) on
--    every table/sequence that ALREADY exists. Run this step (or re-run the
--    whole script) after the FIRST `amicu-server migrate` on a fresh
--    database, once the tables actually exist; it is a harmless no-op
--    before that (there is nothing yet to grant on).
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO amicu_app;
GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO amicu_app;

-- `ALTER DEFAULT PRIVILEGES` makes the above automatic for tables/sequences a
-- FUTURE `amicu-server migrate` run (as amicu_owner) creates, so step 3 need
-- not be re-run after every later migration.
ALTER DEFAULT PRIVILEGES FOR ROLE amicu_owner IN SCHEMA public
    GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO amicu_app;
ALTER DEFAULT PRIVILEGES FOR ROLE amicu_owner IN SCHEMA public
    GRANT USAGE, SELECT ON SEQUENCES TO amicu_app;

-- 4. Narrow the blanket DML above.
--
--    * AUDIT tables are append-only for the service: INSERT + SELECT only, so
--      a compromised/buggy service cannot rewrite or erase the audit trail.
--      (`ON DELETE CASCADE` from `workspaces` still removes a deleted
--      workspace's rows: referential actions run with the OWNER's privileges.)
--      Every FUTURE audit table must be added to this list - the
--      ALTER DEFAULT PRIVILEGES above grants full DML on new tables.
--    * `_sqlx_migrations` is read-only for the service (it only inspects
--      applied versions at startup; migrations are applied by `amicu-server
--      migrate` as the owner).
--
--    Like step 3, run (or re-run) this after the first `amicu-server migrate`
--    and again after any migration that adds an audit table; each REVOKE is
--    skipped while its table does not exist yet, so the script stays
--    re-runnable at any point.
DO $$
DECLARE
    audit_table text;
BEGIN
    FOREACH audit_table IN ARRAY ARRAY['org_audit'] LOOP
        IF to_regclass(format('public.%I', audit_table)) IS NOT NULL THEN
            EXECUTE format(
                'REVOKE UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER ON public.%I FROM amicu_app',
                audit_table
            );
        END IF;
    END LOOP;

    IF to_regclass('public._sqlx_migrations') IS NOT NULL THEN
        REVOKE INSERT, UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER
            ON public._sqlx_migrations FROM amicu_app;
    END IF;
END
$$;
