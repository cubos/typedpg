# `set_not_null_scan`

`ALTER COLUMN … SET NOT NULL` on an existing table, with nothing that proves the column has no NULLs, scans every row under an `ACCESS EXCLUSIVE` lock.

| | |
|---|---|
| Default level | warning |
| Applies to | migrations |
| Fix | no |

## Why

`SET NOT NULL` takes `ACCESS EXCLUSIVE` on the table — no reads, no writes — and then reads every row to make sure none is NULL. The lock lasts as long as the scan, and in a transactional migration until the commit.

PostgreSQL skips the scan when a valid `CHECK (column IS NOT NULL)` constraint already proves it. That constraint can be added without a long lock: add it `NOT VALID`, then `VALIDATE CONSTRAINT` it in a later migration (a scan under `SHARE UPDATE EXCLUSIVE`, which blocks neither reads nor writes). The `SET NOT NULL` that follows then only updates the catalog, and the helper `CHECK` can be dropped.

## What it reports

`ALTER TABLE … ALTER COLUMN c SET NOT NULL` on a table that existed before the current migration file started, unless:

- the column is already `NOT NULL`, or
- the table has a `CHECK` constraint that is validated, enforced and not `NO INHERIT`, whose expression is `c IS NOT NULL` or an `AND` with `c IS NOT NULL` among its top-level terms.

A `NOT VALID`, `NOT ENFORCED` or `NO INHERIT` check proves nothing, and neither does a check written differently (`NOT c IS NULL`, `c IS NOT NULL OR …`). The help line names every column of the statement that needs the check.

## Examples

### Bad

```sql
-- migrations/0001_init.sql
CREATE TABLE users (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    email text,
    name text NOT NULL
);

-- migrations/0002_email_not_null.sql
ALTER TABLE users ALTER COLUMN email SET NOT NULL;
```

```text
warning[set_not_null_scan]: SET NOT NULL scans every row of public.users under a lock
   ╭─[migrations/0002_email_not_null.sql:1:1]
 1 │ ALTER TABLE users ALTER COLUMN email SET NOT NULL;
   · ─────────────────────────────────────────────────
   ╰────
  help: add CHECK (email IS NOT NULL) NOT VALID, VALIDATE it, then SET NOT NULL
```

The statement also gets a [`missing_lock_timeout`](missing_lock_timeout.md) finding (not shown).

### Good

Three migrations: add the check without scanning, validate it without blocking writes, then set the column `NOT NULL` from the proof.

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

Not reported either: `SET NOT NULL` on a column that is already `NOT NULL`, or on a table created in the same file.

## When it stays silent

- A valid check proves the column is never NULL (see above), or the column is already `NOT NULL`.
- The table was created in the same file, or the file is unchanged from `--previous-deployment`, matches `lint.ignore`, or belongs to `extra_migrations`.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
set_not_null_scan = "error"   # "error", "warning" (default), "info" or "off"
```

```json
// typedpg.config.json
{ "lint": { "setNotNullScan": "error" } }
```

For one statement:

```sql
-- typedpg-lint: allow(set_not_null_scan)
ALTER TABLE users ALTER COLUMN email SET NOT NULL;
```

See [Configuring rules](README.md#configuring-rules) for `fail_on` / `failOn` and `ignore`.

## Related rules

- [`constraint_validating`](constraint_validating.md): the `NOT VALID` / `VALIDATE` split for any check.
- [`validate_in_same_transaction`](validate_in_same_transaction.md): validating in the file that added the check.
- [`missing_lock_timeout`](missing_lock_timeout.md)
