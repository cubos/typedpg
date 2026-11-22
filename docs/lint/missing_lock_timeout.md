# `missing_lock_timeout`

A migration waits for a write-blocking lock on an existing table without
`SET lock_timeout` first.

| Level | Applies to | Fix |
|---|---|---|
| warning | migrations | — |

## Example

```sql
-- migrations/0001_init.sql
CREATE TABLE users (id bigint PRIMARY KEY, email text, name text NOT NULL);

-- migrations/0002_nickname.sql
ALTER TABLE users ADD COLUMN nickname text;
ALTER TABLE users ADD COLUMN bio text;
```

```
warning[missing_lock_timeout]: ALTER TABLE waits for a lock on public.users with no lock_timeout: queued behind a long transaction, it blocks every query on the table
   ╭─[migrations/0002_nickname.sql:1:1]
 1 │ ALTER TABLE users ADD COLUMN nickname text;
   · ──────────────────────────────────────────
   ╰────
  help: start the migration with SET LOCAL lock_timeout = '5s' and retry on timeout
```

The fix is `SET LOCAL lock_timeout = '5s';` at the top of the file (`SET`
in a `-- no-transaction` file, where the help says so). When it fires, the
migration fails with `canceling statement due to lock timeout`; run it
again later.

## Why

Even an instant `ALTER TABLE` must first get its lock. Behind a long
transaction it waits, and every query arriving after it queues behind its
pending request: one slow report plus one quick DDL becomes an outage on the
table. `lock_timeout` makes the statement give up instead; PostgreSQL's
default, `0`, waits forever.

## Reported when

The first statement of a file, and only that one, that waits on a table that
existed before the file started, with no non-zero `lock_timeout` in effect:

- `ALTER TABLE` (including `RENAME`), except subcommands that only block
  other DDL (`VALIDATE CONSTRAINT`, `SET STATISTICS`, column `SET (…)`,
  `CLUSTER ON`, most storage parameters, `DETACH PARTITION … CONCURRENTLY`
  / `FINALIZE`). `ATTACH PARTITION` reports the attached table; an added
  foreign key, the referenced one.
- `CREATE TABLE` with a foreign key to an existing table.
- `CREATE INDEX` / `DROP INDEX` without `CONCURRENTLY` (named by the
  index's table), `DROP TABLE`, `TRUNCATE`.
- `CREATE TRIGGER`, `CREATE POLICY`, `ALTER POLICY`, `CREATE RULE`.
- `LOCK TABLE` in `SHARE` mode or stronger, or with no mode.
- `REFRESH MATERIALIZED VIEW`, `CLUSTER`, `REPACK`, `REINDEX` without
  `CONCURRENTLY`; `VACUUM FULL`.

The message says `blocks every query` for an `ACCESS EXCLUSIVE` lock and
`blocks every write` for weaker ones (`CREATE INDEX`, `REINDEX`, triggers,
foreign keys).

## Not reported

- A non-zero `lock_timeout` is in effect, set by:
  - `SET lock_timeout = …` for the rest of the file, or `SET LOCAL` until
    the transaction ends (a no-op in a `-- no-transaction` file);
  - `SELECT set_config('lock_timeout', '5s', true)` with constant arguments.

  `RESET`, `DEFAULT`, `0` (or anything rounding to 0 ms) and a non-constant
  value turn it off; a `ROLLBACK` undoes a `SET` made in its transaction.
- A table created earlier in the same file.
- Statements that block no writes: `CREATE INDEX CONCURRENTLY`,
  `SET STATISTICS`, `LOCK TABLE … IN ROW EXCLUSIVE MODE`.
- Later statements of a file already reported.
- A file unchanged since `--previous-deployment`, matched by `ignore`, or
  in `extra_migrations`.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
missing_lock_timeout = "error"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "missingLockTimeout": "error" } }
```

Allow one statement with `-- typedpg-lint: allow(missing_lock_timeout)`
before it in the migration file; the file's next blocking statement is then
reported instead. See [Configuring rules](README.md#configuring-rules).

## Related

Each of these reports a statement that also needs a timeout:
[`create_index_blocking`](create_index_blocking.md),
[`constraint_validating`](constraint_validating.md),
[`set_not_null_scan`](set_not_null_scan.md),
[`table_rewrite`](table_rewrite.md),
[`drop_index_blocking`](drop_index_blocking.md),
[`blocking_rebuild`](blocking_rebuild.md),
[`detach_partition_blocking`](detach_partition_blocking.md),
[`attach_partition_scan`](attach_partition_scan.md).
