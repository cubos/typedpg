# `scalar_subquery_may_return_many`

A subquery used as a single value that may return more than one row.

| Level | Applies to | Fix |
|---|---|---|
| warning | queries | — |

## Example

```sql
CREATE TABLE users (id bigint PRIMARY KEY, name text);
CREATE TABLE posts (
    id bigint PRIMARY KEY,
    user_id bigint NOT NULL REFERENCES users,
    title text NOT NULL,
    created_at timestamptz NOT NULL
);

SELECT u.id, (SELECT p.title FROM posts p WHERE p.user_id = u.id) AS title FROM users u;
UPDATE users SET name = (SELECT title FROM posts WHERE user_id = $id) WHERE id = $id;
```

```
warning[scalar_subquery_may_return_many]: this subquery is used as a value but may return more than one row: PostgreSQL then fails with "more than one row returned by a subquery used as an expression"
   ╭─[queries.sql:2:14]
 2 │ SELECT u.id, (SELECT p.title FROM posts p WHERE p.user_id = u.id) AS title FROM users u;
   ·              ────────────────────────────────────────────────────
   ╰────
  help: make it return at most one row: filter on a unique key, aggregate, or add ORDER BY … LIMIT 1
```

## Why

PostgreSQL aborts the statement when such a subquery returns two rows. It
depends on the data: the query passes while every user has one post and
fails the day one writes a second.

## Reported when

A `(SELECT …)` used as a value (select list, comparison, `SET`, function
argument), in a row comparison `(a, b) = (SELECT …)` or in
`SET (a, b) = (SELECT …)`, unless it returns at most one row because it:

- equates every column of a primary or unique key to constants, parameters
  or outer columns (`WHERE u.id = p.user_id`);
- aggregates without `GROUP BY`, or groups by columns its `WHERE` pins;
- has `LIMIT 0` or `LIMIT 1` (or `FETCH FIRST 1 ROW ONLY`);
- has no `FROM`, or a `WHERE` that is never true.

A `VALUES` list of two or more rows without `LIMIT` is always reported:
`SELECT (VALUES (1), (2)) AS x`.

## Not reported

- `IN (SELECT …)`, `EXISTS`, `= ANY (SELECT …)` / `ALL`, `ARRAY(SELECT …)`:
  they accept any number of rows.
- Explicitly picking one row:
  `(SELECT p.title FROM posts p WHERE p.user_id = u.id ORDER BY p.created_at DESC, p.id DESC LIMIT 1)`.
- Subqueries reading a view, a CTE, a function or a nested subquery in
  `FROM`, or a table with an expression or partial unique index or an
  exclusion constraint.
- A set-returning function in the select list, `UNION` and other set
  operations, `GROUPING SETS`, `FETCH … WITH TIES`.
- A `LIMIT` that is a parameter or an expression.
- `SELECT DISTINCT` over values that come only from the outer query.
- Queries with a `$..spread` parameter.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
scalar_subquery_may_return_many = "error"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "scalarSubqueryMayReturnMany": "error" } }
```

Allow one query with
`-- typedpg-lint: allow(scalar_subquery_may_return_many)` in its text. A
unique constraint stating what you know about the data is better: it clears
the finding and enforces it. See
[Configuring rules](README.md#configuring-rules).

## Related

- [`fetch_may_return_many`](fetch_may_return_many.md): `fetch_one` /
  `fetchOne` on a query that may return many rows.
- [`unstable_order`](unstable_order.md): `ORDER BY … LIMIT 1` whose keys can
  tie.
