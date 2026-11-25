# A PostgreSQL 19.0 server carrying every third-party extension typedpg_analyzer
# bundles, for the `pg_sanity` oracle (built by scripts/build-extensions-image.sh,
# used with `PG_IMAGE=typedpg-extensions:pg19 scripts/run-pg-sanity.sh`):
#
#   postgis 3.7.0 (postgis, postgis_raster, postgis_topology, postgis_sfcgal)
#                    the official postgis/postgis image this one starts from
#   vector 0.8.7     PGDG's postgresql-19-pgvector
#   pg_cron 1.6.8    PGDG's postgresql-19-cron, preloaded (it refuses to load
#                    otherwise), with cron.database_name = pg_sanity_cron, the
#                    database its pg_sanity tests run in
#   pgmq 1.13.2      the release's SQL (a pure SQL extension), installed as
#                    its Makefile does
#   pg_jsonschema    0.3.4, built here with pgrx: no release ships a PostgreSQL
#                    19 build, and 0.3.4's pgrx (0.16) predates PostgreSQL 19,
#                    so it is built on pgrx 0.19.5 (the newest 0.19 release,
#                    which has a `pg19` feature); the extension's source is
#                    0.3.4's.
#
# The versions must stay those of update_extensions.sh: the analyzer's
# scripts are this image's.

ARG POSTGIS_IMAGE=postgis/postgis:19-3.7

FROM ${POSTGIS_IMAGE} AS jsonschema
ARG PG_JSONSCHEMA_VERSION=0.3.4
ARG PGRX_VERSION=0.19.5
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential ca-certificates curl clang libclang-dev pkg-config \
        postgresql-server-dev-19 \
    && rm -rf /var/lib/apt/lists/*
ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:$PATH
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable
RUN cargo install --locked cargo-pgrx --version "=${PGRX_VERSION}"
RUN cargo pgrx init --pg19 /usr/lib/postgresql/19/bin/pg_config
WORKDIR /src
RUN curl -sSfL "https://github.com/supabase/pg_jsonschema/archive/refs/tags/v${PG_JSONSCHEMA_VERSION}.tar.gz" \
        | tar xz --strip-components=1
# The release's features stop at pg18 and its pgrx at 0.16: add pg19 and move
# to the pgrx the CLI above is.
RUN sed -i \
        -e "s/^pgrx = \".*\"/pgrx = \"=${PGRX_VERSION}\"/" \
        -e "s/^pgrx-tests = \".*\"/pgrx-tests = \"=${PGRX_VERSION}\"/" \
        -e 's/^pg18 = \(.*\)$/pg18 = \1\npg19 = ["pgrx\/pg19", "pgrx-tests\/pg19"]/' \
        Cargo.toml \
    && cargo update -p pgrx -p pgrx-tests
RUN cargo pgrx package --no-default-features --features pg19 \
        --pg-config /usr/lib/postgresql/19/bin/pg_config --out-dir /out

FROM ${POSTGIS_IMAGE} AS pgmq
ARG PGMQ_VERSION=1.13.2
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
RUN curl -sSfL "https://github.com/pgmq/pgmq/archive/refs/tags/v${PGMQ_VERSION}.tar.gz" | tar xz \
    && mkdir /out \
    && cp "pgmq-${PGMQ_VERSION}/pgmq-extension/pgmq.control" /out/ \
    && cp pgmq-${PGMQ_VERSION}/pgmq-extension/sql/pgmq--*--*.sql /out/ \
    && cp "pgmq-${PGMQ_VERSION}/pgmq-extension/sql/pgmq.sql" "/out/pgmq--${PGMQ_VERSION}.sql"

FROM ${POSTGIS_IMAGE}
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        'postgresql-19-pgvector=0.8.7-*' 'postgresql-19-cron=1.6.8-*' \
    && rm -rf /var/lib/apt/lists/*
COPY --from=pgmq /out/ /usr/share/postgresql/19/extension/
COPY --from=jsonschema /out/usr/share/postgresql/19/extension/ /usr/share/postgresql/19/extension/
COPY --from=jsonschema /out/usr/lib/postgresql/19/lib/ /usr/lib/postgresql/19/lib/
# The postgis image's initdb script installs PostGIS into the default
# database and a template: the oracle's databases start empty.
RUN rm -f /docker-entrypoint-initdb.d/*postgis*
# initdb copies the sample: every cluster this image creates preloads pg_cron.
RUN printf "\nshared_preload_libraries = 'pg_cron'\ncron.database_name = 'pg_sanity_cron'\n" \
        >> /usr/share/postgresql/postgresql.conf.sample
