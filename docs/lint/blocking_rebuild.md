# `blocking_rebuild`

A migration rebuilds an existing table or its indexes — `REINDEX`, `VACUUM FULL`, `CLUSTER` or `REPACK` without `CONCURRENTLY` — holding a blocking lock for the whole rebuild.

| | |
|---|---|
| Default level | warning |
| Applies to | migrations |
| Fix | no |

## Why

These commands rebuild data, and they hold their lock until they are done:

- `REINDEX` (without `CONCURRENTLY`) takes a `SHARE` lock on the table, which blocks every write, and an `ACCESS EXCLUSIVE` lock on each index it rebuilds, which blocks every read that would use that index.
- `VACUUM FULL`, `CLUSTER` and `REPACK` (without `CONCURRENTLY`) write a new copy of the table under `ACCESS EXCLUSIVE`: no reads, no writes.

On a large table this is minutes of downtime. PostgreSQL 19 has non-blocking forms: `REINDEX CONCURRENTLY` and `REPACK (CONCURRENTLY)` (which replaces `VACUUM FULL` and `CLUSTER` for this purpose). Neither can run inside a transaction block, so they go in a migration whose first line is `-- no-transaction` — as does `VACUUM`, which cannot run in a transaction at all.

## What it reports

On relations that existed before the current file started:

- `REINDEX TABLE` / `REINDEX INDEX` without `CONCURRENTLY` (an index is reported as its table; a materialized view counts as a table).
- `REINDEX SCHEMA s` when `s` has such a table, and `REINDEX DATABASE` when any user schema has one. `REINDEX SYSTEM` only touches the system catalogs and is not reported.
- `VACUUM FULL` (or `VACUUM (FULL)`) of such a table, or with no table list when any exists.
- `CLUSTER` and `REPACK` without `CONCURRENTLY`, of such a table or of every table.

## Examples

### Bad

```sql
-- migrations/0001_init.sql
CREATE TABLE users (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    email text,
    name text NOT NULL
);
CREATE INDEX users_email_idx ON users (email);
CREATE TABLE posts (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY);

-- migrations/0002_reindex.sql
REINDEX TABLE users;

-- migrations/0003_vacuum.sql
-- no-transaction
VACUUM FULL posts;

-- migrations/0004_cluster.sql
CLUSTER users USING users_email_idx;
```

```text
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

warning[blocking_rebuild]: CLUSTER rewrites public.users under an ACCESS EXCLUSIVE lock, blocking every query on it for the whole rebuild
   ╭─[migrations/0004_cluster.sql:1:1]
 1 │ CLUSTER users USING users_email_idx;
   · ───────────────────────────────────
   ╰────
  help: use REPACK (CONCURRENTLY), in a migration whose first line is -- no-transaction
```

Each statement also gets a [`missing_lock_timeout`](missing_lock_timeout.md) finding (not shown).

### Good

```sql
-- migrations/0002_reindex.sql
-- no-transaction
REINDEX TABLE CONCURRENTLY users;

-- migrations/0003_repack.sql
-- no-transaction
REPACK (CONCURRENTLY) posts;

-- migrations/0004_vacuum.sql
-- no-transaction
-- A plain VACUUM (no FULL) does not rewrite the table.
VACUUM (ANALYZE) posts;
```

Also not reported: a rebuild of a table created in the same file, and `VACUUM (FULL false)`.

## When it stays silent

- `CONCURRENTLY` is used, `FULL` is absent or false, or the target was created in the same file.
- `REINDEX SYSTEM`.
- The file is unchanged from `--previous-deployment`, matches `lint.ignore`, or belongs to `extra_migrations`.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
blocking_rebuild = "error"   # "error", "warning" (default), "info" or "off"
```

```json
// typedpg.config.json
{ "lint": { "blockingRebuild": "error" } }
```

For one statement:

```sql
-- typedpg-lint: allow(blocking_rebuild)
REINDEX TABLE users;
```

See [Configuring rules](README.md#configuring-rules) for `fail_on` / `failOn` and `ignore`.

## Related rules

- [`table_rewrite`](table_rewrite.md): an `ALTER TABLE` that rewrites the table as a side effect.
- [`create_index_blocking`](create_index_blocking.md), [`drop_index_blocking`](drop_index_blocking.md)
- [`missing_lock_timeout`](missing_lock_timeout.md)
