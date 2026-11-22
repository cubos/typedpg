# `missing_lock_timeout`

A migration takes a lock that blocks writes on an existing table without first setting `lock_timeout`, so while it waits for the lock every write to the table — and, for an `ACCESS EXCLUSIVE` lock, every query — queues behind it.

| | |
|---|---|
| Default level | warning |
| Applies to | migrations |
| Fix | no |

## Why

Even an `ALTER TABLE` that finishes in a millisecond, like adding a nullable column, needs an `ACCESS EXCLUSIVE` lock first. If a long transaction is holding any lock on the table — a report, a stuck worker, an idle-in-transaction session — the migration waits behind it. PostgreSQL's lock queue is first-come, first-served: every query that arrives after the migration, even a plain `SELECT`, queues behind the migration's pending request. One slow transaction plus one quick DDL statement becomes a full outage on the table, for as long as the slow transaction lasts.

A weaker lock queues fewer queries: the `SHARE` lock of `CREATE INDEX` and `REINDEX`, or the `SHARE ROW EXCLUSIVE` lock of `CREATE TRIGGER`, of enabling or disabling triggers and of a foreign key (on both tables), conflicts with writers only, so reads keep running while writes pile up. The message says which: `it blocks every query on the table` for `ACCESS EXCLUSIVE`, `it blocks every write to the table` otherwise.

`lock_timeout` bounds the wait: when the lock is not granted in time, the statement fails, the queue behind it drains, and the migration can be retried later. PostgreSQL's default, `0`, waits forever.

## What it reports

The first statement of a migration file that waits for a write-blocking lock on a relation that existed before the file started, while no non-zero `lock_timeout` is in effect. At most one finding per file: once the file is flagged, the fix (one `SET` at the top) covers the rest.

The statements that take such a lock:

- `ALTER TABLE`, unless every subcommand only takes `SHARE UPDATE EXCLUSIVE` (`VALIDATE CONSTRAINT`, `SET STATISTICS`, column `SET (…)` / `RESET (…)`, `CLUSTER ON`, `SET WITHOUT CLUSTER`, storage parameters other than `user_catalog_table`, `DETACH PARTITION … CONCURRENTLY` / `FINALIZE`). An `ATTACH PARTITION` reports the table being attached, which gets `ACCESS EXCLUSIVE`; an added foreign key reports the referenced table, which gets `SHARE ROW EXCLUSIVE`.
- `ALTER TABLE … RENAME` (table or column).
- `CREATE TABLE` with a foreign key to an existing table (the referenced table is locked).
- `CREATE INDEX` and `DROP INDEX` without `CONCURRENTLY`, `DROP TABLE`, `TRUNCATE`.
- `CREATE TRIGGER`, `CREATE POLICY`, `ALTER POLICY`, `CREATE RULE`.
- `LOCK TABLE` in `SHARE` mode or stronger (or with no mode, which is `ACCESS EXCLUSIVE`); `ACCESS SHARE`, `ROW SHARE`, `ROW EXCLUSIVE` and `SHARE UPDATE EXCLUSIVE` do not block writers.
- `REFRESH MATERIALIZED VIEW` without `CONCURRENTLY`, `CLUSTER` / `REPACK` without `CONCURRENTLY`, `VACUUM FULL`, `REINDEX` without `CONCURRENTLY`.

The message names the relation the statement waits on. `DROP INDEX` and `REINDEX INDEX` lock the index's table (`ACCESS EXCLUSIVE` and `SHARE`), so the message names the table, and the index after it: `DROP INDEX waits for a lock on public.users (the table of public.users_email_idx)`.

### How typedpg follows `lock_timeout`

The rule tracks the setting statement by statement, the way PostgreSQL applies it:

- `SET lock_timeout = …` / `SET SESSION …` lasts for the rest of the file's session; `SET LOCAL …` lasts until the end of the current transaction, and outside a transaction block it does nothing (PostgreSQL only warns).
- `SELECT set_config('lock_timeout', '<value>', <is_local>)` counts when it is a plain top-level `SELECT` with constant arguments. An unqualified `set_config` is trusted only while no user function of that name exists.
- `RESET lock_timeout`, `RESET ALL`, `SET … TO DEFAULT`, `NULL` and any value that rounds to `0` ms (`'0'`, `0.4`, `'0.5ms'`) disable the timeout. A value the lint cannot evaluate (an expression) is taken as no timeout; a constant whose spelling it cannot parse is taken as a timeout.
- Transactions are followed: the runner's transaction around the file (unless the file's first line is `-- no-transaction`, or `use_transaction` is off), and explicit `BEGIN` / `START TRANSACTION`, `COMMIT`, `ROLLBACK`, `SAVEPOINT`, `RELEASE`, `ROLLBACK TO` and `… AND CHAIN`. A `ROLLBACK` undoes a `SET` made in the transaction; a `COMMIT` keeps a session `SET` and drops a `SET LOCAL`. Two-phase commit statements forget every proof.

