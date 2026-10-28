#!/usr/bin/env bash
set -uo pipefail

app_dir="$(cd "$(dirname "$0")/../apps/web" && pwd)"
port="${SMOKE_PORT:-8799}"
base="http://127.0.0.1:${port}"
routes=(/ /docs /docs/concepts/plan-apply-state /llms.txt /robots.txt /sitemap.xml)
log="$(mktemp)"
server_pid=""

cleanup() {
  if [ -n "$server_pid" ]; then
    kill -TERM -- "-$server_pid" 2>/dev/null
    sleep 2
    kill -KILL -- "-$server_pid" 2>/dev/null
  fi
  rm -f "$log"
}
trap cleanup EXIT

cd "$app_dir"

if [ "${SKIP_BUILD:-0}" != "1" ]; then
  pnpm exec opennextjs-cloudflare build || { echo "smoke: OpenNext build failed"; exit 1; }
fi

set -m
pnpm exec wrangler dev --local --ip 127.0.0.1 --port "$port" >"$log" 2>&1 &
server_pid=$!
set +m

ready=0
for _ in $(seq 1 60); do
  if ! kill -0 "$server_pid" 2>/dev/null; then
    echo "smoke: wrangler dev exited early"
    cat "$log"
    exit 1
  fi
  if curl -s -o /dev/null --max-time 5 "$base/robots.txt"; then
    ready=1
    break
  fi
  sleep 2
done
if [ "$ready" != 1 ]; then
  echo "smoke: worker not ready after 120s"
  cat "$log"
  exit 1
fi

fail=0
for route in "${routes[@]}"; do
  code="$(curl -s -o /dev/null --max-time 30 -w '%{http_code}' "$base$route")"
  if [ "$code" = "200" ]; then
    echo "ok   $code $route"
  else
    echo "FAIL $code $route"
    fail=1
  fi
done

if grep -E '\[ERROR\]|Error:|^[[:space:]]+at ' "$log" >/dev/null; then
  echo "smoke: worker log contains errors"
  cat "$log"
  fail=1
fi

[ "$fail" = 0 ] && echo "smoke: passed"
exit "$fail"
