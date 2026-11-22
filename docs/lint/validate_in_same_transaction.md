# `validate_in_same_transaction`

`VALIDATE CONSTRAINT` in the transaction that added the constraint
`NOT VALID`.

| Level | Applies to | Fix |
|---|---|---|
| warning | migrations | — |

## Example

```sql
-- migrations/0001_init.sql
CREATE TABLE posts (id bigint PRIMARY KEY, title text NOT NULL, views integer NOT NULL DEFAULT 0);

-- migrations/0002_positive_views.sql
ALTER TABLE posts ADD CONSTRAINT posts_views_positive CHECK (views >= 0) NOT VALID;
ALTER TABLE posts VALIDATE CONSTRAINT posts_views_positive;
```

```
warning[validate_in_same_transaction]: VALIDATE CONSTRAINT posts_views_positive runs in the transaction that added it NOT VALID: ADD CONSTRAINT's lock on public.posts is still held, so the scan blocks writes on the table
   ╭─[migrations/0002_positive_views.sql:2:1]
 2 │ ALTER TABLE posts VALIDATE CONSTRAINT posts_views_positive;
   · ──────────────────────────────────────────────────────────
   ╰────
  help: validate it in a later migration (or after a COMMIT), where VALIDATE CONSTRAINT only takes a SHARE UPDATE EXCLUSIVE lock
```

## Why

`NOT VALID` then `VALIDATE` avoids blocking writes only when they run in
separate transactions. Locks are held until the transaction ends, so in one
transaction the scan runs under `ADD CONSTRAINT`'s lock (`ACCESS EXCLUSIVE`
for a `CHECK`, `SHARE ROW EXCLUSIVE` for a foreign key): the outage the
split was meant to avoid.

## Reported when

The constraint was added `NOT VALID` earlier in the same open transaction,
or earlier in the same `ALTER TABLE`. The transaction is followed as the
runner runs the file: the whole file is one transaction unless its first
line is `-- no-transaction` or `use_transaction` is off; explicit `BEGIN`,
`COMMIT` and `ROLLBACK` (also `AND CHAIN`) are honored.

## Not reported

- Validation in a later migration, or after a `COMMIT`.
- A `-- no-transaction` file, where each statement commits on its own:

  ```sql
  -- no-transaction
  SET lock_timeout = '5s';
  ALTER TABLE posts ADD CONSTRAINT posts_title_not_empty CHECK (title <> '') NOT VALID;
  ALTER TABLE posts VALIDATE CONSTRAINT posts_title_not_empty;
  ```
- A table created in the same file.
- Migrations already deployed (unchanged from `--previous-deployment`),
  ignored ones, and `extra_migrations`.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
validate_in_same_transaction = "error"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "validateInSameTransaction": "error" } }
```

Allow one statement with
`-- typedpg-lint: allow(validate_in_same_transaction)` before it in the
migration. See [Configuring rules](README.md#configuring-rules).

## Related

- [`constraint_validating`](constraint_validating.md): why to split
  `NOT VALID` and `VALIDATE`.
- [`set_not_null_scan`](set_not_null_scan.md): the same split proves a
  column `NOT NULL`.
- [`missing_lock_timeout`](missing_lock_timeout.md): a blocking lock without
  `lock_timeout`.