The help line suggests `SET LOCAL` in a file that runs in a transaction and `SET` in one that does not.

## Examples

### Bad

```sql
-- migrations/0001_init.sql
CREATE TABLE users (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    email text,
    name text NOT NULL
);
CREATE TABLE posts (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY);

-- migrations/0002_nickname.sql
ALTER TABLE users ADD COLUMN nickname text;
ALTER TABLE posts ADD COLUMN subtitle text;
```

```text
warning[missing_lock_timeout]: ALTER TABLE waits for a lock on public.users with no lock_timeout: queued behind a long transaction, it blocks every query on the table
   ╭─[migrations/0002_nickname.sql:1:1]
 1 │ ALTER TABLE users ADD COLUMN nickname text;
   · ──────────────────────────────────────────
   ╰────
  help: start the migration with SET LOCAL lock_timeout = '5s' and retry on timeout
```

Only the first statement is reported. In a `-- no-transaction` file, `SET LOCAL` would be a no-op, so the help changes:

```sql
-- migrations/0002_nickname.sql
-- no-transaction
SET LOCAL lock_timeout = '5s';
ALTER TABLE users ADD COLUMN nickname text;
```

```text
warning[missing_lock_timeout]: ALTER TABLE waits for a lock on public.users with no lock_timeout: queued behind a long transaction, it blocks every query on the table
   ╭─[migrations/0002_nickname.sql:3:1]
 3 │ ALTER TABLE users ADD COLUMN nickname text;
   · ──────────────────────────────────────────
   ╰────
  help: start the migration with SET lock_timeout = '5s' and retry on timeout
```

### Good

```sql
-- migrations/0002_nickname.sql (runs in the runner's transaction)
SET LOCAL lock_timeout = '5s';
ALTER TABLE users ADD COLUMN nickname text;

-- migrations/0003_bio.sql
-- no-transaction
SET lock_timeout = '5s';
ALTER TABLE users ADD COLUMN bio text;
```

Not reported:

```sql
-- A table created in the same file: nobody else can be holding a lock on it.
CREATE TABLE tags (id bigint PRIMARY KEY);
ALTER TABLE tags ADD COLUMN label text;

-- SET STATISTICS only takes SHARE UPDATE EXCLUSIVE.
ALTER TABLE users ALTER COLUMN email SET STATISTICS 500;

-- Concurrent operations do not block writers.
-- (in a -- no-transaction file)
CREATE INDEX CONCURRENTLY users_name_idx ON users (name);
```

When the timeout fires, the migration fails with `canceling statement due to lock timeout`; run it again when the blocking transaction is gone.

## When it stays silent

- A non-zero `lock_timeout` is in effect at the statement (see above).
- The relation was created in the same file, or the statement only takes a lock that does not block writers.
- A statement of the file was already reported.
- The file is unchanged from `--previous-deployment`, matches `lint.ignore`, or belongs to `extra_migrations`.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
missing_lock_timeout = "error"   # "error", "warning" (default), "info" or "off"
```

```json
// typedpg.config.json
{ "lint": { "missingLockTimeout": "error" } }
```

For one statement:

```sql
-- typedpg-lint: allow(missing_lock_timeout)
ALTER TABLE users ADD COLUMN nickname text;
```

Since the rule reports at most one statement per file, allowing that statement lets the next blocking statement of the file be reported instead. See [Configuring rules](README.md#configuring-rules) for `fail_on` / `failOn` and `ignore`.

## Related rules

Every other migration rule reports a statement that also needs a timeout: [`create_index_blocking`](create_index_blocking.md), [`constraint_validating`](constraint_validating.md), [`set_not_null_scan`](set_not_null_scan.md), [`table_rewrite`](table_rewrite.md), [`drop_index_blocking`](drop_index_blocking.md), [`blocking_rebuild`](blocking_rebuild.md), [`detach_partition_blocking`](detach_partition_blocking.md), [`attach_partition_scan`](attach_partition_scan.md).
