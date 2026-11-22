# `constraint_validating`

A `CHECK` or foreign key added to a table that already has rows, which
PostgreSQL validates against every row under a lock.

| Level | Applies to | Fix |
|---|---|---|
| warning | migrations | — |

## Example

```sql
-- migrations/0001_init.sql
CREATE TABLE users (id bigint PRIMARY KEY, name text NOT NULL);
CREATE TABLE posts (id bigint PRIMARY KEY, author_id bigint NOT NULL, title text NOT NULL);
CREATE INDEX posts_author_id_idx ON posts (author_id);

-- migrations/0002_constraints.sql
ALTER TABLE posts ADD CONSTRAINT posts_author_id_fkey FOREIGN KEY (author_id) REFERENCES users (id);
ALTER TABLE posts ADD CONSTRAINT posts_title_not_empty CHECK (title <> '');
ALTER TABLE posts ADD COLUMN editor_id bigint DEFAULT 1 REFERENCES users (id);
```

```
warning[constraint_validating]: adding a constraint validates every row of public.posts under a lock
   ╭─[migrations/0002_constraints.sql:1:1]
 1 │ ALTER TABLE posts ADD CONSTRAINT posts_author_id_fkey FOREIGN KEY (author_id) REFERENCES users (id);
   · ───────────────────────────────────────────────────────────────────────────────────────────────────
   ╰────
  help: add it NOT VALID, then VALIDATE CONSTRAINT in a later migration
```

All three statements are reported: `DEFAULT 1` fills the new column, so its
foreign key has values to check. The fix:

```sql
-- migrations/0002_constraints.sql
SET LOCAL lock_timeout = '5s';
ALTER TABLE posts ADD CONSTRAINT posts_author_id_fkey FOREIGN KEY (author_id) REFERENCES users (id) NOT VALID;
ALTER TABLE posts ADD CONSTRAINT posts_title_not_empty CHECK (title <> '') NOT VALID;
ALTER TABLE posts ADD COLUMN editor_id bigint REFERENCES users (id);

-- migrations/0003_validate.sql
ALTER TABLE posts VALIDATE CONSTRAINT posts_author_id_fkey;
ALTER TABLE posts VALIDATE CONSTRAINT posts_title_not_empty;
```

`NOT VALID` records the constraint with a short lock and checks new rows
from then on; `VALIDATE CONSTRAINT` checks the old rows without blocking
reads or writes. Run it in a later transaction, not the one that added the
constraint ([`validate_in_same_transaction`](validate_in_same_transaction.md)).

## Why

A `CHECK` scans the table under `ACCESS EXCLUSIVE`: no reads, no writes. A
foreign key blocks writes to both tables while it looks up every row. On a
large table that is a long outage, and in a transactional migration the lock
lasts until the commit.

## Reported when

On a table created by an earlier migration file, an `ALTER TABLE` that:

- adds a `CHECK` or `FOREIGN KEY` constraint without `NOT VALID`;
- adds a column with an inline `CHECK`;
- adds a column with an inline `REFERENCES` when some column added by the
  same statement has a default (`DEFAULT …`, even `DEFAULT NULL`, a `serial`
  type, a generated expression), or the statement also adds a foreign key
  constraint.

## Not reported

- `NOT VALID` constraints.
- A new column with `REFERENCES` and no default in the statement: it holds
  only `NULL`s, and PostgreSQL skips the check. An identity column added
  alongside doesn't count as a default.
- `ADD COLUMN IF NOT EXISTS` on a column that already exists.
- A table created earlier in the same file.
- `PRIMARY KEY`, `UNIQUE` and `EXCLUDE`:
  [`create_index_blocking`](create_index_blocking.md) reports them.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
constraint_validating = "error"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "constraintValidating": "error" } }
```

Allow one statement with `-- typedpg-lint: allow(constraint_validating)`
before it in the migration file.
See [Configuring rules](README.md#configuring-rules).

## Related

- [`validate_in_same_transaction`](validate_in_same_transaction.md): a
  `VALIDATE` that undoes the split.
- [`set_not_null_scan`](set_not_null_scan.md): the same scan for
  `SET NOT NULL`.
- [`attach_partition_scan`](attach_partition_scan.md): a `CHECK` that lets
  `ATTACH PARTITION` skip its scan.
- [`missing_lock_timeout`](missing_lock_timeout.md): the same statement
  waiting for its lock with no timeout.
