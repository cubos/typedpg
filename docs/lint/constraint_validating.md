# `constraint_validating`

A migration adds a `CHECK` or `FOREIGN KEY` constraint to an existing table and makes PostgreSQL check every existing row while holding a lock that blocks writes.

| | |
|---|---|
| Default level | warning |
| Applies to | migrations |
| Fix | no |

## Why

`ALTER TABLE … ADD CONSTRAINT` validates the rows already in the table before it commits:

- A `CHECK` constraint is added under an `ACCESS EXCLUSIVE` lock, which blocks reads and writes, and the table is scanned while that lock is held.
- A foreign key takes `SHARE ROW EXCLUSIVE` on both the referencing and the referenced table, which blocks writes to both, and looks up every referencing row in the referenced table.

On a large table this is a long outage for every writer, and in a transactional migration the lock lasts until the transaction commits.

PostgreSQL splits the work in two when you ask for it: `ADD CONSTRAINT … NOT VALID` only records the constraint (a short lock; new and updated rows are checked from then on), and a later `ALTER TABLE … VALIDATE CONSTRAINT` scans the old rows under `SHARE UPDATE EXCLUSIVE`, which does not block reads or writes.

## What it reports

On a table that existed before the current migration file started, an `ALTER TABLE` that:

- adds a `CHECK` or `FOREIGN KEY` constraint without `NOT VALID`, or
- adds a column with an inline `CHECK` or `REFERENCES` constraint, when PostgreSQL will validate it.

The second case follows PostgreSQL's own decision (`transformAlterTableStmt`): a foreign key on a newly added column holds only NULLs, so PostgreSQL skips its validation — but only when no column added by the same statement has a default (an explicit `DEFAULT`, even `DEFAULT NULL`, a `serial` type, or a generated expression; an identity column does not count) and the statement adds no other foreign key. One defaulted column makes every foreign key of the statement scan. An inline `CHECK` on a new column is always reported. `ADD COLUMN IF NOT EXISTS` on a column that already exists adds nothing and is not reported.

As with the other migration rules, every table created by an earlier migration file is assumed to hold rows, and tables created earlier in the same file are assumed empty.

## Examples

### Bad

```sql
-- migrations/0001_init.sql
CREATE TABLE users (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    email text,
    name text NOT NULL
);
CREATE TABLE posts (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    author_id bigint NOT NULL,
    title varchar(200) NOT NULL,
    views integer NOT NULL DEFAULT 0
);
CREATE INDEX posts_author_id_idx ON posts (author_id);

-- migrations/0002_constraints.sql
ALTER TABLE posts ADD CONSTRAINT posts_author_id_fkey
    FOREIGN KEY (author_id) REFERENCES users (id);
ALTER TABLE posts ADD CONSTRAINT posts_title_not_empty CHECK (title <> '');
ALTER TABLE posts ADD COLUMN editor_id bigint DEFAULT 1 REFERENCES users (id);
```

```text
warning[constraint_validating]: adding a constraint validates every row of public.posts under a lock
   ╭─[migrations/0002_constraints.sql:1:1]
 1 │ ALTER TABLE posts ADD CONSTRAINT posts_author_id_fkey
   · ─────────────────────────────────────────────────────
 2 │     FOREIGN KEY (author_id) REFERENCES users (id);
   · ─────────────────────────────────────────────────
   ╰────
  help: add it NOT VALID, then VALIDATE CONSTRAINT in a later migration

warning[constraint_validating]: adding a constraint validates every row of public.posts under a lock
   ╭─[migrations/0002_constraints.sql:3:1]
 3 │ ALTER TABLE posts ADD CONSTRAINT posts_title_not_empty CHECK (title <> '');
   · ──────────────────────────────────────────────────────────────────────────
   ╰────
  help: add it NOT VALID, then VALIDATE CONSTRAINT in a later migration

warning[constraint_validating]: adding a constraint validates every row of public.posts under a lock
   ╭─[migrations/0002_constraints.sql:4:1]
 4 │ ALTER TABLE posts ADD COLUMN editor_id bigint DEFAULT 1 REFERENCES users (id);
   · ─────────────────────────────────────────────────────────────────────────────
   ╰────
  help: add it NOT VALID, then VALIDATE CONSTRAINT in a later migration
```

The third statement's `DEFAULT 1` fills every existing row, so the new foreign key has values to check. The file also gets [`missing_lock_timeout`](missing_lock_timeout.md) and [`fk_without_index`](fk_without_index.md) findings (not shown).

### Good

Add the constraints `NOT VALID`, then validate them in a later migration:

```sql
-- migrations/0002_add_constraints.sql
SET LOCAL lock_timeout = '5s';
ALTER TABLE posts ADD CONSTRAINT posts_author_id_fkey
    FOREIGN KEY (author_id) REFERENCES users (id) NOT VALID;
ALTER TABLE posts ADD CONSTRAINT posts_title_not_empty CHECK (title <> '') NOT VALID;
-- A new column without a DEFAULT holds only NULLs: PostgreSQL skips the check.
ALTER TABLE posts ADD COLUMN editor_id bigint REFERENCES users (id);

-- migrations/0003_validate_constraints.sql
ALTER TABLE posts VALIDATE CONSTRAINT posts_author_id_fkey;
ALTER TABLE posts VALIDATE CONSTRAINT posts_title_not_empty;
```

The validation must not run in the transaction that added the constraint: there the `ADD CONSTRAINT` lock is still held and the scan blocks writes anyway ([`validate_in_same_transaction`](validate_in_same_transaction.md) reports that). A separate migration file, or a `-- no-transaction` file where each statement commits on its own, avoids it.

Also not reported: a constraint added to a table created in the same file.

## When it stays silent

- The constraint is `NOT VALID`.
- The column is new, has a foreign key and nothing in the statement makes PostgreSQL validate it (see above).
- The table was created in the same file, or the file is unchanged from `--previous-deployment`, matches `lint.ignore`, or belongs to `extra_migrations`.
- `PRIMARY KEY`, `UNIQUE` and `EXCLUDE` constraints build an index rather than validate rows: [`create_index_blocking`](create_index_blocking.md) reports them.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
constraint_validating = "error"   # "error", "warning" (default), "info" or "off"
```

```json
// typedpg.config.json
{ "lint": { "constraintValidating": "error" } }
```

For one statement:

```sql
-- typedpg-lint: allow(constraint_validating)
ALTER TABLE posts ADD CONSTRAINT posts_title_not_empty CHECK (title <> '');
```

See [Configuring rules](README.md#configuring-rules) for `fail_on` / `failOn` and `ignore`.

## Related rules

- [`validate_in_same_transaction`](validate_in_same_transaction.md): the `VALIDATE` that undoes the split.
- [`set_not_null_scan`](set_not_null_scan.md): the same scan for `SET NOT NULL`.
- [`attach_partition_scan`](attach_partition_scan.md): a `CHECK` that lets `ATTACH PARTITION` skip its scan.
- [`missing_lock_timeout`](missing_lock_timeout.md)
