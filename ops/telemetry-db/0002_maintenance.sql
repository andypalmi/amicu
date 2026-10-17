-- amicu telemetry database, migration 0002: partitions, downsampling,
-- retention, size report. Safe to re-run.
--
-- telemetry.run_maintenance() does everything below in order and is what the
-- scheduler calls (0004_schedule.sql, hourly):
--   1. ensure_partitions()       daily partitions from the retention edge to 3 days ahead
--   2. downsample_hourly()       new 5-minute rows -> rollup_hourly (additive, exact)
--   3. rebuild_daily()           last 8 days of rollup_daily recomputed from rollup_hourly
--   4. drop_expired_partitions() 5-minute partitions past 7 days, hourly past 90 days
--   5. purge of ingested_batches older than 8 days
--
-- Downsampling runs before the drop so a late report is always counted in the
-- hourly table before its 5-minute partition goes.

begin;

-- Retention, in one place.
create or replace function telemetry.retention(tbl text)
returns interval
language sql immutable parallel safe
set search_path = ''
as $$
  select case tbl
    when 'rollup_5m' then interval '7 days'
    when 'rollup_hourly' then interval '90 days'
  end
$$;

-- Create the daily partitions of rollup_5m and rollup_hourly covering
-- [now - retention, today + days_ahead]. Each partition gets RLS and no grants.
create or replace function telemetry.ensure_partitions(days_ahead int default 3)
returns int
language plpgsql
set search_path = ''
as $$
declare
  tbl text;
  d date;
  first_day date;
  last_day date := (now() at time zone 'utc')::date + days_ahead;
  part text;
  created int := 0;
begin
  foreach tbl in array array['rollup_5m', 'rollup_hourly'] loop
    first_day := ((now() - telemetry.retention(tbl)) at time zone 'utc')::date;
    d := first_day;
    while d <= last_day loop
      part := tbl || '_p' || to_char(d, 'YYYYMMDD');
      if to_regclass('telemetry.' || part) is null then
        execute format(
          'create table telemetry.%I partition of telemetry.%I for values from (%L) to (%L)',
          part, tbl,
          (d::timestamp at time zone 'utc'),
          ((d + 1)::timestamp at time zone 'utc'));
        execute format('alter table telemetry.%I enable row level security', part);
        execute format('revoke all on telemetry.%I from public', part);
        if exists (select 1 from pg_roles where rolname = 'anon') then
          execute format('revoke all on telemetry.%I from anon, authenticated, service_role', part);
        end if;
        created := created + 1;
      end if;
      d := d + 1;
    end loop;
  end loop;
  return created;
end
$$;

-- Drop partitions whose upper bound is older than the table's retention.
create or replace function telemetry.drop_expired_partitions()
returns int
language plpgsql
set search_path = ''
as $$
declare
  tbl text;
  r record;
  dropped int := 0;
begin
  foreach tbl in array array['rollup_5m', 'rollup_hourly'] loop
    for r in
      select c.relname
      from pg_inherits i
      join pg_class c on c.oid = i.inhrelid
      join pg_class p on p.oid = i.inhparent
      join pg_namespace n on n.oid = p.relnamespace
      where n.nspname = 'telemetry' and p.relname = tbl
        and c.relname ~ ('^' || tbl || '_p[0-9]{8}$')
    loop
      -- Partition for day D covers [D, D + 1 day); its upper bound is D + 1.
      if ((to_date(right(r.relname, 8), 'YYYYMMDD') + 1)::timestamp at time zone 'utc')
         <= now() - telemetry.retention(tbl) then
        execute format('drop table telemetry.%I', r.relname);
        dropped := dropped + 1;
      end if;
    end loop;
  end loop;
  return dropped;
end
$$;

-- Hash of the configuration columns that key rollup_hourly.
create or replace function telemetry.hourly_config_key(
  amicu_version text, accelerator text, accelerator_count int, vram_bytes bigint,
  cpu_cores int, memory_bytes bigint, model_name text, quantization text,
  runtime_name text, runtime_version text, provider text, region text,
  context_length int, batch_size int)
returns uuid
language sql immutable parallel safe
set search_path = ''
as $$
  select md5(jsonb_build_array(amicu_version, accelerator, accelerator_count, vram_bytes,
    cpu_cores, memory_bytes, model_name, quantization, runtime_name, runtime_version,
    provider, region, context_length, batch_size)::text)::uuid
