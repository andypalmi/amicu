-- Workspaces: the control-plane tenancy boundary (unified with the Enterprise
-- org). A workspace is the unit a plan/tier and a billing relationship attach
-- to, and the row-level-security boundary every tenant-scoped table keys on.
-- Workspace ids are ALWAYS server-generated (never caller-chosen); provisioning
-- is server-authoritative (see the epic, andypalmi/amicu-planning#127).
--
-- This is the control plane, so `workspaces` itself carries NO row-level
-- security: the registry of workspaces is global (a user must be able to look
-- up the workspace their token/claim points at). Tenant isolation is enforced
-- on the tables that reference a workspace (workspace_members,
-- workspace_invites, configs, state_*, and the auth token tables), not here.
--
-- tier is the workspace's plan; billing (Stripe customer/subscription) attaches
-- to the workspace, not to the global user identity, because the workspace is
-- the billing entity. stripe_customer_id is nullable and set only once a
-- checkout/portal session is opened for the workspace.
--
-- created_at is Unix seconds (BIGINT), matching the codebase's i64 timestamp
-- convention. No chrono.
CREATE TABLE workspaces (
    id                     UUID PRIMARY KEY,
    slug                   TEXT UNIQUE NOT NULL,
    name                   TEXT NOT NULL,
    tier                   TEXT NOT NULL,
    stripe_customer_id     TEXT,
    stripe_subscription_id TEXT,
    created_at             BIGINT NOT NULL
);

-- Look up the workspace that owns an incoming Stripe webhook (which identifies
-- the billing entity only by customer id). Partial: only billing workspaces
-- ever set a customer id.
CREATE INDEX workspaces_stripe_customer_idx
    ON workspaces (stripe_customer_id)
    WHERE stripe_customer_id IS NOT NULL;
