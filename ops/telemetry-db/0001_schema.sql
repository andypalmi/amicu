-- amicu telemetry database, migration 0001: schema, tables, lockdown.
--
-- Target: the dedicated Supabase project `amicu-telemetry` (not the product
-- database). Apply as `postgres` through the SQL Editor or psql, in file order
-- (see README.md). Safe to re-run.
--
-- Layout:
--   telemetry.rollup_5m      one row per deployment per 5-minute window, as
--                            uploaded. Daily range partitions, kept 7 days.
--   telemetry.rollup_hourly  per install / deployment / configuration / hour,
--                            built from rollup_5m. Daily partitions, kept 90 days.
--   telemetry.rollup_daily   anonymous: no install or deployment id. Kept.
--   telemetry.ingested_batches  batch ids seen, for idempotent retries.
--
-- Latency is stored as histogram bucket counts (bigint[]) over the fixed
-- layouts in telemetry.histogram_layouts, so every downsampling step adds
-- counts and is exact. There is no per-request table.
--
-- Access: the `telemetry` schema is not exposed through the Data API and grants
-- nothing to anon, authenticated or service_role. Every table has RLS enabled
-- with no policies. The only entry points are the SECURITY DEFINER functions in
-- 0003_ingest.sql, executable by service_role alone.

begin;

create schema if not exists telemetry;

-- Lock the schema down. Nothing in it is reachable except through the
-- SECURITY DEFINER functions defined later.
revoke all on schema telemetry from public;
do $$
declare r text;
begin
  foreach r in array array['anon', 'authenticated', 'service_role'] loop
    if exists (select 1 from pg_roles where rolname = r) then
      execute format('revoke all on schema telemetry from %I', r);
      execute format('revoke all on all tables in schema telemetry from %I', r);
      execute format('revoke all on all sequences in schema telemetry from %I', r);
      execute format('revoke all on all functions in schema telemetry from %I', r);
      execute format('alter default privileges in schema telemetry revoke all on tables from %I', r);
      execute format('alter default privileges in schema telemetry revoke all on sequences from %I', r);
      execute format('alter default privileges in schema telemetry revoke all on functions from %I', r);
    end if;
  end loop;
end
$$;
alter default privileges in schema telemetry revoke all on tables from public;
alter default privileges in schema telemetry revoke all on functions from public;

-- ---------------------------------------------------------------------------
-- Histogram layouts (schema version 2). Must match the `x-amicu` bounds in
-- crates/amicu-telemetry/schema/telemetry-batch.v2.schema.json.
-- counts[i] holds observations in (bounds[i-1], bounds[i]] ms; one extra
-- trailing count holds observations above the last bound.
-- ---------------------------------------------------------------------------
create table if not exists telemetry.histogram_layouts (
  name           text primary key,
  schema_version smallint not null,
  bounds_ms      double precision[] not null
);

insert into telemetry.histogram_layouts (name, schema_version, bounds_ms) values
  ('ttft_ms', 2, '{10,20,40,60,80,100,250,500,750,1000,2500,5000,7500,10000,20000,40000,80000}'),
  ('e2e_ms',  2, '{300,500,800,1000,1500,2000,2500,5000,10000,15000,20000,30000,40000,50000,60000,120000,240000,480000}'),
  ('tpot_ms', 2, '{10,25,50,75,100,150,200,300,400,500,750,1000,2500}')
on conflict (name) do update
  set schema_version = excluded.schema_version, bounds_ms = excluded.bounds_ms;

-- ---------------------------------------------------------------------------
-- Additive helpers used by downsampling.
-- ---------------------------------------------------------------------------

-- Element-wise sum of two equal-length count arrays. NULL acts as "no
-- histogram"; mismatched lengths cannot happen (ingest checks the layout) and
-- raise rather than silently truncate.
create or replace function telemetry.hist_add(a bigint[], b bigint[])
returns bigint[]
language plpgsql immutable parallel safe
set search_path = ''
as $$
begin
  if a is null then return b; end if;
  if b is null then return a; end if;
  if cardinality(a) <> cardinality(b) then
    raise exception 'histogram length mismatch (% vs %)', cardinality(a), cardinality(b);
  end if;
  return array(select x + y from unnest(a, b) as t(x, y));
end
$$;

do $$
begin
  if not exists (
    select 1 from pg_proc p join pg_namespace n on n.oid = p.pronamespace
    where n.nspname = 'telemetry' and p.proname = 'hist_sum' and p.prokind = 'a'
  ) then
    create aggregate telemetry.hist_sum(bigint[]) (sfunc = telemetry.hist_add, stype = bigint[]);
  end if;
