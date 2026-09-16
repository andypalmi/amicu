-- Two-role production provisioning for amicu-store's Postgres schema
-- (andypalmi/amicu-planning#139, #143).
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
-- Re-run step 1's GRANT/ALTER DEFAULT PRIVILEGES statements (steps 2-3 below)
-- any time you provision a NEW database this way; you do NOT need to re-run
-- them after a later `amicu-server migrate` adds more tables - the ALTER
-- DEFAULT PRIVILEGES clauses make that automatic.

-- 1. The migrator/owner role. LOGIN so `amicu-server migrate` can connect
--    directly as it. CREATEDB/CREATEROLE are NOT needed and are withheld;
--    BYPASSRLS is NEVER granted here (it is unnecessary - the owner already
--    bypasses RLS on its own tables unless FORCE is set, and granting
--    BYPASSRLS besides would make this role a bypass risk on every OTHER
--    table too).
CREATE ROLE amicu_owner LOGIN PASSWORD :'amicu_owner_password'
    NOSUPERUSER NOCREATEDB NOCREATEROLE NOBYPASSRLS;

-- amicu_owner owns `public`, so it (and only it) can CREATE the
-- tables/indexes `amicu-server migrate` applies, and therefore owns every
-- object those migrations create.
ALTER SCHEMA public OWNER TO amicu_owner;

-- 2. The runtime, DML-only role the SERVICE connects as.
CREATE ROLE amicu_app LOGIN PASSWORD :'amicu_app_password'
    NOSUPERUSER NOCREATEDB NOCREATEROLE NOBYPASSRLS;

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
