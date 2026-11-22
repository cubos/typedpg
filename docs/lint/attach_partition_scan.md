# `attach_partition_scan`

`ATTACH PARTITION` that scans the table being attached because its
constraints don't imply the partition bound, or that scans the default
partition.

| Level | Applies to | Fix |
|---|---|---|
| warning | migrations | — |

## Example

```sql
-- migrations/0001_init.sql
CREATE TABLE measurements (id bigint PRIMARY KEY, taken_at timestamptz NOT NULL) PARTITION BY RANGE (id);
CREATE TABLE measurements_1 PARTITION OF measurements FOR VALUES FROM (0) TO (1000);
CREATE TABLE measurements_2 (id bigint PRIMARY KEY, taken_at timestamptz NOT NULL);

-- migrations/0002_attach.sql
ALTER TABLE measurements ATTACH PARTITION measurements_2 FOR VALUES FROM (1000) TO (2000);
```

```
warning[attach_partition_scan]: ATTACH PARTITION scans every row of public.measurements_2 under an ACCESS EXCLUSIVE lock to check it fits the partition bound
   ╭─[migrations/0002_attach.sql:1:1]
 1 │ ALTER TABLE measurements ATTACH PARTITION measurements_2 FOR VALUES FROM (1000) TO (2000);
   · ─────────────────────────────────────────────────────────────────────────────────────────
   ╰────
  help: first add a CHECK constraint on public.measurements_2 that implies the bound, with NOT NULL on the key columns (NOT VALID, then VALIDATE CONSTRAINT in an earlier migration): PostgreSQL then skips the scan
```

The statement also gets a [`missing_lock_timeout`](missing_lock_timeout.md)
finding. The fix (`id` is already `NOT NULL` as the primary key):

```sql
-- migrations/0002_bound_check.sql
SET LOCAL lock_timeout = '5s';
ALTER TABLE measurements_2 ADD CONSTRAINT measurements_2_bound CHECK (id >= 1000 AND id < 2000) NOT VALID;

-- migrations/0003_validate_bound.sql
ALTER TABLE measurements_2 VALIDATE CONSTRAINT measurements_2_bound;

-- migrations/0004_attach.sql
SET LOCAL lock_timeout = '5s';
ALTER TABLE measurements ATTACH PARTITION measurements_2 FOR VALUES FROM (1000) TO (2000);
ALTER TABLE measurements_2 DROP CONSTRAINT measurements_2_bound;
```

## Why

PostgreSQL must check that every row of the attached table fits the bound,
and reads the whole table under `ACCESS EXCLUSIVE` unless the table's own
constraints already prove it. Loading a table and then attaching it is the
usual way to add a partition, so the scan hits exactly the large tables.

## Reported when

The attached table was created by an earlier migration file, and its
validated `CHECK` and `NOT NULL` constraints don't imply the partition
constraint:

- A range bound `FROM (1000) TO (2000)` on `id` needs `id >= 1000`,
  `id < 2000` and `id IS NOT NULL`; a list bound `IN ('eu', 'br')` needs
  `region IN ('eu', 'br')` and `region IS NOT NULL`. When the parent is
  itself a partition, its bound is required too.
- A `CHECK` passes on `NULL`, so it doesn't prove `IS NOT NULL`: the key
  needs `NOT NULL`, or the `CHECK` must say `id IS NOT NULL` itself.
- Implied: `CHECK (id >= 1000 AND id < 2000)`, `CHECK (id BETWEEN 1000 AND
  1999)`, `CHECK (id IN (1000, 1001))`, a narrower range. Not implied:
  `CHECK (id > 0)`, a `NOT VALID` check, `CHECK (id::numeric >= 1000 …)`.
- Comparisons (`<`, `<=`, `=`, `>=`, `>`), `IN`, `= ANY (ARRAY[…])` and
  `BETWEEN`, combined with `AND` / `OR`, count; anything else proves
  nothing.
- Constants of another type are compared between integer types, `date`
  against `timestamp`, and an integer on a `numeric` column. Not against
  `timestamptz` (it depends on the time zone), and not a constant cast to
  another type, e.g. `f >= 1::float4` on a `float8` column.
- Strings are compared only under `C`, `POSIX`, `ucs_basic`, `pg_c_utf8` or
  `pg_unicode_fast` collations; under the default or a locale collation only
  a `CHECK` with exactly the bound's constants is proven.
- Boolean keys: `CHECK (b)` implies `IN (true)` and `CHECK (NOT b)` implies
  `IN (false)`, but `IN (true, false)` needs `CHECK (b IN (true, false))`.
- Always reported: a hash bound (no `CHECK` can imply it), a `DEFAULT` bound
  when the parent has other partitions, an expression key, and a key with a
  non-default operator class or a collation other than the column's.

Each case has its own help line; for a hash bound:

```
  help: no CHECK constraint can imply a hash partition's bound: attach public.h1 while it is empty, or create the partition with PARTITION OF and copy the rows into it
```

When the parent has a default partition created by an earlier file,
PostgreSQL also scans it for rows that belong to the new partition, and that
is a second finding:

```
warning[attach_partition_scan]: ATTACH PARTITION scans every row of the default partition public.e_def under an ACCESS EXCLUSIVE lock, for rows that belong to public.e1
```

## Not reported

- A table created earlier in the same file.
- A partitioned table with no partitions, or whose partitions' constraints
  each imply the bound.
- A `DEFAULT` partition attached to a parent with no other partitions.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
attach_partition_scan = "error"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "attachPartitionScan": "error" } }
```

Allow one statement with `-- typedpg-lint: allow(attach_partition_scan)`
before it in the migration file.
See [Configuring rules](README.md#configuring-rules).

## Related

- [`detach_partition_blocking`](detach_partition_blocking.md): the other
  direction.
- [`constraint_validating`](constraint_validating.md): adding the `CHECK`
  without a blocking scan.
- [`validate_in_same_transaction`](validate_in_same_transaction.md): a
  `VALIDATE` in the transaction that added the constraint.
- [`missing_lock_timeout`](missing_lock_timeout.md): the same statement
  waiting for its lock with no timeout.
