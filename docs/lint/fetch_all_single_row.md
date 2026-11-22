# `fetch_all_single_row`

`fetch_all` (`fetchAll` in TypeScript) on a query the schema proves returns
at most one row.

| | |
|---|---|
| Default level | info |
| Applies to | call sites |
| Fix | no |

## Why

`fetch_all` returns a list. On a query that can only return zero or one row
(a lookup by primary key, an aggregate), the list is always empty or a
singleton, and the caller has to unwrap it by hand (`rows.first()`,
`rows[0]`), usually without handling the empty case. `fetch_optional`
returns exactly what the query can produce, an optional row, and the type
system then makes the caller handle "not found".

It is also a hint that the query might not be what was intended: a `fetch_all`
on a primary-key lookup is sometimes a list query whose filter is wrong.

## What it reports

A call of `fetch_all()` / `fetch_all_as()` (Rust) or `fetchAll()`
(TypeScript) on a query that typedpg proves returns at most one row: the
same proof as [`fetch_may_return_many`](fetch_may_return_many.md), which
accepts a unique key pinned by the `WHERE`, an aggregate without `GROUP BY`,
`LIMIT 1`, a single-row `INSERT … VALUES`, a single-row `VALUES`, an `UPDATE`
/ `DELETE` by key, and so on.

Each such call gets its own finding, placed at the method name in the
calling file.

## Examples

```sql
CREATE TABLE users (
    id bigint PRIMARY KEY,
    email text NOT NULL UNIQUE,
    name text,
    created_at timestamptz NOT NULL
);
```

### Bad

Rust:

```rust
use typedpg::sql;

pub async fn user_by_id(pool: &typedpg::Pool, id: i64) -> typedpg::Result<()> {
    let users = sql!(pool, "SELECT id, name FROM users WHERE id = $id")
        .fetch_all()
        .await?;
    Ok(())
}
```

```
info[fetch_all_single_row]: `.fetch_all()` on a query that returns at most one row
   ╭─[src/users.rs:5:10]
 5 │         .fetch_all()
   ·          ─────────
   ╰────
  help: use .fetch_optional() (or .fetch_one())
```

TypeScript:

```ts
import { sql } from "./db.ts";

export async function userById(pool: Pool, id: number) {
  return sql("SELECT id, name FROM users WHERE id = $id").fetchAll(pool, { id });
}
```

```
info[fetch_all_single_row]: `.fetchAll()` on a query that returns at most one row
   ╭─[src/users.ts:4:59]
 4 │   return sql("SELECT id, name FROM users WHERE id = $id").fetchAll(pool, { id });
   ·                                                           ────────
   ╰────
  help: use .fetchOptional() (or .fetchOne())
```

### Good

```rust
let user = sql!(pool, "SELECT id, name FROM users WHERE id = $id")
    .fetch_optional()
    .await?;
```

```ts
const user = await sql("SELECT id, name FROM users WHERE id = $id").fetchOptional(pool, { id });
```

A `fetch_all` on a query that may return many rows is what `fetch_all` is
for, and is not reported.

## Fix

None: switching to `fetch_optional` changes the Rust or TypeScript type the
caller receives, so the calling code has to change with it.

## When it stays silent

- The proof cannot show at most one row: anything it cannot see through (a
  view, a set-returning function, a `FULL JOIN`, a `$..spread` parameter, …)
  counts as possibly many rows.
- The method is not `fetch_all` / `fetch_all_as` / `fetchAll`
  (`fetch_stream` / `fetchStream` are not checked).
- The call is not where the scan can see it: Rust calls are found directly
  on the `sql!(…)` expression or on a local `let` binding of it; TypeScript
  calls on `sql(…)`, a local binding, or an exported query (`.sql` file or
  `sql(…)` binding) imported by name, alias or namespace.

## Configuration

Rust (`Cargo.toml`):

```toml
[package.metadata.typedpg.lint]
fetch_all_single_row = "warning"   # "error", "warning", "info" or "off"
fail_on = "warning"                # the severity that fails the run (default "error")
ignore = ["tests/**"]              # files whose findings are not reported
```

TypeScript (`typedpg.config.json`):

```json
{
  "lint": {
    "fetchAllSingleRow": "warning",
    "failOn": "warning",
    "ignore": ["tests/**"]
  }
}
```

To allow it for one query, put the allow comment in the SQL (it silences the
rule for every call of that query):

```rust
sql!(pool, "-- typedpg-lint: allow(fetch_all_single_row)\nSELECT id, name FROM users WHERE id = $id")
    .fetch_all()
```

## Related rules

- [`fetch_may_return_many`](fetch_may_return_many.md): `fetch_one` on a query
  that may return many rows.
- [`redundant_limit`](redundant_limit.md): a `LIMIT` on a query that already
  returns at most one row.
