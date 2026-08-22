-- Seed the well-known single-tenant workspace (the nil UUID; see
-- WorkspaceId::DEFAULT). accounts.workspace_id and the token tables all FK to
-- workspaces(id), so without this row the first self-host account/token insert
-- would fail the foreign key. Idempotent.
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