end
$$;

-- NULL-preserving addition: NULL + NULL is NULL (the metric was never
-- reported), otherwise missing sides count as zero.
create or replace function telemetry.add_nullable(a bigint, b bigint)
returns bigint
language sql immutable parallel safe
set search_path = ''
as $$ select case when a is null and b is null then null else coalesce(a, 0) + coalesce(b, 0) end $$;

create or replace function telemetry.add_nullable(a double precision, b double precision)
returns double precision
language sql immutable parallel safe
set search_path = ''
as $$ select case when a is null and b is null then null else coalesce(a, 0) + coalesce(b, 0) end $$;

-- Approximate quantile from bucket counts, by linear interpolation inside the
-- bucket that holds rank q * total. The overflow bucket reports the last
-- bound (a lower bound on the true value). NULL for an empty histogram.
create or replace function telemetry.hist_quantile(counts bigint[], bounds_ms double precision[], q double precision)
returns double precision
language plpgsql immutable parallel safe
set search_path = ''
as $$
declare
  total numeric := 0;
  rank numeric;
  cum numeric := 0;
  i int;
  lo double precision;
  hi double precision;
begin
  if counts is null or q is null or q < 0 or q > 1 then return null; end if;
  select coalesce(sum(c), 0) into total from unnest(counts) as c;
  if total = 0 then return null; end if;
  rank := q * total;
  for i in 1 .. cardinality(counts) loop
    if counts[i] > 0 and cum + counts[i] >= rank then
      if i > cardinality(bounds_ms) then
        return bounds_ms[cardinality(bounds_ms)];
      end if;
      lo := case when i = 1 then 0 else bounds_ms[i - 1] end;
      hi := bounds_ms[i];
      return lo + (hi - lo) * ((rank - cum) / counts[i])::double precision;
    end if;
    cum := cum + counts[i];
  end loop;
  return bounds_ms[cardinality(bounds_ms)];
end
$$;

-- ---------------------------------------------------------------------------
-- 5-minute rollups, as uploaded. Column names follow the v2 payload
-- (crates/amicu-telemetry/src/report.rs): hardware.* and the model/runtime
-- objects are flattened.
-- ---------------------------------------------------------------------------
create table if not exists telemetry.rollup_5m (
  install_id                uuid not null,
  deployment_id             uuid not null,
  window_start              timestamptz not null,
  window_s                  integer not null check (window_s between 1 and 86400),
  schema_version            smallint not null,
  amicu_version             text not null,
  accelerator               text,
  accelerator_count         integer not null default 0,
  vram_bytes                bigint,
  cpu_cores                 integer,
  memory_bytes              bigint,
  model_name                text not null,
  quantization              text,
  runtime_name              text not null,
  runtime_version           text,
  provider                  text,
  region                    text,
  context_length            integer,
  batch_size                integer,
  requests                  bigint,
  errors                    bigint,
  prompt_tokens             bigint not null default 0,
  output_tokens             bigint not null default 0,
  output_tokens_per_s       double precision not null default 0,
  decode_tokens_per_s       double precision,
  ttft_counts               bigint[],
  ttft_sum_ms               double precision,
  e2e_counts                bigint[],
  e2e_sum_ms                double precision,
  tpot_counts               bigint[],
  tpot_sum_ms               double precision,
  running_peak              integer,
  waiting_peak              integer,
  kv_cache_utilization_peak double precision,
  preemptions               bigint,
  gpu_utilization           double precision,
  gpu_memory_peak_bytes     bigint,
  host_memory_peak_bytes    bigint,
  ingested_at               timestamptz not null default now(),
  -- Set by telemetry.downsample_hourly() once the row is in rollup_hourly.
  downsampled               boolean not null default false,
  -- One row per deployment and window: a retried or re-packed report is a
  -- no-op (ON CONFLICT DO NOTHING), so counts are never doubled.
  primary key (install_id, deployment_id, window_start)
) partition by range (window_start);

create index if not exists rollup_5m_pending_idx
  on telemetry.rollup_5m (window_start) where not downsampled;

