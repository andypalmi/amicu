# Amicu

Terraform for AI inference infrastructure: a declarative HCL DSL plus a Rust planner/apply engine that turns model, hardware, routing, and governance definitions into running llama.cpp (local) and vLLM (production) deployments.

## The problem

Standing up an inference stack today means hand-wiring model downloads, runtime flags, GPU provisioning, traffic routing, and data-residency rules - differently on a laptop than in production, and differently again per provider. Amicu makes that stack declarative, diffable, and reviewable: define it, `plan` the changes, `apply` them. Residency and governance are first-class resources you can pin - EU-only is one policy among many (air-gapped, cost-optimized, multi-cloud).

## What you get

- **One file, laptop to production** - the same HCL describes a local llama.cpp run and a multi-GPU vLLM cluster; `plan` and `apply` reconcile the difference.
- **Diffable, reviewable infrastructure** - every change is a plan against tracked state you can read before it runs, not a pile of imperative scripts.
- **Residency and governance as code** - data-class, residency, and routing policies are first-class resources, checked at plan time rather than bolted on later; pin a fully EU-only stack, or air-gapped, cost-optimized, or multi-cloud.
- **Provider-portable** - AWS, Azure, GCP, Hetzner, OVHcloud, Scaleway, RunPod, Nebius, or your own SSH hosts, behind a single model.

## Quickstart

```bash
git clone https://github.com/andypalmi/amicu.git
cd amicu
cargo build --release -p amicu-cli
./target/release/amicu init      # scaffolds amicu.hcl in the current directory
```

`init` writes a minimal, complete config - model, hardware, and the deployment that binds them:

```hcl
resource "model" "starter_chat" {
  source {
    registry  = "hugging_face"
    reference = "Qwen/Qwen3.5-8B-Instruct"
  }
  architecture   = "qwen3"
  modalities     = ["text"]
  artifact {
    format       = "gguf"
    quantization = "q4_k_m"
  }
  context_length = 100000
  variant {
    quantization = "q4_k_m"
    file         = "qwen3.5-8b-instruct-q4_k_m.gguf"
    size         = "5GiB"
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

resource "deployment" "starter_chat_local" {
  model    = model.starter_chat.id
  hardware = hardware_profile.local_dev.id
  runtime  = "llama_cpp"
  llama_cpp {
    context_length = 100000
    port           = 8000
    n_gpu_layers   = -1
  }
}
```

```bash
./target/release/amicu validate  # structural, reference, and capacity checks
./target/release/amicu plan      # shows what apply would do, diffed against state
./target/release/amicu apply     # executes the plan (prompts for approval)
```

Six other resource kinds - `routing_policy`, `data_class`, `governance_rule`, `budget`, `tenant`, `use_case` - extend the same graph with traffic splitting, residency/sensitivity constraints, and spend limits. Full reference lives in the docs site (below).

## Deploy to Modal

`amicu apply` can deploy a model to Modal (serverless GPU) when the hardware profile has `provider = "modal"`; both `vllm` and `llama_cpp` runtimes work, and the model must come from Hugging Face.

```bash
export MODAL_TOKEN_ID=...  MODAL_TOKEN_SECRET=...              # control plane
export MODAL_PROXY_TOKEN_ID=...  MODAL_PROXY_TOKEN_SECRET=...  # calling the endpoint
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

Bind a model to it with a `deployment` as in the quickstart, then `amicu plan` and `amicu apply`. Inspect with `amicu modal status|logs|metrics|stop <app>`, where the app is `amicu-` plus the deployment id with `_` as `-`. The full guide (llama.cpp GGUF example, multi-GPU, metrics, state) is the `modal-deployment.md` reference in the `amicu-dsl` skill: `crates/amicu-cli/skills/amicu-dsl/references/modal-deployment.md`, also available via `amicu skill export`.

## Install & platforms

No published binary yet - build from source with Rust 1.97+:

```bash
cargo build --release -p amicu-cli
```

- **Local runtime (llama.cpp):** macOS (Apple Silicon) and Linux/CPU, run as a native process (launchd on macOS, systemd on Linux).
- **Production runtime (vLLM):** NVIDIA CUDA, AMD ROCm, and Intel XPU GPUs. Locally as a native single process (launchd/systemd, same as llama.cpp) or deployed via Kubernetes/Helm or k3s.
- **Provider adapters:** AWS, Azure, GCP, Hetzner, OVHcloud, Scaleway, RunPod, Nebius, and self-hosted (SSH) targets.

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

`AMICU_ENV=production` (andypalmi/amicu-planning#139) changes two things from the dev
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

Early stage. Working today: the HCL DSL and parser, the CLI (`init`, `fmt`, `validate`, `plan`, `apply`, `doctor`, `skill export`), the planner and local state, and the provider adapters listed above. Not yet built: the browser UI, hosted service, and performance-intelligence dataset. Expect breaking changes to the DSL and CLI.

## Learn more

- Full documentation site - coming soon.

## License

AGPL-3.0 for the core.
