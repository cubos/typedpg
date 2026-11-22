# `redundant_order_by`

`ORDER BY` keys that can't change the order, and whole `ORDER BY` clauses
whose order nothing observes.

| Level | Applies to | Fix |
|---|---|---|
| info | queries | removes the keys, or the whole `ORDER BY` |

## Example

```sql
CREATE TABLE users (id bigint PRIMARY KEY, tenant int NOT NULL, email text NOT NULL UNIQUE, nick text UNIQUE, name text, status text NOT NULL, created_at timestamptz NOT NULL);
CREATE TABLE posts (id bigint PRIMARY KEY, user_id bigint NOT NULL, title text, created_at timestamptz NOT NULL);

SELECT id, email FROM users ORDER BY email, created_at;
SELECT id FROM users WHERE tenant = $tenant ORDER BY tenant, created_at, id;
SELECT id, title FROM posts WHERE id = $id ORDER BY created_at;
SELECT id FROM users WHERE id IN (SELECT user_id FROM posts ORDER BY created_at);
SELECT id FROM users ORDER BY created_at, name, created_at DESC;
SELECT id FROM users ORDER BY id, lower(name);
```

```
info[redundant_order_by]: the ORDER BY key created_at can't change the order: the keys before it already determine a unique row, so it never breaks a tie
   ╭─[queries.sql:2:45]
 2 │ SELECT id, email FROM users ORDER BY email, created_at;
   ·                                             ──────────
   ╰────
  help: remove it

info[redundant_order_by]: ORDER BY has no effect: a subquery that only tests membership never observes its rows' order
   ╭─[queries.sql:8:61]
 8 │ SELECT id FROM users WHERE id IN (SELECT user_id FROM posts ORDER BY created_at);
   ·                                                             ───────────────────
   ╰────
  help: remove the ORDER BY
```

`--fix` gives `ORDER BY email`, `ORDER BY created_at, id`, no `ORDER BY` in
the third and fourth, and `ORDER BY created_at, name`. The last is reported
without a fix (see below).

## Why

Each key is compared whenever the keys before it tie, and one no index
provides can force an explicit sort. A key that never decides anything costs
that for nothing and misleads: `ORDER BY email, created_at` suggests two
users can share an email.

## Reported when

A key is redundant when:

- the keys before it identify a unique row: they cover a `NOT NULL` unique
  key of every table, counting columns pinned by `WHERE` and tables joined
  on their key;
- `WHERE` equates it to a constant or parameter;
- an earlier key sorts by the same column, in either direction.

The whole `ORDER BY` is reported when the query returns at most one row
(`WHERE id = $id`, an aggregate without `GROUP BY`), when it belongs to an
`EXISTS`, `IN`/`= ANY` or `ALL` subquery, or when every key is pinned or
repeated.

## Not reported

- Keys that can tie: `ORDER BY created_at, id`.
- A nullable unique key, or a table an outer join null-extends:
  `ORDER BY nick, id`.
- A `WHERE` that is not a plain equality (`tenant > 3`, `lower(status) = 'a'`,
  `OR`), or an equality in a `LEFT JOIN`'s `ON`.
- A `COLLATE` key: `ORDER BY name, name COLLATE "C"`.
- Every key pinned under `LIMIT`, `OFFSET` or `DISTINCT ON`: that is
  [`unstable_order`](unstable_order.md)'s finding.
- The leading keys of a `DISTINCT ON`.
- The `ORDER BY` of a `FROM` subquery, a CTE, `ARRAY(…)` or a set operation.
- A membership subquery with `LIMIT`, `OFFSET`, `DISTINCT ON` or
  `FOR UPDATE`/`FOR SHARE`.
- Ordering over subqueries, CTEs, functions, partitioned or inheritance
  tables, and queries with `$..spread` parameters.

## Fix not offered

For a key that is not a plain column, an output column's alias or an ordinal
(`ORDER BY 1`): `lower(name)`, `1 / (tenant - 1)` and `random()` may raise
an error or have side effects. A whole-clause finding is fixed only when
every key in it is removable.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
redundant_order_by = "warning"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "redundantOrderBy": "warning" } }
```

Allow one query with `-- typedpg-lint: allow(redundant_order_by)` in its
text. See [Configuring rules](README.md#configuring-rules).

## Related

- [`unstable_order`](unstable_order.md): `ORDER BY` keys under `LIMIT` that
  can tie.
- [`redundant_group_by`](redundant_group_by.md): `GROUP BY` keys that can't
  split a group.
- [`redundant_limit`](redundant_limit.md): a `LIMIT` on a query that
  returns at most one row.
