#!/usr/bin/env bash
#
# Run the client-library integration tests against a live cluster.
#
# The Rust tests (cargo test) start a proxy in-process. These ones need a real
# one on a port, because they are separate programs, so this script starts the
# binary, waits for it, runs each suite, and shuts it down.
#
# Usage:
#   export CLUSTER_ARN=... SECRET_ARN=... DATABASE=postgres
#   ./scripts/test-clients.sh [node|python|go|psql]...
#
# With no arguments it runs every suite whose runtime is installed.

set -uo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(cd "$here/.." && pwd)

: "${CLUSTER_ARN:?set CLUSTER_ARN}"
: "${SECRET_ARN:?set SECRET_ARN}"
: "${DATABASE:?set DATABASE}"
PORT="${PORT:-55433}"
PSQL="${PSQL:-psql}"

suites=("$@")
if [ ${#suites[@]} -eq 0 ]; then
  suites=(psql node python go)
fi

echo "building..."
(cd "$root" && cargo build --quiet) || exit 1

log=$(mktemp)
LISTEN="127.0.0.1:$PORT" "$root/target/debug/aurora-data-api-proxy" >"$log" 2>&1 &
proxy=$!
trap 'kill "$proxy" 2>/dev/null; rm -f "$log"' EXIT

echo "starting the proxy on port $PORT (a cluster scaled to zero may take a while)..."
for _ in $(seq 1 120); do
  grep -q 'listening on' "$log" && break
  if ! kill -0 "$proxy" 2>/dev/null; then
    echo "the proxy exited:"; cat "$log"; exit 1
  fi
  sleep 1
done
if ! grep -q 'listening on' "$log"; then
  echo "the proxy did not start:"; cat "$log"; exit 1
fi

failed=0
run() {
  echo
  echo "=== $1 ==="
  shift
  if "$@"; then echo "--- passed"; else echo "--- FAILED"; failed=1; fi
}

for suite in "${suites[@]}"; do
  case "$suite" in
    psql)
      if command -v "$PSQL" >/dev/null; then
        run "psql" "$PSQL" "host=127.0.0.1 port=$PORT user=test dbname=$DATABASE" \
          -v ON_ERROR_STOP=1 -f "$root/tests/clients/psql/smoke.sql"
      else
        echo "skipping psql: not installed"
      fi
      ;;
    node)
      if command -v node >/dev/null; then
        (cd "$root/tests/clients/node" && npm install --silent --no-fund --no-audit)
        run "node-postgres" env PGPORT="$PORT" DATABASE="$DATABASE" \
          node "$root/tests/clients/node/test.mjs"
      else
        echo "skipping node: not installed"
      fi
      ;;
    python)
      py="${PYTHON:-python3}"
      if "$py" -c 'import psycopg' 2>/dev/null; then
        run "psycopg" env PGPORT="$PORT" DATABASE="$DATABASE" \
          "$py" "$root/tests/clients/python/test_psycopg.py"
      else
        echo "skipping python: psycopg is not installed (pip install 'psycopg[binary]')"
      fi
      ;;
    go)
      if command -v go >/dev/null; then
        run "pgx" env PGPORT="$PORT" DATABASE="$DATABASE" \
          sh -c "cd '$root/tests/clients/go' && go run ."
      else
        echo "skipping go: not installed"
      fi
      ;;
    *)
      echo "unknown suite: $suite"; failed=1 ;;
  esac
done

echo
if [ "$failed" -eq 0 ]; then
  echo "all client suites passed"
else
  echo "some client suites failed"
fi
exit "$failed"
