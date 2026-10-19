# CLI reference

## Implemented commands

| Command | Effect |
|---|---|
| `amicu init [--example [mac\|linux-cuda\|linux-cpu\|modal]]` | Scaffold `variables.amicu.hcl`, `.amicu/` and an `amicu.hcl` holding only a comment header. `--example` writes a working starter instead; a bare `--example` detects macOS (`mac`), Linux with `nvidia-smi` (`linux-cuda`) or other Linux (`linux-cpu`). Local starters declare this machine's memory and pick the largest Qwen3.5 GGUF that fits (27B at 32 GiB or more, then 9B at 16, 4B at 8, 2B at 6, 0.8B below; `linux-cpu` starts at 4B), with a context up to the native 262144 that passes the fit check and output capped at 65536 via `extra_args = ["--n-predict", "65536"]`; on an Intel Mac `mac` writes a `local_cpu` profile with `n_gpu_layers = 0`. Never overwrites a file you edited. Same starters in `examples/`. |
| `amicu fmt [--check]` | Canonically format `*.hcl`. `--check` reports drift (exit 1) without writing. |
| `amicu validate [--fix] [--output json]` | Structural, reference, and capacity checks. `--fix` applies safe deterministic repairs (re-formatting only). |
| `amicu plan [--output json]` | Compute the actions the next apply would take, diffed against state. |
| `amicu apply [--auto-approve]` | Execute the reviewed plan. Prompts for approval unless `--auto-approve`. |
| `amicu doctor [--output json]` | Environment + project diagnostics. |
| `amicu catalog hardware\|presets\|fits [--output json]` | Read-only, offline. `hardware`: known GPUs and Apple Silicon chips with memory sizes. `presets`: the wizard presets. `fits`: which starter models and presets fit this machine's memory. Works without a project. |
| `amicu skill export --target <t> [--path <dir>]` | Render agent skills for a coding agent. Works without a project. |
| `amicu modal <subcommand>` | Operate a Modal deployment; see "Modal commands" below. Works without a project. |
| `amicu runpod <subcommand>` | Operate a RunPod Serverless endpoint or Pod recorded in this project's state; see "RunPod commands" below. |

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

## RunPod commands

Run inside the project. `<deployment>` is the deployment id; the endpoint or
pod id comes from state. Management calls use `RUNPOD_API_KEY`.

| Command | Prints | Exit |
|---|---|---|
| `amicu runpod status <deployment>` | `deployment <d>: RunPod endpoint <id> (<name>)`, then `url:`, `workers: running=N idle=N initializing=N throttled=N unhealthy=N (min N, max N)` (plus a scaled-to-zero note), `gpu:` and `data centers:`. A pod: `deployment <d>: RunPod pod <id> (<name>)`, then `status:`, `url:` (proxy URL), `gpu:`, `cloud:`, `data center:` and `cost:` (USD/hr) | 0; non-zero `error:` if the deployment has no RunPod record or the endpoint is gone |
| `amicu runpod logs <deployment> [--tail N]` (`--output json`: one document per worker with its lines) | Each worker's recent lines (default 100, max 5000) as `[<worker>] text` (`[<worker>] (system) text` for host lines), merged by time. A bounded read, no follow. A scaled-to-zero endpoint prints `no RunPod workers are running ...`. A pod: its own lines as `[<pod>] text` | 0 |
| `amicu runpod metrics <deployment> [--watch] [--interval N] [--wake]` | The same snapshot as `modal metrics`, scraped through the gateway with the deployment's `api_key` (from one worker the load balancer picks), then `hardware: n/a (not available on RunPod)`. With no worker up it refuses (a scrape would start a billed worker) unless `--wake`; `--watch` keeps the endpoint warm, and billed, while it runs. A pod is scraped through its proxy URL; it is always on, so `--wake` does not apply | 0; non-zero `error:` if no worker is up without `--wake`, or the scrape fails |
| `amicu runpod stop <deployment> [--lock-timeout SECS]` | Same as `amicu destroy --target <deployment> --auto-approve`: deletes the endpoint (terminates the pod) and its secrets; the deployment leaves state only once that is confirmed | 0; non-zero `error:` with the record kept otherwise |

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
