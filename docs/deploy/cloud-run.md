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
  `AMICU_SIGNING_KEY_PEM` (amicu-planning#140) are the only two secrets
  required before the Cloud Run resources can even start;
  `AMICU_ISSUER`/`AMICU_AUDIENCE`/`AMICU_WEB_URL`/`AMICU_TRUSTED_PROXIES` and
  the rest are plain env vars Terraform sets directly.

## Signing key (amicu-planning#140)

`amicu-server` no longer generates an ephemeral signing key at boot: the
ACTIVE key comes from `AMICU_SIGNING_KEY_PEM` (Ed25519 PKCS#8 PEM), identical
across every replica, so tokens minted by any one Cloud Run instance verify
on every other. In `AMICU_ENV=production` a missing key is a hard startup
error; only `development` (no `AMICU_SIGNING_KEY_PEM` set) falls back to a
per-boot generated key, with a loud warning.

Generate a fresh key with the operator subcommand this PR adds, and pipe it
straight into Secret Manager — nothing else touches stdout:

```sh
amicu-server gen-signing-key | gcloud secrets versions add \
    AMICU_SIGNING_KEY_PEM --data-file=-
```

Rotation (new active key, old key kept verify-only via
`AMICU_SIGNING_KEYS_RETIRING_PEM` until every token it signed has expired,
then dropped) is the full operational procedure in
`amicu-infra/docs/runbook.md`'s "Rotate the signing key" section; the
in-repo rationale (stable `kid` derivation, retention-window math, what
breaks if a retiring key is dropped too early) lives in
`docs/ops/signing-key-rotation.md`.

## Shared rate limiter + Cloudflare edge trust (amicu-planning#142)

Cloud Run autoscales `0..N` stateless replicas behind Google's own load
balancer, which in turn sits behind the Cloudflare **api-proxy Worker**
(`amicu-infra/fallback/api-proxy-worker`) fronting this service. Two
consequences for anything keyed on "the client":

- The credential-route rate limiter (`crates/amicu-api/src/ratelimit.rs`) is
  **Postgres-backed and shared across every replica** whenever `DATABASE_URL`
  is set — the same single shared pool every replica already opens
  (amicu-planning#139) backs `PostgresRateLimitStore`'s one-round-trip atomic
  upsert (`crates/amicu-store/migrations/0014_rate_limit_buckets.sql`).
  `AMICU_ENV=production` requires this path: it already refuses to boot
  without `DATABASE_URL` (amicu-planning#127's fail-fast gate), so the
  in-memory limiter — kept only for dev/tests/DB-less self-host, where a
  process-local counter resetting on every cold start is an accepted
  trade-off — is never reachable in production. **Fails open** on a database
  error (a `WARNING` log line plus an in-process counter, not a hard
  failure): a transient Postgres hiccup must not turn into a total
  login/signup outage.
- The request chain is `browser → Cloudflare edge → api-proxy Worker →
  Google's LB → Cloud Run`. Cloud Run's own peer is Google's LB, and
  `X-Forwarded-For` ends in Cloudflare's own egress IP — **not** the real
  client. The real client IP is `CF-Connecting-IP`, set by the Cloudflare
  edge itself, but that header is only trustworthy once a request is proven
  to have actually passed through the Worker (`AMICU_TRUSTED_PROXIES` alone
  cannot express "trust a header, not a peer address").

### New env vars (document next to `AMICU_DB_MAX_CONNECTIONS` — see
`crates/amicu-server/src/lib.rs`'s crate-level doc tables)

| Env var | Secret? | Default | Purpose |
| --- | --- | --- | --- |
| `AMICU_EDGE_TOKEN` | **yes** | unset | Shared secret the api-proxy Worker attaches as `X-Amicu-Edge-Token` on every request it forwards. Store in Secret Manager for Cloud Run (`AMICU_EDGE_TOKEN`, see amicu-infra's `docs/env-matrix.md`) **and** as the Worker's `EDGE_TOKEN` secret (`wrangler secret put EDGE_TOKEN` in `amicu-infra/fallback/api-proxy-worker/`) — same value, provisioned independently on each side, never in Terraform state. Generate with `openssl rand -base64 32`. Compared constant-time; never logged. |
| `AMICU_REQUIRE_EDGE_TOKEN` | no | `false` | When `true`, a request without a valid `X-Amicu-Edge-Token` is rejected `403` before any handler runs — closes the "raw `*.run.app` URL bypasses Cloudflare" gap outright. `GET /healthz`/`GET /readyz` are exempt (Cloud Run's own probes carry no such header). Requires `AMICU_EDGE_TOKEN` to also be set — booting with `true` and no token is a hard startup error (a deployment that would `403` every public request is refused rather than started). Defaults `false` in every environment, including production: turning it on is a deliberate operator step, gated behind amicu-infra's `optional_secrets_enabled.AMICU_EDGE_TOKEN`, taken once the Worker + token pair are actually provisioned — defaulting it on would break the very first production boot, before that provisioning step ever runs. |

Independent of both: `AMICU_TRUSTED_PROXIES` keeps governing the
non-Cloudflare `X-Forwarded-For` path unchanged — see
`crates/amicu-server/src/lib.rs`'s existing table entry. The Cloud Run
default recorded in amicu-infra's `docs/env-matrix.md` (`0.0.0.0/0`, flagged
**UNVERIFIED** pending empirical confirmation of the actual ingress peer) is
untouched by this change; once a valid edge token is presented,
`CF-Connecting-IP` is used instead and `AMICU_TRUSTED_PROXIES` is not
consulted for that request at all.

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

## Trigger model

Both deploy workflows run via `workflow_run`, gated on `.github/workflows/ci.yml`
(`name: CI`) completing successfully on `main` — they never deploy a commit
CI hasn't already validated (build/clippy/test/audit/docker build, plus the
`web` job's lint/typecheck/build). Each workflow's `deploy` job also gates on
`github.event.workflow_run.conclusion == 'success'` (a failed/cancelled CI
run does not deploy), and checks out `github.event.workflow_run.head_sha` —
the exact commit CI validated — rather than plain `github.sha`, which under
`workflow_run` refers to the default branch's HEAD at event time and isn't
guaranteed to be the same commit. Both workflows can also be triggered
manually via `workflow_dispatch`.

`workflow_run` doesn't support `paths:` filters, so unlike the previous
push-triggered setup, neither workflow filters by changed paths anymore —
`deploy-api.yml` and `deploy-web.yml` both now run on every successful `main`
CI run, not just ones touching `crates/**`/`Cargo.*`/`Dockerfile` or
`web/**` respectively.

> ⚠️ **Before you configure secrets**: create the `production` GitHub
> environment with a required reviewer **and** enable branch protection on
> `main` first. Both deploy workflows are gated behind that `production`
> environment (Settings → Environments — create it once on this repo, same
> as `amicu-infra`'s own gate on `apply`), but that gate only protects
> anything if it's in place *before* the WIF/Cloudflare secrets and
> variables below are added — otherwise the first CI run that completes on
> `main` after those secrets exist will auto-deploy unreviewed.

## Deploy order

1. **`deploy-api.yml`** (`workflow_run` after CI succeeds on `main`, or
   `workflow_dispatch`): WIF auth → build & push the image to Artifact
   Registry, tagged with the deployed commit's SHA and `:latest` → update
   + execute the `amicu-migrate` Job with the new image, **waiting** for it
   to finish → only on success, `gcloud run deploy amicu-api --image …`.
   Migrate-before-deploy, same order as `safecheck-backend`'s `backend.yml`
   — fail loud before rolling the service onto a schema it doesn't match.
   Every `gcloud` call passes `--image` only; every other setting (env
   vars, secret refs, scaling, probes, ingress) stays Terraform-managed
   (`amicu-infra/modules/cloud-run-api`'s `lifecycle.ignore_changes` on
   `template[0].containers[0].image`/`client`/`client_version` only), so
   this workflow can never fight a later `terraform apply`.
2. **`deploy-web.yml`** (`workflow_run` after CI succeeds on `main`, or
   `workflow_dispatch`): pnpm install → `pnpm --filter @amicu/web cf:deploy`
   (`opennextjs-cloudflare build && opennextjs-cloudflare deploy --
   --keep-vars` — `--keep-vars` preserves whatever vars/secrets are already
   set on the Worker instead of resetting them to `wrangler.jsonc`'s
   committed placeholders). Independent of the API deploy — no ordering
   dependency between the two workflows.

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
