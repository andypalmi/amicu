-- Per-deployment runtime records (handles, endpoints, write-ahead pending
-- records with their stop identity) carried in each state snapshot alongside
-- the graph. See `amicu_state::RuntimeState` (amicu-planning#223).
--
-- Same append-only discipline as `content`: written once by `push`, never
-- updated. Existing rows (and snapshots with no runtime records) hold the
-- default empty object, which reads back as an empty `RuntimeState`.
-- Encrypted the same way as `content` when a StateCipher is configured.
--
-- No new table, so the existing `state_snapshots` row-level-security policy
-- (migration 0009) already covers the column.
ALTER TABLE state_snapshots
    ADD COLUMN runtime JSONB NOT NULL DEFAULT '{}'::jsonb;

-- Rolling-upgrade guard (fail closed for binaries that predate runtime
-- records). An old binary neither reads nor writes `runtime`, so its INSERT
-- takes the bare `'{}'` default - and would silently drop every record the
-- current version carries. Upgraded writers always write a non-empty object
-- (`{"deployments":{}}` at minimum, or an encrypted wrapper), never the bare
-- default. So: once a stack's current version has been written by an
-- upgraded writer (`runtime <> '{}'`), refuse any snapshot insert, and any
-- repoint of `state_current`, that would land on a bare `'{}'` version.
-- Stacks never touched by an upgraded writer are unaffected. (An old binary's
-- `pull` of a version carrying records already fails: such versions use the
-- `sha256-v2` checksum, which it cannot verify.)
CREATE FUNCTION state_runtime_guard_snapshot() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.runtime = '{}'::jsonb AND EXISTS (
        SELECT 1
        FROM state_current c
        JOIN state_snapshots s
          ON s.workspace_id = c.workspace_id
         AND s.stack_id = c.stack_id
         AND s.version = c.version
        WHERE c.workspace_id = NEW.workspace_id
          AND c.stack_id = NEW.stack_id
          AND s.runtime <> '{}'::jsonb
    ) THEN
        RAISE EXCEPTION
            'state runtime records would be dropped by a writer that predates them; upgrade all writers'
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER state_snapshots_runtime_guard
    BEFORE INSERT ON state_snapshots
    FOR EACH ROW EXECUTE FUNCTION state_runtime_guard_snapshot();

CREATE FUNCTION state_runtime_guard_current() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'UPDATE' AND NEW.version <> OLD.version AND EXISTS (
        SELECT 1 FROM state_snapshots n
        WHERE n.workspace_id = NEW.workspace_id
          AND n.stack_id = NEW.stack_id
          AND n.version = NEW.version
          AND n.runtime = '{}'::jsonb
    ) AND EXISTS (
        SELECT 1 FROM state_snapshots o
        WHERE o.workspace_id = OLD.workspace_id
          AND o.stack_id = OLD.stack_id
          AND o.version = OLD.version
          AND o.runtime <> '{}'::jsonb
    ) THEN
        RAISE EXCEPTION
            'state runtime records would be dropped by a writer that predates them; upgrade all writers'
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER state_current_runtime_guard
    BEFORE UPDATE ON state_current
    FOR EACH ROW EXECUTE FUNCTION state_runtime_guard_current();
