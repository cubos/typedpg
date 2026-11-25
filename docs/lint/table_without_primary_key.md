# `table_without_primary_key`

A table without a primary key.

| Category | Level | Applies to | Fix |
|---|---|---|---|
| suspicious | warning | final schema | — |

## Example

```sql
-- migrations/0001_init.sql
CREATE TABLE events (id bigint NOT NULL, happened_at timestamptz NOT NULL);
```

```
warning[suspicious/table_without_primary_key]: public.events has no primary key
   ╭─[migrations/0001_init.sql:1:1]
 1 │ CREATE TABLE events (id bigint NOT NULL, happened_at timestamptz NOT NULL);
   · ──────────────────────────────────────────────────────────────────────────
   ╰────
  help: add a PRIMARY KEY: without one, rows have no identity (duplicates go unnoticed, and logical replication cannot publish UPDATEs and DELETEs of the table)
```

When the `CREATE TABLE` is in a file unchanged since `--previous-deployment`
or matched by `ignore`, the help asks for a new migration instead:

```
  help: in a new migration, CREATE UNIQUE INDEX CONCURRENTLY on the key, then ADD PRIMARY KEY USING INDEX
```

## Why

Without a primary key, duplicate rows go unnoticed and no `UPDATE` or
`DELETE` can tell them apart. Logical replication's default replica
identity is the primary key, so `UPDATE`s and `DELETE`s of a published table
without one fail. ORMs and admin tools also expect a key.

## Reported when

A table or partitioned table in a user schema has no `PRIMARY KEY` after the
last migration. The finding goes on the statement that created it (`CREATE
TABLE`, `CREATE TABLE AS`, `SELECT INTO`, `CREATE SCHEMA … CREATE TABLE`, …);
a later migration adding the key resolves it.

A `UNIQUE` constraint on `NOT NULL` columns doesn't count, and neither does
an inheritance parent's key: `CREATE TABLE child () INHERITS (parent)` is
reported.

## Not reported

- A key added later: `ALTER TABLE audit_log ADD PRIMARY KEY (id)`.
- Partitions (the partitioned table is reported instead when it has no key).
- Temporary tables; tables in `pg_*` schemas or `information_schema`.
- Tables created by `extra_migrations`.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
table_without_primary_key = "off"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "tableWithoutPrimaryKey": "off" } }
```

Allow one table with `-- typedpg-lint: allow(table_without_primary_key)` before
its `CREATE TABLE` in the migration file.
See [Configuring rules](README.md#configuring-rules).

## Related

- [`prefer_bigint_pk`](prefer_bigint_pk.md): the key's type.
- [`prefer_identity`](prefer_identity.md): how the key is generated.
- [`create_index_blocking`](create_index_blocking.md): building the key's
  index on a table with rows.
