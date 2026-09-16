# Deploying amicu (Cloud Run + Cloudflare Workers)

Short runbook for **this** repo's deploy artifacts (amicu-planning#143). The
durable infra (Cloud Run service/Job, Secret Manager, IAM, WIF, Cloudflare
DNS) lives in the separate Terraform repo — **the master plan is
`amicu-infra/docs/plans/cloud-run-deploy.md`**; read that first for the
target topology, numbered decisions, and the first-deploy checklist. This
page covers only what runs from `amicu` itself: the image contract, the two
GitHub Actions deploy workflows, and the repo vars/secrets they need.

## Image contract

One image (`Dockerfile`, root of this repo) serves both Cloud Run resources
amicu-infra provisions:

| Resource | amicu-infra name | Invocation |
| --- | --- | --- |
| Service | `amicu-api` | `ENTRYPOINT` only, no args — binds `$PORT` (Cloud Run's injected env var; `AMICU_BIND`/`--bind` overrides it, see `crates/amicu-server/src/lib.rs`). |
| Job | `amicu-migrate` | Terraform-managed `command` override: `["/usr/local/bin/amicu-server", "migrate"]` (amicu-planning#127's `Command::Migrate` in `crates/amicu-server/src/main.rs`) — applies pending schema migrations as the schema-owning role, prints how many it applied, and exits. No RLS-bypass guard runs (the migrator is expected to own the schema). |

- **Health/readiness**: `GET /healthz` (startup probe) and `GET /readyz`
  (liveness probe, checks the Postgres pool with a `SELECT 1` under
  `AMICU_READYZ_TIMEOUT_MS`, default 2000ms) — both wired in
  `modules/cloud-run-api/variables.tf`'s `startup_probe_path` /
  `liveness_probe_path` defaults.
- **`AMICU_ENV=production`** changes two things (README.md "Production
  database (two-role split)"): the server no longer migrates at boot (the
  `amicu-migrate` Job above does that instead, first, connected as
  `amicu_owner`), and it refuses to start if the connected role
  (`amicu_app`) would bypass row-level security.
- **TLS stack is rustls-only** end to end (sqlx `tls-rustls`, lettre's
  `rustls`/`ring` SMTP transport, reqwest's `rustls`/`aws-lc-rs` backend) —
  no `libssl.so` anywhere, hence the distroless `cc-debian12:nonroot`
  runtime (glibc + libgcc + the CA trust store for validating outbound
  HTTPS/SMTP server certs, nothing else).
- **Required env** — see amicu-infra's `docs/env-matrix.md` for the
  authoritative, component-by-component table (secret-backed vs plain, every
  Cloud Run resource, every gating flag). Short version: `DATABASE_URL` +
  `AMICU_SIGNING_KEY_PEM` (amicu-planning#140 — **not yet consumed by
  `amicu-server` as of this PR**; the server still generates an ephemeral
  in-memory signing key at boot until #140 lands, ready to run alongside
  #143 per epic #145's sequencing) are the only two secrets required before
  the Cloud Run resources can even start; `AMICU_ISSUER`/`AMICU_AUDIENCE`/
  `AMICU_WEB_URL`/`AMICU_TRUSTED_PROXIES` and the rest are plain env vars
  Terraform sets directly.

## `amicu_app` / `amicu_owner` DB roles

`ops/sql/provision_app_role.sql` (landed with amicu-planning#127) is the
canonical provisioning script: a schema-**owning** `amicu_owner` role (runs
`amicu-server migrate`) and a least-privilege, DML-only `amicu_app` role
(runs the service), least-privilege enforced by `AMICU_ENV=production`'s
startup guard refusing to boot as anything that would bypass RLS. Run it
once per database as documented in its header comment.

**Naming note for whoever fills in `DATABASE_URL_MIGRATE`**: amicu-infra's
own README/docs describe the migrate connection as using Supabase's built-in
`postgres` role. This repo's actual provisioning script does not reuse that
role directly — it creates a dedicated `amicu_owner` role instead (never
`SUPERUSER`/`BYPASSRLS`, unlike `postgres`/`supabase_admin` on Supabase) so
the schema owner is a role this repo controls end to end. `DATABASE_URL` /
`DATABASE_URL_MIGRATE` should therefore be the `amicu_app` / `amicu_owner`
connection strings this script produces, not Supabase's own `postgres`
superuser — flagging this because getting it backwards (`amicu_app` owning
tables, or the service connecting as `amicu_owner`/`postgres`) silently
defeats the RLS backstop (see the script's header comment and
`crates/amicu-store`'s docs).

## Deploy order

1. **`deploy-api.yml`** (push to `main` touching `crates/**`, `Cargo.*`,
   `Dockerfile`; or `workflow_dispatch`): WIF auth → build & push the image
   to Artifact Registry, tagged `:${{ github.sha }}` and `:latest` → update
   + execute the `amicu-migrate` Job with the new image, **waiting** for it
   to finish → only on success, `gcloud run deploy amicu-api --image …`.
   Migrate-before-deploy, same order as `safecheck-backend`'s `backend.yml`
   — fail loud before rolling the service onto a schema it doesn't match.
   Every `gcloud` call passes `--image` only; every other setting (env
   vars, secret refs, scaling, probes, ingress) stays Terraform-managed
   (`amicu-infra/modules/cloud-run-api`'s `lifecycle.ignore_changes` on
   `template[0].containers[0].image`/`client`/`client_version` only), so
   this workflow can never fight a later `terraform apply`.
2. **`deploy-web.yml`** (push to `main` touching `web/**`; or
   `workflow_dispatch`): pnpm install → `pnpm --filter @amicu/web cf:deploy`
   (`opennextjs-cloudflare build && opennextjs-cloudflare deploy --
   --keep-vars` — `--keep-vars` preserves whatever vars/secrets are already
   set on the Worker instead of resetting them to `wrangler.jsonc`'s
   committed placeholders). Independent of the API deploy — no ordering
   dependency between the two workflows.

Both jobs are gated behind a GitHub `production` environment (required
reviewer) — create it once on this repo (Settings → Environments), same as
`amicu-infra`'s own gate on `apply`.

## Repo vars / secrets these workflows need

| Name | Kind | Consumed by | Value / where it comes from |
| --- | --- | --- | --- |
| `WIF_PROVIDER` | repo variable | `deploy-api.yml` | `amicu-infra`'s `envs/prod` output `wif_provider_name` (`terraform output wif_provider_name`). |
| `WIF_SERVICE_ACCOUNT` | repo variable | `deploy-api.yml` | `amicu-infra`'s output `deploy_service_account_email`. |
| `GCP_PROJECT_ID` | repo variable | `deploy-api.yml` | `amicu-infra/envs/prod/variables.tf`'s `project_id` (default `amicu-prod`) — whatever's actually in `terraform.tfvars`. |
| `GCP_REGION` | repo variable | `deploy-api.yml` | `amicu-infra/envs/prod/variables.tf`'s `region` (default `europe-west1`). |
| `CLOUDFLARE_API_TOKEN` | repo **secret** | `deploy-web.yml` | Same token amicu-infra's README documents creating (Cloudflare dashboard → API Tokens), scopes listed there. Set on both repos. |
| `CLOUDFLARE_ACCOUNT_ID` | repo variable | `deploy-web.yml` | Cloudflare dashboard (Workers & Pages → Overview, right sidebar) — not a Terraform output (amicu-infra's Cloudflare resources are zone-scoped, never needed the account id). |
| `NEXT_PUBLIC_SITE_URL` | repo variable | `deploy-web.yml` | The UI's public origin, `https://amicu.<domain>` (amicu-infra output `app_hostname`, informational — not Terraform-managed on the DNS side, see `web/apps/web/wrangler.jsonc`). Build-time only: inlined into the client bundle by `next build`, must be set correctly before `cf:build` runs. |

The Artifact Registry repo id (`amicu`), the Cloud Run service name
(`amicu-api`), and the migrate Job name (`amicu-migrate`) are hardcoded in
`deploy-api.yml` rather than repo variables — they're fixed
`amicu-infra`/Terraform-module defaults (`modules/gcp-project`'s
`artifact_registry_repo_id`, `envs/prod/variables.tf`'s `api_service_name` /
`migrate_job_name`), not values meant to vary per deploy.

Everything else the running containers need (`DATABASE_URL`,
`AMICU_SIGNING_KEY_PEM`, `AMICU_SMTP_*`, …) is Cloud Run env/secret
configuration owned entirely by `amicu-infra`'s Terraform — never passed
via `--set-env-vars`/`--set-secrets` from either workflow here. See
`amicu-infra/docs/env-matrix.md` for that full table.