-- ---------------------------------------------------------------------------
-- Hourly rollups per install, deployment and configuration. `config_key` is a
-- hash of the configuration columns (a deployment can change runtime version
-- mid-hour). Rates are stored as rate x seconds plus the seconds observed, so
-- they add; means are derived at query time.
-- ---------------------------------------------------------------------------
create table if not exists telemetry.rollup_hourly (
  install_id                uuid not null,
  deployment_id             uuid not null,
  hour_start                timestamptz not null,
  config_key                uuid not null,
  amicu_version             text not null,
  accelerator               text,
  accelerator_count         integer not null,
  vram_bytes                bigint,
  cpu_cores                 integer,
  memory_bytes              bigint,
  model_name                text not null,
  quantization              text,
  runtime_name              text not null,
  runtime_version           text,
  provider                  text,
  region                    text,
  context_length            integer,
  batch_size                integer,
  reports                   integer not null,
  window_s                  bigint not null,
  requests                  bigint,
  errors                    bigint,
  prompt_tokens             bigint not null,
  output_tokens             bigint not null,
  output_tps_x_s            double precision not null,
  decode_tps_x_s            double precision,
  decode_tps_s              bigint,
  gpu_util_x_s              double precision,
  gpu_util_s                bigint,
  ttft_counts               bigint[],
  ttft_sum_ms               double precision,
  e2e_counts                bigint[],
  e2e_sum_ms                double precision,
  tpot_counts               bigint[],
  tpot_sum_ms               double precision,
  running_peak              integer,
  waiting_peak              integer,
  kv_cache_utilization_peak double precision,
  preemptions               bigint,
  gpu_memory_peak_bytes     bigint,
  host_memory_peak_bytes    bigint,
  primary key (install_id, deployment_id, hour_start, config_key)
) partition by range (hour_start);

-- ---------------------------------------------------------------------------
-- Daily aggregates. Anonymous: no install or deployment id, and no
-- host-specific columns (cpu_cores, memory_bytes, region). `installs` and
-- `deployments` are distinct counts for minimum-sample thresholds. Rebuilt for
-- recent days by telemetry.rebuild_daily(); older days are final.
-- ---------------------------------------------------------------------------
create table if not exists telemetry.rollup_daily (
  day                       date not null,
  config_key                uuid not null,
  amicu_version             text not null,
  accelerator               text,
  accelerator_count         integer not null,
  vram_bytes                bigint,
  model_name                text not null,
  quantization              text,
  runtime_name              text not null,
  runtime_version           text,
  provider                  text,
  context_length            integer,
  batch_size                integer,
  installs                  integer not null,
  deployments               integer not null,
  reports                   integer not null,
  window_s                  bigint not null,
  requests                  bigint,
  errors                    bigint,
  prompt_tokens             bigint not null,
  output_tokens             bigint not null,
  output_tps_x_s            double precision not null,
  decode_tps_x_s            double precision,
  decode_tps_s              bigint,
  gpu_util_x_s              double precision,
  gpu_util_s                bigint,
  ttft_counts               bigint[],
  ttft_sum_ms               double precision,
  e2e_counts                bigint[],
  e2e_sum_ms                double precision,
  tpot_counts               bigint[],
  tpot_sum_ms               double precision,
  running_peak              integer,
  waiting_peak              integer,
  kv_cache_utilization_peak double precision,
  preemptions               bigint,
  gpu_memory_peak_bytes     bigint,
  host_memory_peak_bytes    bigint,
  primary key (day, config_key)
);

-- Benchmark lookups (model on hardware, by runtime, over time).
create index if not exists rollup_daily_model_hw_idx
  on telemetry.rollup_daily (model_name, accelerator, runtime_name, day);
create index if not exists rollup_daily_hw_idx
  on telemetry.rollup_daily (accelerator, accelerator_count, day);

-- ---------------------------------------------------------------------------
-- Batch ids already ingested. Kept 8 days: longer than the sender's 7-day
-- buffer age, after which every report in a batch is past retention anyway.
-- ---------------------------------------------------------------------------
create table if not exists telemetry.ingested_batches (
  batch_id    uuid primary key,
  received_at timestamptz not null default now(),
  reports     smallint not null
);
create index if not exists ingested_batches_received_idx
  on telemetry.ingested_batches (received_at);

-- RLS on every table, no policies: only the owner (postgres, through the
-- SECURITY DEFINER functions) can read or write. Partitions get the same in
-- telemetry.ensure_partitions().
alter table telemetry.histogram_layouts enable row level security;
alter table telemetry.rollup_5m enable row level security;
alter table telemetry.rollup_hourly enable row level security;
alter table telemetry.rollup_daily enable row level security;
alter table telemetry.ingested_batches enable row level security;

revoke all on all tables in schema telemetry from public;
revoke all on all functions in schema telemetry from public;

commit;
