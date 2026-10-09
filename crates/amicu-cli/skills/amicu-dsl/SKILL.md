---
name: amicu-dsl
description: Use when authoring, editing, or reviewing Amicu HCL (`amicu.hcl`, `*.amicu.hcl`) - writing resource blocks, wiring references between model/hardware/deployment, declaring variables or secret references, and formatting. Triggers when the user asks to write, change, or review Amicu configuration.
---

# Amicu DSL

Amicu configuration is HCL. Each file holds `resource "<kind>" "<name>" {}`
blocks; ids cross-reference with `<kind>.<name>.id`. Keep changes minimal and
preserve existing structure.

## Authoring workflow

1. Read the existing `*.hcl` before editing; make the smallest change that works.
2. Write or update the resource block.
3. `amicu fmt` to canonicalize.
4. `amicu validate --output json` and resolve every error-severity diagnostic.

## The three core blocks

- `model` - what to serve (source, architecture, artifact, variants).
- `hardware_profile` - where it runs (provider, accelerator/cpu, memory).
- `deployment` - binds a model id to a hardware id and a runtime block.

See `references/model-examples.md`, `references/hardware-examples.md`, and
`references/deployment-examples.md` for concrete, valid blocks.

## References between resources

```hcl
resource "deployment" "chat_local" {
  model    = model.support_chat.id
  hardware = hardware_profile.dev_mac.id
  runtime  = "llama_cpp"
  llama_cpp { context_length = 8192, port = 8000, n_gpu_layers = -1 }
}
```

Referencing an id that no resource defines yields `AMICU-REF-001`.

## Variables and secrets

Put reusable values in `variables.amicu.hcl`. Never inline a secret value:
secrets are typed references (name + optional scope/version) resolved only at
apply time - they must never appear in HCL, plans, logs, or state. If a real
credential shows up in a config, stop and tell the user to move it to a secret
reference.

## Formatting

`amicu fmt` is canonical: fixed indentation, aligned `=`, dropped stray
comments in resource files. Comments-only files (e.g. `variables.amicu.hcl`) are
left untouched. Run `amicu fmt --check` in CI to detect drift.

## Schema discovery

Field names and enums come from `amicu-core` `references/resource-types.md`.
When `amicu schema` ships, prefer it; today, `amicu validate` is the schema gate.

Unknown attributes and blocks are rejected (`AMICU-SCHEMA-002`), with a
`did you mean` hint for near-misses, so a typo can never silently fall back to
a default. Only free-form maps (`env {}`, `labels {}`) accept arbitrary keys.
