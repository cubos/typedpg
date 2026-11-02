#!/usr/bin/env bash
#
# Downloads extension SQL scripts from the PostgreSQL repository.
#
# These SQL files define the types, functions, operators, and casts that each
# extension provides. They are embedded in typedpg_analyzer via include_str!
# and processed by the DDL interpreter when a migration contains CREATE EXTENSION.
#
# Each extension is defined as a base install script + optional upgrade chain.
# For example, citext ships as:
#   citext--1.4.sql (base)
#   citext--1.4--1.5.sql (upgrade)
#   citext--1.5--1.6.sql (upgrade)
#   ...
#
# The scripts must come from the PostgreSQL release typedpg_pg_query's
# grammar (and the seed) comes from: a newer branch ships extension versions
# whose scripts use grammar that release's parser rejects (postgres_fdw 1.3's
# `ALTER FOREIGN DATA WRAPPER ... CONNECTION` is PostgreSQL 19), and objects
# its server doesn't have. The file lists below follow that release's
# contrib/ too.
#
# Usage:
#   ./update_extensions.sh              # the release libpg_query pins (default)
#   ./update_extensions.sh REL_18_6     # use a specific tag/branch
#
# Output: typedpg_analyzer/src/extensions/*.sql. Every file is downloaded
# into a temporary directory first; the bundled files are replaced only once
# all downloads succeeded, so a failure leaves them untouched.
#
# After running:
# 1. Review the downloaded files for pg_query compatibility
#    (check for reserved-word parameter names)
# 2. Update the REGISTRY array in
#    typedpg_analyzer/src/ddl/extensions/registry.rs to include_str! the new
#    files in order, with each control file's default_version
# 3. Run: cargo nextest run --release -p typedpg_analyzer

set -euo pipefail

cd "$(dirname "$0")"

