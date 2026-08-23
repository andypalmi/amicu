-- State backend storage: canonical graph history, a mutable "current version"
-- pointer, and the state lock. Backs `PostgresStateBackend` (the first
-- production `amicu_state::StateBackendAdapter`).
--
-- Every table is scoped by workspace_id (the tenant boundary, see
-- WorkspaceId) *and* stack_id: one workspace can run several independent
-- state streams (e.g. distinct graphs/environments), each owned by one
-- `PostgresStateBackend` instance keyed by its `AdapterId`. Every query
-- carries both predicates and runs inside a GUC-bound `begin_scoped`
-- transaction so the row-level-security policies below apply.
--
-- created_at/acquired_at/expires_at are Unix seconds (BIGINT), matching the
-- codebase's i64 timestamp convention. No chrono.

-- Append-only snapshot history. A row is inserted by `push`; existing rows are
-- never updated or deleted. `rollback` never touches this table - it only
-- repoints `state_current` at an already-recorded version, so history is
-- never rewritten.
--
-- `seq` is a per-(workspace, stack) monotonically increasing counter that
-- orders history and stands in for a self-referential parent FK. `version` is
-- the backend-minted id ("v{seq}"); the caller-supplied `StateSnapshot::version`
-- on push is intentionally discarded, mirroring the in-memory reference
-- implementation. `checksum` is the "sha256-v1:<hex>" content hash from
-- `amicu_state::snapshot`; `content` is the canonical graph JSON it was
-- computed over.
CREATE TABLE state_snapshots (
    workspace_id   UUID   NOT NULL REFERENCES workspaces(id),
    stack_id       TEXT   NOT NULL,
    seq            BIGINT NOT NULL,
    version        TEXT   NOT NULL,
    parent_version TEXT,
    label          TEXT   NOT NULL,
    recorded_at    TEXT   NOT NULL,
    checksum       TEXT   NOT NULL,
    content        JSONB  NOT NULL,
    created_at     BIGINT NOT NULL,
    PRIMARY KEY (workspace_id, stack_id, seq)
);

-- Looked up by version on `rollback` and referenced by `state_current` below.
CREATE UNIQUE INDEX state_snapshots_version_idx
    ON state_snapshots (workspace_id, stack_id, version);

-- The mutable "current version" pointer. Decoupled from the append-only
-- history above so `rollback` can retarget it without mutating or duplicating
-- prior snapshot rows. Absence means no state has been pushed yet for this
-- (workspace, stack).
CREATE TABLE state_current (
    workspace_id UUID   NOT NULL REFERENCES workspaces(id),
    stack_id     TEXT   NOT NULL,
    version      TEXT   NOT NULL,
    PRIMARY KEY (workspace_id, stack_id),
    FOREIGN KEY (workspace_id, stack_id, version)
        REFERENCES state_snapshots (workspace_id, stack_id, version)
);

-- The state lock: one lease per (workspace, stack). `lock_token` is a
-- backend-minted, unpredictable u64 reinterpreted as i64 bit-for-bit (see
-- `PostgresStateBackend::lock`), matching the anti-forgery requirement on
-- `LockHandle`. A lease is re-acquirable once `expires_at` has passed, even if
-- never explicitly unlocked (crash/timeout recovery).
CREATE TABLE state_locks (
    workspace_id UUID   NOT NULL REFERENCES workspaces(id),
    stack_id     TEXT   NOT NULL,
    holder       TEXT   NOT NULL,
    lock_token   BIGINT NOT NULL,
    acquired_at  BIGINT NOT NULL,
    expires_at   BIGINT NOT NULL,
    PRIMARY KEY (workspace_id, stack_id)
);

-- Row-level security backstop (see 0003 for the rationale on FORCE + the GUC;
-- the query-level workspace_id predicate on every statement remains the
-- primary, always-on defense).
ALTER TABLE state_snapshots ENABLE ROW LEVEL SECURITY;
ALTER TABLE state_snapshots FORCE ROW LEVEL SECURITY;
CREATE POLICY state_snapshots_workspace_isolation ON state_snapshots
    USING (workspace_id = current_setting('app.current_workspace', true)::uuid)
    WITH CHECK (workspace_id = current_setting('app.current_workspace', true)::uuid);

ALTER TABLE state_current ENABLE ROW LEVEL SECURITY;
ALTER TABLE state_current FORCE ROW LEVEL SECURITY;
CREATE POLICY state_current_workspace_isolation ON state_current
    USING (workspace_id = current_setting('app.current_workspace', true)::uuid)
    WITH CHECK (workspace_id = current_setting('app.current_workspace', true)::uuid);

ALTER TABLE state_locks ENABLE ROW LEVEL SECURITY;
ALTER TABLE state_locks FORCE ROW LEVEL SECURITY;
CREATE POLICY state_locks_workspace_isolation ON state_locks
    USING (workspace_id = current_setting('app.current_workspace', true)::uuid)
    WITH CHECK (workspace_id = current_setting('app.current_workspace', true)::uuid);
