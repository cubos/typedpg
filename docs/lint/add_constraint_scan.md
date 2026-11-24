# `add_constraint_scan`

A `CHECK` or foreign key added to a table that already has rows, which
PostgreSQL validates against every row under a lock.

| Category | Level | Applies to | Fix |
|---|---|---|---|
| locking | warning | migrations | — |

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
warning[locking/add_constraint_scan]: adding a constraint validates every row of public.posts under a lock
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
constraint ([`validate_constraint_blocking`](validate_constraint_blocking.md)).

## Why

A `CHECK` scans the table under `ACCESS EXCLUSIVE`: no reads, no writes. A
foreign key blocks writes to both tables while it looks up every row. On a
large table that is a long outage, and in a transactional migration the lock
lasts until the commit.

## Reported when

On a table created by an earlier migration file, an `ALTER TABLE` that:

- adds a `CHECK` or `FOREIGN KEY` constraint without `NOT VALID`;
- adds a column with an inline `CHECK`;
- adds a column with an inline `REFERENCES` when that column has a default
  (`DEFAULT …`, even `DEFAULT NULL`, a `serial` type, a generated
  expression);
- changes a column's type without a rewrite while a validated `CHECK`
  reads it: PostgreSQL adds the check again and validates every row;
- makes a `NOT ENFORCED` constraint `ENFORCED` (`ALTER CONSTRAINT …
  ENFORCED`), which validates it;
- gives a virtual generated column a new expression (`SET EXPRESSION`)
  while a `CHECK` or `NOT NULL` reads it;
- detaches a partition of a table a foreign key references: PostgreSQL
  checks every row of the referencing table.

A partitioned table with no partition (and a foreign table) has no rows to
check, and is not reported; neither is a foreign key every partition
already has (validated), which PostgreSQL adopts instead of checking.

And an `ALTER DOMAIN` that adds a `CHECK` (even `NOT VALID`'s later
`VALIDATE CONSTRAINT`) or a `NOT NULL` to a domain a table uses: it checks
every row under a `SHARE` lock, blocking writes, and for a domain the
`NOT VALID` split doesn't help.

## Not reported

- `NOT VALID` constraints.
- A new column with `REFERENCES` and no default: it holds only `NULL`s,
  and PostgreSQL skips the check, whatever the statement's other columns
  and constraints are. An identity column doesn't count as a default.
- `ADD COLUMN IF NOT EXISTS` on a column that already exists.
- A table created earlier in the same file.
- `PRIMARY KEY`, `UNIQUE` and `EXCLUDE`:
  [`create_index_blocking`](create_index_blocking.md) reports them.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
add_constraint_scan = "error"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "addConstraintScan": "error" } }
```

Allow one statement with `-- typedpg-lint: allow(add_constraint_scan)`
before it in the migration file.
See [Configuring rules](README.md#configuring-rules).

## Related

- [`validate_constraint_blocking`](validate_constraint_blocking.md): a
  `VALIDATE` that undoes the split.
- [`set_not_null_scan`](set_not_null_scan.md): the same scan for
  `SET NOT NULL`.
- [`attach_partition_scan`](attach_partition_scan.md): a `CHECK` that lets
  `ATTACH PARTITION` skip its scan.
- [`lock_without_timeout`](lock_without_timeout.md): the same statement
  waiting for its lock with no timeout.