$$;

-- Hash of the (anonymous) configuration columns that key rollup_daily.
create or replace function telemetry.daily_config_key(
  amicu_version text, accelerator text, accelerator_count int, vram_bytes bigint,
  model_name text, quantization text, runtime_name text, runtime_version text,
  provider text, context_length int, batch_size int)
returns uuid
language sql immutable parallel safe
set search_path = ''
as $$
  select md5(jsonb_build_array(amicu_version, accelerator, accelerator_count, vram_bytes,
    model_name, quantization, runtime_name, runtime_version, provider, context_length,
    batch_size)::text)::uuid
$$;

-- Fold every 5-minute row not yet downsampled into rollup_hourly, adding to
-- any existing hourly row. Each 5-minute row is claimed (downsampled = true)
-- in the same statement, so a row is counted exactly once even when reports
-- for an hour arrive late or across several runs. Rows of transactions still
-- in flight are invisible here and are picked up by the next run.
create or replace function telemetry.downsample_hourly()
returns int
language plpgsql
set search_path = ''
as $$
declare
  n int;
begin
  with claimed as (
    update telemetry.rollup_5m set downsampled = true
    where not downsampled
    returning *
  ),
  grouped as (
    select
      install_id, deployment_id,
      date_trunc('hour', window_start, 'UTC') as hour_start,
      telemetry.hourly_config_key(amicu_version, accelerator, accelerator_count, vram_bytes,
        cpu_cores, memory_bytes, model_name, quantization, runtime_name, runtime_version,
        provider, region, context_length, batch_size) as config_key,
      amicu_version, accelerator, accelerator_count, vram_bytes, cpu_cores, memory_bytes,
      model_name, quantization, runtime_name, runtime_version, provider, region,
      context_length, batch_size,
      count(*)::int as reports,
      sum(window_s)::bigint as window_s,
      sum(requests)::bigint as requests,
      sum(errors)::bigint as errors,
      sum(prompt_tokens)::bigint as prompt_tokens,
      sum(output_tokens)::bigint as output_tokens,
      sum(output_tokens_per_s * window_s) as output_tps_x_s,
      sum(decode_tokens_per_s * window_s) as decode_tps_x_s,
      sum(window_s) filter (where decode_tokens_per_s is not null)::bigint as decode_tps_s,
      sum(gpu_utilization * window_s) as gpu_util_x_s,
      sum(window_s) filter (where gpu_utilization is not null)::bigint as gpu_util_s,
      telemetry.hist_sum(ttft_counts) as ttft_counts,
      sum(ttft_sum_ms) as ttft_sum_ms,
      telemetry.hist_sum(e2e_counts) as e2e_counts,
      sum(e2e_sum_ms) as e2e_sum_ms,
      telemetry.hist_sum(tpot_counts) as tpot_counts,
      sum(tpot_sum_ms) as tpot_sum_ms,
      max(running_peak) as running_peak,
      max(waiting_peak) as waiting_peak,
      max(kv_cache_utilization_peak) as kv_cache_utilization_peak,
      sum(preemptions)::bigint as preemptions,
      max(gpu_memory_peak_bytes) as gpu_memory_peak_bytes,
      max(host_memory_peak_bytes) as host_memory_peak_bytes
    from claimed
    group by install_id, deployment_id, date_trunc('hour', window_start, 'UTC'),
      amicu_version, accelerator, accelerator_count, vram_bytes, cpu_cores, memory_bytes,
      model_name, quantization, runtime_name, runtime_version, provider, region,
      context_length, batch_size
  )
  insert into telemetry.rollup_hourly as h (
    install_id, deployment_id, hour_start, config_key,
    amicu_version, accelerator, accelerator_count, vram_bytes, cpu_cores, memory_bytes,
    model_name, quantization, runtime_name, runtime_version, provider, region,
    context_length, batch_size, reports, window_s, requests, errors, prompt_tokens,
    output_tokens, output_tps_x_s, decode_tps_x_s, decode_tps_s, gpu_util_x_s, gpu_util_s,
    ttft_counts, ttft_sum_ms, e2e_counts, e2e_sum_ms, tpot_counts, tpot_sum_ms,
    running_peak, waiting_peak, kv_cache_utilization_peak, preemptions,
    gpu_memory_peak_bytes, host_memory_peak_bytes)
  select * from grouped
  on conflict (install_id, deployment_id, hour_start, config_key) do update set
    reports = h.reports + excluded.reports,
    window_s = h.window_s + excluded.window_s,
    requests = telemetry.add_nullable(h.requests, excluded.requests),
    errors = telemetry.add_nullable(h.errors, excluded.errors),
    prompt_tokens = h.prompt_tokens + excluded.prompt_tokens,
    output_tokens = h.output_tokens + excluded.output_tokens,
    output_tps_x_s = h.output_tps_x_s + excluded.output_tps_x_s,
    decode_tps_x_s = telemetry.add_nullable(h.decode_tps_x_s, excluded.decode_tps_x_s),
    decode_tps_s = telemetry.add_nullable(h.decode_tps_s, excluded.decode_tps_s),
    gpu_util_x_s = telemetry.add_nullable(h.gpu_util_x_s, excluded.gpu_util_x_s),
    gpu_util_s = telemetry.add_nullable(h.gpu_util_s, excluded.gpu_util_s),
    ttft_counts = telemetry.hist_add(h.ttft_counts, excluded.ttft_counts),
    ttft_sum_ms = telemetry.add_nullable(h.ttft_sum_ms, excluded.ttft_sum_ms),
    e2e_counts = telemetry.hist_add(h.e2e_counts, excluded.e2e_counts),
    e2e_sum_ms = telemetry.add_nullable(h.e2e_sum_ms, excluded.e2e_sum_ms),
    tpot_counts = telemetry.hist_add(h.tpot_counts, excluded.tpot_counts),
    tpot_sum_ms = telemetry.add_nullable(h.tpot_sum_ms, excluded.tpot_sum_ms),
    running_peak = greatest(h.running_peak, excluded.running_peak),
    waiting_peak = greatest(h.waiting_peak, excluded.waiting_peak),
    kv_cache_utilization_peak = greatest(h.kv_cache_utilization_peak, excluded.kv_cache_utilization_peak),
    preemptions = telemetry.add_nullable(h.preemptions, excluded.preemptions),
    gpu_memory_peak_bytes = greatest(h.gpu_memory_peak_bytes, excluded.gpu_memory_peak_bytes),
    host_memory_peak_bytes = greatest(h.host_memory_peak_bytes, excluded.host_memory_peak_bytes);
  get diagnostics n = row_count;
  return n;
