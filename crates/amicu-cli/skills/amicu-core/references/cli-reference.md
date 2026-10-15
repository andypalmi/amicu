# CLI reference

## Implemented commands

| Command | Effect |
|---|---|
| `amicu init [--example [mac\|linux-cuda\|linux-cpu\|modal]]` | Scaffold `variables.amicu.hcl`, `.amicu/` and an `amicu.hcl` holding only a comment header. `--example` writes a working starter instead; a bare `--example` detects macOS (`mac`), Linux with `nvidia-smi` (`linux-cuda`) or other Linux (`linux-cpu`). Local starters declare this machine's memory and pick a Qwen3 GGUF that fits; on an Intel Mac `mac` writes a `local_cpu` profile with `n_gpu_layers = 0`. Never overwrites a file you edited. Same starters in `examples/`. |
| `amicu fmt [--check]` | Canonically format `*.hcl`. `--check` reports drift (exit 1) without writing. |
| `amicu validate [--fix] [--output json]` | Structural, reference, and capacity checks. `--fix` applies safe deterministic repairs (re-formatting only). |
| `amicu plan [--output json]` | Compute the actions the next apply would take, diffed against state. |
| `amicu apply [--auto-approve]` | Execute the reviewed plan. Prompts for approval unless `--auto-approve`. |
| `amicu doctor [--output json]` | Environment + project diagnostics. |
| `amicu catalog hardware\|presets\|fits [--output json]` | Read-only, offline. `hardware`: known GPUs and Apple Silicon chips with memory sizes. `presets`: the wizard presets. `fits`: which starter models and presets fit this machine's memory. Works without a project. |
| `amicu skill export --target <t> [--path <dir>]` | Render agent skills for a coding agent. Works without a project. |
| `amicu modal <subcommand>` | Operate a Modal deployment; see "Modal commands" below. Works without a project. |

`--output json|human` is global; default `human`.

## Modal commands

Project-independent (no `*.hcl` needed). They read `MODAL_TOKEN_ID` /
`MODAL_TOKEN_SECRET` from the environment (never flags). `<app>` is the Modal
app name recorded in state: `amicu-<project>-<deployment>-<digest>`
(apps made by older versions: `amicu-` + the deployment id). See amicu-dsl `references/modal-deployment.md`.

| Command | Prints | Exit |
|---|---|---|
| `amicu modal auth-check` | `authenticated` (plus any `warning:` / `server warning:` lines Modal returns) | 0 on success; non-zero `error:` if credentials are missing (`MODAL_TOKEN_ID is not set`) or rejected |
| `amicu modal status <app>` | `app <app> (<app-id>): <state>`, then per Function `<name> (<id>): running=N backlog=N|n/a` and a `url:` line when it has one; `no functions found` if empty | 0; 1 with `no deployment named <app>` |
| `amicu modal logs <app> [--follow] [--since <entry-id>]` | Lines as `[stdout|stderr|info] text`. Without `--follow`, one call held open ~5s (or until the app ends), then returns. `--follow` streams until the app ends or Ctrl-C. `--since` resumes just past a log entry id | 0; 1 with `no deployment named <app>` |
| `amicu modal metrics <app> [--watch] [--interval N]` | A snapshot: `metrics <app> (vLLM|llama.cpp)`, requests, tokens, tokens/s, kv cache, preemptions, e2e latency, ttft, per token (`n/a` where a series is absent). `--watch` prints a new snapshot every `N` seconds (default 5, min 1) and derives tokens/s from successive scrapes; without it tokens/s is `n/a (run with --watch)`. Needs `MODAL_PROXY_TOKEN_ID`/`MODAL_PROXY_TOKEN_SECRET`; a scaled-to-zero app is woken by the scrape | 0; 1 with `no deployment named <app>` or `<app> has no URL to scrape`; non-zero `error:` if the scrape fails |
| `amicu modal stop <app>` | `stopped <app> (<app-id>)`; inside a project that holds the deployment, also `removed deployment <id> from local state` | 0; 1 with `no deployment named <app>` |

## Exit codes

- `0` - success / valid / no error-severity diagnostics.
- `1` - diagnostics with severity `error`, or `fmt --check` drift.
- non-zero from `error:` line - an operational failure (I/O, parse).

## Diagnostic contract

`validate`/`plan`/`doctor --output json` emit a stable report:
`{ "version": "v1", "valid": bool, "diagnostics": [ { "code", "severity", "message", ... } ] }`.
Codes follow `AMICU-<CATEGORY>-<NNN>`; categories are `PARSE`, `SCHEMA`, `REF`,
`CAP`, `MEM`, `POLICY`, `STATE`, `PROVIDER`. See amicu-troubleshooting for the
error-code catalog.

## State

Local state lives in `.amicu/state/state.json` and records the applied graph and
started endpoints. Treat it as sensitive; never edit by hand. Writes are atomic
(temp file, then rename). For Modal deployments, `plan`/`apply` reconcile state
with the live apps first; see amicu-dsl `references/modal-deployment.md`.
