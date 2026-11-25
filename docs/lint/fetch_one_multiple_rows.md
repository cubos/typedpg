# `fetch_one_multiple_rows`

`fetch_one`, `fetch_optional`, `fetch_value` or `fetch_value_optional`
(`fetchOne`, `fetchOptional`, `fetchValue`, `fetchValueOptional` in
TypeScript) on a query the schema doesn't limit to one row.

| Category | Level | Applies to | Fix |
|---|---|---|---|
| correctness | error | call sites | — |

## Example

```sql
CREATE TABLE users (id bigint PRIMARY KEY, email text NOT NULL UNIQUE, name text);
CREATE TABLE posts (id bigint PRIMARY KEY, user_id bigint NOT NULL REFERENCES users,
                    title text NOT NULL, created_at timestamptz NOT NULL);
```

```ts
// src/posts.ts
import type { Pool } from "pg";
import { sql } from "./db.ts";
export async function latestPost(pool: Pool, userId: number) {
  return sql("SELECT id, title FROM posts WHERE user_id = $userId").fetchOne(pool, { userId });
}
```

```
error[correctness/fetch_one_multiple_rows]: `.fetchOne()` fails with TooManyRows if this query returns a second row, and nothing in the schema limits it to one
   ╭─[src/posts.ts:5:69]
 5 │   return sql("SELECT id, title FROM posts WHERE user_id = $userId").fetchOne(pool, { userId });
   ·                                                                     ────────
   ╰────
  help: filter by a unique key, add LIMIT 1 with an ORDER BY, or use .fetchAll()
```

In Rust, `sql!(pool, "SELECT id, title FROM posts WHERE user_id = $user_id").fetch_one()`
is reported the same way, with `or use .fetch_all()` in the help.

## Why

These methods promise at most one row. On a second row the call fails with
`TooManyRows` rather than pick one arbitrarily. The code above works for
every user with one post and fails the first time someone writes a second.

## Reported when

The method is called on a query that isn't proven to return at most one
row:

| Rust | TypeScript |
|---|---|
| `fetch_one()`, `fetch_one_as()`, `fetch_value()` | `fetchOne()`, `fetchValue()` |
| `fetch_optional()`, `fetch_optional_as()`, `fetch_value_optional()` | `fetchOptional()`, `fetchValueOptional()` |

Each call is reported at its method name, so a query called from several
places is reported at each one. A `$..spread` stands for any number of
values or rows: `WHERE id IN $..ids` and `INSERT … VALUES $..rows` are
never proven to return one row, while an aggregate over them still is.

## Not reported

A query proven to return at most one row:

- every column of a primary key, `UNIQUE` constraint or unique index pinned
  by `=` to a constant, a parameter or a column of an already-determined row
  (also across join `ON` conditions): `WHERE email = $email`. The value may
  be any non-volatile expression of those, a cast, `COALESCE` or `CASE`
  included (`ON c.code = e.code::integer`,
  `ON u.principal = coalesce(s.holder, 'x')`), a `LATERAL` subquery's
  column reading the row on its left, the merged column of a chain of
  `USING` joins, or a scalar subquery that doesn't read the row (a bare
  table name in it is that subquery's own whole row);
  `id IN (SELECT … LIMIT 1)` counts as `id = (SELECT …)`. A value may also
  be a column of a `FROM` subquery around the query, `(row).*` fields
  included. A key column cast to `text` counts as the column when its text
  tells its values apart: an integer, `numeric`, `uuid`, `boolean`, enum or
  string column (`WHERE n.id::text = $id`), not a float, date or time one,
  whose text depends on session settings;
- a `LEFT` / `RIGHT` join whose preserved side is one row onto a join whose
  relations are each pinned by a key: by the outer join's `ON` from the
  preserved side's values, by an inner join's `ON` inside that side, or by a
  nested outer join's nullable side. `FROM (SELECT $p AS who) q LEFT JOIN
  (users u JOIN profiles pr ON pr.user_id = u.principal) ON u.principal =
  q.p` is one row with `users.principal` and `profiles.user_id` unique; a
  `FULL JOIN` in that side never does;
- a partial unique index whose predicate the `WHERE` implies
  (`… WHERE state IN ('queued', 'done')` by `state = 'done'`), and a unique
  index on a function of columns matched by the same call
  (`UNIQUE (lower(email))` by `WHERE lower(email) = lower($login)`);
- the `DISTINCT ON` keys of a view or `FROM` subquery, each pinned to one
  value: `SELECT … FROM current_builds WHERE project_id = $p AND arch = $a`;
- an aggregate without `GROUP BY`, or `GROUP BY` keys pinned to one value:
  `SELECT count(*) AS n FROM posts WHERE user_id = $userId`;
- `LIMIT 1` / `LIMIT 0` / `FETCH FIRST 1 ROW ONLY` / `FETCH FIRST ROW ONLY`:
  `… ORDER BY created_at DESC, id DESC LIMIT 1`;
- a one-row `INSERT … VALUES`, `INSERT … DEFAULT VALUES`, or
  `INSERT … SELECT` of one row, whatever its `ON CONFLICT`;
- an `UPDATE` / `DELETE` whose `WHERE` (with its `FROM` / `USING` joins)
  pins a key of the target: `UPDATE users SET … WHERE id = $id RETURNING id`;
- a `MERGE` whose `ON` pins a key of the target and whose source is one row;
- a `UNION ALL` of two such queries that can't both return a row: one only
  when a CTE of the statement has a row (it reads the CTE, tests
  `EXISTS (SELECT … FROM cte)` or compares with `(SELECT … FROM cte)`), the
  other only when it has none (`WHERE NOT EXISTS (SELECT 1 FROM cte)`), as
  in a find-or-create:
  `WITH found AS (…), updated AS (UPDATE … WHERE id = (SELECT id FROM found) RETURNING …), inserted AS (INSERT … SELECT … WHERE NOT EXISTS (SELECT 1 FROM found) RETURNING …) SELECT … FROM updated UNION ALL SELECT … FROM inserted`.

Also not reported:

- `fetch_all` / `fetchAll`, `fetch_stream` / `fetchStream`, `execute`.
- Calls the scan doesn't see: Rust calls must be on the `sql!(…)` expression
  or a local `let` binding of it; TypeScript calls on `sql(…)`, a local
  binding, or an exported query (from a `.sql` file or a `sql(…)` binding)
  imported by name, alias or namespace.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
fetch_one_multiple_rows = "error"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "fetchOneMultipleRows": "error" } }
```

Allow one query with `-- typedpg-lint: allow(fetch_one_multiple_rows)` in its
SQL; it covers every call of that query.
See [Configuring rules](README.md#configuring-rules).

## Related

- [`fetch_all_single_row`](fetch_all_single_row.md): the opposite mismatch.
- [`scalar_subquery_multiple_rows`](scalar_subquery_multiple_rows.md):
  a subquery used as a value that may return several rows.
- [`unstable_order`](unstable_order.md): an `ORDER BY … LIMIT 1` whose keys
  can tie.
