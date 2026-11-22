# `fetch_may_return_many`

`fetch_one` / `fetch_optional` / `fetch_value` / `fetch_value_optional`
(`fetchOne` / `fetchOptional` / `fetchValue` / `fetchValueOptional` in
TypeScript) on a query the schema does not limit to one row.

| | |
|---|---|
| Default level | warning |
| Applies to | call sites |
| Fix | no |

## Why

`fetch_one` and `fetch_optional` promise the caller at most one row. When the
query returns a second one, typedpg's runtime does not silently pick the
first: the call fails with `TooManyRows`. That is the right behavior — a
silently arbitrary row is worse — but it means the call is a runtime error
waiting for the data to change:

```rust
sql!(pool, "SELECT id, title FROM posts WHERE user_id = $user_id").fetch_one()
```

works for every user with one post, and fails the first time someone writes
a second. typedpg can tell at build time whether anything guarantees one
row, because it knows the schema's unique keys.

## What it reports

A call of one of these methods on a query, when the analyzer cannot prove
the statement returns at most one row:

| Rust (`cargo typedpg lint`) | TypeScript (`typedpg lint`) |
|---|---|
| `fetch_one()`, `fetch_one_as()`, `fetch_value()` | `fetchOne()`, `fetchValue()` |
| `fetch_optional()`, `fetch_optional_as()`, `fetch_value_optional()` | `fetchOptional()`, `fetchValueOptional()` |

`fetch_value()` and `fetch_value_optional()` call `fetch_one()` and
`fetch_optional()` and fail the same way on a second row.

The proof accepts a statement that:

- filters every column of a unique key (primary key, `UNIQUE`, a unique
  index on plain columns) by equality to a constant, a parameter, or a
  column of a row already determined (keys completed through join `ON`
  conditions count);
- aggregates without `GROUP BY`, or groups only by values pinned to one
  value;
- has `LIMIT 1` or `LIMIT 0` (`FETCH FIRST 1 ROW ONLY`);
- is an `INSERT … VALUES` of exactly one row, or `INSERT … DEFAULT VALUES`
  (whatever its `ON CONFLICT`), or an `INSERT … SELECT` whose `SELECT` is
  one row;
- is an `UPDATE` / `DELETE` whose target row is determined by its `WHERE`
  (and its `FROM` / `USING` join conditions), or a `MERGE` whose target is
  determined by its `ON` key and whose source is one row.

Each offending call gets its own finding, placed at the method name in the
calling file: a query called from several places is reported at each
`fetch_one` / `fetch_optional` call. A query with a `$..spread` parameter is
never proven to return one row.

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

Rust:

```rust
use typedpg::sql;

pub async fn latest_post(pool: &typedpg::Pool, user_id: i64) -> typedpg::Result<()> {
    let post = sql!(pool, "SELECT id, title FROM posts WHERE user_id = $user_id")
        .fetch_one()
        .await?;
    Ok(())
}
```

```
warning[fetch_may_return_many]: `.fetch_one()` fails with TooManyRows if this query returns a second row, and nothing in the schema limits it to one
   ╭─[src/posts.rs:5:10]
 5 │         .fetch_one()
   ·          ─────────
   ╰────
  help: filter by a unique key, add LIMIT 1 with an ORDER BY, or use .fetch_all()
```

TypeScript:

```ts
import { sql } from "./db.ts";

export async function latestPost(pool: Pool, userId: number) {
  return sql("SELECT id, title FROM posts WHERE user_id = $userId").fetchOne(pool, { userId });
}

export async function latestTitle(pool: Pool, userId: number) {
  return sql("SELECT title FROM posts WHERE user_id = $userId").fetchValue(pool, { userId });
}
```

