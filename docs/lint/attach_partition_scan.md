# `attach_partition_scan`

`ALTER TABLE … ATTACH PARTITION` of an existing table with no `CHECK` constraint on the partition key makes PostgreSQL scan the whole table under an `ACCESS EXCLUSIVE` lock to check that every row fits the partition bound.

| | |
|---|---|
| Default level | warning |
| Applies to | migrations |
| Fix | no |

## Why

Attaching a table as a partition takes `SHARE UPDATE EXCLUSIVE` on the partitioned table but `ACCESS EXCLUSIVE` on the table being attached, and PostgreSQL must prove that every row of that table satisfies the partition's bound (`FOR VALUES FROM (1000) TO (2000)`). Unless the table's own constraints already imply the bound, it reads every row while holding that lock. Loading a big table first and attaching it afterwards is the usual way to add a partition, so this scan hits exactly the tables that are large.

PostgreSQL skips the scan when a valid `CHECK` constraint implies the partition constraint. Add one that matches the bound `NOT VALID`, validate it in an earlier migration (no blocking lock), and the attach is a catalog change.

## What it reports

An `ATTACH PARTITION` of a table (or partitioned table) that existed before the current file started, when that table has no `CHECK` constraint that is validated, enforced and reads one of the partition key's columns.

typedpg does not build the partition constraint from the bound to prove the implication: any valid, enforced `CHECK` that reads a key column counts. A `CHECK (id > 0)` on a partition bound to `FROM (1000) TO (2000)` silences the rule although PostgreSQL will still scan. A `NOT VALID` or `NOT ENFORCED` check, or one on another column, does not count.

When the partitioned table has a default partition, PostgreSQL also scans the default partition (under lock) for rows that belong to the new one; the rule does not model that.

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
CREATE TABLE measurements_2 (
    id bigint NOT NULL PRIMARY KEY,
    taken_at timestamptz NOT NULL
);

-- migrations/0002_attach.sql
ALTER TABLE measurements ATTACH PARTITION measurements_2
    FOR VALUES FROM (1000) TO (2000);
```

```text
warning[attach_partition_scan]: ATTACH PARTITION scans every row of public.measurements_2 under an ACCESS EXCLUSIVE lock to check it fits the partition bound
   ╭─[migrations/0002_attach.sql:1:1]
 1 │ ALTER TABLE measurements ATTACH PARTITION measurements_2
   · ────────────────────────────────────────────────────────
 2 │     FOR VALUES FROM (1000) TO (2000);
   · ────────────────────────────────────
   ╰────
  help: first add a CHECK constraint on public.measurements_2 that matches the bound (NOT VALID, then VALIDATE CONSTRAINT in an earlier migration): PostgreSQL then skips the scan
```

The statement also gets a [`missing_lock_timeout`](missing_lock_timeout.md) finding on `measurements_2` (not shown).

### Good

```sql
-- migrations/0002_bound_check.sql
SET LOCAL lock_timeout = '5s';
ALTER TABLE measurements_2 ADD CONSTRAINT measurements_2_bound
    CHECK (id >= 1000 AND id < 2000) NOT VALID;

-- migrations/0003_validate_bound.sql
ALTER TABLE measurements_2 VALIDATE CONSTRAINT measurements_2_bound;

-- migrations/0004_attach.sql
SET LOCAL lock_timeout = '5s';
ALTER TABLE measurements ATTACH PARTITION measurements_2
    FOR VALUES FROM (1000) TO (2000);
-- The partition constraint now does the check's job.
ALTER TABLE measurements_2 DROP CONSTRAINT measurements_2_bound;
```

Not reported either: attaching a table created in the same file.

## When it stays silent

- The table to attach has a valid, enforced `CHECK` reading a key column — whether or not it implies the bound (see above).
- The table was created in the same file, or the file is unchanged from `--previous-deployment`, matches `lint.ignore`, or belongs to `extra_migrations`.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
attach_partition_scan = "error"   # "error", "warning" (default), "info" or "off"
```

```json
// typedpg.config.json
{ "lint": { "attachPartitionScan": "error" } }
```

For one statement:

```sql
-- typedpg-lint: allow(attach_partition_scan)
ALTER TABLE measurements ATTACH PARTITION measurements_2
    FOR VALUES FROM (1000) TO (2000);
```

See [Configuring rules](README.md#configuring-rules) for `fail_on` / `failOn` and `ignore`.

## Related rules

- [`detach_partition_blocking`](detach_partition_blocking.md)
- [`constraint_validating`](constraint_validating.md), [`validate_in_same_transaction`](validate_in_same_transaction.md): adding the check without a blocking scan.
- [`missing_lock_timeout`](missing_lock_timeout.md)