end
$$;

-- Recompute rollup_daily for the last `days` UTC days (today included) from
-- rollup_hourly. 8 days covers every report that can still arrive (7-day
-- 5-minute retention) and keeps the distinct install counts exact. Days older
-- than that are final; a deletion by install id does not touch them.
create or replace function telemetry.rebuild_daily(days int default 8)
returns int
language plpgsql
set search_path = ''
as $$
declare
  since date := (now() at time zone 'utc')::date - days;
  n int;
begin
  delete from telemetry.rollup_daily where day >= since;
  insert into telemetry.rollup_daily (
    day, config_key, amicu_version, accelerator, accelerator_count, vram_bytes,
    model_name, quantization, runtime_name, runtime_version, provider, context_length,
    batch_size, installs, deployments, reports, window_s, requests, errors, prompt_tokens,
    output_tokens, output_tps_x_s, decode_tps_x_s, decode_tps_s, gpu_util_x_s, gpu_util_s,
    ttft_counts, ttft_sum_ms, e2e_counts, e2e_sum_ms, tpot_counts, tpot_sum_ms,
    running_peak, waiting_peak, kv_cache_utilization_peak, preemptions,
    gpu_memory_peak_bytes, host_memory_peak_bytes)
  select
    (hour_start at time zone 'utc')::date,
    telemetry.daily_config_key(amicu_version, accelerator, accelerator_count, vram_bytes,
      model_name, quantization, runtime_name, runtime_version, provider, context_length,
      batch_size),
    amicu_version, accelerator, accelerator_count, vram_bytes, model_name, quantization,
    runtime_name, runtime_version, provider, context_length, batch_size,
    count(distinct install_id)::int,
    count(distinct deployment_id)::int,
    sum(reports)::int,
    sum(window_s)::bigint,
    sum(requests)::bigint,
    sum(errors)::bigint,
    sum(prompt_tokens)::bigint,
    sum(output_tokens)::bigint,
    sum(output_tps_x_s),
    sum(decode_tps_x_s),
    sum(decode_tps_s)::bigint,
    sum(gpu_util_x_s),
    sum(gpu_util_s)::bigint,
    telemetry.hist_sum(ttft_counts), sum(ttft_sum_ms),
    telemetry.hist_sum(e2e_counts), sum(e2e_sum_ms),
    telemetry.hist_sum(tpot_counts), sum(tpot_sum_ms),
    max(running_peak), max(waiting_peak), max(kv_cache_utilization_peak),
    sum(preemptions)::bigint, max(gpu_memory_peak_bytes), max(host_memory_peak_bytes)
  from telemetry.rollup_hourly
  where hour_start >= (since::timestamp at time zone 'utc')
  group by 1, 2, amicu_version, accelerator, accelerator_count, vram_bytes, model_name,
    quantization, runtime_name, runtime_version, provider, context_length, batch_size;
  get diagnostics n = row_count;
  return n;
