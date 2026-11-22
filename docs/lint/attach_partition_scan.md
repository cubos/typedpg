# `attach_partition_scan`

`ALTER TABLE … ATTACH PARTITION` of an existing table whose constraints don't imply the partition bound makes PostgreSQL scan the whole table under an `ACCESS EXCLUSIVE` lock to check that every row fits it.

| | |
|---|---|
| Default level | warning |
| Applies to | migrations |
| Fix | no |

## Why

Attaching a table as a partition takes `SHARE UPDATE EXCLUSIVE` on the partitioned table but `ACCESS EXCLUSIVE` on the table being attached, and PostgreSQL must prove that every row of that table satisfies the partition's bound (`FOR VALUES FROM (1000) TO (2000)`). Unless the table's own constraints already imply the bound, it reads every row while holding that lock. Loading a big table first and attaching it afterwards is the usual way to add a partition, so this scan hits exactly the tables that are large.

PostgreSQL skips the scan when the table's constraints imply the partition constraint. Add a `CHECK` that matches the bound `NOT VALID`, validate it in an earlier migration (no blocking lock), make the key columns `NOT NULL`, and the attach is a catalog change.

## What it reports

An `ATTACH PARTITION` of a table (or partitioned table) that existed before the current file started, unless its constraints imply the partition constraint the way PostgreSQL proves it (`PartConstraintImpliedByRelConstraint`):

- The partition constraint is built from the bound as PostgreSQL builds it. A range bound `FROM (1000) TO (2000)` on `id` is `id IS NOT NULL AND id >= 1000 AND id < 2000` (several key columns give the `OR` form PostgreSQL uses; `MINVALUE` / `MAXVALUE` leave a side out); a list bound `IN ('eu', 'br')` is `region IS NOT NULL AND region IN ('eu', 'br')` (`region IS NULL OR …` when the list has `NULL`). When the partitioned table is itself a partition, its own bound is added.
- It must follow from the table's validated, enforced `CHECK` constraints and from `NOT NULL` (a valid NOT NULL constraint, not a `NOT VALID` one). A `CHECK` passes when its value is NULL, so **a `CHECK` does not prove `id IS NOT NULL`**: the key column needs a NOT NULL constraint, or the `CHECK` must say `id IS NOT NULL` itself.
- The proof is PostgreSQL's: a `CHECK (id >= 1000 AND id < 2000)`, `CHECK (id > 1200 AND id < 1500)`, `CHECK (id BETWEEN 1000 AND 1999)` or `CHECK (id IN (1000, 1001))` implies the range above; `CHECK (id > 0)`, `CHECK (id <= 2000)` or `CHECK (id::bigint >= 1000)` doesn't. Comparisons with `<`, `<=`, `=`, `>=`, `>`, `IN`, `= ANY (ARRAY[…])` and `BETWEEN`, combined with `AND` / `OR`, are read; anything else in a `CHECK` proves nothing.
- Constants of another type compare where PostgreSQL compares them: between integer types, `date` against `timestamp` (`CHECK (d >= '2024-01-01'::timestamp AND d < '2024-02-01'::timestamp)` implies `FROM ('2024-01-01') TO ('2024-02-01')` on a `date` column, a date being its midnight), and an integer constant on a `numeric` column (`CHECK (n >= 1::integer AND n < 3::integer)`). Not with `timestamptz`: those comparisons depend on the time zone, and PostgreSQL doesn't use them in the proof.
- On a boolean key, PostgreSQL folds `b = true` / `b <> false` to `b` and `b = false` / `b <> true` to `NOT b` on both sides before the proof. So `CHECK (b)` or `CHECK (b = true)` implies `FOR VALUES IN (true)`, `CHECK (NOT b)` implies `IN (false)`; but neither implies `IN (true, false)` (an `= ANY (ARRAY[…])` that isn't folded: `CHECK (b IN (true, false))` does) nor `FROM (false) TO (true)` (whose `b >= false AND b < true` only the same comparisons imply).
- A partitioned table whose own constraints don't imply the bound is fine when each of its partitions' do (PostgreSQL then checks the partitions), and has nothing to scan when it has no partitions.

A hash bound is always reported: no `CHECK` constraint implies `satisfies_hash_partition(…)`. A `DEFAULT` bound is reported when the table has other partitions: its constraint is the negation of all of theirs, which the rule doesn't try to prove.

Where the rule can't compare a `CHECK`'s constants with the bound's, it reports, although PostgreSQL may skip the scan:

- Strings under a collation that doesn't order by code point — the database's default collation (unknown to the rule) or a libc / ICU locale: the order is the locale's, which the rule doesn't reproduce. `C`, `POSIX`, `ucs_basic`, `pg_c_utf8` and `pg_unicode_fast` are compared. Equal constants always compare: a `CHECK (s >= 'a' AND s < 'm')` for `FROM ('a') TO ('m')` is proven under any collation.
- A constant cast to another type than the key's, other than the cross-type comparisons above: e.g. an integer, `float4` or `numeric` constant on a `float8` column.
- An expression key.

It also reports a key with a non-default operator class or a collation other than the column's, where PostgreSQL compares with other operators and scans too.

When the partitioned table has a default partition (that existed before the file), PostgreSQL also scans it under an `ACCESS EXCLUSIVE` lock, for rows that belong to the new partition; that is reported as a second finding. Only a `CHECK` on the default partition that refutes the new bound (and every other partition's) would let PostgreSQL skip it, which the rule doesn't try to prove.

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
  help: first add a CHECK constraint on public.measurements_2 that implies the bound, with NOT NULL on the key columns (NOT VALID, then VALIDATE CONSTRAINT in an earlier migration): PostgreSQL then skips the scan
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

`id` is `NOT NULL` already (it is the primary key), so the `CHECK` is enough. Not reported either: attaching a table created in the same file.

Still reported: a `CHECK` that doesn't imply the bound.

```sql
-- migrations/0005_measurements_3.sql
CREATE TABLE measurements_3 (id bigint NOT NULL PRIMARY KEY, taken_at timestamptz NOT NULL);

-- migrations/0006_positive.sql
SET LOCAL lock_timeout = '5s';
-- Rows below 2000 would pass it.
ALTER TABLE measurements_3 ADD CONSTRAINT positive CHECK (id > 0);  -- (also constraint_validating)

-- migrations/0007_attach.sql
SET LOCAL lock_timeout = '5s';
ALTER TABLE measurements ATTACH PARTITION measurements_3 FOR VALUES FROM (2000) TO (3000);
```

The same `CHECK (id >= 2000 AND id < 3000)` on a table whose `id` is nullable is reported too: the `CHECK` passes on NULL, so PostgreSQL would have to read every row to rule NULLs out.

## When it stays silent

- The table's validated `CHECK` and `NOT NULL` constraints imply the partition constraint (see above), or, for a partitioned table, each of its partitions' do.
- A `DEFAULT` partition attached to a table without other partitions.
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
