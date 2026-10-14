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

## Modal

Setup and details: amicu-dsl `references/modal-deployment.md`. App name is the one recorded in state
(`amicu-<project>-<deployment>-<digest>`; older apps: `amicu-` + deployment id); inspect with
`amicu modal status|logs <app>`.

- `MODAL_TOKEN_ID is not set` (or `MODAL_TOKEN_SECRET`) - export the control
  plane token pair. Verify with `amicu modal auth-check`.
- `503` from the endpoint - normal while the container cold-starts; the first
  run also downloads the weights. Watch `amicu modal logs <app> --follow` and
  retry. Warm starts reuse the cache volumes and are much faster.
- `401`/`403` from the endpoint - the proxy tokens are missing or wrong. Send
  `Modal-Key: $MODAL_PROXY_TOKEN_ID` and `Modal-Secret: $MODAL_PROXY_TOKEN_SECRET`
  (not `Authorization: Bearer`). `amicu modal metrics` needs them too.
- `NCCL error: unhandled cuda error` on a multi-GPU vLLM deployment (L4 and
  other non-NVLink GPUs, intermittent, at the first all-reduce) - add
  `env { NCCL_P2P_DISABLE = "1" }` to the `vllm` block and re-apply.
- `apply` says "No changes" but nothing is running - the app was stopped
  behind amicu's back. Builds that include the state refresh
  detect this, print a `drift:` line and recreate it. On an older
  build, delete `.amicu/state` (or run `amicu modal stop <app>` inside the
  project) and re-apply.
- Qwen3.5-style thinking models return `content: null` with
  `finish_reason=length` - `max_tokens` was spent on reasoning. Raise
  `max_tokens` (or disable thinking in the chat template).
- `plan`/`apply` print `warning: could not check Modal for live apps` - the
  Modal lookup failed (credentials or network); local state was used as-is.

## Safety

- Never run destructive state commands without explicit approval.
- Never paste secret values into logs or issues when sharing errors.

## Reference

- `references/error-codes.md` - the code catalog and what each means.
