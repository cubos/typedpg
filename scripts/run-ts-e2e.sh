#!/usr/bin/env bash
#
# End-to-end check of typedpg for TypeScript (typedpg_ts + typedpg_npm),
# on the project in typedpg_ts_example:
#
#   1. the runtime builds and its unit tests pass;
#   2. the example's generated module is up to date (`typedpg check`);
#   3. the errors fixture reports exactly errors/expected.stderr;
#   4. tsc (7 and 5.9) accepts the example: its type-level assertions, and
#      every `@ts-expect-error` of the errors fixture reporting; and rejects
#      the types errors/mapping maps PG types to that don't fit them;
#   5. the queries, COPY, streams and migrations run on a real PostgreSQL
#      through node-postgres and postgres.js, and the migrations interoperate
#      with `typedpg migrate` (a Docker container, torn down even on
#      failure).
#
# Usage: scripts/run-ts-e2e.sh            (BLESS=1 rewrites the expected outputs)

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EXAMPLE="$ROOT/typedpg_ts_example"
PG_IMAGE="${PG_IMAGE:-postgres:18}"
CONTAINER_NAME="typedpg-ts-e2e-$$"

cleanup() {
    docker rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

cargo build -q --release -p typedpg_ts --bin typedpg
TYPEDPG="$ROOT/target/release/typedpg"

echo "ts-e2e: runtime"
(cd "$ROOT/typedpg_npm" && npm ci --silent && npm test --silent)
# The example installs the runtime as a copy (`install-links`), as from the
# registry: refresh it.
(cd "$EXAMPLE" && npm ci --silent)

echo "ts-e2e: typedpg check"
(cd "$EXAMPLE" && "$TYPEDPG" check)

echo "ts-e2e: errors fixture"
# The summary line carries a timing: compare the diagnostics only.
actual=$(cd "$EXAMPLE/errors" && "$TYPEDPG" gen 2>&1 | grep -v '^\[typedpg\]' || true)
if [[ "${BLESS:-}" == 1 ]]; then
    printf '%s\n' "$actual" > "$EXAMPLE/errors/expected.stderr"
fi
diff -u "$EXAMPLE/errors/expected.stderr" <(printf '%s\n' "$actual")

echo "ts-e2e: tsc 7 and 5.9"
(cd "$EXAMPLE" && npm run --silent typecheck)

echo "ts-e2e: type mapping checks"
# The types `types` maps to that don't fit what their PG types are read as
# fail tsc in the generated module: tsc 7's report is pinned, tsc 5.9 (which
# words one error differently) must fail at the same places.
MAPPING="$EXAMPLE/errors/mapping"
actual=$(cd "$MAPPING" && "$TYPEDPG" gen 2>&1 | grep -v '^\[typedpg\]' || true)
tsc7=$(cd "$MAPPING" && node ../../node_modules/typescript/bin/tsc -p . || true)
tsc5=$(cd "$MAPPING" && node ../../node_modules/typescript-5/bin/tsc -p . || true)
if [[ "${BLESS:-}" == 1 ]]; then
    printf '%s\n' "$actual" > "$MAPPING/expected.stderr"
    printf '%s\n' "$tsc7" > "$MAPPING/expected.tsc"
fi
diff -u "$MAPPING/expected.stderr" <(printf '%s\n' "$actual")
diff -u "$MAPPING/expected.tsc" <(printf '%s\n' "$tsc7")
locations() { grep -o '^src/[^:]*' || true; }
diff -u <(printf '%s\n' "$tsc7" | locations) <(printf '%s\n' "$tsc5" | locations)

echo "ts-e2e: starting $PG_IMAGE as $CONTAINER_NAME..."
docker run -d --rm --name "$CONTAINER_NAME" -e POSTGRES_PASSWORD=postgres \
    -p 127.0.0.1:0:5432 "$PG_IMAGE" >/dev/null
PG_PORT=$(docker port "$CONTAINER_NAME" 5432 | head -n 1 | awk -F: '{print $NF}')
# A `SELECT 1` over TCP, twice: see scripts/run-pg-sanity.sh for why
# `pg_isready` is not enough.
ready=0
for _ in $(seq 1 120); do
    if docker exec "$CONTAINER_NAME" psql -h 127.0.0.1 -U postgres -c "SELECT 1" >/dev/null 2>&1 \
        && docker exec "$CONTAINER_NAME" psql -h 127.0.0.1 -U postgres -c "SELECT 1" >/dev/null 2>&1; then
        ready=1
        break
    fi
    sleep 0.5
done
if [[ "$ready" -ne 1 ]]; then
    docker logs "$CONTAINER_NAME" >&2 || true
    exit 1
fi

echo "ts-e2e: running on PostgreSQL"
(cd "$EXAMPLE" && DATABASE_URL="postgres://postgres:postgres@127.0.0.1:$PG_PORT/postgres" \
    TYPEDPG_BIN="$TYPEDPG" npm test --silent)
