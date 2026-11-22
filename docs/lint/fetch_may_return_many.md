# `fetch_may_return_many`

`fetch_one`, `fetch_optional`, `fetch_value` or `fetch_value_optional`
(`fetchOne`, `fetchOptional`, `fetchValue`, `fetchValueOptional` in
TypeScript) on a query the schema doesn't limit to one row.

| Level | Applies to | Fix |
|---|---|---|
| warning | call sites | — |

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
warning[fetch_may_return_many]: `.fetchOne()` fails with TooManyRows if this query returns a second row, and nothing in the schema limits it to one
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
places is reported at each one. A query with a `$..spread` parameter, such
as `WHERE id IN $..ids`, is never proven to return one row.

## Not reported

A query proven to return at most one row:

- every column of a primary key, `UNIQUE` constraint or unique index pinned
  by `=` to a constant, a parameter or a column of an already-determined row
  (also across join `ON` conditions): `WHERE email = $email`;
- an aggregate without `GROUP BY`, or `GROUP BY` keys pinned to one value:
  `SELECT count(*) AS n FROM posts WHERE user_id = $userId`;
- `LIMIT 1` / `LIMIT 0` / `FETCH FIRST 1 ROW ONLY`:
  `… ORDER BY created_at DESC, id DESC LIMIT 1`;
- a one-row `INSERT … VALUES`, `INSERT … DEFAULT VALUES`, or
  `INSERT … SELECT` of one row, whatever its `ON CONFLICT`;
- an `UPDATE` / `DELETE` whose `WHERE` (with its `FROM` / `USING` joins)
  pins a key of the target: `UPDATE users SET … WHERE id = $id RETURNING id`;
- a `MERGE` whose `ON` pins a key of the target and whose source is one row.

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
fetch_may_return_many = "error"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "fetchMayReturnMany": "error" } }
```

Allow one query with `-- typedpg-lint: allow(fetch_may_return_many)` in its
SQL; it covers every call of that query.
See [Configuring rules](README.md#configuring-rules).

## Related

- [`fetch_all_single_row`](fetch_all_single_row.md): the opposite mismatch.
- [`scalar_subquery_may_return_many`](scalar_subquery_may_return_many.md):
  a subquery used as a value that may return several rows.
- [`unstable_order`](unstable_order.md): an `ORDER BY … LIMIT 1` whose keys
  can tie.
