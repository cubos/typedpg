# `redundant_distinct`

Reports a plain `DISTINCT` on rows the schema proves are already distinct.

| | |
|---|---|
| Default level | info |
| Applies to | queries |
| Fix | yes (`--fix`): removes the `DISTINCT` keyword |

## Why

`DISTINCT` is not free: PostgreSQL sorts or hashes every output row to remove
duplicates. When the output already contains a unique key of every table it
reads, there are no duplicates to remove, and the keyword costs a sort or hash
aggregate for nothing. It also misleads: a reader assumes the query can
produce duplicates and looks for the join that causes them.

## What it reports

A plain `SELECT DISTINCT` (not `DISTINCT ON`) whose rows typedpg proves
distinct without it. The proof is the one behind the analyzer's `distinct`
query simplification: the output values must tell, for every table the
select reads, which row of it the output row came from. A table is
determined either

- by a unique key whose columns are all bare output columns, or
- by a unique key equated, column by column, to values the other tables
  determine (constants, parameters, already-determined columns) — a join to
  a table on its own primary key, for example.

Only keys PostgreSQL enforces at every statement count: primary keys and
unique constraints or indexes that are not deferrable, not partial and on
plain columns, compared with the type's default equality under a
deterministic collation. A key column must be under a validated `NOT NULL`
(or the index `NULLS NOT DISTINCT` on a table no outer join null-extends):
otherwise two rows with a `NULL` key would look alike. Views, subqueries,
functions, CTEs, inheritance parents, partitioned tables, tables under
row-level security, and set-returning, aggregate or volatile calls make the
proof give up.

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

SELECT DISTINCT id, name FROM users;
SELECT DISTINCT p.id, u.email FROM posts p JOIN users u ON u.id = p.user_id;
SELECT DISTINCT id, 100 / views AS ratio FROM posts;
```

```
info[redundant_distinct]: DISTINCT has no effect: the rows are already distinct
   ╭─[queries.sql:2:8]
 2 │ SELECT DISTINCT id, name FROM users;
   ·        ─────────
   ╰────
  help: remove DISTINCT (a unique key of every table is in the output)

info[redundant_distinct]: DISTINCT has no effect: the rows are already distinct
   ╭─[queries.sql:5:8]
 5 │ SELECT DISTINCT p.id, u.email FROM posts p JOIN users u ON u.id = p.user_id;
   ·        ─────────
   ╰────
  help: remove DISTINCT (a unique key of every table is in the output)

info[redundant_distinct]: DISTINCT has no effect: the rows are already distinct
   ╭─[queries.sql:8:8]
 8 │ SELECT DISTINCT id, 100 / views AS ratio FROM posts;
   ·        ─────────
   ╰────
  help: remove DISTINCT (a unique key of every table is in the output)
```

`--fix` rewrites the first two to `SELECT id, name FROM users` and
`SELECT p.id, u.email FROM posts p JOIN users u ON u.id = p.user_id`. The
third keeps its finding without a fix (see below).

### Good

```sql
-- name is not unique.
SELECT DISTINCT name FROM users;
SELECT DISTINCT user_id FROM posts;

-- The join repeats a user once per post: DISTINCT removes those repeats.
SELECT DISTINCT u.email FROM users u JOIN posts p ON p.user_id = u.id;
```

`DISTINCT ON (…)` is never reported by this rule.

## Fix

The fix deletes the `DISTINCT` keyword (comments around it are kept). It is
withheld when the select is not proven error-free: with `DISTINCT`, a hash
aggregate evaluates the select list on every row before returning any, while
without it the scan may stop early (under a `LIMIT`, or a client that stops
fetching). An expression that can raise — `100 / views` raises on
`views = 0` — would change which errors the query raises. The analyzer's
`distinct` simplification keeps the query as written for the same reason.

Every fix is also re-analyzed before it is offered and must keep the result
columns' names, types, nullability and value refinements, and the parameters,
unchanged; a fix that doesn't is withheld and reported as an
`internal_error` (a typedpg bug).

## When it stays silent

- Whenever the proof can't see that the rows are distinct (see above): a
  table with no usable unique key in the output, a join that may duplicate
  rows, or a relation it can't see through.
- Queries with `$..spread` parameters.
- Migrations are not read: this rule runs over queries only.

## Configuration

Rust (`Cargo.toml`):

```toml
[package.metadata.typedpg.lint]
redundant_distinct = "warning"   # "error", "warning", "info" (default) or "off"
fail_on = "error"                # the severity the run fails on (default)
ignore = ["tests/**"]            # query files whose findings are dropped
```

TypeScript (`typedpg.config.json`):

```json
{
  "lint": {
    "redundantDistinct": "warning",
    "failOn": "error",
    "ignore": ["tests/**"]
  }
}
```

An info finding never fails the run unless `fail_on` / `failOn` is `"info"`.

To allow one query, put the comment in the query text (the `sql!` / `sql()`
string, or the statement in a `.sql` file):

```sql
-- typedpg-lint: allow(redundant_distinct)
SELECT DISTINCT id, name FROM users;
```

## Related rules

- [`redundant_limit`](redundant_limit.md): a `LIMIT` on a query that returns
  at most one row.
- [`redundant_join`](redundant_join.md): a join that can't change the rows.
- [`redundant_group_by`](redundant_group_by.md): `GROUP BY` keys a unique key
  among the other keys already determines.