end
$$;

-- The last completed maintenance run, for telemetry_health(). One row.
create table if not exists telemetry.maintenance_runs (
  id          boolean primary key default true check (id),
  finished_at timestamptz not null,
  result      jsonb not null
);
alter table telemetry.maintenance_runs enable row level security;

-- The scheduled job. Serialized with an advisory lock so overlapping runs
-- (a slow run, or a manual call during a scheduled one) cannot double-fold.
create or replace function telemetry.run_maintenance()
returns jsonb
language plpgsql
set search_path = ''
as $$
declare
  result jsonb;
  purged int;
begin
  if not pg_try_advisory_xact_lock(hashtext('amicu.telemetry.maintenance')) then
    return jsonb_build_object('skipped', 'another run holds the lock');
  end if;
  result := jsonb_build_object('partitions_created', telemetry.ensure_partitions());
  result := result || jsonb_build_object('hourly_rows_upserted', telemetry.downsample_hourly());
  result := result || jsonb_build_object('daily_rows_rebuilt', telemetry.rebuild_daily());
  result := result || jsonb_build_object('partitions_dropped', telemetry.drop_expired_partitions());
  delete from telemetry.ingested_batches where received_at < now() - interval '8 days';
  get diagnostics purged = row_count;
  result := result || jsonb_build_object('batch_ids_purged', purged);
  insert into telemetry.maintenance_runs (id, finished_at, result) values (true, now(), result)
    on conflict (id) do update set finished_at = excluded.finished_at, result = excluded.result;
  return result;
end
$$;

-- Health of the scheduled maintenance: a stalled job means no new partitions,
-- and inserts start failing (503 to senders) once the 3 days made ahead run
-- out. Check weekly, or after any incident:
--   select public.telemetry_health();
-- `ok` is true when tomorrow's partition exists and maintenance finished in
-- the last 3 hours. Callable by service_role (and postgres); no data rows.
create or replace function public.telemetry_health()
returns jsonb
language plpgsql
security definer
set search_path = ''
as $$
declare
  newest_5m date;
  newest_hourly date;
  last_run timestamptz;
  last_cron timestamptz;
  last_report timestamptz;
begin
  select max(to_date(right(c.relname, 8), 'YYYYMMDD')) into newest_5m
    from pg_inherits i join pg_class c on c.oid = i.inhrelid
    where i.inhparent = 'telemetry.rollup_5m'::regclass;
  select max(to_date(right(c.relname, 8), 'YYYYMMDD')) into newest_hourly
    from pg_inherits i join pg_class c on c.oid = i.inhrelid
    where i.inhparent = 'telemetry.rollup_hourly'::regclass;
  select finished_at into last_run from telemetry.maintenance_runs;
  if to_regclass('cron.job_run_details') is not null then
    execute $q$
      select max(d.end_time) from cron.job_run_details d join cron.job j on j.jobid = d.jobid
      where j.jobname = 'amicu-telemetry-maintenance' and d.status = 'succeeded'
    $q$ into last_cron;
  end if;
  select max(ingested_at) into last_report from telemetry.rollup_5m;
  return jsonb_build_object(
    'ok', coalesce(newest_5m > (now() at time zone 'utc')::date
                   and newest_hourly > (now() at time zone 'utc')::date
                   and last_run > now() - interval '3 hours', false),
    'newest_5m_partition', newest_5m,
    'newest_hourly_partition', newest_hourly,
    'last_maintenance_at', last_run,
    'last_cron_success_at', last_cron,
    'newest_report_ingested_at', last_report);
