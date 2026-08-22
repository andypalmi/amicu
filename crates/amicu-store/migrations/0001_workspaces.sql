-- Workspaces: the control-plane isolation boundary (unified with the
-- Enterprise org). A solo self-host user is a workspace-of-one; the nil-UUID
-- workspace is the well-known single-tenant default (see WorkspaceId::DEFAULT).
--
-- created_at is Unix seconds (BIGINT), matching the codebase's i64 timestamp
-- convention. No chrono.
CREATE TABLE workspaces (
    id         UUID PRIMARY KEY,
    slug       TEXT UNIQUE NOT NULL,
    name       TEXT NOT NULL,
    tier       TEXT NOT NULL,
    created_at BIGINT NOT NULL
);