```
warning[fetch_may_return_many]: `.fetchOne()` fails with TooManyRows if this query returns a second row, and nothing in the schema limits it to one
   ╭─[src/posts.ts:4:69]
 4 │   return sql("SELECT id, title FROM posts WHERE user_id = $userId").fetchOne(pool, { userId });
   ·                                                                     ────────
   ╰────
  help: filter by a unique key, add LIMIT 1 with an ORDER BY, or use .fetchAll()

warning[fetch_may_return_many]: `.fetchValue()` fails with TooManyRows if this query returns a second row, and nothing in the schema limits it to one
   ╭─[src/posts.ts:8:65]
 8 │   return sql("SELECT title FROM posts WHERE user_id = $userId").fetchValue(pool, { userId });
   ·                                                                 ──────────
   ╰────
  help: filter by a unique key, add LIMIT 1 with an ORDER BY, or use .fetchAll()
```

The same applies to a query written in a `.sql` file and called through its
generated export (`postsByUser.fetchOne(pool, { userId })`).

### Good

```rust
// Pick one row explicitly (with a unique tiebreaker; see unstable_order).
sql!(pool, "SELECT id, title FROM posts WHERE user_id = $user_id ORDER BY created_at DESC, id DESC LIMIT 1")
    .fetch_optional()

// email is UNIQUE.
sql!(pool, "SELECT id, name FROM users WHERE email = $email").fetch_optional()

// An aggregate without GROUP BY is exactly one row.
sql!(pool, "SELECT count(*) AS n FROM posts WHERE user_id = $user_id").fetch_one()

// An INSERT of one row returns one row.
sql!(pool, "INSERT INTO users (id, email, created_at) VALUES ($id, $email, now()) RETURNING id")
    .fetch_one()

// Many rows are expected: fetch them all.
sql!(pool, "SELECT id, title FROM posts WHERE user_id = $user_id").fetch_all()
```

```ts
const post = await sql("SELECT id, title FROM posts WHERE user_id = $userId ORDER BY created_at DESC, id DESC LIMIT 1").fetchOptional(pool, { userId });
const user = await sql("SELECT id, name FROM users WHERE id = $id").fetchOptional(pool, { id });
const n = await sql("SELECT count(*) AS n FROM posts WHERE user_id = $userId").fetchValue(pool, { userId });
```

## Fix

None. Whether to add a key filter, an `ORDER BY … LIMIT 1`, a unique
constraint, or switch to `fetch_all` depends on what the code means.

## When it stays silent

- The proof shows at most one row (see above).
- The method is not one of those above: `fetch_all` / `fetchAll`,
  `fetch_stream` / `fetchStream` and `execute` are not checked by this
  rule.
- The query is not called where the scan can see it: Rust calls are found
  directly on the `sql!(…)` expression or on a local `let` binding of it;
  TypeScript calls on `sql(…)`, a local binding, or an exported query
  (`.sql` file or `sql(…)` binding) imported by name, alias or namespace.

## Configuration

Rust (`Cargo.toml`):

```toml
[package.metadata.typedpg.lint]
fetch_may_return_many = "error"   # "error", "warning", "info" or "off"
fail_on = "warning"               # the severity that fails the run (default "error")
ignore = ["tests/**"]             # files whose findings are not reported
```

TypeScript (`typedpg.config.json`):

```json
{
  "lint": {
    "fetchMayReturnMany": "error",
    "failOn": "warning",
    "ignore": ["tests/**"]
  }
}
```

To allow it for one query, put the allow comment in the SQL (it silences the
rule for every call of that query):

```rust
sql!(pool, "-- typedpg-lint: allow(fetch_may_return_many)\nSELECT id FROM posts WHERE user_id = $user_id")
    .fetch_one()
```

```sql
/* @name postsByUser */
-- typedpg-lint: allow(fetch_may_return_many)
SELECT id, title FROM posts WHERE user_id = $userId;
```

## Related rules

- [`fetch_all_single_row`](fetch_all_single_row.md): the opposite mismatch.
- [`scalar_subquery_may_return_many`](scalar_subquery_may_return_many.md):
  the same proof applied to a subquery used as a value.
- [`unstable_order`](unstable_order.md): checks that the `ORDER BY` before a
  `LIMIT 1` picks one well-defined row.
