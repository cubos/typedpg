#!/usr/bin/env bash
#
# Downloads extension SQL scripts from the PostgreSQL repository, and the
# third-party extensions' (pgvector, PostGIS, pg_cron, pgmq, pg_jsonschema)
# from their releases — PostGIS's from its Docker image, so Docker is needed.
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
# The scripts must come from the PostgreSQL release typedpg targets
# (typedpg_pg_query/POSTGRES_VERSION), the parser's and the seed's: a newer branch ships extension versions
# whose scripts use grammar that release's parser rejects, and objects its
# server doesn't have. The file lists below follow that release's
# contrib/ too.
#
# Usage:
#   ./update_extensions.sh              # the release typedpg targets (default)
#   ./update_extensions.sh REL_19_0     # use a specific tag/branch
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
    # `19.0` -> REL_19_0, the tag format of every release since
    # PostgreSQL 10 (REL_<major>_<minor>).
    PG_VERSION=$(tr -d '[:space:]' < typedpg_pg_query/POSTGRES_VERSION)
    if [[ ! "$PG_VERSION" =~ ^([0-9]+)\.([0-9]+)$ ]]; then
        echo "error: cannot derive a release tag from \"${PG_VERSION}\" in" \
            "typedpg_pg_query/POSTGRES_VERSION; pass the tag explicitly" >&2
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

echo -e "\n--- btree_gin (1.0 → 1.4) ---"
download_ext "btree_gin" \
    "btree_gin--1.0.sql" \
    "btree_gin--1.0--1.1.sql" \
    "btree_gin--1.1--1.2.sql" \
    "btree_gin--1.2--1.3.sql" \
    "btree_gin--1.3--1.4.sql"

# PostgreSQL 19 ships btree_gist 1.9's installation script alone: the update
# scripts from 1.2 only serve databases upgraded from an older release.
echo -e "\n--- btree_gist (1.9) ---"
download_ext "btree_gist" \
    "btree_gist--1.9.sql"

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

echo -e "\n--- pg_buffercache (1.2 → 1.7) ---"
download_ext "pg_buffercache" \
    "pg_buffercache--1.2.sql" \
    "pg_buffercache--1.2--1.3.sql" \
    "pg_buffercache--1.3--1.4.sql" \
    "pg_buffercache--1.4--1.5.sql" \
    "pg_buffercache--1.5--1.6.sql" \
    "pg_buffercache--1.6--1.7.sql"

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

echo -e "\n--- pg_stash_advice (1.0) ---"
download_ext "pg_stash_advice" \
    "pg_stash_advice--1.0.sql"

echo -e "\n--- pg_stat_statements (1.4 → 1.13) ---"
download_ext "pg_stat_statements" \
    "pg_stat_statements--1.4.sql" \
    "pg_stat_statements--1.4--1.5.sql" \
    "pg_stat_statements--1.5--1.6.sql" \
    "pg_stat_statements--1.6--1.7.sql" \
    "pg_stat_statements--1.7--1.8.sql" \
    "pg_stat_statements--1.8--1.9.sql" \
    "pg_stat_statements--1.9--1.10.sql" \
    "pg_stat_statements--1.10--1.11.sql" \
    "pg_stat_statements--1.11--1.12.sql" \
    "pg_stat_statements--1.12--1.13.sql"

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

echo -e "\n--- postgres_fdw (1.0 → 1.3) ---"
download_ext "postgres_fdw" \
    "postgres_fdw--1.0.sql" \
    "postgres_fdw--1.0--1.1.sql" \
    "postgres_fdw--1.1--1.2.sql" \
    "postgres_fdw--1.2--1.3.sql"

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

# Built apart from the server (the official image ships neither; the
# extension oracle's typedpg-pg-pl image, scripts/pg-pl, does).
echo -e "\n--- plperl, plperlu (1.0, src/pl/plperl) ---"
fetch "${BASE_URL}/src/pl/plperl/plperl--1.0.sql" "plperl--1.0.sql"
fetch "${BASE_URL}/src/pl/plperl/plperlu--1.0.sql" "plperlu--1.0.sql"

echo -e "\n--- plpython3u (1.0, src/pl/plpython) ---"
fetch "${BASE_URL}/src/pl/plpython/plpython3u--1.0.sql" "plpython3u--1.0.sql"

# ═══════════════════════════════════════════════════════════════════════════
# Third-party extensions
# ═══════════════════════════════════════════════════════════════════════════

# What the pgvector/pgvector:pg19 image ships: 0.8.7's install script,
# no older version and no update script.
echo -e "\n--- vector / pgvector (0.8.7) ---"
fetch "https://raw.githubusercontent.com/pgvector/pgvector/v0.8.7/sql/vector.sql" \
    "vector--0.8.7.sql"

