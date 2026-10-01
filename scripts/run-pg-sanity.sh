#!/usr/bin/env bash
#
# Spin up a real PostgreSQL Docker container, export POSTGRES_URL pointing
# at it, run `cargo nextest run --features pg_sanity` (passing through any
# extra args), print the nullability oracle's counters, then tear the
# container down — even on failure.
#
# Each `PgCatalog::new()` in the analyzer's tests will create its own
# scratch database inside the cluster (and DROP it on Drop). All tests
# share one PG cluster, so initdb / pg_ctl runs once total.
#
# Usage:
#   scripts/run-pg-sanity.sh                 # run the whole analyzer suite
#   scripts/run-pg-sanity.sh alter_column    # filter by test name
#   scripts/run-pg-sanity.sh --run-ignored all -E 'test(fuzz_analyze_against_pg)'
#
# Extra args are appended to the default
# `--release --features pg_sanity -p typedpg_analyzer` invocation — callers
# only pass filters / extra nextest flags, never the boilerplate.

set -euo pipefail

PG_IMAGE="${PG_IMAGE:-postgres:18}"
CONTAINER_NAME="typedpg-pg-sanity-$$"
PG_USER="postgres"
PG_PASS="postgres"
PG_DB="postgres"

cleanup() {
    # Stop wins over rm so we exit fast on Ctrl-C; --rm on the container
    # handles the actual deletion. Errors during cleanup are swallowed —
    # the container may already be gone if the test process exited.
    docker rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

echo "pg_sanity: starting $PG_IMAGE as $CONTAINER_NAME..."
docker run -d --rm \
    --name "$CONTAINER_NAME" \
    -e POSTGRES_USER="$PG_USER" \
    -e POSTGRES_PASSWORD="$PG_PASS" \
    -e POSTGRES_DB="$PG_DB" \
    -p 127.0.0.1:0:5432 \
    "$PG_IMAGE" >/dev/null

# Discover the host port the daemon picked. `docker port` reports
# `0.0.0.0:NNNNN` (and on macOS often a v6 line too); take the first
# numeric tail.
PG_PORT=$(docker port "$CONTAINER_NAME" 5432 | head -n 1 | awk -F: '{print $NF}')
if [[ -z "$PG_PORT" ]]; then
    echo "pg_sanity: could not determine host port for $CONTAINER_NAME" >&2
    exit 1
fi

# Wait until the cluster accepts real TCP queries. `pg_isready` over the
# Unix socket is NOT enough: the postgres image's entrypoint starts a
# temporary socket-only server for initdb scripts, stops it, and only then
# launches the final TCP-listening one — a readiness probe that races into
# that window passes and the test run then hits a connection refused. A
# `SELECT 1` over TCP can only succeed against the final server; require a
# couple of consecutive successes for good measure.
echo "pg_sanity: waiting for PG to accept TCP queries on 127.0.0.1:$PG_PORT..."
ready=0
for _ in $(seq 1 120); do
    if docker exec "$CONTAINER_NAME" psql -h 127.0.0.1 -U "$PG_USER" -d "$PG_DB" -c "SELECT 1" >/dev/null 2>&1 \
        && docker exec "$CONTAINER_NAME" psql -h 127.0.0.1 -U "$PG_USER" -d "$PG_DB" -c "SELECT 1" >/dev/null 2>&1; then
        ready=1
        break
    fi
    sleep 0.5
done
if [[ "$ready" -ne 1 ]]; then
    echo "pg_sanity: PG never became ready; container logs:" >&2
    docker logs "$CONTAINER_NAME" >&2 || true
    exit 1
fi

export POSTGRES_URL="host=127.0.0.1 port=$PG_PORT user=$PG_USER password=$PG_PASS dbname=$PG_DB"
echo "pg_sanity: POSTGRES_URL=$POSTGRES_URL"
echo "pg_sanity: running tests..."

# The nullability soundness oracle (typedpg_analyzer/src/pg_sanity/
# soundness.rs) appends each catalog's counters here; summed up below.
TYPEDPG_SOUNDNESS_STATS="$(mktemp -t typedpg-soundness.XXXXXX)"
export TYPEDPG_SOUNDNESS_STATS

# Defaults always apply; extra args (filters, --run-ignored, -E …) are
# appended. Repeating `-p`/`--features` from the command line is harmless —
# cargo merges duplicates.
status=0
cargo nextest run --release --features pg_sanity -p typedpg_analyzer "$@" || status=$?

echo "pg_sanity: nullability oracle —"
awk -F'\t' '
    $1 == "count" { n[$2] += $3 }
    $1 == "skip"  { skips[$2] = 1 }
    $1 == "error" { errors[$2] += $3 }
    END {
        printf "  queries executed over adversarial data: %d (%d scenario runs, %d failed at runtime)\n",
            n["checked"], n["executions"], n["exec_errors"]
        for (c in errors) printf "    SQLSTATE %s: %d\n", c, errors[c]
        printf "  not executed: %d with no NOT NULL promise, %d not SELECT/DML, %d calling unsafe functions, %d lacking a parameter sample\n",
            n["no_promise"], n["not_executable"], n["unsafe_calls"], n["no_param_sample"]
        k = 0
        for (s in skips) k++
        printf "  tables the seeder could not fill: %d distinct\n", k
        for (s in skips) printf "    %s\n", s
    }' "$TYPEDPG_SOUNDNESS_STATS"
rm -f "$TYPEDPG_SOUNDNESS_STATS"
exit "$status"
