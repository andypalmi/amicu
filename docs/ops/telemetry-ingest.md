# Telemetry ingestion: deploy runbook

`https://api.amicu.dev/v1/telemetry/rollups` receives opt-in telemetry batches
from amicu deployments and stores them in the Supabase project
`amicu-telemetry`. For now a Cloudflare Worker serves it
(`web/apps/telemetry-ingest`). It stands in for the planned dedicated Cloud Run
ingestion service, and keeps the same URL and status codes so that the swap
does not affect senders.

| Piece | Where |
|---|---|
| Database schema, retention, verification | `ops/telemetry-db/` (README there) |
| Worker | `web/apps/telemetry-ingest/` (`src/handler.ts`, `wrangler.jsonc`) |
| Wire format | `crates/amicu-telemetry/schema/telemetry-batch.v2.schema.json` (bundled copy in `web/apps/telemetry-ingest/src/schema/`) |

## Behaviour

| Response | When | Sender |
|---|---|---|
| `202` | stored, or a duplicate of a stored batch | drains the batch |
| `400` | not JSON, fails the schema, implausible values, more than one install id, a window that is not an aligned 5 minutes or starts more than 8 days ago or more than 10 minutes ahead, or `Idempotency-Key` differs from `batch_id` | drops the batch |
| `413` | body over 64 KB | drops the batch |
| `405` / `415` | not `POST` / not `application/json` | retries |
| `429` | over the per-install or global rate limit | retries with backoff |
| `503` | Supabase unreachable or erroring, or the secret is missing | retries with backoff |

- The Worker sends one HTTPS request per batch to
  `/rest/v1/rpc/ingest_rollup_batch`, with the secret key in the `apikey`
  header. It sends no `Authorization` header, because new-style keys are not
  JWTs. It never connects to Postgres and never uses the database password.
- No payload content and no client IP are logged or stored. Workers
  invocation logs, which would record request metadata, are off in
  `wrangler.jsonc`. No CORS headers are sent.
- Rate limiting uses the Workers Rate Limiting binding: 10 requests per 60 s
  per install id, 30 per 60 s per client IP (keyed by a truncated SHA-256 of
  the IP, which is never logged or stored), and 600 per 60 s overall. Every
  one of these is counted per Cloudflare location and is approximate, so the
  "global" limit is not a zone-wide total. If `wrangler deploy` rejects the
  `ratelimits` block on this account's plan, delete it; the code runs without
  the bindings.
- Recommended in addition (zone-wide, enforced before the Worker runs): a WAF
  rate-limiting rule in the dashboard, Security > WAF > Rate limiting rules:
  - If: `http.host eq "api.amicu.dev" and http.request.uri.path eq "/v1/telemetry/rollups"`
  - Counting characteristic: IP. Rate: 30 requests per 1 minute (the Free
    plan allows a 10-second period only: use 10 requests per 10 seconds).
  - Action: Block for the shortest duration offered. Senders treat the 429 as
    "retry later".
- Abuse limits: each batch carries exactly one install id, and every window is
  one aligned 5-minute slot per deployment, so one request can no longer mint
  hundreds of installs or rows at arbitrary timestamps. Forging installs is
  still possible, at the cost of one request per fake install, spread over
  many IPs to get past the limits. The 5-install threshold of
  `telemetry.benchmark_30d` can therefore still be gamed by a determined
  sender until uploads carry a server-signed install token (planned for the
  Cloud Run ingestion service). Treat published aggregates accordingly.
- Route: `api.amicu.dev/v1/telemetry/rollups` on the `amicu.dev` zone. The
  api-proxy Worker holds `api.amicu.dev/*`. Cloudflare runs the more specific
  route, so only this exact path goes to this Worker.

## Deploy

1. **Apply the SQL** to `amicu-telemetry` as `postgres` (SQL Editor, or psql
   over the session pooler), in order: `0001_schema.sql`,
   `0002_maintenance.sql`, `0003_ingest.sql`, `0004_schedule.sql`. Then run
   `verify.sql` and check that every row says `ok = t`. See
   `ops/telemetry-db/README.md`.

2. **Set the secret key** (a new-style `sb_secret_...` key, which maps to
   `service_role`). Pipe it in, so that it never lands in shell history:

   ```sh
   cd web/apps/telemetry-ingest
   pnpm exec wrangler login     # OAuth in the browser; no token in shell history
   security find-generic-password -s supabase-amicu-telemetry-secret-key -a default -w \
     | pnpm exec wrangler secret put SUPABASE_SECRET_KEY
   ```

   The Worker reads `SUPABASE_SECRET_KEY`, not `SUPABASE_SERVICE_ROLE_KEY`.
   `SUPABASE_URL` is a plain var in `wrangler.jsonc`.

3. **Deploy**:

   ```sh
   pnpm exec wrangler deploy
   ```

   Run `pnpm check:wrangler` (`wrangler deploy --dry-run`) first to see the
   bindings without deploying.

4. **Smoke test.** Send the golden example with a fresh batch id and a current
   window. Expect `202`, then `202` again for the same body (a duplicate):

   ```sh
   now=$(( $(date +%s) / 300 * 300 - 300 ))
   id=$(uuidgen | tr 'A-Z' 'a-z')
   jq -c --argjson s "$now" --arg id "$id" \
     '.batch_id = $id | .reports[0].window.start_unix_s = $s' \
     web/apps/telemetry-ingest/test/fixtures/telemetry-batch.v2.example.json > /tmp/batch.json
   curl -sS -i -X POST https://api.amicu.dev/v1/telemetry/rollups \
     -H 'content-type: application/json' -H "idempotency-key: $id" \
     --data-binary @/tmp/batch.json
   curl -sS -o /dev/null -w '%{http_code}\n' https://api.amicu.dev/v1/telemetry/rollups   # 405
   ```

   Then, in the SQL Editor:

   ```sql
   select count(*) from telemetry.rollup_5m where install_id = '01010101-0101-4101-8101-010101010101';
   select public.delete_install('01010101-0101-4101-8101-010101010101');  -- remove the test rows
   ```

## Operations

- **Health, weekly**: `select public.telemetry_health();` (or re-run
  `verify.sql`). `ok` is false when maintenance has not finished in the last
  3 hours or tomorrow's partitions are missing, i.e. the pg_cron job has
  stalled; inserts start failing (503) once the 3 days of partitions made
  ahead run out. Fix by running `select telemetry.run_maintenance();` and
  checking `cron.job_run_details`.
- **Size**: `select * from telemetry.db_size;`. Move to Pro when
  `over_alert_300mb` turns true.
- **Maintenance**: pg_cron runs `telemetry.run_maintenance()` hourly. Inspect
  runs with `select * from cron.job_run_details order by start_time desc limit 10;`.
- **Deletion request** (privacy@amicu.dev):
  `select public.delete_install('<install id>');`
- **Rotate the key**: create a new secret key in Supabase, run
  `wrangler secret put SUPABASE_SECRET_KEY` again, then revoke the old key.
- **Logs**: `pnpm exec wrangler tail` shows one JSON line per request: event,
  status and counts only.
