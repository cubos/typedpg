# `drop_index_blocking`

`DROP INDEX` without `CONCURRENTLY` on a table that already has rows.

| Category | Level | Applies to | Fix |
|---|---|---|---|
| locking | warning | migrations | — |

## Example

```sql
-- migrations/0001_init.sql
CREATE TABLE users (id bigint PRIMARY KEY, email text, name text NOT NULL);
CREATE INDEX users_email_idx ON users (email);

-- migrations/0002_drop_index.sql
DROP INDEX users_email_idx;
```

```
warning[locking/drop_index_blocking]: DROP INDEX without CONCURRENTLY takes an ACCESS EXCLUSIVE lock on public.users, blocking every query on the table
   ╭─[migrations/0002_drop_index.sql:1:1]
 1 │ DROP INDEX users_email_idx;
   · ──────────────────────────
   ╰────
  help: use DROP INDEX CONCURRENTLY, in a migration whose first line is -- no-transaction
```

The statement also gets a [`lock_without_timeout`](lock_without_timeout.md)
finding. The fix:

```sql
-- migrations/0002_drop_index.sql
-- no-transaction
DROP INDEX CONCURRENTLY users_email_idx;
```

## Why

The drop itself is fast, but it first waits for every transaction using the
table, and every query arriving meanwhile, reads included, queues behind
it. In a transactional migration the lock then lasts until the commit.

## Reported when

The dropped index and its table were both created by an earlier migration
file. When one statement drops several indexes, the help asks for one
`DROP INDEX CONCURRENTLY` per index (it accepts only one, and no
`CASCADE`).

## Not reported

- `DROP INDEX CONCURRENTLY`.
- An index or table created earlier in the same file.
- The index of a partitioned table: PostgreSQL can't drop it concurrently.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
drop_index_blocking = "error"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "dropIndexBlocking": "error" } }
```

Allow one statement with `-- typedpg-lint: allow(drop_index_blocking)`
before it in the migration file.
See [Configuring rules](README.md#configuring-rules).

## Related

- [`create_index_blocking`](create_index_blocking.md): the `CREATE INDEX`
  counterpart.
- [`redundant_index`](redundant_index.md): an index worth dropping.
- [`lock_without_timeout`](lock_without_timeout.md): the same statement
  waiting for its lock with no timeout.
