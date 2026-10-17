-- amicu telemetry database, migration 0004: schedule maintenance with pg_cron.
-- Safe to re-run (cron.schedule with a job name replaces that job).
--
-- pg_cron ships with Supabase (Free included) but is not enabled by default.
-- Where it is unavailable this file only prints a notice; schedule
--   select telemetry.run_maintenance();
-- hourly by other means instead (README.md, "Without pg_cron").

do $$
begin
  if not exists (select 1 from pg_available_extensions where name = 'pg_cron') then
    raise notice 'pg_cron is not available: schedule telemetry.run_maintenance() hourly yourself';
    return;
  end if;

  create extension if not exists pg_cron with schema pg_catalog;
  grant usage on schema cron to postgres;
  grant all privileges on all tables in schema cron to postgres;

  -- Minute 7 of every hour: off the hour, after the 5-minute windows of the
  -- previous hour have had time to arrive.
  perform cron.schedule('amicu-telemetry-maintenance', '7 * * * *',
    'select telemetry.run_maintenance()');
end
$$;
