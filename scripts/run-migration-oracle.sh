#!/usr/bin/env bash
#
# The migration oracle's corpus run (typedpg_lint/tests/migration_oracle.rs):
# the docs/lint migrations, the migrations of the analyzer's test suite (as
# recorded, and one statement per file) and generated ALTER TABLE & co.,
# each replayed on a real PostgreSQL that the migration rules' claims and
# silence are checked against (typedpg_lint/src/migration_oracle.rs).
# Findings land in target/migration-oracle-findings.
#
# Usage:
#   scripts/run-migration-oracle.sh
#   MIGRATION_ORACLE_FILTER='ATTACH PARTITION' scripts/run-migration-oracle.sh
#   MIGRATION_ORACLE_LIMIT=200 scripts/run-migration-oracle.sh   # fewer recorded sets
#
# The analyzer's migrations are recorded as for scripts/run-lint-oracle.sh
# (LINT_ORACLE_RERECORD=1 records them again).

set -euo pipefail
cd "$(dirname "$0")/.."
exec scripts/run-lint-oracle.sh migration_oracle_corpus "$@"
