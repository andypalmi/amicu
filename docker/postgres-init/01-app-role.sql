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
-- This file runs once, at first container initialization of an EMPTY data
-- volume (the postgres image's docker-entrypoint-initdb.d convention: it is
-- skipped entirely once `amicu_pgdata` already has a database). If you have
-- an existing `amicu_pgdata` volume from before this role existed, either
-- `docker compose down -v` to reinitialize from empty, or run the statements
-- below by hand against it.
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
GRANT ALL PRIVILEGES ON DATABASE amicu TO amicu_app;
