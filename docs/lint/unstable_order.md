# `unstable_order`

An `ORDER BY` under `LIMIT`, `OFFSET`, `FETCH` or `DISTINCT ON` whose keys
can tie, so which tied rows come back is arbitrary.

| Level | Applies to | Fix |
|---|---|---|
| warning | queries | — |

## Example

```sql
CREATE TABLE users (id bigint PRIMARY KEY, email text NOT NULL UNIQUE, tenant bigint NOT NULL, nick text, UNIQUE (tenant, nick));
CREATE TABLE posts (
    id bigint PRIMARY KEY,
    user_id bigint NOT NULL REFERENCES users,
    title text NOT NULL,
    status text NOT NULL,
    created_at timestamptz NOT NULL
);

SELECT id, title FROM posts ORDER BY created_at DESC LIMIT 20 OFFSET $offset;
SELECT DISTINCT ON (user_id) user_id, title FROM posts ORDER BY user_id, created_at DESC;
```

```
warning[unstable_order]: the ORDER BY keys can tie: they don't determine a unique row of posts — under LIMIT, rows that tie come back in an arbitrary order, so pages may skip or repeat rows
   ╭─[queries.sql:2:29]
 2 │ SELECT id, title FROM posts ORDER BY created_at DESC LIMIT 20 OFFSET $offset;
   ·                             ────────────────────────
   ╰────
  help: add a unique tiebreaker as the last ORDER BY key, such as the primary key: posts.id
```

## Why

Rows that tie on every key come back in whatever order the plan produces,
which can change between runs. Under `LIMIT` / `OFFSET`, pages then repeat
or skip rows, `ORDER BY created_at DESC LIMIT 1` returns a different row
each call, and `DISTINCT ON` keeps an arbitrary row per group. It depends on
the data, so tests rarely catch it. End the `ORDER BY` with a unique column.

## Reported when

The `ORDER BY` keys, together with the columns `WHERE` equates to constants
or parameters, don't cover a unique key of every table in `FROM`:

- a table joined on its primary key needs no key of its own;
- in a grouped query, the `GROUP BY` columns count;
- a unique key counts only if its columns are `NOT NULL` (or proven non-NULL
  by `WHERE`) or it is `NULLS NOT DISTINCT`. With `nick` nullable,
  `WHERE tenant = $t ORDER BY nick LIMIT 20` is reported.

The help names each undetermined table's primary key.

## Not reported

- A unique tiebreaker: `ORDER BY created_at DESC, id DESC LIMIT 20`, or a
  unique `NOT NULL` key: `SELECT id, email FROM users ORDER BY email LIMIT 20`.
- No `LIMIT`, `OFFSET` or `DISTINCT ON`.
- A query that returns at most one row, or `LIMIT 0`.
- `FETCH FIRST … WITH TIES`, which returns every tie.
- `FOR UPDATE … SKIP LOCKED` queues:
  `ORDER BY created_at LIMIT 1 FOR UPDATE SKIP LOCKED`.
- An `EXISTS` subquery, or a batching `IN (SELECT … LIMIT n)` in an `UPDATE`
  or `DELETE`.
- A select list that is the same for every row (`SELECT 1 … LIMIT 1`).
- `FROM` with a subquery, CTE, function, partitioned or inheritance table, or
  a table with a partial, expression or deferrable unique index.
- No `ORDER BY` at all: see [`limit_without_order`](limit_without_order.md).

No fix is offered: which tiebreaker to add, and in which direction, is your
choice.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
unstable_order = "error"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "unstableOrder": "error" } }
```

Allow one query with `-- typedpg-lint: allow(unstable_order)` in its text.
See [Configuring rules](README.md#configuring-rules).

## Related

- [`limit_without_order`](limit_without_order.md): `LIMIT` without
  `ORDER BY`.
- [`redundant_order_by`](redundant_order_by.md): `ORDER BY` keys after keys
  that already determine a unique row.
- [`scalar_subquery_may_return_many`](scalar_subquery_may_return_many.md):
  suggests `ORDER BY … LIMIT 1`, which this rule then checks.
