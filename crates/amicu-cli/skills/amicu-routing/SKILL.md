---
name: amicu-routing
description: Use for Amicu traffic and policy resources - routing policies across deployments, failover, data-residency and governance rules, tenant quotas, and budgets. Triggers on routing_policy, data_class, governance_rule, budget, tenant, residency/EU constraints, quotas, or "route/split traffic between models".
---

# Amicu Routing & Policy

These resources sit above deployments: they route traffic, enforce residency and
governance, and cap spend per tenant.

## Routing policies

```hcl
resource "routing_policy" "chat" {
  aliases     = ["chat", "default"]
  optimize_for = "latency"        # or "cost"/"quality"
  targets {
    deployment = deployment.chat_prod.id
    weight     = 80
  }
  targets {
    deployment = deployment.chat_local.id
    weight     = 20
  }
  failover {
    enabled                    = true
    health_check_interval_secs = 30
    max_failures               = 3
  }
}
```

Weights split traffic; failover shifts away from unhealthy targets.

## Residency & governance

- `data_class` declares `sensitivity`, `residency` (allowed jurisdictions: `eu`,
  `uk`, `us`, `ca`, `apac`), `allow_external_providers`, and `handling`. A
  region's jurisdiction is resolved from the provider catalog and checked
  against this list.
- `governance_rule` allows/denies by `data_classes`, `allowed_providers`, and
  `allowed_regions` (region codes, not jurisdictions).
- The planner rejects any provider/region that violates declared residency or
  allowlists with an `AMICU-POLICY-*` diagnostic - a violating stack never
  reaches apply. Keep EU-residency constraints first-class when required.

## Tenants & budgets

- `tenant` sets `quota` (`max_requests_per_minute`, `max_concurrent`) and labels.
- `budget` caps `limit` per `period` for a `tenant`, with `alert_thresholds`.

## Safety

- Never weaken a residency or governance rule to make a plan pass; fix the stack
  instead. Never inline secrets in any of these blocks.

## Reference

- `references/routing-examples.md` - weighted routing, failover, residency, and
  budget/tenant examples.
