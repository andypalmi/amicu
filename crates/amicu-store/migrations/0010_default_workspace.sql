-- Seed the well-known single-tenant workspace (the nil UUID; see
-- WorkspaceId::DEFAULT). This is NOT part of the new identity model - no user
-- references it and sign-up never uses it. It exists solely for the still
-- single-tenant self-host stores that have not been reworked onto claim-bound
-- tenancy yet: the config store (0008) and the state backend (0009) both scope
-- to WorkspaceId::DEFAULT on a self-host deployment and FK to workspaces(id),
-- so this row must exist for their first insert to satisfy the foreign key.
-- (Threading a real per-request workspace into those stores is
-- andypalmi/amicu-planning#116, out of scope here.) Idempotent.
--
-- created_at is Unix seconds (BIGINT); 0 (the epoch) marks it as the seeded
-- bootstrap workspace rather than a user-provisioned one.
INSERT INTO workspaces (id, slug, name, tier, created_at)
VALUES (
    '00000000-0000-0000-0000-000000000000',
    'default',
    'Default workspace',
    'community',
    0
)
ON CONFLICT (id) DO NOTHING;
