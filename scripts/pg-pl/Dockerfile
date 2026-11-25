# The PostgreSQL release typedpg targets, with the procedural languages the
# official image leaves out (PL/Perl, PL/Python): the server the extension
# oracle installs plperl, plperlu, plpython3u and their transforms
# (bool_plperl, hstore_plpython3u, ...) on. scripts/run-pg-sanity.sh builds
# it when PG_IMAGE names it (typedpg-pg-pl:<version>).
ARG PG_VERSION=19.0
FROM postgres:${PG_VERSION}
ARG PG_VERSION
RUN set -eux; \
    major="${PG_VERSION%%.*}"; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
        "postgresql-plperl-${major}" "postgresql-plpython3-${major}"; \
    rm -rf /var/lib/apt/lists/*
