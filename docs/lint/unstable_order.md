# `unstable_order`

An `ORDER BY` under `LIMIT`, `OFFSET` / `FETCH` or `DISTINCT ON` whose keys
can tie, so which of the tied rows come back is arbitrary.

| | |
|---|---|
| Default level | warning |
| Applies to | queries |
| Fix | no |

## Why

SQL guarantees the order of rows only as far as the `ORDER BY` keys go. Rows
that tie on every key come back in whatever order the plan produces, and that
order can change between executions: a different plan, a parallel scan, a
table rewritten by `VACUUM FULL`, a page of rows updated in between.

On its own that is harmless. Under `LIMIT` / `OFFSET` it is not:

- Pagination with `ORDER BY created_at LIMIT 20 OFFSET 40` skips or repeats
  rows when a run of equal `created_at` values straddles a page boundary.
  Users see the same item on two pages, and another one on none.
- `ORDER BY created_at DESC LIMIT 1` ("the latest") may return a different
  row on each call when two rows share the timestamp.
- `DISTINCT ON (user_id) … ORDER BY user_id, created_at DESC` keeps an
  arbitrary row of each group when timestamps tie.

These bugs are data-dependent and intermittent, so they rarely show up in
tests. The fix is cheap: end the `ORDER BY` with a unique column.

## What it reports

A query level with an `ORDER BY` and a `LIMIT`, `OFFSET`, `FETCH FIRST` or
`DISTINCT ON`, whose sort keys typedpg cannot prove determine a unique row.
The keys are unique when, together with:

- the columns `WHERE` equates to a constant or a parameter
  (`WHERE tenant = $1 ORDER BY email` covers `UNIQUE (tenant, email)`),
- the tables the others determine through equalities to their key (a join
  on a primary key adds nothing to sort by),
- for a grouped query, the grouping columns (one row per group),

they cover a unique key of every table in `FROM`. A unique key only counts
if its columns are `NOT NULL` (or proven non-NULL by the `WHERE`), or it is
`NULLS NOT DISTINCT`: rows with a NULL in a plain unique column tie with
each other.

The finding spans the `ORDER BY` clause. Its help line names the primary key
of each undetermined table as a tiebreaker to append, or asks for any unique
column when the table has no primary key.

## Examples

```sql
CREATE TABLE users (
    id bigint PRIMARY KEY,
    email text NOT NULL UNIQUE,
    created_at timestamptz NOT NULL
);

CREATE TABLE posts (
    id bigint PRIMARY KEY,
    user_id bigint NOT NULL REFERENCES users,
    title text NOT NULL,
    status text NOT NULL,
    created_at timestamptz NOT NULL
);
```

### Bad

```sql
SELECT id, title
FROM posts
ORDER BY created_at DESC
LIMIT 20 OFFSET $offset;
```

```
warning[unstable_order]: the ORDER BY keys can tie: they don't determine a unique row of posts — under LIMIT, rows that tie come back in an arbitrary order, so pages may skip or repeat rows
   ╭─[queries.sql:3:1]
 3 │ ORDER BY created_at DESC
   · ────────────────────────
   ╰────
  help: add a unique tiebreaker as the last ORDER BY key, such as the primary key: posts.id
```

```sql
SELECT DISTINCT ON (user_id) user_id, title
FROM posts
ORDER BY user_id, created_at DESC;
```

```
warning[unstable_order]: the ORDER BY keys can tie: they don't determine a unique row of posts — under DISTINCT ON, DISTINCT ON keeps an arbitrary row of the tied ones in each group
   ╭─[queries.sql:3:1]
 3 │ ORDER BY user_id, created_at DESC;
   · ─────────────────────────────────
   ╰────
  help: add a unique tiebreaker as the last ORDER BY key, such as the primary key: posts.id
```

### Good

```sql
-- A unique tiebreaker as the last key.
SELECT id, title
FROM posts
ORDER BY created_at DESC, id DESC
LIMIT 20 OFFSET $offset;

-- WITH TIES returns every tied row.
SELECT id, title
FROM posts
WHERE user_id = $user
ORDER BY created_at DESC
FETCH FIRST 5 ROWS WITH TIES;

-- A SKIP LOCKED queue takes any of the oldest rows, on purpose.
SELECT id
FROM posts
WHERE status = 'draft'
ORDER BY created_at
LIMIT 1
FOR UPDATE SKIP LOCKED;

-- email is UNIQUE and NOT NULL.
SELECT id, email FROM users ORDER BY email LIMIT 20;

-- Without LIMIT / OFFSET the order of ties picks nothing.
SELECT id, title FROM posts ORDER BY created_at DESC;
```

## Fix

None: which tiebreaker to add (and in which direction) is the author's
choice. The help line names the primary key.

## When it stays silent

- The query returns at most one row anyway (`WHERE id = $1 … LIMIT 1`, an
  aggregate without `GROUP BY`), or its `LIMIT` is `0`.
- `FETCH FIRST … WITH TIES`.
- `FOR UPDATE / SHARE … SKIP LOCKED`, the job-queue pattern.
- An `EXISTS` subquery, or a batching `IN (SELECT … LIMIT n)` in an `UPDATE`
  or `DELETE`: they do not care which rows they get.
- The select list is the same for every row (`SELECT 1 FROM … LIMIT 1`).
- The proof cannot see the whole `FROM`: a subquery, a CTE, a function, a
  partitioned or inheritance table, or a table with a partial, expression or
  deferrable unique index. The keys may be unique there.
- There is no `ORDER BY` at all: that is
  [`limit_without_order`](limit_without_order.md)'s finding.

## Configuration

```toml
[package.metadata.typedpg.lint]
unstable_order = "error"   # "error", "warning", "info" or "off"
```

```json
{
  "lint": { "unstableOrder": "error" }
}
```

`fail_on` / `failOn` sets the severity that fails the run (default `error`),
and `ignore` lists glob patterns of files whose query findings are dropped.
For one statement:

```sql
-- typedpg-lint: allow(unstable_order)
SELECT id, title FROM posts ORDER BY created_at DESC LIMIT 20;
```

## Related rules

- [`limit_without_order`](limit_without_order.md): `LIMIT`, `OFFSET` or
  `DISTINCT ON` with no `ORDER BY` at all.
- [`scalar_subquery_may_return_many`](scalar_subquery_may_return_many.md):
  its help suggests `ORDER BY … LIMIT 1`, which this rule then checks.
