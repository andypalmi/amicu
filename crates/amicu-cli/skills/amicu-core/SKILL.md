---
name: amicu-core
description: Use when starting any work on an Amicu project - setting it up, discovering what resources and commands exist, or running the core validate/plan/apply workflow. Always load this first; it carries the safety rules every other Amicu skill depends on. Triggers on amicu.hcl, `amicu` CLI usage, or "Terraform for AI inference".
---

# Amicu Core

Amicu is declarative infrastructure for AI inference: you describe models,
hardware, and deployments in HCL, then `validate` → `plan` → `apply`. It is
agent-native but not vendor-locked - discover the schema and capabilities
through the CLI rather than guessing configuration.

## Safety rules (non-negotiable)

- Never run `amicu apply` without a reviewed plan and explicit user approval.
- Never run destructive state commands without explicit approval.
- Never reveal, commit, or transmit secret values.
- Always validate before plan, and plan before apply.
- Preserve existing configuration and make minimal changes.

## Core workflow

```bash
amicu init                 # scaffold amicu.hcl + .amicu/ in the current dir
amicu fmt                  # canonical formatting (--check to detect drift only)
amicu validate --output json   # structural + reference + capacity checks
amicu plan --output json       # actions the next apply would take
amicu apply                    # execute the reviewed plan (asks for approval)
amicu doctor --output json     # environment + project diagnostics
```

Prefer `--output json` when acting programmatically; every automation command
emits a stable contract. A non-zero exit means diagnostics with severity error.

## Command discovery

Only `init`, `fmt`, `validate`, `plan`, `apply`, `doctor`, and
`skill export` are implemented today. Commands the workflow references but that
are not yet available (`schema`, `capabilities`, `models`, `presets`,
`recommend`) are planned; do not invent flags for them or assume they run.

Run `amicu <command> --help` to confirm the real surface before scripting it.

## Schema discovery

Resource kinds are `model`, `hardware_profile`, `deployment`, `routing_policy`,
`data_class`, `governance_rule`, `budget`, `tenant`, `use_case`. See
`references/resource-types.md` for their fields and `references/cli-reference.md`
for exit codes and the diagnostic contract. When `amicu schema` ships it is the
authoritative source; until then treat validate diagnostics as the schema check.

## Routing to other skills

- Writing/reviewing HCL → `amicu-dsl`
- Local llama.cpp / Apple Silicon / CPU → `amicu-local`
- vLLM / Kubernetes / cloud GPUs → `amicu-production`
- Picking a model + quantization → `amicu-model-selection`
- Routing, residency, quota, budgets, tenants → `amicu-routing`
- `doctor`, failures, recovery → `amicu-troubleshooting`
- Guided requirement gathering → `amicu-presets`