# PostGIS's scripts are generated when it is built: they are taken from the
# official image of PostGIS for PostgreSQL 19 (scripts/extensions-image builds
# the oracle's server from the same image), as installed in its
# share/extension. That image ships postgis, postgis_raster,
# postgis_topology and postgis_sfcgal at 3.7.0: their installation scripts,
# and the two pseudo-versions' small scripts — `unpackaged` installs nothing,
# `3.7.0--ANY` only tags the version. The scripts back to 3.7.0
# (`--ANY--3.7.0`, `--unpackaged--3.7.0`, the full upgrade, for databases
# upgraded from an older PostGIS release) are not bundled. It has no
# postgis_tiger_geocoder or address_standardizer.
POSTGIS_IMAGE="postgis/postgis:19-3.7"
POSTGIS_VERSION="3.7.0"
echo -e "\n--- postgis, postgis_raster, postgis_topology, postgis_sfcgal (${POSTGIS_VERSION}, ${POSTGIS_IMAGE}) ---"
docker pull -q "$POSTGIS_IMAGE" >/dev/null
for ext in postgis postgis_raster postgis_topology postgis_sfcgal; do
    for file in "${ext}--${POSTGIS_VERSION}.sql" "${ext}--unpackaged.sql" \
        "${ext}--${POSTGIS_VERSION}--ANY.sql"; do
        echo "  ${file}..."
        if ! docker run --rm --entrypoint cat "$POSTGIS_IMAGE" \
            "/usr/share/postgresql/19/extension/${file}" > "${TMP_DIR}/raw.sql"; then
            echo "error: ${POSTGIS_IMAGE} has no ${file}" >&2
            exit 1
        fi
        grep -v '^\\\(echo\|quit\)' "${TMP_DIR}/raw.sql" > "${TMP_DIR}/${file}" || true
        rm -f "${TMP_DIR}/raw.sql"
    done
done

# pg_cron builds its 1.0 installation script from pg_cron.sql (its Makefile's
# DATA_built); every later version is reached by its update scripts.
PG_CRON_VERSION="1.6.8"
echo -e "\n--- pg_cron (1.0 -> 1.6, release ${PG_CRON_VERSION}) ---"
PG_CRON_URL="https://raw.githubusercontent.com/citusdata/pg_cron/v${PG_CRON_VERSION}"
fetch "${PG_CRON_URL}/pg_cron.sql" "pg_cron--1.0.sql"
for step in 1.0--1.1 1.1--1.2 1.2--1.3 1.3--1.4 1.4--1.4-1 1.4-1--1.5 1.5--1.6; do
    fetch "${PG_CRON_URL}/pg_cron--${step}.sql" "pg_cron--${step}.sql"
done

# pgmq's installation script is sql/pgmq.sql, copied to pgmq--<version>.sql
# when packaged — the only installation script a release ships: its update
# scripts serve databases created by an older release, so no older version
# can be installed.
PGMQ_VERSION="1.13.2"
echo -e "\n--- pgmq (${PGMQ_VERSION}) ---"
fetch "https://raw.githubusercontent.com/pgmq/pgmq/v${PGMQ_VERSION}/pgmq-extension/sql/pgmq.sql" \
    "pgmq--${PGMQ_VERSION}.sql"

# pg_jsonschema's script is generated by pgrx when packaged: it is taken from
# the release's PostgreSQL 18 package, the newest it ships (pgrx generates
# the same statements for 19, only its comments naming Rust types differ:
# scripts/extensions-image builds 0.3.4 for 19).
PG_JSONSCHEMA_VERSION="0.3.4"
echo -e "\n--- pg_jsonschema (${PG_JSONSCHEMA_VERSION}) ---"
deb="pg_jsonschema-v${PG_JSONSCHEMA_VERSION}-pg18-amd64-linux-gnu.deb"
if ! curl -sfL -o "${TMP_DIR}/${deb}" \
    "https://github.com/supabase/pg_jsonschema/releases/download/v${PG_JSONSCHEMA_VERSION}/${deb}"; then
    echo "error: failed to download ${deb}" >&2
    exit 1
fi
mkdir "${TMP_DIR}/deb"
(cd "${TMP_DIR}/deb" && ar x "../${deb}" && tar xf data.tar.*)
install -m 0644 "${TMP_DIR}/deb/usr/share/postgresql/18/extension/pg_jsonschema--${PG_JSONSCHEMA_VERSION}.sql" \
    "${TMP_DIR}/"
rm -rf "${TMP_DIR}/deb" "${TMP_DIR}/${deb}"

# ── Every download succeeded: replace the bundled files ────────────────────
rm -f "${OUT_DIR}"/*.sql
mv "${TMP_DIR}"/*.sql "${OUT_DIR}/"
