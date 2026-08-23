-- HCL configs and their immutable version history. `configs` is the mutable
-- pointer (name -> current_version); `config_versions` is an append-only log
-- of every revision the store never UPDATEs or DELETEs. Content is stored
-- exactly as the caller supplies it; canonicalizing HCL is the API layer's
-- job (a later sub-issue), not the store's.
--
-- Workspace-scoped with the same RLS backstop as the auth tables (see 0003
-- for the full rationale, including the superuser/BYPASSRLS caveat).
--
-- created_at/updated_at are Unix seconds (BIGINT), matching the codebase's i64
-- timestamp convention. No chrono.
CREATE TABLE configs (
    workspace_id    UUID NOT NULL REFERENCES workspaces(id),
    id              UUID NOT NULL,
    name            TEXT NOT NULL,
    created_at      BIGINT NOT NULL,
    updated_at      BIGINT NOT NULL,
    current_version INT NOT NULL,
    PRIMARY KEY (workspace_id, id),
    UNIQUE (workspace_id, name)
);

-- Append-only version history: a version_no is minted once and its row is
-- never UPDATEd or DELETEd. Rollback mints a brand-new version that copies an
-- older one's content rather than rewriting history, so every past revision
-- stays reachable via list_versions/get_version.
--
-- content_sha256 is computed server-side by the store (never trusted from the
-- caller), so it can never disagree with `content`.
--
-- created_by is a soft, nullable reference to an account: not every writer is
-- an authenticated account (e.g. a rollback), and accounts are identified by
-- workspace_id+id, so a hard FK here is deliberately skipped in favor of the
-- workspace_id predicate + RLS already enforced on both tables.
CREATE TABLE config_versions (
    workspace_id   UUID NOT NULL REFERENCES workspaces(id),
    config_id      UUID NOT NULL,
    version_no     INT NOT NULL,
    content        TEXT NOT NULL,
    content_sha256 BYTEA NOT NULL,
    created_at     BIGINT NOT NULL,
    created_by     UUID,
    message        TEXT,
    PRIMARY KEY (workspace_id, config_id, version_no),
    FOREIGN KEY (workspace_id, config_id) REFERENCES configs (workspace_id, id)
);

-- Every list_versions / get_current lookup filters by (workspace_id, config_id).
CREATE INDEX config_versions_config_idx ON config_versions (workspace_id, config_id);

-- Row-level security backstop (see 0003 for the rationale on FORCE + the GUC).
ALTER TABLE configs ENABLE ROW LEVEL SECURITY;
ALTER TABLE configs FORCE ROW LEVEL SECURITY;
CREATE POLICY configs_workspace_isolation ON configs
    USING (workspace_id = current_setting('app.current_workspace', true)::uuid)
    WITH CHECK (workspace_id = current_setting('app.current_workspace', true)::uuid);

ALTER TABLE config_versions ENABLE ROW LEVEL SECURITY;
ALTER TABLE config_versions FORCE ROW LEVEL SECURITY;
CREATE POLICY config_versions_workspace_isolation ON config_versions
    USING (workspace_id = current_setting('app.current_workspace', true)::uuid)
    WITH CHECK (workspace_id = current_setting('app.current_workspace', true)::uuid);
