# `missing_primary_key`

A table without a primary key.

| | |
|---|---|
| Default level | warning |
| Applies to | the final schema |
| Fix | no |

## Why

Without a primary key a row has no identity. Nothing stops two identical rows from being inserted, and once they are, no `UPDATE` or `DELETE` can tell them apart. Tools assume one: logical replication cannot publish `UPDATE`s and `DELETE`s of a table with no replica identity — and by default the replica identity is the primary key — so those statements fail on the publisher once the table is in a publication. ORMs, admin tools and `ON CONFLICT` upserts all want a key to address a row by.

A `UNIQUE` constraint on `NOT NULL` columns gives uniqueness, but it is not a primary key: it is not the default replica identity, and the rule still reports the table.

## What it reports

Every ordinary or partitioned table in a user schema (not `pg_*` or `information_schema`) of the final schema that has no `PRIMARY KEY` constraint, except:

- temporary tables;
- partitions: a partition gets its partitioned table's primary key, and when that has none, the partitioned table is the one reported.

An inheritance child (`INHERITS`) is reported: a primary key is not inherited.

### Placement and `--previous-deployment`

A schema rule: it runs after the last migration, so a primary key added by any later migration resolves it, and the finding goes on the `CREATE TABLE` (or `CREATE TABLE AS`) that created the table. When that statement is in a migration file unchanged from `--previous-deployment <ref>` (or matching `lint.ignore`), the finding stays there with a help line for a new migration that doesn't lock the table during the index build:

```text
help: in a new migration, CREATE UNIQUE INDEX CONCURRENTLY on the key, then ADD PRIMARY KEY USING INDEX
```

A table created by `extra_migrations` is not reported.

## Examples

### Bad

```sql
-- migrations/0001_init.sql
CREATE TABLE events (
    id bigint NOT NULL,
    happened_at timestamptz NOT NULL
);
```

```text
warning[missing_primary_key]: public.events has no primary key
   ╭─[migrations/0001_init.sql:1:1]
 1 │ CREATE TABLE events (
   · ─────────────────────
 2 │     id bigint NOT NULL,
   · ───────────────────────
 3 │     happened_at timestamptz NOT NULL
   · ────────────────────────────────────
 4 │ );
   · ─
   ╰────
  help: add a PRIMARY KEY: without one, rows have no identity (duplicates go unnoticed, and logical replication cannot publish UPDATEs and DELETEs of the table)
```

### Good

```sql
CREATE TABLE events (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    happened_at timestamptz NOT NULL
);
-- A primary key added later counts.
CREATE TABLE audit_log (
    id bigint NOT NULL,
    at timestamptz NOT NULL
);
ALTER TABLE audit_log ADD PRIMARY KEY (id);
-- Temporary tables are skipped.
CREATE TEMP TABLE scratch (id bigint);
-- A partition gets its parent's key.
CREATE TABLE logs (id bigint NOT NULL, PRIMARY KEY (id)) PARTITION BY RANGE (id);
CREATE TABLE logs_1 PARTITION OF logs FOR VALUES FROM (0) TO (1000);
-- A table that really has no key, accepted on purpose.
-- typedpg-lint: allow(missing_primary_key)
CREATE TABLE page_views (path text NOT NULL, viewed_at timestamptz NOT NULL);
```

On an existing table with rows, add the key without blocking writes during the index build: `CREATE UNIQUE INDEX CONCURRENTLY` in a `-- no-transaction` migration, then `ALTER TABLE … ADD PRIMARY KEY USING INDEX` in the next one ([`create_index_blocking`](create_index_blocking.md)).

## When it stays silent

- Temporary tables, partitions, and tables in `pg_*` schemas or `information_schema`.
- Tables created by `extra_migrations`.
- An allow comment on the statement that created the table.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
missing_primary_key = "off"   # "error", "warning" (default), "info" or "off"
```

```json
// typedpg.config.json
{ "lint": { "missingPrimaryKey": "off" } }
```

For one table, the allow comment goes on its `CREATE TABLE`, as above. See [Configuring rules](README.md#configuring-rules) for `fail_on` / `failOn`, `ignore` and `--previous-deployment`.

## Related rules

- [`prefer_bigint_pk`](prefer_bigint_pk.md): the key's type.
- [`prefer_identity`](prefer_identity.md): how the key is generated.
