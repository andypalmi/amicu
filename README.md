# Amicu

Manage open LLMs and deploy them as code: locally or on cloud GPUs.

Write HCL that describes a model and where it runs. `amicu plan` checks that it fits in memory, `amicu apply` starts it and hands you an OpenAI-compatible endpoint URL and a ready-to-copy `curl` command, and `amicu destroy` removes it. The endpoint key is one you set with `api_key = secret(...)` (required off-machine). amicu records a deployment in state before starting it and only removes it after the stop is confirmed, so `destroy` can find what may still be running. If you know Terraform, the workflow is the same.

## Example

```hcl
resource "model" "qwen35_27b" {
  source {
    registry  = "hugging_face"
    reference = "unsloth/Qwen3.5-27B-GGUF"
  }
  architecture   = "qwen35"
  modalities     = ["text"]
  artifact {
    format       = "gguf"
    quantization = "q4_k_m"
  }
  context_length = 262144
  n_layers       = 64
  n_kv_layers    = 16
  n_kv_heads     = 4
  head_dim       = 256
  variant {
    quantization = "q4_k_m"
    file         = "Qwen3.5-27B-Q4_K_M.gguf"
    size         = "16740812704B"
  }
}

resource "hardware_profile" "local_dev" {
  provider = "local_apple"
  accelerator {
    kind   = "apple_silicon"
    model  = "m4_pro"
    count  = 1
    memory = "48GiB"
  }
  memory = "48GiB"
}

resource "deployment" "qwen35_27b_local" {
  model    = model.qwen35_27b.id
  hardware = hardware_profile.local_dev.id
  runtime  = "llama_cpp"
  llama_cpp {
    context_length = 262144
    port           = 8000
    n_gpu_layers   = -1
    extra_args     = ["--n-predict", "65536"]
  }
}
```

```bash
amicu init --example   # scaffolds a config like the above, sized to this machine (plain `amicu init` writes an empty file)
amicu plan      # checks it fits in memory and shows what apply would do, diffed against state
amicu apply     # shows the plan, asks for approval, starts the model, checks the endpoint
amicu destroy   # stops and removes the recorded deployments (asks for approval)
```

