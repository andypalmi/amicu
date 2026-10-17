-- amicu telemetry database, migration 0003: the write path. Safe to re-run.
--
-- public.ingest_rollup_batch(batch jsonb)  POST /rest/v1/rpc/ingest_rollup_batch
-- public.delete_install(install_id uuid)   POST /rest/v1/rpc/delete_install
--
-- Both are SECURITY DEFINER (they run as their owner, postgres) with an empty
-- search_path, and only service_role may execute them. They live in `public`
-- because that is the schema the Data API exposes; the tables stay in the
-- unexposed `telemetry` schema.
--
-- ingest_rollup_batch validates a schema-v2 batch (the format in
-- crates/amicu-telemetry/schema/telemetry-batch.v2.schema.json) and inserts
-- it in one transaction. A malformed batch raises SQLSTATE PT400, which the
-- Data API answers as HTTP 400. Idempotency: each report is keyed
-- (install_id, deployment_id, window_start) and inserted with ON CONFLICT DO
-- NOTHING, so a retried batch, or a report re-sent in another batch, is never
-- counted twice. Batch ids are recorded for 8 days and a repeat is reported
-- as "duplicate": true.
-- Flood resistance: every report in a batch must carry the same install id,
-- and windows must be aligned 5-minute windows (start divisible by 300,
-- duration 300) starting between 8 days ago and 10 minutes from now; anything
-- else is rejected (PT400). A window 7 to 8 days old is valid but past
-- 5-minute retention: it is skipped, not stored, and the batch is still
-- accepted so the sender drains it.

begin;

-- Validation helpers. Each raises PT400 with a message that names the field
-- only, never its value.
create or replace function telemetry.bad(field text)
returns void
language plpgsql
set search_path = ''
as $$
begin
  raise exception 'invalid telemetry batch: %', field using errcode = 'PT400';
end
$$;

