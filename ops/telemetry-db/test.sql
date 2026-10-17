-- amicu telemetry database: behaviour test. NOT for production: it writes and
-- deletes rows. Run against a scratch database after 0001-0003, e.g.
--   ./test-local.sh        (starts a throwaway local Postgres, applies, runs this)
-- Uses the golden example from the sender
-- (crates/amicu-telemetry/schema/telemetry-batch.v2.example.json) with the
-- window moved to the current hour. Any failure raises and aborts.

\set ON_ERROR_STOP on
\set example `cat "$EXAMPLE_JSON"`

begin;

create temp table t_ex as select :'example'::jsonb as b;

create function pg_temp.at(b jsonb, start_s bigint, batch uuid) returns jsonb language sql as $$
  select jsonb_set(jsonb_set(b, '{reports,0,window,start_unix_s}', to_jsonb(start_s)),
                   '{batch_id}', to_jsonb(batch::text))
$$;

create function pg_temp.expect_400(b jsonb, what text) returns void language plpgsql as $$
begin
  perform public.ingest_rollup_batch(b);
  raise exception 'expected PT400 for %', what;
exception when sqlstate 'PT400' then null;
end
$$;

create function pg_temp.check(ok boolean, what text) returns void language plpgsql as $$
begin
  if not coalesce(ok, false) then raise exception 'FAILED: %', what; end if;
  raise notice 'ok: %', what;
end
$$;

do $$
declare
  ex jsonb := (select b from t_ex);
  h0 bigint := (extract(epoch from date_trunc('hour', now())) - 3600)::bigint;
  r jsonb;
  install uuid := '01010101-0101-4101-8101-010101010101';
begin
  -- The golden example as sent (window in 2025) is valid but past retention.
  r := public.ingest_rollup_batch(ex);
  perform pg_temp.check(r->>'skipped' = '1' and r->>'inserted' = '0', 'golden example accepted, stale window skipped');

  r := public.ingest_rollup_batch(pg_temp.at(ex, h0, gen_random_uuid()));
  perform pg_temp.check(r->>'inserted' = '1', 'fresh report inserted');

  r := public.ingest_rollup_batch(pg_temp.at(ex, h0 + 300, '0a0a0a0a-0a0a-4a0a-8a0a-0a0a0a0a0a0a'));
  perform pg_temp.check(r->>'inserted' = '1', 'second window inserted');
  r := public.ingest_rollup_batch(pg_temp.at(ex, h0 + 300, '0a0a0a0a-0a0a-4a0a-8a0a-0a0a0a0a0a0a'));
  perform pg_temp.check((r->>'duplicate')::boolean, 'retried batch_id is a no-op');
  r := public.ingest_rollup_batch(pg_temp.at(ex, h0 + 300, gen_random_uuid()));
  perform pg_temp.check(r->>'duplicates' = '1' and r->>'inserted' = '0', 're-packed report not double-counted');

  r := public.ingest_rollup_batch(pg_temp.at(ex, (extract(epoch from now()) + 7200)::bigint, gen_random_uuid()));
  perform pg_temp.check(r->>'skipped' = '1', 'future window skipped');

  perform pg_temp.expect_400(ex || '{"extra": 1}', 'unknown batch field');
  perform pg_temp.expect_400(jsonb_set(ex, '{reports,0,metrics,ip}', '"1.2.3.4"'), 'unknown metrics field');
  perform pg_temp.expect_400(jsonb_set(ex, '{reports,0,metrics,ttft_ms,counts}', '[1,2,3]'), 'histogram length');
  perform pg_temp.expect_400(jsonb_set(ex, '{reports,0,metrics,errors}', '500'), 'errors > requests');
  perform pg_temp.expect_400(jsonb_set(ex, '{reports,0,model,name}', '"has space"'), 'label charset');
  perform pg_temp.expect_400(jsonb_set(ex, '{schema_version}', '1'), 'schema version');
  perform pg_temp.expect_400(jsonb_set(ex, '{reports}', '[]'), 'empty reports');
  perform pg_temp.expect_400(jsonb_set(ex, '{batch_id}', '"not-a-uuid"'), 'batch_id');
  perform pg_temp.expect_400(jsonb_set(ex, '{reports,0,hardware,vram_bytes}', '-1'), 'negative bytes');
  perform pg_temp.expect_400(jsonb_set(ex, '{reports,0,metrics,gpu_utilization}', '1.5'), 'fraction > 1');

  -- Downsampling: two 5-minute rows of one hour -> one hourly row, exact sums.
  r := telemetry.run_maintenance();
  perform pg_temp.check((select reports = 2 and requests = 240 and ttft_counts[7] = 240
      and ttft_sum_ms = 36000 and window_s = 600 and output_tps_x_s = 60000
    from telemetry.rollup_hourly), 'hourly row is the exact sum of its 5-minute rows');
  perform pg_temp.check((select count(*) = 0 from telemetry.rollup_5m where not downsampled), 'all 5-minute rows claimed');
  perform pg_temp.check((select installs = 1 and reports = 2 and e2e_counts[8] = 240 from telemetry.rollup_daily),
    'daily row rebuilt, anonymous');

  -- A late report for the same hour is added once, on the next run.
  perform public.ingest_rollup_batch(pg_temp.at(ex, h0 + 600, gen_random_uuid()));
  perform telemetry.run_maintenance();
  perform telemetry.run_maintenance();
  perform pg_temp.check((select reports = 3 and requests = 360 from telemetry.rollup_hourly),
    'late report folded in exactly once');
  perform pg_temp.check((select reports = 3 from telemetry.rollup_daily), 'daily follows');

  -- Quantile helper on the daily histogram (all TTFT in (100, 250] ms).
  perform pg_temp.check((select telemetry.hist_quantile(ttft_counts,
      (select bounds_ms from telemetry.histogram_layouts where name = 'ttft_ms'), 0.5) between 100 and 250
    from telemetry.rollup_daily), 'hist_quantile');

  -- Deletion by install id.
  r := public.delete_install(install);
  perform pg_temp.check(r->>'rollup_5m_deleted' = '3' and r->>'rollup_hourly_deleted' = '1', 'delete_install removes 5m and hourly');
  perform telemetry.run_maintenance();
  perform pg_temp.check((select count(*) = 0 from telemetry.rollup_daily), 'recent daily rebuilt without the install');

  -- Retention drop.
  create table telemetry.rollup_5m_p20200101 partition of telemetry.rollup_5m
    for values from ('2020-01-01 00:00+00') to ('2020-01-02 00:00+00');
  perform pg_temp.check((telemetry.run_maintenance()->>'partitions_dropped')::int >= 1
    and to_regclass('telemetry.rollup_5m_p20200101') is null, 'expired partition dropped');
end
$$;

-- Role checks: service_role may call the RPC but not read the tables.
set local role service_role;
do $$
begin
  perform public.ingest_rollup_batch('{}'::jsonb);
exception
  when sqlstate 'PT400' then raise notice 'ok: service_role can execute ingest_rollup_batch';
  when insufficient_privilege then raise exception 'FAILED: service_role cannot execute the RPC';
end
$$;
do $$
begin
  perform 1 from telemetry.rollup_5m;
  raise exception 'FAILED: service_role can read rollup_5m';
exception when insufficient_privilege then raise notice 'ok: service_role has no table access';
end
$$;
reset role;

rollback;
