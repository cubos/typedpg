#!/usr/bin/env bash
#
# Build `typedpg-extensions:pg19`, a PostgreSQL 19 server with every
# third-party extension typedpg_analyzer bundles (PostGIS, pgvector, pg_cron,
# pgmq, pg_jsonschema; see scripts/extensions-image/Dockerfile), so the
# pg_sanity oracle can compare their installation and the queries using them
# against a real server:
#
#   scripts/build-extensions-image.sh
#   PG_IMAGE=typedpg-extensions:pg19 scripts/run-pg-sanity.sh third_party
#
# On the default `postgres` image the mirror stops comparing a test once the
# server reports one of these extensions as not available.
#
# The first build compiles pg_jsonschema (and cargo-pgrx) and takes several
# minutes; Docker's cache makes later ones instant.

set -euo pipefail

cd "$(dirname "$0")/extensions-image"
TAG="${TAG:-typedpg-extensions:pg19}"
docker build -t "$TAG" .
echo "built $TAG"
