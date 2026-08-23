-- Index for PostgresUserStore::get_by_stripe_customer, which looks up an
-- account by (workspace_id, stripe_customer_id) on every Stripe webhook
-- delivery. Without this index that lookup is a sequential scan of accounts.
-- stripe_customer_id is nullable and most accounts never set it (only
-- Pro/Enterprise billing customers do), so a plain btree index over both
-- columns is small and cheap to maintain; no partial-index WHERE clause is
-- needed for that reason alone, but restricting it to non-null customer ids
-- keeps it that way explicitly and matches the only predicate this lookup
-- ever uses.
CREATE INDEX accounts_stripe_customer_idx
    ON accounts (workspace_id, stripe_customer_id)
    WHERE stripe_customer_id IS NOT NULL;
