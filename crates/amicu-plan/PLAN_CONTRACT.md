# Amicu Plan Contract

`amicu plan` renders a single, stable JSON object - the **plan contract** -
that the CLI displays and that the API and UI consume unchanged. This document
describes that contract and its stability guarantee.

The contract is defined in [`src/contract.rs`](src/contract.rs) as the
[`Plan`] type and derives a JSON Schema via
[`schemars`](https://docs.rs/schemars). The schema is exercised in
[`tests/contract.rs`](tests/contract.rs), which validates a representative
`Plan` against the generated schema using the
[`jsonschema`](https://docs.rs/jsonschema) crate.

## Versioning and stability

- `Plan.version` is a stable string, currently `"v1"` (`Plan::VERSION`).
- Within a version the contract only changes in backward-compatible ways:
  new **optional** fields may be added; existing fields are neither removed,
  renamed, nor retyped.
- Any breaking change ships under a new `version` value.
- The self-contained field types (plain strings and small structs for
  resource references, artifacts, hardware, provider and region) keep the wire
  shape independent of internal domain modelling.

## Shape

| Field | Type | Notes |
|-------|------|-------|
| `version` | string | Contract version, currently `"v1"`. |
| `actions` | array of `PlannedAction` | Changes in application order. |
| `policy` | `PolicyOutcome`? | Aggregate governance outcome. |

### `PlannedAction`

| Field | Type | Notes |
|-------|------|-------|
| `kind` | `create` \| `update` \| `replace` \| `destroy` | Change kind. |
| `address` | string | Target resource address. |
| `artifact` | `Artifact`? | Selected model artifact. |
| `runtime` | array of `{key, value}` | Runtime / launch settings. |
| `hardware` | string? | Target hardware descriptor. |
| `provider` | string? | Target provider. |
| `region` | string? | Target region. |
| `cost_performance` | `CostPerformance`? | Estimate ranges + provenance. |
| `required_secrets` | array of string | Secret **names** only. |
| `required_action` | string? | Manual step the user must take. |
| `reason` | string? | Why an update is planned. |
| `changes` | array of `PlannedChange` | Per-field differences behind an update, launch fields first. |

### `PlannedChange`

`key`, `op` (`added` \| `removed` \| `changed`), `old` and `new` (string or
null), `restart` (bool) and `redacted` (bool). A field that may hold a secret
or arbitrary user text (env values, `extra_args`, `api_key`, templates,
credential-like keys) is `redacted`: its `old` and `new` are always null.

### `Artifact`

`name`, plus optional `format`, `quantization`, and
`estimated_download_size_bytes`.

### `CostPerformance`

Optional `cost` and `performance` ranges (`{min, max, unit?}`), a list of
`assumptions`, a `provenance` label, and a `confidence` of
`low` \| `medium` \| `high`.

### `PolicyOutcome`

`allowed` (bool), plus lists of `risks` and `denials`.

## Secrets

`required_secrets` carries secret **names only** (`Vec<String>`). Secret
**values never appear** anywhere in a plan. A test asserts the field is a list
of names.
