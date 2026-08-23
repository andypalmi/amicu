---
name: amicu-troubleshooting
description: Use when an Amicu command fails or a deployment is unhealthy - interpreting `AMICU-*` diagnostic codes, running `amicu doctor`, and recovering from validate/plan/apply/state errors. Triggers on error output, non-zero exits, "amicu doctor", failed apply, unhealthy endpoints, or a specific `AMICU-<CATEGORY>-NNN` code.
---

# Amicu Troubleshooting

Diagnose and recover from failures. Start by reading the diagnostic code and its
category, then apply the matching fix - never work around a policy or capacity
error by weakening the config.

## First step: doctor

```bash
amicu doctor --output json
```

Reports environment and project health (runtime binaries, service manager,
config presence). The JSON has `{ "version": "v1", "ok": bool, "checks": [...] }`;
fix any failing check before re-running plan/apply.

## Reading diagnostics

`validate`/`plan --output json` emit diagnostics with a stable
`AMICU-<CATEGORY>-<NNN>` code and severity. An error-severity diagnostic fails
the command (exit 1). Categories: `PARSE`, `SCHEMA`, `REF`, `CAP`, `MEM`,
`POLICY`, `STATE`, `PROVIDER`. See `references/error-codes.md` for each.

## Recovery by category

- `PARSE`/`SCHEMA` - fix HCL syntax/fields, then `amicu fmt` and re-validate.
- `REF` - a referenced id does not exist; correct the `<kind>.<name>.id`.
- `CAP`/`MEM` - model does not fit the runtime/hardware; change quantization,
  context length, or hardware (see amicu-model-selection).
- `POLICY` - residency/governance/budget violation; change provider/region or
  the stack, never the rule, unless the user explicitly changes policy.
- `STATE` - state lock/drift/corruption; do not hand-edit state. Re-plan to see
  drift; only run state-mutating recovery with explicit user approval.
- `PROVIDER` - provisioning/availability; check region capacity and credentials
  (credentials resolve at apply time and never appear in plans or logs).

## Safety

- Never run destructive state commands without explicit approval.
- Never paste secret values into logs or issues when sharing errors.

## Reference

- `references/error-codes.md` - the code catalog and what each means.
