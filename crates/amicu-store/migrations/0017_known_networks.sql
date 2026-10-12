-- "Known networks" for the per-email failed-sign-in budget
-- (andypalmi/amicu-planning#194). One row per pseudonym(email | client
-- network): written after a SUCCESSFUL sign-in from that network, so that when
-- an account's failed-attempt budget is exhausted (a distributed guessing
-- attack) the owner can still sign in from a network they have used recently
-- while unknown networks are refused. `key` is a keyed hash, never a plaintext
-- email or IP. Not workspace-scoped, no RLS (consulted before authentication),
-- like `rate_limit_buckets` and `login_failures`.
CREATE TABLE known_networks (
    key     TEXT   PRIMARY KEY,
    seen_at BIGINT NOT NULL
);

CREATE INDEX known_networks_seen_at_idx ON known_networks (seen_at);
