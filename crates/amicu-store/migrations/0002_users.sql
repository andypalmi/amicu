-- Users: GLOBAL identity (replaces the old workspace-scoped `accounts` model).
-- A user is one person, identified by a globally-unique email, independent of
-- any workspace. Membership in a workspace is the separate join
-- (workspace_members); a user may belong to many workspaces, or eventually none.
--
-- This is the control plane: `users` carries NO row-level security. Identity is
-- global and must be resolvable by email/id without a workspace context (that
-- is exactly what sign-in does before any workspace is selected). Tenant
-- isolation lives on the workspace-scoped tables, not here.
--
-- id is a real UUID primary key (not the old opaque `usr_...` surrogate); it is
-- the token subject (`sub`) and the target of the auth token tables' foreign
-- keys, so account deletion can cascade.
--
-- email_verified_at is nullable and holds the Unix-seconds instant the email was
-- confirmed (NULL = unverified). It is a BIGINT rather than the issue's
-- TIMESTAMPTZ deliberately: this crate's sqlx build enables neither the chrono
-- nor the time feature, and every other timestamp in the schema is Unix-seconds
-- BIGINT ("No chrono"). The domain surfaces it as a bool (verified iff non-null).
--
-- personal_workspace_id points at the workspace provisioned for this user at
-- sign-up (where they are Owner). It is the user's billing/tier home: because a
-- workspace-scoped claim (`wsid`) does not exist yet,
-- tier and billing for a user are resolved
-- through this pointer to their one personal workspace. It is a FK into the
-- no-RLS `workspaces` table, so the lookup needs no workspace context. Once the
-- token carries `wsid`, billing/tier become claim-driven and this pointer's
-- role narrows to "the user's default workspace".
--
-- created_at is Unix seconds (BIGINT). No chrono.
CREATE TABLE users (
    id                    UUID PRIMARY KEY,
    email                 TEXT UNIQUE NOT NULL,
    password_hash         TEXT NOT NULL,
    email_verified_at     BIGINT,
    personal_workspace_id UUID NOT NULL REFERENCES workspaces(id),
    created_at            BIGINT NOT NULL
);