if [[ $# -ge 1 ]]; then
    BRANCH="$1"
else
    # `#define PG_VERSION "18.6"` -> REL_18_6, the tag format of every
    # release since PostgreSQL 10 (REL_<major>_<minor>).
    PG_VERSION=$(tr -d '\r' < typedpg_pg_query/libpg_query/pg_query.h \
        | sed -n 's/^[[:space:]]*#[[:space:]]*define[[:space:]]\{1,\}PG_VERSION[[:space:]]\{1,\}"\([^"]*\)"[[:space:]]*$/\1/p')
    if [[ ! "$PG_VERSION" =~ ^([0-9]+)\.([0-9]+)$ ]]; then
        echo "error: cannot derive a release tag from PG_VERSION \"${PG_VERSION}\" in" \
            "typedpg_pg_query/libpg_query/pg_query.h; pass the tag explicitly" >&2
        exit 1
    fi
    BRANCH="REL_${BASH_REMATCH[1]}_${BASH_REMATCH[2]}"
fi
BASE_URL="https://raw.githubusercontent.com/postgres/postgres/${BRANCH}"
OUT_DIR="typedpg_analyzer/src/extensions"
TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

# Download `url` into the temporary directory as `outfile`, without the
# psql-only \echo and \quit lines. Any failure aborts the script.
fetch() {
    local url="$1"
    local outfile="$2"

    echo "  ${outfile}..."
    if ! curl -sfL "$url" -o "${TMP_DIR}/raw.sql"; then
        echo "error: failed to download ${url}" >&2
        exit 1
    fi
    grep -v '^\\\(echo\|quit\)' "${TMP_DIR}/raw.sql" > "${TMP_DIR}/${outfile}" || true
    rm -f "${TMP_DIR}/raw.sql"
}

# Helper: download files for an extension from contrib/<subdir>/
download_ext() {
    local subdir="$1"
    shift
    for file in "$@"; do
        fetch "${BASE_URL}/contrib/${subdir}/${file}" "$file"
    done
}

echo "==> Downloading extension SQL files from postgres/${BRANCH}..."
echo "    Target: ${OUT_DIR}/"

# ═══════════════════════════════════════════════════════════════════════════
# Extensions (alphabetical order)
# ═══════════════════════════════════════════════════════════════════════════

echo -e "\n--- amcheck (1.0 → 1.5) ---"
download_ext "amcheck" \
    "amcheck--1.0.sql" \
    "amcheck--1.0--1.1.sql" \
    "amcheck--1.1--1.2.sql" \
    "amcheck--1.2--1.3.sql" \
    "amcheck--1.3--1.4.sql" \
    "amcheck--1.4--1.5.sql"

echo -e "\n--- autoinc (1.0) ---"
download_ext "spi" \
    "autoinc--1.0.sql"

echo -e "\n--- bloom (1.0) ---"
download_ext "bloom" \
    "bloom--1.0.sql"

echo -e "\n--- bool_plperl (1.0) ---"
download_ext "bool_plperl" \
    "bool_plperl--1.0.sql"

echo -e "\n--- bool_plperlu (1.0) ---"
download_ext "bool_plperl" \
    "bool_plperlu--1.0.sql"

echo -e "\n--- btree_gin (1.0 → 1.3) ---"
download_ext "btree_gin" \
    "btree_gin--1.0.sql" \
    "btree_gin--1.0--1.1.sql" \
    "btree_gin--1.1--1.2.sql" \
    "btree_gin--1.2--1.3.sql"

echo -e "\n--- btree_gist (1.2 → 1.8) ---"
download_ext "btree_gist" \
    "btree_gist--1.2.sql" \
    "btree_gist--1.2--1.3.sql" \
    "btree_gist--1.3--1.4.sql" \
    "btree_gist--1.4--1.5.sql" \
    "btree_gist--1.5--1.6.sql" \
    "btree_gist--1.6--1.7.sql" \
    "btree_gist--1.7--1.8.sql"

echo -e "\n--- citext (1.4 → 1.8) ---"
download_ext "citext" \
    "citext--1.4.sql" \
    "citext--1.4--1.5.sql" \
    "citext--1.5--1.6.sql" \
    "citext--1.6--1.7.sql" \
    "citext--1.7--1.8.sql"

echo -e "\n--- cube (1.2 → 1.5) ---"
download_ext "cube" \
    "cube--1.2.sql" \
    "cube--1.2--1.3.sql" \
    "cube--1.3--1.4.sql" \
    "cube--1.4--1.5.sql"

echo -e "\n--- dblink (1.2) ---"
download_ext "dblink" \
    "dblink--1.2.sql"

echo -e "\n--- dict_int (1.0) ---"
download_ext "dict_int" \
    "dict_int--1.0.sql"

echo -e "\n--- dict_xsyn (1.0) ---"
download_ext "dict_xsyn" \
    "dict_xsyn--1.0.sql"

echo -e "\n--- earthdistance (1.1 → 1.2) ---"
download_ext "earthdistance" \
    "earthdistance--1.1.sql" \
    "earthdistance--1.1--1.2.sql"

echo -e "\n--- file_fdw (1.0) ---"
download_ext "file_fdw" \
    "file_fdw--1.0.sql"

echo -e "\n--- fuzzystrmatch (1.1 → 1.2) ---"
download_ext "fuzzystrmatch" \
    "fuzzystrmatch--1.1.sql" \
    "fuzzystrmatch--1.1--1.2.sql"

echo -e "\n--- hstore (1.4 → 1.8) ---"
download_ext "hstore" \
    "hstore--1.4.sql" \
    "hstore--1.4--1.5.sql" \
    "hstore--1.5--1.6.sql" \
    "hstore--1.6--1.7.sql" \
    "hstore--1.7--1.8.sql"

echo -e "\n--- hstore_plperl (1.0) ---"
download_ext "hstore_plperl" \
    "hstore_plperl--1.0.sql"

echo -e "\n--- hstore_plperlu (1.0) ---"
download_ext "hstore_plperl" \
    "hstore_plperlu--1.0.sql"

echo -e "\n--- hstore_plpython3u (1.0) ---"
download_ext "hstore_plpython" \
    "hstore_plpython3u--1.0.sql"

echo -e "\n--- intagg (1.1) ---"
download_ext "intagg" \
    "intagg--1.1.sql"

echo -e "\n--- intarray (1.2 → 1.5) ---"
download_ext "intarray" \
    "intarray--1.2.sql" \
    "intarray--1.2--1.3.sql" \
    "intarray--1.3--1.4.sql" \
    "intarray--1.4--1.5.sql"

echo -e "\n--- insert_username (1.0) ---"
download_ext "spi" \
    "insert_username--1.0.sql"

echo -e "\n--- isn (1.1 → 1.3) ---"
download_ext "isn" \
    "isn--1.1.sql" \
    "isn--1.1--1.2.sql" \
    "isn--1.2--1.3.sql"

echo -e "\n--- jsonb_plperl (1.0) ---"
download_ext "jsonb_plperl" \
    "jsonb_plperl--1.0.sql"

echo -e "\n--- jsonb_plperlu (1.0) ---"
download_ext "jsonb_plperl" \
    "jsonb_plperlu--1.0.sql"

echo -e "\n--- jsonb_plpython3u (1.0) ---"
download_ext "jsonb_plpython" \
    "jsonb_plpython3u--1.0.sql"

echo -e "\n--- lo (1.1 → 1.2) ---"
download_ext "lo" \
    "lo--1.1.sql" \
    "lo--1.1--1.2.sql"

echo -e "\n--- ltree (1.1 → 1.3) ---"
download_ext "ltree" \
    "ltree--1.1.sql" \
    "ltree--1.1--1.2.sql" \
    "ltree--1.2--1.3.sql"

echo -e "\n--- ltree_plpython3u (1.0) ---"
download_ext "ltree_plpython" \
    "ltree_plpython3u--1.0.sql"

echo -e "\n--- moddatetime (1.0) ---"
download_ext "spi" \
    "moddatetime--1.0.sql"

echo -e "\n--- pageinspect (1.5 → 1.13) ---"
download_ext "pageinspect" \
    "pageinspect--1.5.sql" \
    "pageinspect--1.5--1.6.sql" \
    "pageinspect--1.6--1.7.sql" \
    "pageinspect--1.7--1.8.sql" \
    "pageinspect--1.8--1.9.sql" \
    "pageinspect--1.9--1.10.sql" \
    "pageinspect--1.10--1.11.sql" \
    "pageinspect--1.11--1.12.sql" \
    "pageinspect--1.12--1.13.sql"

echo -e "\n--- pg_buffercache (1.2 → 1.6) ---"
download_ext "pg_buffercache" \
    "pg_buffercache--1.2.sql" \
    "pg_buffercache--1.2--1.3.sql" \
    "pg_buffercache--1.3--1.4.sql" \
    "pg_buffercache--1.4--1.5.sql" \
    "pg_buffercache--1.5--1.6.sql"

echo -e "\n--- pg_freespacemap (1.1 → 1.3) ---"
download_ext "pg_freespacemap" \
    "pg_freespacemap--1.1.sql" \
    "pg_freespacemap--1.1--1.2.sql" \
    "pg_freespacemap--1.2--1.3.sql"

echo -e "\n--- pg_logicalinspect (1.0) ---"
download_ext "pg_logicalinspect" \
    "pg_logicalinspect--1.0.sql"

echo -e "\n--- pg_prewarm (1.1 → 1.2) ---"
download_ext "pg_prewarm" \
    "pg_prewarm--1.1.sql" \
    "pg_prewarm--1.1--1.2.sql"

echo -e "\n--- pg_stat_statements (1.4 → 1.12) ---"
download_ext "pg_stat_statements" \
    "pg_stat_statements--1.4.sql" \
    "pg_stat_statements--1.4--1.5.sql" \
    "pg_stat_statements--1.5--1.6.sql" \
    "pg_stat_statements--1.6--1.7.sql" \
    "pg_stat_statements--1.7--1.8.sql" \
    "pg_stat_statements--1.8--1.9.sql" \
    "pg_stat_statements--1.9--1.10.sql" \
    "pg_stat_statements--1.10--1.11.sql" \
    "pg_stat_statements--1.11--1.12.sql"

echo -e "\n--- pg_surgery (1.0) ---"
download_ext "pg_surgery" \
    "pg_surgery--1.0.sql"

echo -e "\n--- pg_trgm (1.3 → 1.6) ---"
download_ext "pg_trgm" \
    "pg_trgm--1.3.sql" \
    "pg_trgm--1.3--1.4.sql" \
    "pg_trgm--1.4--1.5.sql" \
    "pg_trgm--1.5--1.6.sql"

echo -e "\n--- pg_visibility (1.1 → 1.2) ---"
download_ext "pg_visibility" \
    "pg_visibility--1.1.sql" \
    "pg_visibility--1.1--1.2.sql"

echo -e "\n--- pg_walinspect (1.0 → 1.1) ---"
download_ext "pg_walinspect" \
    "pg_walinspect--1.0.sql" \
    "pg_walinspect--1.0--1.1.sql"

echo -e "\n--- pgcrypto (1.3 → 1.4) ---"
download_ext "pgcrypto" \
    "pgcrypto--1.3.sql" \
    "pgcrypto--1.3--1.4.sql"

echo -e "\n--- pgrowlocks (1.2) ---"
download_ext "pgrowlocks" \
    "pgrowlocks--1.2.sql"

echo -e "\n--- pgstattuple (1.4 → 1.5) ---"
download_ext "pgstattuple" \
    "pgstattuple--1.4.sql" \
    "pgstattuple--1.4--1.5.sql"

echo -e "\n--- postgres_fdw (1.0 → 1.2) ---"
download_ext "postgres_fdw" \
    "postgres_fdw--1.0.sql" \
    "postgres_fdw--1.0--1.1.sql" \
    "postgres_fdw--1.1--1.2.sql"

echo -e "\n--- refint (1.0) ---"
download_ext "spi" \
    "refint--1.0.sql"

echo -e "\n--- seg (1.1 → 1.4) ---"
download_ext "seg" \
    "seg--1.1.sql" \
    "seg--1.1--1.2.sql" \
    "seg--1.2--1.3.sql" \
    "seg--1.3--1.4.sql"

echo -e "\n--- sslinfo (1.2) ---"
download_ext "sslinfo" \
    "sslinfo--1.2.sql"

echo -e "\n--- tablefunc (1.0) ---"
download_ext "tablefunc" \
    "tablefunc--1.0.sql"

echo -e "\n--- tcn (1.0) ---"
download_ext "tcn" \
    "tcn--1.0.sql"

echo -e "\n--- tsm_system_rows (1.0) ---"
download_ext "tsm_system_rows" \
    "tsm_system_rows--1.0.sql"

echo -e "\n--- tsm_system_time (1.0) ---"
download_ext "tsm_system_time" \
    "tsm_system_time--1.0.sql"

echo -e "\n--- unaccent (1.1) ---"
download_ext "unaccent" \
    "unaccent--1.1.sql"

echo -e "\n--- uuid-ossp (1.1) ---"
download_ext "uuid-ossp" \
    "uuid-ossp--1.1.sql"

echo -e "\n--- xml2 (1.1 → 1.2) ---"
download_ext "xml2" \
    "xml2--1.1.sql" \
    "xml2--1.1--1.2.sql"

echo -e "\n--- plpgsql (1.0, src/pl/plpgsql) ---"
fetch "${BASE_URL}/src/pl/plpgsql/src/plpgsql--1.0.sql" "plpgsql--1.0.sql"

# ═══════════════════════════════════════════════════════════════════════════
# Third-party extensions
# ═══════════════════════════════════════════════════════════════════════════

echo -e "\n--- vector / pgvector (0.8.2) ---"
fetch "https://raw.githubusercontent.com/pgvector/pgvector/v0.8.2/sql/vector.sql" \
    "vector--0.8.2.sql"

# ── Every download succeeded: replace the bundled files ────────────────────
rm -f "${OUT_DIR}"/*.sql
mv "${TMP_DIR}"/*.sql "${OUT_DIR}/"
