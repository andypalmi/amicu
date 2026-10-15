# Deploying amicu (Cloud Run + Cloudflare Workers)

Short runbook for **this** repo's deploy artifacts. The
durable infra (Cloud Run service/Job, Secret Manager, IAM, WIF, Cloudflare
DNS) lives in the separate Terraform repo — **the master plan is
`docs/plans/cloud-run-deploy.md` in the infrastructure repo**; read that first for the
target topology, numbered decisions, and the first-deploy checklist. This
page covers only what runs from `amicu` itself: the image contract, the two
GitHub Actions deploy workflows, and the repo vars/secrets they need.

## Image contract

One image (`Dockerfile`, root of this repo) serves both Cloud Run resources
the infrastructure repo provisions:

| Resource | Infrastructure name | Invocation |
| --- | --- | --- |
| Service | `amicu-api` | `ENTRYPOINT` only, no args — binds `$PORT` (Cloud Run's injected env var; `AMICU_BIND`/`--bind` overrides it, see `crates/amicu-server/src/lib.rs`). |
| Job | `amicu-migrate` | Terraform-managed `command` override: `["/usr/local/bin/amicu-server", "migrate"]` (the `Command::Migrate` in `crates/amicu-server/src/main.rs`) — applies pending schema migrations as the schema-owning role, prints how many it applied, and exits. No RLS-bypass guard runs (the migrator is expected to own the schema). |

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
- **Required env** — see the infrastructure repo's `docs/env-matrix.md` for the
  authoritative, component-by-component table (secret-backed vs plain, every
  Cloud Run resource, every gating flag). Short version: `DATABASE_URL` +
  `AMICU_SIGNING_KEY_PEM` are the only two secrets
  required before the Cloud Run resources can even start;
  `AMICU_ISSUER`/`AMICU_AUDIENCE`/`AMICU_WEB_URL`/`AMICU_TRUSTED_PROXIES` and
  the rest are plain env vars Terraform sets directly. The outbox-drain
  settings (`AMICU_INTERNAL_AUDIENCE`, `AMICU_INTERNAL_CALLER_EMAILS`,
  `AMICU_OUTBOX_DRAIN_BATCH_SIZE`, `AMICU_OUTBOX_DRAIN_BUDGET_MS`,
  `AMICU_OUTBOX_INLINE_SEND`, `AMICU_OUTBOX_INLINE_BUDGET_MS`, plus the
  optional `AMICU_INTERNAL_TOKEN` secret) are covered in "Cloud Scheduler →
  outbox drain" below.

## Signing key

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
the infrastructure repo's `docs/runbook.md`'s "Rotate the signing key" section; the
in-repo rationale (stable `kid` derivation, retention-window math, what
breaks if a retiring key is dropped too early) lives in
`docs/ops/signing-key-rotation.md`.

## Shared rate limiter + Cloudflare edge trust

Cloud Run autoscales `0..N` stateless replicas behind Google's own load
balancer, which in turn sits behind the Cloudflare **api-proxy Worker**
(the infrastructure repo's `fallback/api-proxy-worker`) fronting this service. Two
consequences for anything keyed on "the client":

- The credential-route rate limiter (`crates/amicu-api/src/ratelimit.rs`) is
  **Postgres-backed and shared across every replica** whenever `DATABASE_URL`
  is set — the same single shared pool every replica already opens
  backs `PostgresRateLimitStore`'s one-round-trip atomic
  upsert (`crates/amicu-store/migrations/0014_rate_limit_buckets.sql`).
  `AMICU_ENV=production` requires this path: it already refuses to boot
  without `DATABASE_URL` (the fail-fast gate), so the
  in-memory limiter — kept only for dev/tests/DB-less self-host, where a
  process-local counter resetting on every cold start is an accepted
  trade-off — is never reachable in production. **Fails open** on a database
  error, or on a hit that doesn't complete within `AMICU_RATE_LIMIT_TIMEOUT_MS`
  (a sampled `WARNING` log line — first failure, then every 100th — plus an
  exact in-process counter, not a hard failure): a transient Postgres hiccup,
  or a saturated pool, must not turn into a total login/signup outage.
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
| `AMICU_EDGE_TOKEN` | **yes** | unset | Shared secret the api-proxy Worker attaches as `X-Amicu-Edge-Token` on every request it forwards. Store in Secret Manager for Cloud Run (`AMICU_EDGE_TOKEN`, see the infrastructure repo's `docs/env-matrix.md`) **and** as the Worker's `EDGE_TOKEN` secret (`wrangler secret put EDGE_TOKEN` in the infrastructure repo's api-proxy Worker directory) — same value, provisioned independently on each side, never in Terraform state. Generate with `openssl rand -base64 32`. Compared constant-time; never logged. |
| `AMICU_REQUIRE_EDGE_TOKEN` | no | `false` | When `true`, a request without a valid `X-Amicu-Edge-Token` is rejected `403` before any handler runs — closes the "raw `*.run.app` URL bypasses Cloudflare" gap outright. `GET /healthz`/`GET /readyz` are exempt (Cloud Run's own probes carry no such header). Requires `AMICU_EDGE_TOKEN` to also be set — booting with `true` and no token is a hard startup error (a deployment that would `403` every public request is refused rather than started). Defaults `false` in every environment, including production: turning it on is a deliberate operator step, gated behind the infrastructure repo's `optional_secrets_enabled.AMICU_EDGE_TOKEN`, taken once the Worker + token pair are actually provisioned — defaulting it on would break the very first production boot, before that provisioning step ever runs. |
| `AMICU_SIGNUPS_ENABLED` | no | `false` | Public sign-ups are CLOSED unless this is exactly `true` (`--signups-enabled`). Closed => `POST /signup` answers `403` `{"error":..., "code":"signups_disabled"}` before doing any work; invite acceptance, login, verify and verify/resend are unaffected. Self-host/dev must set `true` to allow registration. The web app's `SIGNUPS_ENABLED` mirrors this for the UI. |
| `AMICU_RATE_LIMIT_TIMEOUT_MS` | no | `1000` | Upper bound the Postgres-backed rate limiter's `check` waits for the database hit before failing OPEN — its own dedicated budget, independent of `AMICU_DB_ACQUIRE_TIMEOUT_SECS`, mirroring `AMICU_READYZ_TIMEOUT_MS`'s relationship to that same acquire timeout. A timeout is treated exactly like a database error: it fails OPEN and counts toward the same sampled `WARNING`/`fail_open_count` reporting. Only consulted on the Postgres path — the in-memory limiter never awaits. |

Independent of both: `AMICU_TRUSTED_PROXIES` keeps governing the
non-Cloudflare `X-Forwarded-For` path unchanged — see
`crates/amicu-server/src/lib.rs`'s existing table entry. The Cloud Run
default recorded in the infrastructure repo's `docs/env-matrix.md` (`0.0.0.0/0`, flagged
**UNVERIFIED** pending empirical confirmation of the actual ingress peer) is
untouched by this change; once a valid edge token is presented,
`CF-Connecting-IP` is used instead and `AMICU_TRUSTED_PROXIES` is not
consulted for that request at all.

## Auth hardening env

| Env var | Secret? | Default | Purpose |
| --- | --- | --- | --- |
| `AMICU_SECURITY_HASH_KEY` | **yes** | unset | Secret key (**>= 32 bytes**, e.g. `openssl rand -base64 48`) used to pseudonymize emails and client IPs in `amicu::security` log events and in the sign-in backoff / per-email limiter keys. **Required when `AMICU_ENV=production`** (startup error otherwise); the SAME value on every replica. Store in Secret Manager. Rotating it resets backoff counters and changes the hashes in logs. |
| `AMICU_EMAIL_RATE_LIMIT_MAX` / `AMICU_EMAIL_RATE_LIMIT_WINDOW_SECS` | no | `5` / `900` | Per-email budget for `/recover` and `/verify/resend`. |
| `AMICU_SIGNIN_EMAIL_RATE_LIMIT_MAX` | no | `100` | Per-email budget of FAILED sign-ins per window. Once exhausted, only networks that signed in successfully to that account in the last 30 days may still attempt; others get `429` until the window passes (so during an active distributed attack the owner on a brand-new network is refused). |
| `SENTRY_DSN` | **yes** | unset | Opt-in error reporting. Unset = disabled, no behaviour change. When set, panics and 5xx responses (method + route template only) are sent to Sentry, errors only (no tracing or profiling). Events are scrubbed server-side of user, request data, headers, cookies, emails, bearer tokens and `token=`/`code=`/`key=`/`secret=`/`password=` values. In the Sentry project enable "Prevent Storing of IP Addresses". Store in Secret Manager. |
| `SENTRY_ENVIRONMENT` | no | `AMICU_ENV` value | Environment name tagged on events. |

**Production client identity.** The per-network sign-in backoff and the
per-IP limits trust the client IP, which behind Cloud Run/Cloudflare is only
trustworthy from the verified edge (`AMICU_TRUSTED_PROXIES` alone either
collapses every client onto the proxy address or trusts a spoofable
`X-Forwarded-For`). So in `AMICU_ENV=production`, **regardless of
`AMICU_TRUSTED_PROXIES`, startup is refused unless `AMICU_EDGE_TOKEN` is set
and `AMICU_REQUIRE_EDGE_TOKEN=true`**: identity then always comes from the
verified `CF-Connecting-IP`. Production example:

```sh
AMICU_ENV=production
AMICU_TRUSTED_PROXIES=0.0.0.0/0
AMICU_REQUIRE_EDGE_TOKEN=true
# secrets (Secret Manager): AMICU_EDGE_TOKEN, AMICU_SECURITY_HASH_KEY
```

## Cloud Scheduler → outbox drain

Cloud Run gives a container CPU only while a request is in flight, so
transactional email cannot be sent "after" a response. Verification,
password-reset and account-exists messages are therefore written to the
`email_outbox` table inside the request that triggers them (in the same
transaction as the token mint, on the Postgres path), and actually delivered by
an authenticated endpoint that the infrastructure repo's `modules/scheduler` calls on a
fixed cadence.

| | |
| --- | --- |
| Method + path | `POST {api_base_url}/internal/outbox/drain` |
| Cadence | every 15 minutes (`*/15 * * * *`) — a retry sweep, see "Inline send" below; `attempt_deadline = 30s`, `retry_count = 1` |
| Auth header | `Authorization: Bearer <Google-signed OIDC ID token>` (Cloud Scheduler's `oidc_token` block — no shared secret, nothing in Terraform state) |
| Caller identity | the token's `email` claim, which must be **verified** and listed in `AMICU_INTERNAL_CALLER_EMAILS` (`terraform output scheduler_service_account_email`) |
| `aud` claim | `AMICU_INTERNAL_AUDIENCE`, defaulting to `AMICU_BASE_URL` — the Cloud Run **service URL**, not the `/internal/outbox/drain` path, matching Google's documented pattern and `modules/scheduler`'s `oidc_audience` default |
| `iss` claim | `https://accounts.google.com` (the bare `accounts.google.com` spelling is also accepted) |
| Signature | verified against Google's live JWKS at `https://www.googleapis.com/oauth2/v3/certs`, cached per the response's `Cache-Control: max-age` (clamped to 5 minutes … 24 hours) |
| Success response | `200` + JSON counts: `{"claimed":…,"sent":…,"failed":…,"dead_lettered":…,"budget_exhausted":…}` |
| Rejections | `401` unauthenticated (missing/bad credential, wrong `aud`, wrong `iss`, expired), `403` verified-but-not-allowlisted caller, `503` no auth mechanism configured at all |

- **Bounded work per call**: one request claims at most
  `AMICU_OUTBOX_DRAIN_BATCH_SIZE` (default 50) messages at a time and stops
  entirely after `AMICU_OUTBOX_DRAIN_BUDGET_MS` (default 20000ms), returning
  partial counts with `budget_exhausted: true`. That keeps every call well
  inside the scheduler's 30s attempt deadline; the remainder is picked up by
  the next invocation.
- **Quiet logs**: an empty sweep logs at `debug` only; a sweep that sent or
  failed something emits one `info` line with the counts.
- **Safe to overlap**: claiming is a single `FOR UPDATE SKIP LOCKED` statement
  that also stamps a lease, so concurrent replicas and overlapping Scheduler
  invocations never hand the same message to two senders.
- **Retry/dead-letter**: a failed send backs off exponentially (30s, doubling,
  capped at 30 minutes) and is dead-lettered after 8 attempts. A dead-lettered
  or delivered row has its raw one-shot token cleared.
- **Inline send (prompt delivery)**: so mail does not wait for the sweep,
  `/signup`, `/recover` and `/verify/resend` attempt delivery themselves after
  the enqueue transaction has committed and before the response is written
  (Cloud Run throttles CPU after the response, so it cannot be a detached
  task). It uses the same `SKIP LOCKED` + lease claim and the same
  attempt/backoff accounting as the drain, so it cannot double-send with a
  concurrent sweep; any failure is swallowed (a `warn` with counts only), the
  response is unchanged, and the message simply waits for the sweep. Because
  sending only when mail was queued would leak whether an account exists, the
  inline phase always lasts exactly the configured budget (the send is detached at the
  deadline and finishes in the background, or the phase is padded if quicker), on every outcome. Settings:
  `AMICU_OUTBOX_INLINE_SEND` (default `true`; `false` restores sweep-only
  delivery; no-op without a database) and `AMICU_OUTBOX_INLINE_BUDGET_MS`
  (default `1000`; `0` disables the inline send; this is added to the response time of those three
  endpoints).
- **Self-hosted alternative**: set the `AMICU_INTERNAL_TOKEN` *environment
  variable* (env only, never a flag) and present it as a static bearer token
  from cron + curl; it is compared in constant time. Or skip HTTP entirely and
  run `amicu-server outbox-drain`, which does one pass through the same code
  path with the database credential in hand.
- **Not in `openapi.json`**: the route is infra plumbing with a machine caller,
  so — exactly like `/healthz` and `/readyz` — it is kept out of the generated
  spec and never forces a web client regeneration.

> ⚠️ If neither `AMICU_INTERNAL_CALLER_EMAILS` nor `AMICU_INTERNAL_TOKEN` is
> set, the drain endpoint returns `503` and **no queued email is ever
> delivered**. The server warns about this at startup, loudly in
> `AMICU_ENV=production`.

## `amicu_app` / `amicu_owner` DB roles

`ops/sql/provision_app_role.sql` is the
canonical provisioning script: a schema-**owning** `amicu_owner` role (runs
`amicu-server migrate`) and a least-privilege, DML-only `amicu_app` role
(runs the service), least-privilege enforced by `AMICU_ENV=production`'s
startup guard refusing to boot as anything that would bypass RLS. Run it
once per database as documented in its header comment.

**Naming note for whoever fills in `DATABASE_URL_MIGRATE`**: the infrastructure repo's
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

Production deploys are **manual**. Both workflows are `workflow_dispatch`-only:
Actions → Deploy API / Deploy Web → Run workflow on `main`, or
`gh workflow run deploy-api.yml --ref main` (likewise `deploy-web.yml`).
GitHub's free plan doesn't offer required reviewers on environments for
private repos, so the manual dispatch is the gate. The `production`
environment is restricted to the `main` branch, and each job also has
`if: github.ref == 'refs/heads/main'`.

Before deploying, each job checks that the latest `ci.yml` run for
`github.sha` concluded `success` (`gh run list --workflow ci.yml --commit
"$GITHUB_SHA"`), and fails otherwise. The optional boolean input
`skip_ci_check` (default false) bypasses that check with a loud warning; use
it only when Actions minutes are exhausted and CI was run locally
(`gh workflow run deploy-api.yml --ref main -f skip_ci_check=true`).
Deploys use `github.sha` (the dispatched `main` commit).

> Before you configure secrets: create the `production` GitHub environment
> (Settings → Environments) restricted to the `main` branch, and enable
> branch protection on `main`. Required reviewers are not available on the
> free plan, so nothing deploys unless someone dispatches the workflow.

## Deploy order

1. **`deploy-api.yml`** (manual `workflow_dispatch` on `main`): WIF auth →
   build & push the image to Artifact Registry, tagged with the deployed commit's SHA and `:latest` → update
   + execute the `amicu-migrate` Job with the new image, **waiting** for it
   to finish → only on success, `gcloud run deploy amicu-api --image …`.
   Migrate-before-deploy — fail loud before rolling the service onto a schema it doesn't match.
   Every `gcloud` call passes `--image` only; every other setting (env
   vars, secret refs, scaling, probes, ingress) stays Terraform-managed
   (the infrastructure repo's `modules/cloud-run-api`'s `lifecycle.ignore_changes` on
   `template[0].containers[0].image`/`client`/`client_version` only), so
   this workflow can never fight a later `terraform apply`.
2. **`deploy-web.yml`** (manual `workflow_dispatch` on `main`): pnpm install →
   `pnpm --filter @amicu/web cf:deploy`
   (`opennextjs-cloudflare build && opennextjs-cloudflare deploy --
   --keep-vars` — `--keep-vars` preserves whatever vars/secrets are already
   set on the Worker instead of resetting them to `wrangler.jsonc`'s
   committed placeholders). Independent of the API deploy — no ordering
   dependency between the two workflows.

## Repo vars / secrets these workflows need

| Name | Kind | Consumed by | Value / where it comes from |
| --- | --- | --- | --- |
| `WIF_PROVIDER` | repo variable | `deploy-api.yml` | the infrastructure repo's `envs/prod` output `wif_provider_name` (`terraform output wif_provider_name`). |
| `WIF_SERVICE_ACCOUNT` | repo variable | `deploy-api.yml` | the infrastructure repo's output `deploy_service_account_email`. |
| `GCP_PROJECT_ID` | repo variable | `deploy-api.yml` | the infrastructure repo's `envs/prod/variables.tf`'s `project_id` (default `amicu-prod`) — whatever's actually in `terraform.tfvars`. |
| `GCP_REGION` | repo variable | `deploy-api.yml` | the infrastructure repo's `envs/prod/variables.tf`'s `region` (default `europe-west1`). |
| `CLOUDFLARE_API_TOKEN` | repo **secret** | `deploy-web.yml` | Same token the infrastructure repo's README documents creating (Cloudflare dashboard → API Tokens), scopes listed there. Set on both repos. |
| `CLOUDFLARE_ACCOUNT_ID` | repo variable | `deploy-web.yml` | Cloudflare dashboard (Workers & Pages → Overview, right sidebar) — not a Terraform output (the infrastructure repo's Cloudflare resources are zone-scoped, never needed the account id). |
| `NEXT_PUBLIC_SITE_URL` | repo variable | `deploy-web.yml` | The UI's public origin, `https://amicu.dev` (served by the Worker custom-domain route in `web/apps/web/wrangler.jsonc`; `www.amicu.dev` redirects to it; the API is `https://api.amicu.dev`). Build-time only: inlined into the client bundle by `next build`, must be set correctly before `cf:build` runs. |

The Artifact Registry repo id (`amicu`), the Cloud Run service name
(`amicu-api`), and the migrate Job name (`amicu-migrate`) are hardcoded in
`deploy-api.yml` rather than repo variables — they're fixed
infrastructure/Terraform-module defaults (`modules/gcp-project`'s
`artifact_registry_repo_id`, `envs/prod/variables.tf`'s `api_service_name` /
`migrate_job_name`), not values meant to vary per deploy.

Everything else the running containers need (`DATABASE_URL`,
`AMICU_SIGNING_KEY_PEM`, `AMICU_SMTP_*`, …) is Cloud Run env/secret
configuration owned entirely by the infrastructure repo's Terraform — never passed
via `--set-env-vars`/`--set-secrets` from either workflow here. See
the infrastructure repo's `docs/env-matrix.md` for that full table.
