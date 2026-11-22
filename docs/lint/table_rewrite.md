# `table_rewrite`

An `ALTER TABLE` on an existing table makes PostgreSQL rewrite every row (or copy every block) while holding an `ACCESS EXCLUSIVE` lock.

| | |
|---|---|
| Default level | warning |
| Applies to | migrations |
| Fix | no |

## Why

Some `ALTER TABLE` changes cannot be done in the catalog alone: PostgreSQL writes a new copy of the table, rebuilds every index on it, and only then swaps it in. The whole time it holds `ACCESS EXCLUSIVE`, which blocks every query on the table, reads included. The rewrite also needs disk space for a second copy of the table. On a table of any size this is an outage for the length of the copy.

## What it reports

On a table that existed before the current migration file started:

- `ALTER COLUMN … TYPE` when the change needs a rewrite. typedpg asks the analyzer the question PostgreSQL asks (`ATColumnChangeRequiresRewrite`): a binary-coercible change such as `varchar(20)` → `varchar(30)`, `varchar` → `text` or a wider `numeric` precision, with no `USING` or a `USING` that is just the column, does not rewrite; `integer` → `bigint`, a narrower `varchar(n)`, `text` → `varchar(n)` or a computing `USING` does.
- `ADD COLUMN` whose value must be computed for every existing row: a volatile `DEFAULT` (`clock_timestamp()`, `random()`, `nextval(…)`), a `serial` type, an identity column, or a `STORED` generated column. A constant or stable default (`DEFAULT 0`, `DEFAULT now()`), a `VIRTUAL` generated column and a plain nullable column are catalog-only changes.
- `SET LOGGED` / `SET UNLOGGED` when it changes the table's persistence.
- `SET ACCESS METHOD` to anything but `heap` (typedpg assumes every table uses `heap`, the default).
- `SET TABLESPACE`, which copies every block of the table and its indexes (reported with its own message).

The last three only apply to a table with storage: on a partitioned table they only set what future partitions get, and are not reported.

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

-- migrations/0002_rewrites.sql
ALTER TABLE posts ALTER COLUMN views TYPE bigint;
ALTER TABLE posts ADD COLUMN created_at timestamptz NOT NULL DEFAULT clock_timestamp();
ALTER TABLE posts ADD COLUMN slug text GENERATED ALWAYS AS (lower(title)) STORED;
ALTER TABLE users SET UNLOGGED;
ALTER TABLE users SET TABLESPACE fast;
```

```text
warning[table_rewrite]: ALTER TABLE rewrites every row of public.posts under an ACCESS EXCLUSIVE lock
   ╭─[migrations/0002_rewrites.sql:1:1]
 1 │ ALTER TABLE posts ALTER COLUMN views TYPE bigint;
   · ────────────────────────────────────────────────
   ╰────
  help: rewrite in steps: add a new column, backfill it in batches, then swap

warning[table_rewrite]: ALTER TABLE rewrites every row of public.posts under an ACCESS EXCLUSIVE lock
   ╭─[migrations/0002_rewrites.sql:2:1]
 2 │ ALTER TABLE posts ADD COLUMN created_at timestamptz NOT NULL DEFAULT clock_timestamp();
   · ──────────────────────────────────────────────────────────────────────────────────────
   ╰────
  help: rewrite in steps: add a new column, backfill it in batches, then swap

warning[table_rewrite]: ALTER TABLE rewrites every row of public.posts under an ACCESS EXCLUSIVE lock
   ╭─[migrations/0002_rewrites.sql:3:1]
 3 │ ALTER TABLE posts ADD COLUMN slug text GENERATED ALWAYS AS (lower(title)) STORED;
   · ────────────────────────────────────────────────────────────────────────────────
   ╰────
  help: rewrite in steps: add a new column, backfill it in batches, then swap

warning[table_rewrite]: ALTER TABLE rewrites every row of public.users under an ACCESS EXCLUSIVE lock
   ╭─[migrations/0002_rewrites.sql:4:1]
 4 │ ALTER TABLE users SET UNLOGGED;
   · ──────────────────────────────
   ╰────
  help: create a new table with the new setting, copy the rows in batches, then swap the tables

