# `redundant_limit`

A `LIMIT` or `FETCH FIRST` (with a count or without: `FETCH FIRST ROW ONLY`) on
a query that returns at most one row anyway.

| Level | Applies to | Fix |
|---|---|---|
| info | queries | removes the clause |

## Example

```sql
CREATE TABLE users (id bigint PRIMARY KEY, email text NOT NULL UNIQUE);
CREATE TABLE posts (id bigint PRIMARY KEY, views integer NOT NULL);

SELECT id, email FROM users WHERE id = $id LIMIT 1;
SELECT count(*) AS n FROM posts FETCH FIRST 1 ROWS ONLY;
```

```
info[redundant_limit]: LIMIT 1 has no effect: the query returns at most one row
   ╭─[queries.sql:2:44]
 2 │ SELECT id, email FROM users WHERE id = $id LIMIT 1;
   ·                                            ───────
   ╰────
  help: remove it
```

`--fix` gives `SELECT id, email FROM users WHERE id = $id` and
`SELECT count(*) AS n FROM posts`.

## Why

`WHERE id = $id LIMIT 1` reads as if several rows could match and one is
picked. With `id` a primary key none can, so the `LIMIT` misstates the data
model.

## Reported when

The query is known to return at most one row through:

- a primary or unique key fully equated to constants, parameters or columns
  of other such rows (also across joins);
- an aggregate without `GROUP BY`, or `GROUP BY` columns pinned by `WHERE`;
- no `FROM`, or a one-row `VALUES`.

## Not reported

- `LIMIT` with `OFFSET`, a parameter count, `0`, `ALL`, `NULL`, or
  `FETCH … WITH TIES`.
- The `LIMIT` of a `UNION`/`INTERSECT`/`EXCEPT`.
- Queries over views (other than `pg_catalog` ones), inheritance parents,
  foreign tables, set-returning functions or `FULL JOIN`.

## Fix not offered

When a filter or output expression can raise an error on some rows, e.g.
`WHERE id = $id AND 100 / views > 1 LIMIT 1`: the `LIMIT` can stop the
scan before a row that would divide by zero.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
redundant_limit = "warning"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "redundantLimit": "warning" } }
```

Allow one query with `-- typedpg-lint: allow(redundant_limit)` in its text.
See [Configuring rules](README.md#configuring-rules).

## Related

- [`limit_without_order`](limit_without_order.md): `LIMIT` without
  `ORDER BY` on a query that can return many rows.
- [`fetch_all_single_row`](fetch_all_single_row.md): `fetch_all` on a
  single-row query.
- [`redundant_distinct`](redundant_distinct.md): `DISTINCT` on rows already
  distinct.
