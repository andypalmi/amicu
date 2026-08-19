# CLI reference

## Implemented commands

| Command | Effect |
|---|---|
| `amicu init` | Scaffold `amicu.hcl`, `variables.amicu.hcl`, `.amicu/`. Idempotent; never overwrites. |
| `amicu fmt [--check]` | Canonically format `*.hcl`. `--check` reports drift (exit 1) without writing. |
| `amicu validate [--fix] [--output json]` | Structural, reference, and capacity checks. `--fix` applies safe deterministic repairs (re-formatting only). |
| `amicu plan [--output json]` | Compute the actions the next apply would take, diffed against state. |
| `amicu apply [--auto-approve]` | Execute the reviewed plan. Prompts for approval unless `--auto-approve`. |
| `amicu doctor [--output json]` | Environment + project diagnostics. |
| `amicu skill export --target <t> [--path <dir>]` | Render agent skills for a coding agent. Works without a project. |

`--output json|human` is global; default `human`.

## Exit codes

- `0` — success / valid / no error-severity diagnostics.
- `1` — diagnostics with severity `error`, or `fmt --check` drift.
- non-zero from `error:` line — an operational failure (I/O, parse).

## Diagnostic contract

`validate`/`plan`/`doctor --output json` emit a stable report:
`{ "version": "v1", "valid": bool, "diagnostics": [ { "code", "severity", "message", ... } ] }`.
Codes follow `AMICU-<CATEGORY>-<NNN>`; categories are `PARSE`, `SCHEMA`, `REF`,
`CAP`, `MEM`, `POLICY`, `STATE`, `PROVIDER`. See amicu-troubleshooting for the
error-code catalog.

## State

Local state lives in `.amicu/state/state.json` and records the applied graph and
started endpoints. Treat it as sensitive; never edit by hand.
