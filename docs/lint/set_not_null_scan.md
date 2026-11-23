# `set_not_null_scan`

`ALTER COLUMN … SET NOT NULL` that scans every row of an existing table
under an `ACCESS EXCLUSIVE` lock.

| Level | Applies to | Fix |
|---|---|---|
| warning | migrations | — |

## Example

```sql
-- migrations/0001_init.sql
CREATE TABLE users (id bigint PRIMARY KEY, email text, name text);

-- migrations/0002_email_not_null.sql
ALTER TABLE users ALTER COLUMN email SET NOT NULL, ALTER COLUMN name SET NOT NULL;
```

```
warning[set_not_null_scan]: SET NOT NULL scans every row of public.users under a lock
   ╭─[migrations/0002_email_not_null.sql:1:1]
 1 │ ALTER TABLE users ALTER COLUMN email SET NOT NULL, ALTER COLUMN name SET NOT NULL;
   · ─────────────────────────────────────────────────────────────────────────────────
   ╰────
  help: add CHECK (email IS NOT NULL AND name IS NOT NULL) NOT VALID, VALIDATE it, then SET NOT NULL
```

Instead, in three migrations:

```sql
-- migrations/0002_email_check.sql
SET LOCAL lock_timeout = '5s';
ALTER TABLE users ADD CONSTRAINT users_email_not_null
    CHECK (email IS NOT NULL) NOT VALID;

-- migrations/0003_email_validate.sql
ALTER TABLE users VALIDATE CONSTRAINT users_email_not_null;

-- migrations/0004_email_not_null.sql
SET LOCAL lock_timeout = '5s';
ALTER TABLE users ALTER COLUMN email SET NOT NULL;
ALTER TABLE users DROP CONSTRAINT users_email_not_null;
```

## Why

`SET NOT NULL` blocks every read and write of the table while it checks each
row, and in a transactional migration until the commit. PostgreSQL skips
the scan when a validated `CHECK (col IS NOT NULL)` already proves it; that
check can be validated without blocking writes.

## Reported when

`SET NOT NULL` (or `ADD CONSTRAINT … NOT NULL column` without `NOT VALID`,
or `ADD PRIMARY KEY USING INDEX`, which makes the index's columns NOT NULL)
on a table that existed before the migration file started,
for a column not yet `NOT NULL` and without a `CHECK` that is:

- validated (not `NOT VALID`) and enforced;
- exactly `col IS NOT NULL`, or an `AND` with `col IS NOT NULL` as one of its
  terms. `NOT col IS NULL` or `col IS NOT NULL OR …` don't count.

`ADD COLUMN … GENERATED ALWAYS AS (…) VIRTUAL NOT NULL` on such a table is
reported too: nothing stores the column, so PostgreSQL computes it for every
row to check it, under the same lock. Add the column without `NOT NULL`,
then follow the three migrations above.

The children of an inheritance parent (unless the statement says `ONLY`)
and the partitions of a partitioned table are checked the same way: a
`NO INHERIT` check proves the column on its own table only, and a
partitioned table whose partitions each have such a check is not
reported.

The help names every column of the statement that needs the check.

## Not reported

- A table created in the same file.
- A column already `NOT NULL`.
- Migrations already deployed (unchanged from `--previous-deployment`),
  ignored ones, and `extra_migrations`.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
set_not_null_scan = "error"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "setNotNullScan": "error" } }
```

Allow one statement with `-- typedpg-lint: allow(set_not_null_scan)` before
it in the migration. See [Configuring rules](README.md#configuring-rules).

## Related

- [`constraint_validating`](constraint_validating.md): the `NOT VALID` /
  `VALIDATE` split for any check.
- [`validate_in_same_transaction`](validate_in_same_transaction.md):
  validating in the transaction that added the check.
- [`missing_lock_timeout`](missing_lock_timeout.md): a blocking lock without
  `lock_timeout`.
