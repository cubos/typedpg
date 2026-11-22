# `blocking_rebuild`

`REINDEX`, `VACUUM FULL`, `CLUSTER` or `REPACK` without `CONCURRENTLY` on a
table that already has rows, holding a blocking lock for the whole rebuild.

| Level | Applies to | Fix |
|---|---|---|
| warning | migrations | — |

## Example

```sql
-- migrations/0001_init.sql
CREATE TABLE users (id bigint PRIMARY KEY, email text, name text NOT NULL);
CREATE INDEX users_email_idx ON users (email);
CREATE TABLE posts (id bigint PRIMARY KEY);

-- migrations/0002_reindex.sql
REINDEX TABLE users;

-- migrations/0003_vacuum.sql
-- no-transaction
VACUUM FULL posts;
```

```
warning[blocking_rebuild]: REINDEX without CONCURRENTLY blocks writes on public.users, and reads that use the indexes it rebuilds, for the whole rebuild
   ╭─[migrations/0002_reindex.sql:1:1]
 1 │ REINDEX TABLE users;
   · ───────────────────
   ╰────
  help: use REINDEX CONCURRENTLY, in a migration whose first line is -- no-transaction

warning[blocking_rebuild]: VACUUM FULL rewrites public.posts under an ACCESS EXCLUSIVE lock, blocking every query on it for the whole rebuild
   ╭─[migrations/0003_vacuum.sql:2:1]
 2 │ VACUUM FULL posts;
   · ─────────────────
   ╰────
  help: use REPACK (CONCURRENTLY), in a migration whose first line is -- no-transaction
```

Each statement also gets a [`missing_lock_timeout`](missing_lock_timeout.md)
finding. The fix:

```sql
-- migrations/0002_rebuild.sql
-- no-transaction
REINDEX TABLE CONCURRENTLY users;
REPACK (CONCURRENTLY) posts;
```

## Why

`REINDEX` blocks every write to the table, and every read that would use an
index it rebuilds. `VACUUM FULL`, `CLUSTER` and `REPACK` write a new copy of
the table under `ACCESS EXCLUSIVE`: no reads, no writes. On a large table
that is minutes of downtime.

## Reported when

On a table or materialized view created by an earlier migration file:

- `REINDEX TABLE` / `REINDEX INDEX` (reported on the index's table);
- `REINDEX SCHEMA s` when `s` has such a table; `REINDEX DATABASE` when any
  user schema has one;
- `VACUUM FULL` / `VACUUM (FULL)` of such a table, or of every table;
- `CLUSTER` and `REPACK`, of such a table or of every table.

## Not reported

- `REINDEX … CONCURRENTLY` and `REPACK (CONCURRENTLY)` (both need
  `-- no-transaction` as the file's first line).
- `REINDEX SYSTEM`: only the system catalogs.
- Plain `VACUUM`.
- A table created earlier in the same file.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
blocking_rebuild = "error"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "blockingRebuild": "error" } }
```

Allow one statement with `-- typedpg-lint: allow(blocking_rebuild)` before
it in the migration file.
See [Configuring rules](README.md#configuring-rules).

## Related

- [`create_index_blocking`](create_index_blocking.md): `CREATE INDEX`
  without `CONCURRENTLY`.
- [`table_rewrite`](table_rewrite.md): `ALTER TABLE` that rewrites every
  row.
- [`missing_lock_timeout`](missing_lock_timeout.md): the same statement
  waiting for its lock with no timeout.