-- Reject keys outside `allowed` (the payload's additionalProperties: false).
create or replace function telemetry.check_object(obj jsonb, allowed text[], field text)
returns jsonb
language plpgsql
set search_path = ''
as $$
begin
  if obj is null or jsonb_typeof(obj) <> 'object' then
    perform telemetry.bad(field);
  end if;
  if exists (select 1 from jsonb_object_keys(obj) k where k <> all (allowed)) then
    perform telemetry.bad(field || ': unknown field');
  end if;
  return obj;
end
$$;

-- A number in [lo, hi]; integral when `integral`. NULL when absent and not
-- required. JSON null is not accepted (the schema has no nullable fields).
create or replace function telemetry.num(obj jsonb, key text, lo numeric, hi numeric,
  integral boolean, required boolean, field text)
returns numeric
language plpgsql
set search_path = ''
as $$
declare
  v jsonb := obj -> key;
  n numeric;
begin
  if v is null then
    if required then perform telemetry.bad(field); end if;
    return null;
  end if;
  if jsonb_typeof(v) <> 'number' then perform telemetry.bad(field); end if;
  n := v::numeric;
  if n < lo or n > hi or (integral and n <> trunc(n)) then perform telemetry.bad(field); end if;
  return n;
end
$$;

-- A label token: 1 to 64 chars of [A-Za-z0-9._+/-]. The Worker additionally
-- rejects IP-shaped and credential-shaped labels.
create or replace function telemetry.label(obj jsonb, key text, required boolean, field text)
returns text
language plpgsql
set search_path = ''
as $$
declare
  v jsonb := obj -> key;
  s text;
begin
  if v is null then
    if required then perform telemetry.bad(field); end if;
    return null;
  end if;
  if jsonb_typeof(v) <> 'string' then perform telemetry.bad(field); end if;
  s := v #>> '{}';
  if s !~ '^[A-Za-z0-9._+/-]{1,64}$' then perform telemetry.bad(field); end if;
  return s;
end
$$;

-- A byte count that must be a whole number of GiB (memory peaks, VRAM), and
-- optionally a power-of-two number of GiB (host memory).
create or replace function telemetry.gib(obj jsonb, key text, power_of_two boolean, field text)
returns bigint
language plpgsql
set search_path = ''
as $$
declare
  n numeric := telemetry.num(obj, key, 0, 1125899906842624, true, false, field);
begin
  if n is null then return null; end if;
  if n % 1073741824 <> 0 then perform telemetry.bad(field); end if;
  if power_of_two and n > 0
     and (n / 1073741824) not in (select (2::numeric) ^ e from generate_series(0, 20) e) then
    perform telemetry.bad(field);
  end if;
  return n;
end
$$;

create or replace function telemetry.uuid_v4(v jsonb, field text)
returns uuid
language plpgsql
set search_path = ''
as $$
begin
  if v is null or jsonb_typeof(v) <> 'string'
     or (v #>> '{}') !~ '^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$' then
    perform telemetry.bad(field);
  end if;
  return (v #>> '{}')::uuid;
end
$$;

-- A histogram {counts, sum_ms} over layout `layout`; returns the counts and
-- writes the sum through `sum_ms`. NULL when absent.
create or replace function telemetry.histogram(obj jsonb, key text, layout text, out counts bigint[], out sum_ms double precision)
language plpgsql stable
set search_path = ''
as $$
declare
  h jsonb := obj -> key;
  expected int;
  total numeric;
begin
  if h is null then return; end if;
  perform telemetry.check_object(h, array['counts', 'sum_ms'], key);
  if jsonb_typeof(h -> 'counts') is distinct from 'array' then perform telemetry.bad(key || '.counts'); end if;
  select cardinality(bounds_ms) + 1 into expected from telemetry.histogram_layouts where name = layout;
  if jsonb_array_length(h -> 'counts') <> expected then perform telemetry.bad(key || '.counts'); end if;
  if exists (
    select 1 from jsonb_array_elements(h -> 'counts') e
    where case when jsonb_typeof(e) <> 'number' then true
      else e::numeric < 0 or e::numeric <> trunc(e::numeric) or e::numeric > 10000000000 end
  ) then
    perform telemetry.bad(key || '.counts');
  end if;
  select array_agg(e::numeric::bigint order by i), sum(e::numeric)
    into counts, total
    from jsonb_array_elements(h -> 'counts') with ordinality as t(e, i);
  if total > 10000000000 then perform telemetry.bad(key || '.counts'); end if;
  sum_ms := telemetry.num(h, 'sum_ms', 0, 86400000 * total, false, true, key || '.sum_ms');
end
$$;

create or replace function public.ingest_rollup_batch(batch jsonb)
returns jsonb
language plpgsql
security definer
set search_path = ''
as $$
declare
  bid uuid;
  first_install jsonb;
  row5 telemetry.rollup_5m%rowtype;
  rep jsonb;
  hw jsonb;
  m jsonb;
  w jsonb;
  win_start timestamptz;
  requests bigint;
  errors bigint;
  accelerator text;
  accelerator_count int;
  ttft record;
  e2e record;
  tpot record;
  n int;
  inserted int := 0;
  duplicates int := 0;
  skipped int := 0;
begin
  perform telemetry.check_object(batch, array['schema_version', 'batch_id', 'reports'], 'batch');
  if batch -> 'schema_version' is distinct from '2'::jsonb then perform telemetry.bad('schema_version'); end if;
  bid := telemetry.uuid_v4(batch -> 'batch_id', 'batch_id');
  if jsonb_typeof(batch -> 'reports') is distinct from 'array'
     or jsonb_array_length(batch -> 'reports') not between 1 and 288 then
    perform telemetry.bad('reports');
  end if;

  first_install := batch #> '{reports,0,install_id}';
  for rep in select value from jsonb_array_elements(batch -> 'reports') loop
    perform telemetry.check_object(rep, array['schema_version', 'install_id', 'deployment_id',
      'window', 'amicu_version', 'hardware', 'model', 'runtime', 'provider', 'region',
      'context_length', 'batch_size', 'metrics'], 'report');
    if rep -> 'schema_version' is distinct from '2'::jsonb then perform telemetry.bad('report.schema_version'); end if;

    w := telemetry.check_object(rep -> 'window', array['start_unix_s', 'duration_s'], 'window');
    if rep -> 'install_id' is distinct from first_install then perform telemetry.bad('reports.install_id'); end if;
    win_start := to_timestamp(telemetry.num(w, 'start_unix_s', 1704067200, 4102444800, true, true, 'window.start_unix_s')::double precision);
    if (w ->> 'start_unix_s')::numeric % 300 <> 0 then perform telemetry.bad('window.start_unix_s'); end if;
    perform telemetry.num(w, 'duration_s', 300, 300, true, true, 'window.duration_s');
    if win_start < now() - interval '8 days' or win_start > now() + interval '10 minutes' then
      perform telemetry.bad('window.start_unix_s');
    end if;

    hw := coalesce(rep -> 'hardware', '{}'::jsonb);
    perform telemetry.check_object(hw, array['accelerator', 'accelerator_count', 'vram_bytes',
      'cpu_cores', 'memory_bytes'], 'hardware');
    accelerator := telemetry.label(hw, 'accelerator', false, 'hardware.accelerator');
    accelerator_count := coalesce(telemetry.num(hw, 'accelerator_count', 0, 1024, true, false, 'hardware.accelerator_count'), 0);
    if accelerator is not null and accelerator_count = 0 then perform telemetry.bad('hardware.accelerator_count'); end if;
    -- Fixed provider values (anything else is sent as "other").
    if (rep ? 'provider') and (rep -> 'provider') not in
       ('"local"', '"local_apple"', '"local_cpu"', '"on_prem"', '"modal"', '"other"') then
      perform telemetry.bad('provider');
    end if;

    perform telemetry.check_object(rep -> 'model', array['name', 'quantization'], 'model');
    perform telemetry.check_object(rep -> 'runtime', array['name', 'version'], 'runtime');

    m := telemetry.check_object(rep -> 'metrics', array['requests', 'errors', 'prompt_tokens',
      'output_tokens', 'output_tokens_per_s', 'decode_tokens_per_s', 'ttft_ms', 'e2e_ms',
      'tpot_ms', 'running_peak', 'waiting_peak', 'kv_cache_utilization_peak', 'preemptions',
      'gpu_utilization', 'gpu_memory_peak_bytes', 'host_memory_peak_bytes'], 'metrics');
    requests := telemetry.num(m, 'requests', 0, 10000000000, true, false, 'metrics.requests');
    errors := telemetry.num(m, 'errors', 0, 10000000000, true, false, 'metrics.errors');
    if errors > coalesce(requests, 0) then perform telemetry.bad('metrics.errors'); end if;
    select * into ttft from telemetry.histogram(m, 'ttft_ms', 'ttft_ms');
    select * into e2e from telemetry.histogram(m, 'e2e_ms', 'e2e_ms');
    select * into tpot from telemetry.histogram(m, 'tpot_ms', 'tpot_ms');

    row5.install_id := telemetry.uuid_v4(rep -> 'install_id', 'install_id');
    row5.deployment_id := telemetry.uuid_v4(rep -> 'deployment_id', 'deployment_id');
    row5.window_start := win_start;
    row5.window_s := (w ->> 'duration_s')::int;
    row5.schema_version := 2;
    row5.amicu_version := telemetry.label(rep, 'amicu_version', true, 'amicu_version');
    row5.accelerator := accelerator;
    row5.accelerator_count := accelerator_count;
    -- Sizes arrive coarsened: whole GiB; host memory a power-of-two number of GiB.
    row5.vram_bytes := telemetry.gib(hw, 'vram_bytes', false, 'hardware.vram_bytes');
    row5.cpu_cores := telemetry.num(hw, 'cpu_cores', 0, 256, true, false, 'hardware.cpu_cores');
    row5.memory_bytes := telemetry.gib(hw, 'memory_bytes', true, 'hardware.memory_bytes');
    row5.model_name := telemetry.label(rep -> 'model', 'name', true, 'model.name');
    row5.quantization := telemetry.label(rep -> 'model', 'quantization', false, 'model.quantization');
    row5.runtime_name := telemetry.label(rep -> 'runtime', 'name', true, 'runtime.name');
    row5.runtime_version := telemetry.label(rep -> 'runtime', 'version', false, 'runtime.version');
    row5.provider := telemetry.label(rep, 'provider', false, 'provider');
    row5.region := telemetry.label(rep, 'region', false, 'region');
    row5.context_length := telemetry.num(rep, 'context_length', 0, 16777216, true, false, 'context_length');
    row5.batch_size := telemetry.num(rep, 'batch_size', 0, 1048576, true, false, 'batch_size');
    row5.requests := requests;
    row5.errors := errors;
    row5.prompt_tokens := coalesce(telemetry.num(m, 'prompt_tokens', 0, 1125899906842624, true, false, 'metrics.prompt_tokens'), 0);
    row5.output_tokens := coalesce(telemetry.num(m, 'output_tokens', 0, 1125899906842624, true, false, 'metrics.output_tokens'), 0);
    row5.output_tokens_per_s := coalesce(telemetry.num(m, 'output_tokens_per_s', 0, 10000000, false, false, 'metrics.output_tokens_per_s'), 0);
    row5.decode_tokens_per_s := telemetry.num(m, 'decode_tokens_per_s', 0, 10000000, false, false, 'metrics.decode_tokens_per_s');
    row5.ttft_counts := ttft.counts;
    row5.ttft_sum_ms := ttft.sum_ms;
    row5.e2e_counts := e2e.counts;
    row5.e2e_sum_ms := e2e.sum_ms;
    row5.tpot_counts := tpot.counts;
    row5.tpot_sum_ms := tpot.sum_ms;
    row5.running_peak := telemetry.num(m, 'running_peak', 0, 1048576, true, false, 'metrics.running_peak');
    row5.waiting_peak := telemetry.num(m, 'waiting_peak', 0, 1048576, true, false, 'metrics.waiting_peak');
    row5.kv_cache_utilization_peak := telemetry.num(m, 'kv_cache_utilization_peak', 0, 1, false, false, 'metrics.kv_cache_utilization_peak');
    row5.preemptions := telemetry.num(m, 'preemptions', 0, 10000000000, true, false, 'metrics.preemptions');
    row5.gpu_utilization := telemetry.num(m, 'gpu_utilization', 0, 1, false, false, 'metrics.gpu_utilization');
    row5.gpu_memory_peak_bytes := telemetry.gib(m, 'gpu_memory_peak_bytes', false, 'metrics.gpu_memory_peak_bytes');
    row5.host_memory_peak_bytes := telemetry.gib(m, 'host_memory_peak_bytes', false, 'metrics.host_memory_peak_bytes');
    row5.ingested_at := now();
    row5.downsampled := false;

    -- Valid but past 5-minute retention: accept the batch, store nothing.
    if win_start < now() - interval '7 days' then
      skipped := skipped + 1;
      continue;
    end if;

    insert into telemetry.rollup_5m values (row5.*)
    on conflict (install_id, deployment_id, window_start) do nothing;
    get diagnostics n = row_count;
    if n = 1 then inserted := inserted + 1; else duplicates := duplicates + 1; end if;
  end loop;

  -- Recorded last, so a retried batch is validated like any other; its reports
  -- all hit the primary key above and insert nothing.
  insert into telemetry.ingested_batches (batch_id, reports)
    values (bid, jsonb_array_length(batch -> 'reports'))
    on conflict (batch_id) do nothing;
  get diagnostics n = row_count;

  return jsonb_build_object('duplicate', n = 0, 'inserted', inserted,
    'duplicates', duplicates, 'skipped', skipped);
end
$$;

-- Deletion by install id (privacy requests): removes the install's 5-minute and
-- hourly rows. Daily aggregates hold no install id and are kept, except that
-- the next maintenance run rebuilds the last 8 days without this install.
create or replace function public.delete_install(install_id uuid)
returns jsonb
language plpgsql
security definer
set search_path = ''
as $$
declare
  n5 int;
  nh int;
begin
  delete from telemetry.rollup_5m r where r.install_id = delete_install.install_id;
  get diagnostics n5 = row_count;
  delete from telemetry.rollup_hourly r where r.install_id = delete_install.install_id;
  get diagnostics nh = row_count;
  return jsonb_build_object('rollup_5m_deleted', n5, 'rollup_hourly_deleted', nh);
end
$$;

-- Execute: service_role only.
revoke all on function public.ingest_rollup_batch(jsonb) from public;
revoke all on function public.delete_install(uuid) from public;
revoke all on all functions in schema telemetry from public;
do $$
begin
  if exists (select 1 from pg_roles where rolname = 'anon') then
    revoke all on function public.ingest_rollup_batch(jsonb) from anon, authenticated;
    revoke all on function public.delete_install(uuid) from anon, authenticated;
    revoke all on all functions in schema telemetry from anon, authenticated, service_role;
  end if;
  if exists (select 1 from pg_roles where rolname = 'service_role') then
    grant execute on function public.ingest_rollup_batch(jsonb) to service_role;
    grant execute on function public.delete_install(uuid) to service_role;
  end if;
end
$$;

commit;
