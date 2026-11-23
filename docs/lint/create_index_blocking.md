# `create_index_blocking`

`CREATE INDEX` without `CONCURRENTLY`, or a constraint that builds an index,
on a table that already has rows.

| Level | Applies to | Fix |
|---|---|---|
| warning | migrations | — |

## Example

```sql
-- migrations/0001_init.sql
CREATE TABLE users (id bigint PRIMARY KEY, email text NOT NULL, name text NOT NULL);

-- migrations/0002_index_email.sql
CREATE INDEX users_email_idx ON users (email);
ALTER TABLE users ADD CONSTRAINT users_name_key UNIQUE (name);
```

```
warning[create_index_blocking]: CREATE INDEX without CONCURRENTLY blocks writes on public.users for the whole build
   ╭─[migrations/0002_index_email.sql:1:1]
 1 │ CREATE INDEX users_email_idx ON users (email);
   · ─────────────────────────────────────────────
   ╰────
  help: use CREATE INDEX CONCURRENTLY, in a migration whose first line is -- no-transaction

warning[create_index_blocking]: adding the constraint builds its index under an ACCESS EXCLUSIVE lock, blocking reads and writes on public.users for the whole build
   ╭─[migrations/0002_index_email.sql:2:1]
 2 │ ALTER TABLE users ADD CONSTRAINT users_name_key UNIQUE (name);
   · ─────────────────────────────────────────────────────────────
   ╰────
  help: CREATE UNIQUE INDEX CONCURRENTLY first (in a migration whose first line is -- no-transaction), then ADD CONSTRAINT … UNIQUE / PRIMARY KEY USING INDEX, which only takes the lock briefly
```

The `CREATE INDEX` also gets a
[`missing_lock_timeout`](missing_lock_timeout.md) finding.

## Why

A plain `CREATE INDEX` blocks every `INSERT`, `UPDATE` and `DELETE` on the
table until the build ends, which takes minutes on a large table. Adding a
`PRIMARY KEY`, `UNIQUE` or `EXCLUDE` constraint builds the index under an
`ACCESS EXCLUSIVE` lock, which blocks reads too. In a transactional
migration the lock lasts until the commit.

## Reported when

On a table, partitioned table or foreign table created by an earlier
migration file:

- `CREATE INDEX` without `CONCURRENTLY` (but not `CREATE INDEX ON ONLY` a
  partitioned table, nor `IF NOT EXISTS` naming an existing index, which
  build nothing). On a partitioned table, only when a partition gets an
  index built: one with a matching index adopts it, and a partitioned table
  with no partition builds nothing;
- `ALTER TABLE … ADD CONSTRAINT … PRIMARY KEY / UNIQUE / EXCLUDE`, or
  `ADD COLUMN` with an inline `PRIMARY KEY` / `UNIQUE`;
- `ALTER COLUMN … TYPE` that rewrites no row but can't keep an index of the
  column: one whose collation changes, an expression or partial index, or a
  partition's index attached to the partitioned table's (all built again
  under `ACCESS EXCLUSIVE`);
- `ATTACH PARTITION` of a table missing an index the partitioned table
  has: PostgreSQL builds it under `ACCESS EXCLUSIVE`. Build it with
  `CREATE INDEX CONCURRENTLY` first, and the attach adopts it.

For `EXCLUDE`, which can't adopt an existing index, the help says to add it
while the table is small or can be locked for the whole build.

## Not reported

- `CREATE INDEX CONCURRENTLY` (it needs `-- no-transaction` as the file's
  first line).
- A constraint that adopts an index with `USING INDEX`. The safe sequence:

  ```sql
  -- migrations/0003_unique_name_index.sql
  -- no-transaction
  CREATE UNIQUE INDEX CONCURRENTLY users_name_key ON users (name);

  -- migrations/0004_unique_name.sql
  SET LOCAL lock_timeout = '5s';
  ALTER TABLE users ADD CONSTRAINT users_name_key UNIQUE USING INDEX users_name_key;
  ```

- A table created earlier in the same file: it has no rows yet.
- An index on a materialized view.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
create_index_blocking = "error"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "createIndexBlocking": "error" } }
```

Allow one statement with `-- typedpg-lint: allow(create_index_blocking)`
before it in the migration file.
See [Configuring rules](README.md#configuring-rules).

## Related

- [`missing_lock_timeout`](missing_lock_timeout.md): the same statement
  waiting for its lock with no timeout.
- [`drop_index_blocking`](drop_index_blocking.md): the `DROP INDEX`
  counterpart.
- [`blocking_rebuild`](blocking_rebuild.md): `REINDEX` without
  `CONCURRENTLY`.
- [`fk_without_index`](fk_without_index.md): the index you are probably
  adding.
