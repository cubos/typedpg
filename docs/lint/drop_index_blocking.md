# `drop_index_blocking`

`DROP INDEX` without `CONCURRENTLY` on an index of an existing table takes an `ACCESS EXCLUSIVE` lock on the table, blocking every query on it.

| | |
|---|---|
| Default level | warning |
| Applies to | migrations |
| Fix | no |

## Why

Dropping an index is fast once it has its lock, but a plain `DROP INDEX` locks the index's table with `ACCESS EXCLUSIVE`, the strongest lock: no reads, no writes. It has to wait for every transaction already using the table, and everything that arrives meanwhile queues behind it. In a transactional migration the lock is then held until the commit, across every later statement of the file.

`DROP INDEX CONCURRENTLY` waits for conflicting transactions without blocking new queries on the table. It drops one index per statement, does not accept `CASCADE`, and cannot run inside a transaction block — so it goes in a migration whose first line is `-- no-transaction`.

## What it reports

`DROP INDEX` without `CONCURRENTLY` that drops an index which existed before the current file started, on a table that also existed then. When the statement drops several indexes, the help asks for one `DROP INDEX CONCURRENTLY` per index.

A partitioned index (the parent index of a partitioned table) is not reported: PostgreSQL refuses to drop one concurrently, so there is nothing better to suggest.

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

-- migrations/0002_drop_index.sql
DROP INDEX users_email_idx;
```

```text
warning[drop_index_blocking]: DROP INDEX without CONCURRENTLY takes an ACCESS EXCLUSIVE lock on public.users, blocking every query on the table
   ╭─[migrations/0002_drop_index.sql:1:1]
 1 │ DROP INDEX users_email_idx;
   · ──────────────────────────
   ╰────
  help: use DROP INDEX CONCURRENTLY, in a migration whose first line is -- no-transaction
```

The statement also gets a [`missing_lock_timeout`](missing_lock_timeout.md) finding (not shown).

### Good

```sql
-- migrations/0002_drop_index.sql
-- no-transaction
DROP INDEX CONCURRENTLY users_email_idx;
```

Not reported: dropping an index created earlier in the same file (here [`create_index_blocking`](create_index_blocking.md) still reports the `CREATE INDEX` on the existing table).

```sql
CREATE INDEX users_name_idx ON users (name);
DROP INDEX users_name_idx;
```

## When it stays silent

- `CONCURRENTLY` is used, the index is partitioned, or the index or its table was created in the same file.
- The file is unchanged from `--previous-deployment`, matches `lint.ignore`, or belongs to `extra_migrations`.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
drop_index_blocking = "error"   # "error", "warning" (default), "info" or "off"
```

```json
// typedpg.config.json
{ "lint": { "dropIndexBlocking": "error" } }
```

For one statement:

```sql
-- typedpg-lint: allow(drop_index_blocking)
DROP INDEX users_email_idx;
```

See [Configuring rules](README.md#configuring-rules) for `fail_on` / `failOn` and `ignore`.

## Related rules

- [`create_index_blocking`](create_index_blocking.md)
- [`redundant_index`](redundant_index.md): an index worth dropping.
- [`missing_lock_timeout`](missing_lock_timeout.md)