end
$$;
revoke all on function public.telemetry_health() from public;
do $$
begin
  if exists (select 1 from pg_roles where rolname = 'anon') then
    revoke all on function public.telemetry_health() from anon, authenticated;
  end if;
  if exists (select 1 from pg_roles where rolname = 'service_role') then
    grant execute on function public.telemetry_health() to service_role;
  end if;
end
$$;

-- Size report against the Free plan (500 MB; alert at 300 MB).
--   select * from telemetry.db_size;
create or replace view telemetry.db_size as
with tables as (
  select
    coalesce(p.relname, c.relname) as table_name,
    sum(pg_total_relation_size(c.oid)) as bytes
  from pg_class c
  join pg_namespace n on n.oid = c.relnamespace
  left join pg_inherits i on i.inhrelid = c.oid
  left join pg_class p on p.oid = i.inhparent
  where n.nspname = 'telemetry' and c.relkind = 'r'
  group by 1
)
select
  pg_database_size(current_database()) as database_bytes,
  pg_size_pretty(pg_database_size(current_database())) as database_size,
  pg_database_size(current_database()) >= 300 * 1024 * 1024 as over_alert_300mb,
  (select jsonb_object_agg(table_name, pg_size_pretty(bytes::bigint)) from tables) as telemetry_tables;

-- Benchmark view over the anonymous daily table, for amicu-recommend and
-- published benchmarks. Only configurations seen from at least 5 distinct
-- installs on some day are listed (max of the daily distinct counts: a lower
-- bound on the distinct installs over the period).
create or replace view telemetry.benchmark_30d as
select
  model_name, quantization, runtime_name, accelerator, accelerator_count, context_length,
  max(installs) as min_installs,
  sum(reports) as reports,
  sum(requests) as requests,
  case when sum(window_s) > 0 then sum(output_tps_x_s) / sum(window_s) end as output_tokens_per_s,
  case when sum(decode_tps_s) > 0 then sum(decode_tps_x_s) / sum(decode_tps_s) end as decode_tokens_per_s,
  telemetry.hist_quantile(telemetry.hist_sum(ttft_counts),
    (select bounds_ms from telemetry.histogram_layouts where name = 'ttft_ms'), 0.5) as ttft_p50_ms,
  telemetry.hist_quantile(telemetry.hist_sum(ttft_counts),
    (select bounds_ms from telemetry.histogram_layouts where name = 'ttft_ms'), 0.95) as ttft_p95_ms,
  telemetry.hist_quantile(telemetry.hist_sum(tpot_counts),
    (select bounds_ms from telemetry.histogram_layouts where name = 'tpot_ms'), 0.5) as tpot_p50_ms,
  telemetry.hist_quantile(telemetry.hist_sum(e2e_counts),
    (select bounds_ms from telemetry.histogram_layouts where name = 'e2e_ms'), 0.5) as e2e_p50_ms
from telemetry.rollup_daily
where day >= (now() at time zone 'utc')::date - 30
group by model_name, quantization, runtime_name, accelerator, accelerator_count, context_length
having max(installs) >= 5;

-- Views run with the caller's rights; keep them owner-only like the tables.
alter view telemetry.db_size set (security_invoker = true);
alter view telemetry.benchmark_30d set (security_invoker = true);
revoke all on telemetry.db_size, telemetry.benchmark_30d from public;
revoke all on all functions in schema telemetry from public;
do $$
begin
  if exists (select 1 from pg_roles where rolname = 'anon') then
    revoke all on telemetry.db_size, telemetry.benchmark_30d from anon, authenticated, service_role;
    revoke all on all functions in schema telemetry from anon, authenticated, service_role;
  end if;
end
$$;

select telemetry.run_maintenance();

commit;