warning[table_rewrite]: SET TABLESPACE copies every block of public.users under an ACCESS EXCLUSIVE lock
   ╭─[migrations/0002_rewrites.sql:5:1]
 5 │ ALTER TABLE users SET TABLESPACE fast;
   · ─────────────────────────────────────
   ╰────
  help: create a new table in the new tablespace, copy the rows in batches, then swap the tables
```

The file also gets a [`missing_lock_timeout`](missing_lock_timeout.md) finding on its first statement (not shown).

### Good

Change a column's type in steps, none of which rewrites the table. Add the new column (nullable, no default: a catalog change), backfill it in batches outside the migration (keeping it in sync with a trigger or from the application meanwhile), prove it is never NULL without a scan under lock, then swap:

```sql
-- migrations/0002_views_new.sql
SET LOCAL lock_timeout = '5s';
ALTER TABLE posts ADD COLUMN views_new bigint;

-- (backfill: UPDATE posts SET views_new = views WHERE id BETWEEN … in batches)

-- migrations/0003_views_check.sql
SET LOCAL lock_timeout = '5s';
ALTER TABLE posts ADD CONSTRAINT posts_views_new_not_null
    CHECK (views_new IS NOT NULL) NOT VALID;

-- migrations/0004_views_validate.sql
ALTER TABLE posts VALIDATE CONSTRAINT posts_views_new_not_null;

-- migrations/0005_views_swap.sql
SET LOCAL lock_timeout = '5s';
ALTER TABLE posts ALTER COLUMN views_new SET NOT NULL;
ALTER TABLE posts DROP CONSTRAINT posts_views_new_not_null;
ALTER TABLE posts DROP COLUMN views;
ALTER TABLE posts RENAME COLUMN views_new TO views;
```

Run the last step only once no deployed code reads the old column: [`breaks_deployed_queries`](breaks_deployed_queries.md) checks that with `--previous-deployment`.

These changes do not rewrite and are not reported:

```sql
SET LOCAL lock_timeout = '5s';
-- Widening a varchar (or turning it into text) is binary-coercible.
ALTER TABLE posts ALTER COLUMN title TYPE varchar(300);
-- A stable or constant default is stored once in the catalog.
ALTER TABLE posts ADD COLUMN created_at timestamptz NOT NULL DEFAULT now();
ALTER TABLE posts ADD COLUMN score integer NOT NULL DEFAULT 0;
-- A virtual generated column has no storage.
ALTER TABLE posts ADD COLUMN title_length integer GENERATED ALWAYS AS (length(title)) VIRTUAL;
```

## When it stays silent

- The table was created in the same file (it has no rows to rewrite), or the file is unchanged from `--previous-deployment`, matches `lint.ignore`, or belongs to `extra_migrations`.
- `ADD COLUMN IF NOT EXISTS` on a column that already exists.
- `SET LOGGED` / `SET UNLOGGED` to the persistence the table already has, and `SET ACCESS METHOD heap` / `DEFAULT`.

Tablespaces are not tracked, so a `SET TABLESPACE` to the tablespace the table is already in (a no-op) is still reported.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
table_rewrite = "error"   # "error", "warning" (default), "info" or "off"
```

```json
// typedpg.config.json
{ "lint": { "tableRewrite": "error" } }
```

For one statement:

```sql
-- typedpg-lint: allow(table_rewrite)
ALTER TABLE posts ALTER COLUMN views TYPE bigint;
```

See [Configuring rules](README.md#configuring-rules) for `fail_on` / `failOn` and `ignore`.

## Related rules

- [`blocking_rebuild`](blocking_rebuild.md): `VACUUM FULL`, `CLUSTER` and `REPACK`, which rewrite the table on purpose.
- [`set_not_null_scan`](set_not_null_scan.md): the `NOT NULL` step of the pattern above.
- [`missing_lock_timeout`](missing_lock_timeout.md)
