-- amicu telemetry database: post-migration checks. Read-only (the role probes
-- run inside a transaction that is rolled back). Every row should say ok = true.

begin;

create temp table verify_result (check_name text, ok boolean) on commit drop;

insert into verify_result
select 'rls enabled on every telemetry table and partition', bool_and(c.relrowsecurity)
from pg_class c join pg_namespace n on n.oid = c.relnamespace
where n.nspname = 'telemetry' and c.relkind in ('r', 'p');

insert into verify_result
select 'no rls policies in telemetry (owner-only access)', count(*) = 0
from pg_policies where schemaname = 'telemetry';

insert into verify_result
select format('%s has no access to schema telemetry', r), not has_schema_privilege(r, 'telemetry', 'usage')
from unnest(array['anon', 'authenticated', 'service_role']) r
where exists (select 1 from pg_roles where rolname = r);

insert into verify_result
select format('%s has no privilege on any telemetry table', r),
  not exists (
    select 1 from pg_class c join pg_namespace n on n.oid = c.relnamespace
    where n.nspname = 'telemetry' and c.relkind in ('r', 'p', 'v')
      and (has_table_privilege(r, c.oid, 'select') or has_table_privilege(r, c.oid, 'insert')
        or has_table_privilege(r, c.oid, 'update') or has_table_privilege(r, c.oid, 'delete')))
from unnest(array['anon', 'authenticated', 'service_role']) r
where exists (select 1 from pg_roles where rolname = r);

insert into verify_result
select format('%s cannot execute %s', r, f),
  not has_function_privilege(r, f, 'execute')
from unnest(array['anon', 'authenticated']) r,
     unnest(array['public.ingest_rollup_batch(jsonb)', 'public.delete_install(uuid)']) f
where exists (select 1 from pg_roles where rolname = r);

insert into verify_result
select format('service_role can execute %s', f), has_function_privilege('service_role', f, 'execute')
from unnest(array['public.ingest_rollup_batch(jsonb)', 'public.delete_install(uuid)']) f
where exists (select 1 from pg_roles where rolname = 'service_role');

insert into verify_result
select format('%s is security definer with empty search_path', p.oid::regprocedure),
  p.prosecdef and coalesce(p.proconfig @> array['search_path=""'], false)
from pg_proc p join pg_namespace n on n.oid = p.pronamespace
where n.nspname = 'public' and p.proname in ('ingest_rollup_batch', 'delete_install');

insert into verify_result
select 'partitions exist for today in rollup_5m and rollup_hourly', count(*) = 2
from pg_class
where relname in ('rollup_5m_p' || to_char(now() at time zone 'utc', 'YYYYMMDD'),
                  'rollup_hourly_p' || to_char(now() at time zone 'utc', 'YYYYMMDD'));

do $$
declare
  scheduled boolean := false;
begin
  if to_regclass('cron.job') is not null then
    execute $q$select exists (select 1 from cron.job where jobname = 'amicu-telemetry-maintenance')$q$
      into scheduled;
  end if;
  insert into verify_result
    values ('pg_cron maintenance job scheduled (false is expected without pg_cron)', scheduled);
end
$$;

-- Live probe: as anon, every path into the data must fail.
do $$
declare
  leaks text[] := '{}';
begin
  if not exists (select 1 from pg_roles where rolname = 'anon') then return; end if;
  set local role anon;
  begin
    perform public.ingest_rollup_batch('{}'::jsonb);
    leaks := leaks || 'ingest_rollup_batch';
  exception when insufficient_privilege then null;
  end;
  begin
    perform public.delete_install(gen_random_uuid());
    leaks := leaks || 'delete_install';
  exception when insufficient_privilege then null;
  end;
  begin
    perform 1 from telemetry.rollup_daily limit 1;
    leaks := leaks || 'select rollup_daily';
  exception when insufficient_privilege then null;
  end;
  begin
    insert into telemetry.ingested_batches (batch_id, reports) values (gen_random_uuid(), 1);
    leaks := leaks || 'insert ingested_batches';
  exception when insufficient_privilege then null;
  end;
  reset role;
  insert into verify_result values ('anon probe: no call or table access succeeds'
    || case when cardinality(leaks) > 0 then ' (leaked: ' || array_to_string(leaks, ', ') || ')' else '' end,
    cardinality(leaks) = 0);
end
$$;

select * from verify_result order by ok, check_name;
select * from telemetry.db_size;

rollback;
