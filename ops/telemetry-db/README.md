# Telemetry database (Supabase `amicu-telemetry`)

Schema for the opt-in telemetry rollups. It lives in its own Supabase project
(EU, Frankfurt), separate from the product database. Its only writer is the
ingestion endpoint (`web/apps/telemetry-ingest`, see
`docs/ops/telemetry-ingest.md`), which calls one RPC with the project's secret
key. The sender's wire format is
`crates/amicu-telemetry/schema/telemetry-batch.v2.schema.json`.

## Apply

As `postgres`, in the SQL Editor or with psql over the session pooler, in this
order. Each file is re-runnable.

| File | What it does |
|---|---|
| `0001_schema.sql` | `telemetry` schema, tables, histogram layouts, RLS, revokes |
| `0002_maintenance.sql` | partitions, downsampling, retention, `telemetry.db_size`, `telemetry.benchmark_30d` |
| `0003_ingest.sql` | `public.ingest_rollup_batch(jsonb)` and `public.delete_install(uuid)`, executable by `service_role` only |
| `0004_schedule.sql` | enables `pg_cron` and schedules `telemetry.run_maintenance()` hourly (minute 7) |
| `verify.sql` | read-only checks; every row must say `ok = t` |

```sh
for f in 0001_schema.sql 0002_maintenance.sql 0003_ingest.sql 0004_schedule.sql verify.sql; do
  psql "$TELEMETRY_DB_URL" -v ON_ERROR_STOP=1 -f "ops/telemetry-db/$f"
done
```

Quick check after applying:

```sql
select * from telemetry.db_size;
select telemetry.run_maintenance();   -- safe to run by hand at any time
```

`test.sql` (run by `./test-local.sh` against a throwaway local Postgres) writes
and deletes rows. Never run it against the project.

## Design

| Table | Grain | Partitions | Kept |
|---|---|---|---|
| `telemetry.rollup_5m` | one report: install, deployment, 5-minute window | daily, by `window_start` | 7 days |
| `telemetry.rollup_hourly` | install, deployment, configuration, hour | daily, by `hour_start` | 90 days |
| `telemetry.rollup_daily` | configuration, day; no install or deployment id | none | indefinitely |
| `telemetry.ingested_batches` | batch id | none | 8 days |

- Latency (TTFT, end-to-end, time per output token) is stored as bucket
  counts over the fixed layouts in `telemetry.histogram_layouts`, which match
  the schema's `x-amicu` bounds. Counts and sums add, so hourly and daily rows
  are exact. Rates are stored as rate x seconds plus seconds observed. Peaks are
  maxima. `telemetry.hist_quantile(counts, bounds, q)` reads percentiles.
- Idempotency: `rollup_5m`'s primary key is (install, deployment, window
  start) and inserts use `ON CONFLICT DO NOTHING`, so a retried or re-packed
  report never counts twice. Batch ids are recorded too, and a repeat is
  answered as `"duplicate": true`.
- Reports with a window more than 7 days old, or more than an hour in the
  future, are accepted and skipped, so the sender drains them.
- `telemetry.run_maintenance()` creates partitions 3 days ahead, folds new
  5-minute rows into `rollup_hourly`, rebuilds the last 8 days of
  `rollup_daily`, drops expired partitions, and purges old batch ids. It runs
  under an advisory lock. Each 5-minute row is claimed (`downsampled = true`)
  in the statement that adds it to the hourly table, so late reports are
  counted exactly once. If the job stops for more than 3 days, inserts fail
  for want of a partition. The endpoint then answers 503 and senders retry.
- `public.delete_install(install_id)` deletes the install's 5-minute and
  hourly rows. Daily rows hold no install id and are kept. The next
  maintenance run still drops the install from the last 8 days, because those
  days are rebuilt.
- `rollup_daily` also drops `cpu_cores`, `memory_bytes` and `region`. It keeps
  `installs` and `deployments` (distinct counts) for minimum-sample thresholds.
  `telemetry.benchmark_30d` lists only configurations seen from at least 5
  installs.

## Access

- The `telemetry` schema is not in the Data API's exposed schemas. Nothing in
  it is granted to `anon`, `authenticated` or `service_role`.
- Every table and partition has RLS enabled, with no policies.
- The two RPCs in `public` are `SECURITY DEFINER` with `search_path = ''`.
  `EXECUTE` is revoked from `PUBLIC`, `anon` and `authenticated`, and granted
  to `service_role` only. The project's `sb_secret_...` key maps to
  `service_role`. The publishable key can reach nothing. `verify.sql` checks
  all of this and also probes as `anon`.

## Without pg_cron

`0004_schedule.sql` prints a notice and does nothing if `pg_cron` is missing.
Then call `select telemetry.run_maintenance();` hourly from any scheduler
connected as `postgres`.

## Size budget (Free plan: 500 MB, alert at 300 MB)

Measured on Postgres 18 with the golden example (one deployment, one week of
5-minute reports):

| Table | Row (tuple) | On disk with indexes | Per always-on deployment |
|---|---|---|---|
| `rollup_5m` | ~800 B | ~1.8 KB (PK, partial index, downsampling update) | 2,016 rows (7 days): ~3.6 MB |
| `rollup_hourly` | ~820 B | ~1 KB | 2,160 rows (90 days): ~2.2 MB |
| `rollup_daily` | ~780 B | ~1 KB | per configuration: ~0.4 MB per year |

Fixed overhead: about 100 empty partitions take about 2 MB. Supabase's own
schemas come on top; `telemetry.db_size` reports the real total.

So each continuously reporting deployment costs about 6 MB in steady state.
The 300 MB alert fires at roughly 45 to 50 such deployments, and 500 MB at
roughly 80. Deployments that run only part of the day cost proportionally
less. When the alert fires, move to Pro (8 GB, roughly 1,300 deployments), as
decided. Shortening the 5-minute retention is the other lever: 2 days instead
of 7 saves about 2.6 MB per deployment.

```sql
select database_size, over_alert_300mb, telemetry_tables from telemetry.db_size;
```
