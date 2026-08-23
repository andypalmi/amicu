-- Provisions a dedicated, non-superuser application role for local/self-host
-- development.
--
-- `amicu-store`'s migrations enable `FORCE ROW LEVEL SECURITY` on every
-- workspace-scoped table (see e.g. 0003_refresh_tokens.sql) as a backstop
-- behind the query-level workspace_id predicate. FORCE makes RLS apply even
-- to the table owner - but a SUPERUSER or a role with BYPASSRLS bypasses RLS
-- entirely regardless of FORCE. `POSTGRES_USER` (the compose bootstrap role)
-- is a Postgres superuser, so if the application connected as that role the
-- RLS backstop would be a permanent no-op. This script provisions a second,
-- ordinary role - `amicu_app` - for the application to connect as instead;
-- the bootstrap superuser is used only by initdb and this script.
--
-- This file runs ONLY at first container initialization of an EMPTY data
-- volume (the postgres image's docker-entrypoint-initdb.d convention: it is
-- skipped entirely once `amicu_pgdata` already has a database). On a fresh
-- volume there is nothing else to do - amicu_app owns `public` before any
-- migration runs, so it owns every table/index those migrations CREATE.
--
-- ============================================================================
-- EXISTING VOLUME (you already ran `docker compose up` before this role
-- existed, so accounts/refresh_tokens/etc. already exist and are owned by
-- the bootstrap superuser): this script does NOT run for you, and simply
-- applying its statements by hand is NOT enough. `ALTER SCHEMA public OWNER
-- TO amicu_app` does not retroactively transfer ownership of tables that
-- already exist in that schema, so amicu_app would authenticate but every
-- SELECT/INSERT/UPDATE/DELETE would fail with "permission denied for table
-- accounts" (etc.). You have two options:
--
--   Option A - keep your data. Connect as the bootstrap superuser
--   (`psql postgres://amicu:amicu@localhost:5432/amicu`) and run:
--
--       CREATE ROLE amicu_app LOGIN PASSWORD 'amicu_app'
--           NOSUPERUSER NOCREATEDB NOCREATEROLE NOBYPASSRLS;
--       ALTER SCHEMA public OWNER TO amicu_app;
--       REASSIGN OWNED BY amicu TO amicu_app;
--
--   `REASSIGN OWNED BY` (run while connected to the `amicu` database) is
--   what actually transfers ownership of every pre-existing table, index,
--   and sequence the bootstrap superuser owns over to amicu_app - unlike
--   `ALTER SCHEMA ... OWNER TO`, which only affects the schema object
--   itself and future objects created in it, not objects that already
--   exist. Then switch DATABASE_URL to amicu_app as documented in
--   docker-compose.yml / README.md.
--
--   Option B - discard your data and start clean:
--
--       docker compose down -v
--       docker compose up -d postgres
--
--   WARNING: `down -v` DESTROYS the `amicu_pgdata` volume, i.e. every
--   workspace, account, token, config, and state snapshot it holds. Only
--   acceptable for disposable local/dev data; never run it against a volume
--   holding real self-host data without a separate backup.
-- ============================================================================
DO $$
BEGIN
    IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'amicu_app') THEN
        CREATE ROLE amicu_app LOGIN PASSWORD 'amicu_app'
            NOSUPERUSER NOCREATEDB NOCREATEROLE NOBYPASSRLS;
    END IF;
END
$$;

-- amicu_app owns the public schema, so it can CREATE the tables/indexes it
-- migrates at startup (amicu_store::run_migrations connects and migrates as
-- amicu_app) and therefore owns every object those migrations create.
-- Ownership without BYPASSRLS/SUPERUSER is exactly what FORCE ROW LEVEL
-- SECURITY needs in order to bind the policies to the app's own connection
-- rather than being a no-op.
ALTER SCHEMA public OWNER TO amicu_app;

-- No further GRANT is needed: amicu_app already owns `public` (so it has
-- CREATE there), and CONNECT on the database is granted to PUBLIC by
-- default. A database-level `GRANT ALL PRIVILEGES` would only add unused
-- privileges (e.g. database-level CREATE), which cuts against the
-- least-privilege point of this role.
