# `validate_in_same_transaction`

`ALTER TABLE … VALIDATE CONSTRAINT` runs in the same transaction that added the constraint `NOT VALID`, so the validation scan runs under the `ADD CONSTRAINT` lock and blocks writes after all.

| | |
|---|---|
| Default level | warning |
| Applies to | migrations |
| Fix | no |

## Why

Adding a constraint `NOT VALID` and validating it later is how you add a `CHECK` or a foreign key to a big table without an outage ([`constraint_validating`](constraint_validating.md)): `ADD CONSTRAINT … NOT VALID` takes its lock only briefly, and `VALIDATE CONSTRAINT` scans the old rows under `SHARE UPDATE EXCLUSIVE`, which blocks neither reads nor writes.

But PostgreSQL holds a lock until the transaction ends. If both statements run in one transaction — the same migration file under the runner's transaction, or one explicit `BEGIN … COMMIT` — the `ADD CONSTRAINT` lock (`ACCESS EXCLUSIVE` for a `CHECK`, `SHARE ROW EXCLUSIVE` for a foreign key) is still held while `VALIDATE` scans. The result is exactly the long blocking scan the split was meant to avoid, with a migration that looks correct.

## What it reports

A `VALIDATE CONSTRAINT c` on a table that existed before the current file started, where `c` was added `NOT VALID` in the transaction that is still open: earlier in the same transaction, or earlier in the same `ALTER TABLE` statement.

typedpg follows the transaction: the runner's transaction around the whole file (unless the first line is `-- no-transaction` or `use_transaction` is off), explicit `BEGIN` / `START TRANSACTION`, and `COMMIT` / `ROLLBACK` (with or without `AND CHAIN`), after which the constraint no longer counts. This tracking runs even when [`missing_lock_timeout`](missing_lock_timeout.md) is off.

## Examples

### Bad

```sql
-- migrations/0001_init.sql
CREATE TABLE posts (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    title varchar(200) NOT NULL,
    views integer NOT NULL DEFAULT 0
);

-- migrations/0002_positive_views.sql
ALTER TABLE posts ADD CONSTRAINT posts_views_positive CHECK (views >= 0) NOT VALID;
ALTER TABLE posts VALIDATE CONSTRAINT posts_views_positive;
```

```text
warning[validate_in_same_transaction]: VALIDATE CONSTRAINT posts_views_positive runs in the transaction that added it NOT VALID: ADD CONSTRAINT's lock on public.posts is still held, so the scan blocks writes on the table
   ╭─[migrations/0002_positive_views.sql:2:1]
 2 │ ALTER TABLE posts VALIDATE CONSTRAINT posts_views_positive;
   · ──────────────────────────────────────────────────────────
   ╰────
  help: validate it in a later migration (or after a COMMIT), where VALIDATE CONSTRAINT only takes a SHARE UPDATE EXCLUSIVE lock
```

The first statement also gets a [`missing_lock_timeout`](missing_lock_timeout.md) finding (not shown).

### Good

Validate in the next migration:

```sql
-- migrations/0002_positive_views.sql
SET LOCAL lock_timeout = '5s';
ALTER TABLE posts ADD CONSTRAINT posts_views_positive CHECK (views >= 0) NOT VALID;

-- migrations/0003_validate_positive_views.sql
ALTER TABLE posts VALIDATE CONSTRAINT posts_views_positive;
```

Or in a `-- no-transaction` file, where each statement commits on its own:

```sql
-- migrations/0004_title_not_empty.sql
-- no-transaction
SET lock_timeout = '5s';
ALTER TABLE posts ADD CONSTRAINT posts_title_not_empty CHECK (title <> '') NOT VALID;
ALTER TABLE posts VALIDATE CONSTRAINT posts_title_not_empty;
```

Also not reported: validating a constraint on a table created in the same file, and a `VALIDATE` after a `COMMIT` that ended the adding transaction.

## When it stays silent

- The constraint was added in an earlier transaction (an earlier file, or before a `COMMIT`), or was not added `NOT VALID`.
- The table was created in the same file.
- The file is unchanged from `--previous-deployment`, matches `lint.ignore`, or belongs to `extra_migrations`.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
validate_in_same_transaction = "error"   # "error", "warning" (default), "info" or "off"
```

```json
// typedpg.config.json
{ "lint": { "validateInSameTransaction": "error" } }
```

For one statement:

```sql
-- typedpg-lint: allow(validate_in_same_transaction)
ALTER TABLE posts VALIDATE CONSTRAINT posts_views_positive;
```

See [Configuring rules](README.md#configuring-rules) for `fail_on` / `failOn` and `ignore`.

## Related rules

- [`constraint_validating`](constraint_validating.md): why the split exists.
- [`set_not_null_scan`](set_not_null_scan.md): the same split proves a column `NOT NULL`.
- [`attach_partition_scan`](attach_partition_scan.md)
