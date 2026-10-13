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