Point the `hardware_profile` at `provider = "modal"` instead and the same deployment runs on a cloud GPU (see [Deploy to Modal](#deploy-to-modal)). Step by step: [quickstart](https://amicu.dev/docs/quickstart).

More starters are in [`examples/`](examples): `mac`, `linux-cuda`, `linux-cpu` and `modal`.

Local runs need the GGUF weights on disk first; see [Get the GGUF](https://amicu.dev/docs/quickstart#get-the-gguf) in the quickstart.

## Where it runs

| Target | Runtimes | Status |
|---|---|---|
| Your Mac (launchd) | llama.cpp | Supported |
| Linux (systemd) | llama.cpp, vLLM (GPU) | Supported |
| Modal (serverless GPU) | llama.cpp, vLLM | Supported |
| Hetzner, OVHcloud, Scaleway, RunPod, SSH hosts | n/a | Preview: adapters built, not wired into `apply` |
| AWS, Azure, GCP, Nebius | n/a | Catalog and pricing data only; `apply` fails with an error |

Local runs target macOS, Linux and Windows. A provider `apply` cannot deploy to fails with a clear error; it never falls back to running locally.

## Why amicu

- **Your models, on infrastructure you chose.** Your prompts go to your own endpoint, not a model API provider. Data residency is a policy you write: a `data_class` plus a region (for example Modal `eu-west`), checked at plan time. A Modal deployment runs on Modal; the point is that you choose where.
- **Config in git, with a reviewable plan.** Every change is a plan against recorded state that you read before it runs, not a pile of imperative scripts.
- **Built to avoid orphaned GPUs.** `apply` records a deployment in state before it starts anything, and it leaves state only after its stop is confirmed, so `destroy` is designed to find what may still be running.

## Install

macOS and Linux (arm64 and x86_64), and Windows (x86_64).

```bash
curl -LsSf https://amicu.dev/install.sh | sh
```

The installer downloads a prebuilt binary and puts `amicu` in `~/.local/bin`. It edits your shell profile, so open a new shell before running `amicu`.

On Windows, use the PowerShell installer:

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://amicu.dev/install.ps1 | iex"
```

With Homebrew:

```bash
brew install andypalmi/tap/amicu
```

From source (needs Rust 1.97 or newer):

```bash
cargo install --locked --git https://github.com/andypalmi/amicu amicu-cli   # installs into ~/.cargo/bin
```

Or clone and build:

```bash
git clone https://github.com/andypalmi/amicu.git
cd amicu
cargo build --release -p amicu-cli   # binary: ./target/release/amicu
```

## Deploy to Modal

`amicu apply` can deploy a model to Modal (serverless GPU) when the hardware profile has `provider = "modal"`; both `vllm` and `llama_cpp` runtimes work, and the model must come from Hugging Face.

```bash
export MODAL_TOKEN_ID=...  MODAL_TOKEN_SECRET=...              # control plane
export MODAL_PROXY_TOKEN_ID=...  MODAL_PROXY_TOKEN_SECRET=...  # calling the endpoint
export CHAT_API_KEY=$(openssl rand -hex 32)                    # the endpoint's own API key
```

```hcl
resource "hardware_profile" "modal_l4" {
  provider = "modal"
  accelerator {
    kind   = "gpu"
    model  = "l4"
    count  = 1
    memory = "24GiB"
  }
  memory = "32GiB"
}
```

Bind a model to it with a `deployment` as in the quickstart, plus an `api_key` secret reference (required off-machine, see below), then `amicu plan` and `amicu apply`:

```hcl
resource "deployment" "chat" {
  model    = model.qwen35_27b.id
  hardware = hardware_profile.modal_l4.id
  runtime  = "vllm"
  api_key  = secret("CHAT_API_KEY")   # read from the environment variable of that name at apply time
  vllm {
    context_length = 8192
    port           = 8000
  }
}
```

Inspect with `amicu modal status|logs|metrics|stop <app>`, where the app is `amicu-` plus the deployment id with `_` as `-`. The full guide (llama.cpp GGUF example, multi-GPU, metrics, state) is the `modal-deployment.md` reference in the `amicu-dsl` skill: `crates/amicu-cli/skills/amicu-dsl/references/modal-deployment.md`, also available via `amicu skill export`.

## Endpoint API keys

Every deployment can declare an `api_key = secret("NAME")`. It is a reference, never a value: a literal string is rejected (`AMICU-POLICY-004`), `plan` shows `api_key: ******` and only checks that the variable is set (an unset one fails `plan` and `apply` with `AMICU-POLICY-002`, before anything starts), and `apply` reads the value from the environment variable `NAME` and hands it to the runtime through its environment (`LLAMA_API_KEY` for llama.cpp, `VLLM_API_KEY` for vLLM), never argv, logs, state, or plan output. Users cannot set those variables or `--api-key` themselves through `extra_args`/`env`.

- **Required off-machine.** A deployment on any provider other than `local`, `local_apple`, `local_cpu` and `on_prem` (Modal today) must have an `api_key`; `amicu validate`/`plan` fail with `AMICU-POLICY-007` otherwise. Local loopback deployments may omit it.
- **Checked after deploy.** For every endpoint `apply` starts, it sends an unauthenticated `GET /v1/models` (no key, no provider headers). A `401`/`403` passes; a `2xx` means the endpoint is open, so it is stopped and apply fails (a failed stop is reported with the command to run by hand). It then sends the key (plus the Modal proxy headers) and requires a `2xx`. Loopback endpoints without a key skip the check.
- **Calling it.** After a successful apply the output includes a ready-to-copy `curl` and OpenAI SDK snippet that refer to the key and, on Modal, the `Modal-Key`/`Modal-Secret` proxy token by environment variable name.

## Platform notes

- **llama.cpp, local:** macOS (Apple Silicon), Linux/CPU and Linux with an NVIDIA GPU (CUDA), and Windows x86_64 (CPU, CUDA or Vulkan), run as a native process (launchd on macOS, a persistent systemd user unit on Linux, a per-user Task Scheduler task that starts at logon on Windows, no admin rights needed). On Windows, install `llama-server.exe` with `winget install llama.cpp` or from the llama.cpp releases (CUDA or Vulkan zip) and put it on `PATH`; vLLM is Linux-only. On Linux a deployment comes back after a reboot at boot only if user lingering is on (`sudo loginctl enable-linger $USER`), otherwise at your next login; `apply` prints a hint when lingering is off. For GPU offload the `llama-server` on `PATH` must be a CUDA build (`n_gpu_layers = -1` offloads all layers; a CPU-only build silently runs on the CPU), and the memory-fit check is made against the card's VRAM.
- **vLLM:** NVIDIA CUDA, AMD ROCm and Intel XPU GPUs, locally as a native single process (systemd on Linux).

## Development

### Development database

The persistence layer (`amicu-store`) is Postgres-only. For local development and
self-host, bring up the bundled Postgres with Docker Compose:

```bash
docker compose up -d postgres
export DATABASE_URL=postgres://amicu_app:amicu_app@localhost:5432/amicu
```

That `DATABASE_URL` is the dev/self-host connection string used by the store and its
migration integration test. It connects as `amicu_app`, a dedicated non-superuser role
provisioned by `docker/postgres-init/01-app-role.sql` on first container startup - not as
`amicu`, the Postgres bootstrap superuser (`POSTGRES_USER`/`POSTGRES_PASSWORD` in
`docker-compose.yml`). This matters: `amicu-store`'s migrations enable `FORCE ROW LEVEL
SECURITY` on every workspace-scoped table as a backstop behind the query-level
`workspace_id` predicate on every statement, but a superuser (or any role with
`BYPASSRLS`) bypasses RLS entirely, even with `FORCE`. Connecting as the superuser would
make that backstop a permanent no-op. A production/self-host deployment should provision
its own equivalent least-privilege role rather than ever connecting as a database
superuser - `docker/postgres-init/01-app-role.sql` is a template for the required GRANTs.

**Already have an `amicu_pgdata` volume from before `amicu_app` existed?** The init
script only runs against an empty volume, so it will not retroactively create the role
or transfer ownership of your existing tables. Either:

- keep your data: connect as the superuser (`psql postgres://amicu:amicu@localhost:5432/amicu`)
  and run the `CREATE ROLE` / `ALTER SCHEMA ... OWNER TO` / `REASSIGN OWNED BY amicu TO
  amicu_app` sequence documented at the top of `docker/postgres-init/01-app-role.sql`, or
- start clean: `docker compose down -v && docker compose up -d postgres`. **`down -v`
  destroys the `amicu_pgdata` volume** - every workspace, account, token, config, and
  state snapshot in it - so only do this with disposable local/dev data.

This single-role shape - one role both owns the schema and runs the server, which
migrates at boot - is a **dev/self-host convenience only**. `amicu-server` still accepts
it (`AMICU_ENV=development`, the default: the RLS-bypass startup guard only warns), but
production should use the two-role split below.

### Production database (two-role split)

`AMICU_ENV=production` changes two things from the dev
shape above:

1. `amicu-server` no longer migrates at boot. Apply migrations separately, as a
   schema-OWNING role, with the new `amicu-server migrate` subcommand:
   ```bash
   DATABASE_URL=postgres://amicu_owner:...@host/amicu amicu-server migrate
   ```
   It connects, applies any pending migration, prints how many it applied, and exits -
   no RLS guard runs (the migrator is expected to own the schema).
2. The running service must connect as a **separate, non-owning** role - `amicu_app` -
   with neither `SUPERUSER` nor `BYPASSRLS`, direct or inherited:
   ```bash
   DATABASE_URL=postgres://amicu_app:...@host/amicu AMICU_ENV=production amicu-server
   ```
   On boot it verifies the schema has no pending migrations (failing fast, rather than
   racing concurrent replicas against each other or serving against a stale schema, if
   step 1 was skipped) and refuses to start if the connected role would bypass row-level
   security - superuser/`BYPASSRLS` (including inherited via role membership - this
   generically catches a managed Postgres's own privileged front role, e.g. Cloud SQL's
   `cloudsqlsuperuser` or Supabase's `postgres`/`supabase_admin`, with no role name
   hardcoded), or ownership of any `FORCE ROW LEVEL SECURITY` table.

`ops/sql/provision_app_role.sql` provisions both roles (`amicu_owner` + `amicu_app`) with
exactly this split, including the `ALTER DEFAULT PRIVILEGES` grants so a later migration
adding new tables does not need a manual re-grant. See its header comment for the full
walkthrough and the managed-Postgres note above in more detail.

Self-hosted state is control-plane metadata and secret *references* only - never plaintext
secret values (see `amicu-core`'s `SecretValue`/`SecretRef` split). `amicu-store` does not
encrypt it at the application layer by default: **self-host operators are responsible for
their own database's encryption at rest** (disk/volume encryption, filesystem encryption, or
your Postgres provider's own offering). The hosted SaaS backend plugs application-level
envelope encryption into `PostgresStateBackend` via the `amicu_state::StateCipher` seam; see
that module and `amicu-store`'s `state` module docs for details.

## Status

Early stage. Working today: the HCL DSL and parser, the CLI (`init`, `fmt`, `validate`, `plan`, `apply`, `destroy`, `doctor`, `skill export`), the planner and local state (with write-ahead stop records), and the local and Modal providers above. State is local only. Not yet built: the browser UI and hosted service. Expect breaking changes to the DSL and CLI.

## Learn more

- Documentation: https://amicu.dev/docs
- Website: https://amicu.dev
- Contributing: see [CONTRIBUTING.md](./CONTRIBUTING.md) for setup and the checks CI runs. Report vulnerabilities privately as described in [SECURITY.md](./SECURITY.md).

## License

AGPL-3.0 for the core.
