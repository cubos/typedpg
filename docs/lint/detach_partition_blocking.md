# `detach_partition_blocking`

`ALTER TABLE … DETACH PARTITION` without `CONCURRENTLY` on an existing partitioned table takes an `ACCESS EXCLUSIVE` lock on it, blocking every query on every partition.

| | |
|---|---|
| Default level | warning |
| Applies to | migrations |
| Fix | no |

## Why

A plain `DETACH PARTITION` locks both the partitioned table and the partition with `ACCESS EXCLUSIVE`. Every query on the partitioned table — whichever partition it would read — waits for the lock, and everything queued behind it waits too. Detaching an old partition to archive it is routine maintenance, and doing it this way stops the whole table.

`DETACH PARTITION … CONCURRENTLY` runs in two transactions and only takes `SHARE UPDATE EXCLUSIVE` on the partitioned table, so queries keep running. It cannot run inside a transaction block (put it in a migration whose first line is `-- no-transaction`), and PostgreSQL refuses it while the partitioned table has a default partition. If it is interrupted, `ALTER TABLE … DETACH PARTITION … FINALIZE` completes it.

## What it reports

An `ALTER TABLE` with a `DETACH PARTITION` subcommand without `CONCURRENTLY`, on a partitioned table that existed before the current file started.

When the partitioned table has a default partition, `CONCURRENTLY` is not an option, and the help says so: the remaining defense is a short `lock_timeout` and a retry.

## Examples

### Bad

```sql
-- migrations/0001_init.sql
CREATE TABLE measurements (
    id bigint NOT NULL,
    taken_at timestamptz NOT NULL,
    PRIMARY KEY (id)
) PARTITION BY RANGE (id);
CREATE TABLE measurements_1 PARTITION OF measurements FOR VALUES FROM (0) TO (1000);

-- migrations/0002_detach.sql
ALTER TABLE measurements DETACH PARTITION measurements_1;
```

```text
warning[detach_partition_blocking]: DETACH PARTITION without CONCURRENTLY takes an ACCESS EXCLUSIVE lock on public.measurements, blocking every query on the partitioned table
   ╭─[migrations/0002_detach.sql:1:1]
 1 │ ALTER TABLE measurements DETACH PARTITION measurements_1;
   · ────────────────────────────────────────────────────────
   ╰────
  help: use DETACH PARTITION … CONCURRENTLY, in a migration whose first line is -- no-transaction
```

With a default partition (`CREATE TABLE measurements_other PARTITION OF measurements DEFAULT;` in `0001_init.sql`), the same statement gets another help line:

```text
  help: CONCURRENTLY is not allowed while public.measurements has a default partition: set a short lock_timeout and retry the detach on timeout
```

The statement also gets a [`missing_lock_timeout`](missing_lock_timeout.md) finding (not shown).

### Good

```sql
-- migrations/0002_detach.sql
-- no-transaction
SET lock_timeout = '5s';
ALTER TABLE measurements DETACH PARTITION measurements_1 CONCURRENTLY;
```

Not reported: detaching from a partitioned table created in the same file.

## When it stays silent

- `CONCURRENTLY` or `FINALIZE` is used, or the partitioned table was created in the same file.
- The file is unchanged from `--previous-deployment`, matches `lint.ignore`, or belongs to `extra_migrations`.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
detach_partition_blocking = "error"   # "error", "warning" (default), "info" or "off"
```

```json
// typedpg.config.json
{ "lint": { "detachPartitionBlocking": "error" } }
```

For one statement (for example, with a default partition, once a `lock_timeout` is set):

```sql
SET LOCAL lock_timeout = '2s';
-- typedpg-lint: allow(detach_partition_blocking)
ALTER TABLE measurements DETACH PARTITION measurements_1;
```

See [Configuring rules](README.md#configuring-rules) for `fail_on` / `failOn` and `ignore`.

## Related rules

- [`attach_partition_scan`](attach_partition_scan.md): the other direction.
- [`missing_lock_timeout`](missing_lock_timeout.md)
