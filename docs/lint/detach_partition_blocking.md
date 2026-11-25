# `detach_partition_blocking`

`ALTER TABLE … DETACH PARTITION` without `CONCURRENTLY` on a partitioned
table that already has rows.

| Category | Level | Applies to | Fix |
|---|---|---|---|
| locking | warning | migrations | — |

## Example

```sql
-- migrations/0001_init.sql
CREATE TABLE measurements (id bigint PRIMARY KEY, taken_at timestamptz NOT NULL) PARTITION BY RANGE (id);
CREATE TABLE measurements_1 PARTITION OF measurements FOR VALUES FROM (0) TO (1000);

-- migrations/0002_detach.sql
ALTER TABLE measurements DETACH PARTITION measurements_1;
```

```
warning[locking/detach_partition_blocking]: DETACH PARTITION without CONCURRENTLY takes an ACCESS EXCLUSIVE lock on public.measurements, blocking every query on the partitioned table
   ╭─[migrations/0002_detach.sql:1:1]
 1 │ ALTER TABLE measurements DETACH PARTITION measurements_1;
   · ────────────────────────────────────────────────────────
   ╰────
  help: use DETACH PARTITION … CONCURRENTLY, in a migration whose first line is -- no-transaction
```

The statement also gets a [`lock_without_timeout`](lock_without_timeout.md)
finding. The fix:

```sql
-- migrations/0002_detach.sql
-- no-transaction
SET lock_timeout = '5s';
ALTER TABLE measurements DETACH PARTITION measurements_1 CONCURRENTLY;
```

If a concurrent detach is interrupted, `DETACH PARTITION … FINALIZE`
completes it.

## Why

Every query on the partitioned table waits, whichever partition it reads,
and everything queued behind it waits too. Detaching an old partition is
routine maintenance; done this way it stops the whole table.

## Reported when

The partitioned table was created by an earlier migration file.

When it has a default partition, PostgreSQL refuses `CONCURRENTLY`, and the
help says so instead:

```
  help: CONCURRENTLY is not allowed while public.ev has a default partition: set a short lock_timeout and retry the detach on timeout
```

## Not reported

- `DETACH PARTITION … CONCURRENTLY` and `… FINALIZE`.
- A partitioned table created earlier in the same file.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
detach_partition_blocking = "error"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "detachPartitionBlocking": "error" } }
```

Allow one statement with `-- typedpg-lint: allow(detach_partition_blocking)`
before it in the migration file, e.g. after a short `SET LOCAL lock_timeout`
when there is a default partition.
See [Configuring rules](README.md#configuring-rules).

## Related

- [`attach_partition_scan`](attach_partition_scan.md): the other direction.
- [`lock_without_timeout`](lock_without_timeout.md): the same statement
  waiting for its lock with no timeout.
