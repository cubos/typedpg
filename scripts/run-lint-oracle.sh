#!/usr/bin/env bash
#
# The lint oracle: every lint finding's fix and claim checked against a real
# PostgreSQL over adversarial rows (typedpg_lint/src/rules/oracle.rs), and
# every migration rule's claim and silence checked against the locks,
# rewrites and scans PostgreSQL makes replaying the migrations
# (typedpg_lint/src/migration_oracle.rs). Runs the typedpg_lint suite with
# the oracles on (each rule's own tests), then the corpus runs:
# typedpg_lint/tests/lint_oracle.rs (the docs/lint examples, the queries the
# analyzer's test suite analyzes, and generated queries; findings in
# target/lint-oracle-findings), typedpg_lint/tests/migration_oracle.rs
# (the docs/lint migrations, the analyzer suite's migrations, and generated
# ALTER TABLE & co.; findings in target/migration-oracle-findings) and
# typedpg_lint/tests/deployed_oracle.rs (deployed_query_breakage: generated
# migration changes x deployed query shapes; findings in
# target/deployed-oracle-findings).
# scripts/run-migration-oracle.sh runs the migration corpus alone.
#
# The analyzer's queries are recorded once into target/lint-oracle-corpus
# (its tests built with the `record_corpus` feature); LINT_ORACLE_RERECORD=1
# records them again.
#
# Usage:
#   scripts/run-lint-oracle.sh                     # everything
#   scripts/run-lint-oracle.sh lint_oracle_corpus  # the query corpus run only
#   scripts/run-lint-oracle.sh deployed_oracle_corpus  # deployed_query_breakage
#   LINT_ORACLE_ITERS=20000 LINT_ORACLE_SEED=7 scripts/run-lint-oracle.sh lint_oracle_corpus
#   LINT_ORACLE_ONLY=catalogs LINT_ORACLE_GEN_CASES=2000 LINT_ORACLE_SEED=7 \
#       scripts/run-lint-oracle.sh lint_oracle_corpus   # random catalogs alone
#   LINT_ORACLE_ONLY=catalogs LINT_ORACLE_GEN_CASE=42 LINT_ORACLE_SEED=7 \
#       scripts/run-lint-oracle.sh lint_oracle_corpus   # one case again
#
# Extra args are nextest filters / flags, as for scripts/run-pg-sanity.sh.

set -euo pipefail
cd "$(dirname "$0")/.."

corpus="${LINT_ORACLE_CORPUS:-target/lint-oracle-corpus}"
# A corpus recorded before the migrations were recorded too is stale.
if [[ ! -d "$corpus" || -n "${LINT_ORACLE_RERECORD:-}" ]] \
    || ! grep -qs '"migrations"' "$corpus"/*; then
    echo "lint oracle: recording the analyzer's queries into $corpus..."
    rm -rf "$corpus"
    mkdir -p "$corpus"
    TYPEDPG_RECORD_CORPUS="$(realpath "$corpus")" \
        cargo nextest run --release -p typedpg_analyzer --features record_corpus \
        --no-fail-fast >/dev/null || true
fi
LINT_ORACLE_CORPUS="$(realpath "$corpus")"
export LINT_ORACLE_CORPUS
# The lint crate's tests build catalogs the analyzer's own comparisons
# can't follow (clones sharing a mirror, roles shared by the cluster):
# only the schema is mirrored, and only the lint oracle compares.
export PG_SANITY_MIRROR_ONLY=1
export PG_SANITY_PACKAGE=typedpg_lint
exec scripts/run-pg-sanity.sh --run-ignored all --no-fail-fast "$@"
