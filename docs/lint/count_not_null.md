# `count_not_null`

Reports `count(expr)` over a value that is never NULL, which counts every
row: that is `count(*)`.

| | |
|---|---|
| Default level | info |
| Applies to | queries |
| Fix | yes (`--fix`): rewrites the argument to `*` |

## Why

`count(expr)` and `count(*)` differ in one thing: `count(expr)` skips the
rows where `expr` is NULL. When `expr` can never be NULL, the two return
the same number, and the argument only misleads. A reader of
`count(u.email)` wonders which users have no email; a reader of `count(1)`
wonders whether it differs from `count(*)` (it doesn't, in PostgreSQL).
`count(*)` also skips evaluating the argument for every row.

The opposite mistake matters more, and this rule draws the line between
them: `count(p.id)` on the nullable side of a `LEFT JOIN` counts matches,
and is not reported.

## What it reports

A call to `pg_catalog.count` with one argument, without `DISTINCT` or an
`ORDER BY` inside the call, whose argument the analyzer proves is never
NULL:

- a `NOT NULL` column of a table no outer join null-extends (the preserved
  side of a `LEFT JOIN` counts);
- a non-NULL literal (`count(1)`, `count('x')`);
- an expression of those that can't produce NULL (`count(id / 2)`);
- a nullable column the call's own `FILTER (WHERE … IS NOT NULL)` proves
  non-NULL.

The argument must not be a row or composite value, and must not contain a
parameter (it may be bound to NULL). The aggregate must belong to the query
level it is written in: PostgreSQL evaluates an aggregate whose argument
reads only an outer query's columns in that outer query, so `count(u.id)`
inside a subquery over `posts` counts users, not posts, and is left alone.

The same applies with `FILTER (…)` or as a window function (`OVER (…)`).
The finding spans the call.

## Examples

```sql
CREATE TABLE users (
    id bigint PRIMARY KEY,
    email text NOT NULL,
    name text
);

CREATE TABLE posts (
    id bigint PRIMARY KEY,
    user_id bigint NOT NULL REFERENCES users
);
```

### Bad

```sql
SELECT count(id) FROM users;
SELECT count(1) FROM posts;
SELECT u.id, count(u.email) FROM users u LEFT JOIN posts p ON p.user_id = u.id GROUP BY u.id;
SELECT count(name) FILTER (WHERE name IS NOT NULL) FROM users;
SELECT count(id / 2) FROM users;
```

```
info[count_not_null]: count(id) counts every row: its argument is never NULL here
   ╭─[queries.sql:2:8]
 2 │ SELECT count(id) FROM users;
   ·        ─────────
   ╰────
  help: use count(*)

info[count_not_null]: count(1) counts every row: its argument is never NULL here
   ╭─[queries.sql:5:8]
 5 │ SELECT count(1) FROM posts;
   ·        ────────
   ╰────
  help: use count(*)

info[count_not_null]: count(u.email) counts every row: its argument is never NULL here
   ╭─[queries.sql:8:14]
 8 │ SELECT u.id, count(u.email) FROM users u LEFT JOIN posts p ON p.user_id = u.id GROUP BY u.id;
   ·              ──────────────
   ╰────
  help: use count(*)

info[count_not_null]: count(name) counts every row: its argument is never NULL here
    ╭─[queries.sql:11:8]
 11 │ SELECT count(name) FILTER (WHERE name IS NOT NULL) FROM users;
    ·        ───────────
    ╰────
   help: use count(*)

info[count_not_null]: count(id / 2) counts every row: its argument is never NULL here
    ╭─[queries.sql:14:8]
 14 │ SELECT count(id / 2) FROM users;
    ·        ─────────────
    ╰────
   help: use count(*)
```

`--fix` rewrites the first four to:

```sql
SELECT count(*) FROM users;
SELECT count(*) FROM posts;
SELECT u.id, count(*) FROM users u LEFT JOIN posts p ON p.user_id = u.id GROUP BY u.id;
SELECT count(*) FILTER (WHERE name IS NOT NULL) FROM users;
```

The last keeps its finding without a fix (see below).

### Good

```sql
SELECT count(*) FROM users;

-- name is nullable: count(name) skips the users without one.
SELECT count(name) FROM users;

-- p is null-extended by the LEFT JOIN: this counts each user's posts.
SELECT u.id, count(p.id) FROM users u LEFT JOIN posts p ON p.user_id = u.id GROUP BY u.id;

-- DISTINCT counts distinct values.
SELECT count(DISTINCT user_id) FROM posts;

-- A parameter may be bound to NULL.
SELECT count($value::int) FROM users;
```

## Fix

The fix replaces the argument with `*`, keeping `FILTER (…)` and `OVER (…)`.
It is only offered when the argument is a plain column or a non-NULL
constant, whose evaluation can't raise or have an effect. Any other
argument (`count(id / 2)`, a function call) is reported without a fix:
`count(*)` would no longer evaluate it, and with it any error it raises.

Every fix is also re-analyzed before it is offered and must keep the result
columns' names, types, nullability and value refinements, and the
parameters, unchanged; a fix that doesn't is withheld and reported as an
[`internal_error`](internal_error.md) (a typedpg bug).

## When it stays silent

- The argument may be NULL: a nullable column, a column of a null-extended
  table, a parameter, `NULL::int`.
- `count(DISTINCT …)`, `count(x ORDER BY …)`, `count(*)` itself.
- A row or composite argument (`count(t)` over a table alias, a composite
  column).
- An aggregate of an outer query's columns written in a subquery.
- A `WHERE` condition doesn't count: `SELECT count(name) FROM users WHERE
  name IS NOT NULL` is not reported, because the analyzer's nullability
  inference of the call doesn't see the `WHERE` clause's facts. Only the
  call's own `FILTER` does.
- A `count` in another schema than `pg_catalog`.

## Configuration

```toml
[package.metadata.typedpg.lint]
count_not_null = "warning"   # "error", "warning", "info" (default) or "off"
```

```json
{
  "lint": { "countNotNull": "warning" }
}
```

`fail_on` / `failOn` sets the severity that fails the run (default `error`;
an info finding only fails it with `"info"`), and `ignore` lists glob
patterns of files whose query findings are dropped. For one statement:

```sql
-- typedpg-lint: allow(count_not_null)
SELECT count(1) FROM posts;
```

## Related rules

- [`redundant_null_check`](redundant_null_check.md): `IS [NOT] NULL` or
  `COALESCE` on a value that is never NULL — the same nullability facts.
- [`left_join_nullified`](left_join_nullified.md): an outer join that the
  `WHERE` clause turns into an inner join.
