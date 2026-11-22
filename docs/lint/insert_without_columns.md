# `insert_without_columns`

Reports an `INSERT INTO t VALUES …` or `INSERT INTO t SELECT …` without a
column list.

| | |
|---|---|
| Default level | warning |
| Applies to | queries |
| Fix | yes (`--fix`): writes the column list the statement targets now |

## Why

Without a column list, the values go to the table's columns by position, in
the order the columns were created. The statement then depends on that
order, which nothing in the query shows:

- a migration that adds a column, or recreates the table with the columns in
  another order, silently moves the values to other columns;
- it fails only when the types disagree. Two `text` columns swapped, or an
  `integer` landing in a `bigint`, insert wrong data without an error.

Listing the columns makes the statement say where each value goes, and makes
it immune to column order.

## What it reports

Every `INSERT` with a `VALUES` list or a `SELECT` (set operations included)
and no column list, including one inside a data-modifying CTE. The message
names the table as written.

## Examples

### Bad

```sql
CREATE TABLE users (
    id bigint PRIMARY KEY,
    email text NOT NULL UNIQUE,
    name text,
    deleted_at timestamptz
);
CREATE TABLE posts (
    id bigint PRIMARY KEY,
    user_id bigint REFERENCES users,
    title text NOT NULL,
    status text NOT NULL CHECK (status IN ('draft', 'published', 'archived')),
    views integer NOT NULL DEFAULT 0 CHECK (views >= 0)
);
CREATE TABLE archived_posts (
    id bigint PRIMARY KEY,
    user_id bigint,
    title text NOT NULL,
    status text NOT NULL,
    views integer NOT NULL
);

INSERT INTO users VALUES ($id, $email, $name);
INSERT INTO posts SELECT * FROM archived_posts;
```

```
warning[insert_without_columns]: INSERT into users without a column list depends on the table's column order
   ╭─[queries.sql:2:1]
 2 │ INSERT INTO users VALUES ($id, $email, $name);
   · ─────────────────
   ╰────
  help: list the columns the values go to

warning[insert_without_columns]: INSERT into posts without a column list depends on the table's column order
   ╭─[queries.sql:5:1]
 5 │ INSERT INTO posts SELECT * FROM archived_posts;
   · ─────────────────
   ╰────
  help: list the columns the values go to
```

`--fix` rewrites the first to
`INSERT INTO users (id, email, name) VALUES ($id, $email, $name)`; the
`deleted_at` column it leaves out takes its default, as it did before. The
second keeps its finding without a fix: with `SELECT *` the number of values
isn't plain from the text.

### Good

```sql
INSERT INTO users (id, email, name) VALUES ($id, $email, $name);

INSERT INTO posts (id, user_id, title, status, views)
SELECT id, user_id, title, status, views FROM archived_posts;
```

`INSERT INTO t DEFAULT VALUES` names no value and is not reported either.

## Fix

Without a column list, PostgreSQL (`checkInsertTargets`) targets the table's
columns in their stored order, dropped columns excluded, and the N values of
each row go to the first N of them. The fix writes exactly those N column
names, quoted when needed (`"Name"`), after the table name and its alias
(`INSERT INTO public.posts AS p (id, user_id, title) …`). Generated and
identity columns are listed too: they then accept only `DEFAULT`, as they did
without the list.

The fix is withheld when the number of values isn't plain from the text (a
`*` or `(row).*` in the select list), when the table name has a database
qualifier or doesn't resolve, or when the table has fewer columns than the
row has values.

Every fix is also re-analyzed before it is offered and must keep the result
columns' names, types, nullability and value refinements, and the parameters,
unchanged; a fix that doesn't is withheld and reported as an
`internal_error` (a typedpg bug).

## When it stays silent

- `INSERT … DEFAULT VALUES`.
- Any `INSERT` with a column list.
- Migrations are not read: this rule runs over queries only.

## Configuration

Rust (`Cargo.toml`):

```toml
[package.metadata.typedpg.lint]
insert_without_columns = "error"   # "error", "warning" (default), "info" or "off"
fail_on = "warning"                # fail the run on warnings too (default "error")
ignore = ["tests/**"]              # query files whose findings are dropped
```

TypeScript (`typedpg.config.json`):

```json
{
  "lint": {
    "insertWithoutColumns": "error",
    "failOn": "warning",
    "ignore": ["tests/**"]
  }
}
```

At the default `fail_on` / `failOn` of `"error"`, a warning is printed but
does not fail the run.

To allow one query, put the comment in the query text (the `sql!` / `sql()`
string, or the statement in a `.sql` file):

```sql
-- typedpg-lint: allow(insert_without_columns)
INSERT INTO users VALUES ($id, $email, $name);
```

## Related rules

- [`check_violation`](check_violation.md): an `INSERT` storing a constant a
  `CHECK` constraint rejects.
- [`breaks_deployed_queries`](breaks_deployed_queries.md): a migration that
  breaks a deployed query, such as an `INSERT … SELECT` missing a new
  `NOT NULL` column.
