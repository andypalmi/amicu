#!/usr/bin/env bash
# Apply 0001-0004 to a throwaway local Postgres (15+), twice (they must be
# re-runnable), then run test.sql and verify.sql. Needs initdb, pg_ctl and psql
# on PATH. Creates stand-ins for the Supabase roles (anon, authenticated,
# service_role) so the grants are exercised. Never point this at Supabase.
#
#   ./test-local.sh
#   EXAMPLE_JSON=path/to/telemetry-batch.v2.example.json ./test-local.sh
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
: "${EXAMPLE_JSON:=$here/../../crates/amicu-telemetry/schema/telemetry-batch.v2.example.json}"
export EXAMPLE_JSON
tmp="$(mktemp -d)"
port="${PGPORT_TEST:-55432}"
cleanup() {
  pg_ctl -D "$tmp/data" -m immediate stop >/dev/null 2>&1 || true
  rm -rf "$tmp"
}
trap cleanup EXIT
initdb -D "$tmp/data" -U postgres --auth=trust >/dev/null
pg_ctl -D "$tmp/data" -o "-p $port -k $tmp -c listen_addresses=''" -l "$tmp/log" -w start >/dev/null
run() { psql -X -q -v ON_ERROR_STOP=1 -h "$tmp" -p "$port" -U postgres -d postgres "$@"; }
run -c "create role anon nologin; create role authenticated nologin;
        create role service_role nologin bypassrls;
        grant anon, authenticated, service_role to postgres;
        grant usage on schema public to anon, authenticated, service_role;"
for pass in 1 2; do
  for f in 0001_schema.sql 0002_maintenance.sql 0003_ingest.sql 0004_schedule.sql; do
    run -f "$here/$f" >/dev/null
  done
done
run -f "$here/test.sql"
run -f "$here/verify.sql"
