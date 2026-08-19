# Routing & policy examples

## Data class with EU residency

```hcl
resource "data_class" "company_confidential" {
  sensitivity              = "confidential"
  residency                = ["eu-central", "eu-west"]
  allow_external_providers = false
  handling                 = ["no_logging", "encrypt_at_rest"]
}
```

## Governance rule (EU-only providers)

```hcl
resource "governance_rule" "eu_only" {
  effect            = "allow"
  data_classes      = [data_class.company_confidential.id]
  allowed_providers = ["hetzner", "ovhcloud", "scaleway", "nebius"]
  allowed_regions   = ["eu-central", "eu-west"]
}
```

A deployment on a provider/region outside these lists for that data class is
rejected at plan time (`AMICU-POLICY-*`).

## Tenant and budget

```hcl
resource "tenant" "team_research" {
  display_name = "Research"
  quota {
    max_requests_per_minute = 600
    max_concurrent          = 20
  }
  labels = { cost_center = "rnd" }
}

resource "budget" "research_monthly" {
  tenant           = tenant.team_research.id
  limit            = "2000 EUR"
  period           = "monthly"
  alert_thresholds = [0.5, 0.8, 1.0]
}
```

## Weighted routing with failover

See the `routing_policy` block in the skill: `targets { deployment, weight }`
splits traffic proportionally; `failover { enabled, health_check_interval_secs,
max_failures }` drains unhealthy targets. Use `optimize_for` (`latency`, `cost`,
`quality`) to express intent.
