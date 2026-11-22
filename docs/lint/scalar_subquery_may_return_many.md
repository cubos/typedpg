# `scalar_subquery_may_return_many`

A subquery used as a single value that may return more than one row, which
makes PostgreSQL fail the whole statement at run time.

| | |
|---|---|
| Default level | warning |
| Applies to | queries |
| Fix | no |

## Why

A parenthesized `SELECT` used where a value goes — in a select list, a
comparison, a `SET`, a function argument, `(a, b) = (SELECT …)` — must return
at most one row. When it returns zero rows the value is NULL; when it returns
two, PostgreSQL aborts the statement:

```
ERROR:  more than one row returned by a subquery used as an expression
```

The failure depends on the data, so it passes every test where each user
happens to have one post and breaks in production the day someone writes a
second. typedpg checks the subquery against the schema's unique keys at
build time instead.

## What it reports

A subquery used as an expression (`(SELECT …)`), in a row comparison
(`(a, b) = (SELECT …)`) or as the source of a multi-column `SET (a, b) =
(SELECT …)`, when typedpg cannot prove it returns at most one row.

The proof (the same one behind
[`fetch_may_return_many`](fetch_may_return_many.md)) accepts a subquery that:

- aggregates without `GROUP BY` (`SELECT count(*) …`), or groups by values
  its `WHERE` pins;
- pins every column of a unique key to a constant, a parameter or an outer
  column (`WHERE u.id = p.user_id` for a correlated lookup);
- has `LIMIT 1` or `LIMIT 0` (`FETCH FIRST 1 ROW ONLY` too);
- has no `FROM`, or is a single-row `VALUES`;
- has a `WHERE` that is never true.

A `VALUES` list of two rows or more without a `LIMIT` is always reported.

## Examples

```sql
CREATE TABLE users (
    id bigint PRIMARY KEY,
    email text NOT NULL UNIQUE,
    name text,
    created_at timestamptz NOT NULL
);

CREATE TABLE posts (
    id bigint PRIMARY KEY,
    user_id bigint NOT NULL REFERENCES users,
    title text NOT NULL,
    created_at timestamptz NOT NULL
);
```

### Bad

A user can have many posts:

```sql
SELECT u.id,
       (SELECT p.title FROM posts p WHERE p.user_id = u.id) AS title
FROM users u;
```

```
warning[scalar_subquery_may_return_many]: this subquery is used as a value but may return more than one row: PostgreSQL then fails with "more than one row returned by a subquery used as an expression"
   ╭─[queries.sql:2:8]
 2 │        (SELECT p.title FROM posts p WHERE p.user_id = u.id) AS title
   ·        ────────────────────────────────────────────────────
   ╰────
  help: make it return at most one row: filter on a unique key, aggregate, or add ORDER BY … LIMIT 1
```

### Good

```sql
-- Pick one row explicitly.
SELECT u.id,
       (SELECT p.title FROM posts p WHERE p.user_id = u.id
        ORDER BY p.created_at DESC, p.id DESC LIMIT 1) AS title
FROM users u;

-- A correlated lookup on the parent's primary key: at most one row.
SELECT p.id, (SELECT u.name FROM users u WHERE u.id = p.user_id) AS author
FROM posts p;

-- An aggregate without GROUP BY returns exactly one row.
SELECT u.id, (SELECT count(*) FROM posts p WHERE p.user_id = u.id) AS n
FROM users u;

-- ARRAY(…) takes any number of rows.
SELECT u.id, ARRAY(SELECT p.title FROM posts p WHERE p.user_id = u.id) AS titles
FROM users u;
```

`IN (SELECT …)`, `EXISTS (…)`, `= ANY (SELECT …)` / `ALL` and `ARRAY(…)`
accept any number of rows and are never reported.

## Fix

None: which row to keep (or whether to aggregate) is a decision about what
the query means.

## When it stays silent

A finding is only reported where the proof's limits cannot be hiding a
single-row subquery. Nothing is reported when:

- a `FROM` item is anything but a plain or partitioned table: a view, a CTE,
  a set-returning function, a subquery;
- a table in `FROM` has a unique index on an expression, a partial unique
  index or an exclusion constraint (a key the proof cannot use);
- the select list calls a set-returning function, the subquery is a set
  operation (`UNION`, …), it uses `GROUPING SETS`, or `FETCH … WITH TIES`;
- the `LIMIT` is a parameter or another expression (only a constant of 2 or
  more counts as "may return many");
- it is `SELECT DISTINCT` over values that come only from the outer query;
- the statement has a `$..spread` parameter.

## Configuration

```toml
[package.metadata.typedpg.lint]
scalar_subquery_may_return_many = "error"   # "error", "warning", "info" or "off"
```

```json
{
  "lint": { "scalarSubqueryMayReturnMany": "error" }
}
```

`fail_on` / `failOn` sets the severity that fails the run (default `error`),
and `ignore` lists glob patterns of files whose query findings are dropped.
When you know the data better than the schema does:

```sql
-- typedpg-lint: allow(scalar_subquery_may_return_many)
SELECT u.id, (SELECT p.title FROM posts p WHERE p.user_id = u.id) AS title
FROM users u;
```

A unique constraint that states that knowledge is better: it makes the
finding go away and protects the data too.

## Related rules

- [`fetch_may_return_many`](fetch_may_return_many.md): the same proof,
  applied to `fetch_one` / `fetchOne` call sites.
- [`unstable_order`](unstable_order.md): an `ORDER BY … LIMIT 1` whose keys
  can tie picks an arbitrary row.
