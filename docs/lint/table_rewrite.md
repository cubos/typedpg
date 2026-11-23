# `table_rewrite`

An `ALTER TABLE` that rewrites every row of an existing table under an
`ACCESS EXCLUSIVE` lock.

| Level | Applies to | Fix |
|---|---|---|
| warning | migrations | — |

## Example

```sql
-- migrations/0001_init.sql
CREATE TABLE posts (
    id bigint PRIMARY KEY,
    title varchar(200) NOT NULL,
    views integer NOT NULL DEFAULT 0
);

-- migrations/0002_rewrites.sql
ALTER TABLE posts ALTER COLUMN views TYPE bigint;
ALTER TABLE posts ADD COLUMN created_at timestamptz NOT NULL DEFAULT clock_timestamp();
ALTER TABLE posts ADD COLUMN slug text GENERATED ALWAYS AS (lower(title)) STORED;
ALTER TABLE posts SET UNLOGGED;
ALTER TABLE posts SET TABLESPACE fast;
```

```
warning[table_rewrite]: ALTER TABLE rewrites every row of public.posts under an ACCESS EXCLUSIVE lock
   ╭─[migrations/0002_rewrites.sql:1:1]
 1 │ ALTER TABLE posts ALTER COLUMN views TYPE bigint;
   · ────────────────────────────────────────────────
   ╰────
  help: rewrite in steps: add a new column, backfill it in batches, then swap

warning[table_rewrite]: SET TABLESPACE copies every block of public.posts under an ACCESS EXCLUSIVE lock
   ╭─[migrations/0002_rewrites.sql:5:1]
 5 │ ALTER TABLE posts SET TABLESPACE fast;
   · ─────────────────────────────────────
   ╰────
  help: create a new table in the new tablespace, copy the rows in batches, then swap the tables
```

Each of the five statements is reported. `SET UNLOGGED` gets the help
`create a new table with the new setting, copy the rows in batches, then swap the tables`.

## Why

PostgreSQL writes a new copy of the table and rebuilds its indexes, blocking
every query on it, reads included, for the length of the copy. It also needs
disk space for the second copy.

## Reported when

On a table that existed before the migration file started:

- `ALTER COLUMN … TYPE` to a type that is not binary-compatible
  (`integer` → `bigint`, a narrower `varchar(n)`, `text` → `varchar(n)`), or
  with a `USING` that computes a value.
- `ADD COLUMN` with a volatile `DEFAULT` (`clock_timestamp()`, `random()`,
  `nextval(…)`; a SQL function PostgreSQL inlines counts by its body, so
  one returning a constant does not), a `serial` type, an identity column, a `STORED`
  generated column, or a domain type with a `CHECK` or `NOT NULL`
  constraint (each row is checked, default or not).
- `ALTER COLUMN … SET EXPRESSION` of a `STORED` generated column.
- `ALTER TYPE … ALTER ATTRIBUTE … TYPE … CASCADE` that changes a typed
  table's column as `ALTER COLUMN … TYPE` would.
- `SET LOGGED` / `SET UNLOGGED` that changes the table's persistence.
- `SET ACCESS METHOD` to anything but `heap`.
- `SET TABLESPACE`, even to the tablespace the table is already in
  (tablespaces are not tracked).

## Not reported

- Binary-compatible type changes: `varchar(200)` → `varchar(300)`,
  `varchar` → `text`, a wider `numeric`.
- `ADD COLUMN` with a constant or stable default (`DEFAULT 0`,
  `DEFAULT now()`), a `VIRTUAL` generated column, or no default.
- `ADD COLUMN IF NOT EXISTS` on a column that exists.
- `SET LOGGED` / `UNLOGGED` to the current persistence,
  `SET ACCESS METHOD heap` / `DEFAULT`.
- Storage settings on a partitioned table (they only apply to future
  partitions).
- A table created in the same file.
- Migrations already deployed (unchanged from `--previous-deployment`),
  ignored ones, and `extra_migrations`.

To change a column's type without a rewrite: add a nullable column, backfill
it in batches, prove it `NOT NULL` as in
[`set_not_null_scan`](set_not_null_scan.md), then drop the old column and
rename the new one once no deployed code reads the old one
([`breaks_deployed_queries`](breaks_deployed_queries.md) checks that).

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
table_rewrite = "error"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "tableRewrite": "error" } }
```

Allow one statement with `-- typedpg-lint: allow(table_rewrite)` before it
in the migration. See [Configuring rules](README.md#configuring-rules).

## Related

- [`blocking_rebuild`](blocking_rebuild.md): `VACUUM FULL`, `CLUSTER`,
  `REPACK`.
- [`set_not_null_scan`](set_not_null_scan.md): `SET NOT NULL` scanning the
  table.
- [`missing_lock_timeout`](missing_lock_timeout.md): a blocking lock without
  `lock_timeout`.
