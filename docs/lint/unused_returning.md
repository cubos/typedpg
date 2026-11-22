# `unused_returning`

`execute()` on an `INSERT` / `UPDATE` / `DELETE` / `MERGE` whose `RETURNING`
rows it throws away.

| | |
|---|---|
| Default level | info |
| Applies to | call sites |
| Fix | yes (`--fix`): deletes the `RETURNING` clause, when that provably changes nothing else |

## Why

`execute()` runs a statement and returns the number of affected rows; any
`RETURNING` rows are discarded. A `RETURNING` nobody reads still costs
something: PostgreSQL evaluates the list for every written row and sends the
rows to the client, and typedpg generates a row type for them. It is usually
a leftover from a version of the code that used the result, or a sign that
the code meant to `fetch_one()` the new id and forgot to.

## What it reports

A top-level `INSERT`, `UPDATE`, `DELETE` or `MERGE` with a `RETURNING`
clause, when every call the scan sees on that query is `execute()`. The
finding is placed at each `execute` call.

The rule needs to know every way the query is used, so it stays silent when
it cannot:

- Rust: the query is called directly on the `sql!(…)` expression or through
  a local `let` binding, and every use of that binding is a method call the
  scan recognizes. Passing the query to a function (`run(q)`), borrowing it,
  or calling any other method on it (`fetch_stream`, `fetch_value`, …)
  silences the rule.
- TypeScript: a `.sql` query is always an export that can be used anywhere,
  so this rule never reports it; neither does it report an exported `sql(…)`
  binding, or a binding passed to a function. Inline `sql(…).execute(…)` and
  local bindings are checked.

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

pub async fn create_user(pool: &typedpg::Pool, id: i64, email: &str) -> typedpg::Result<()> {
    sql!(pool, "INSERT INTO users (id, email, created_at) VALUES ($id, $email, now()) RETURNING id")
        .execute()
        .await?;
    Ok(())
}
```

```
info[unused_returning]: `.execute()` discards the RETURNING rows
   ╭─[src/users.rs:5:10]
 5 │         .execute()
   ·          ───────
   ╰────
  help: remove RETURNING, or fetch the rows
```

TypeScript:

```ts
import { sql } from "./db.ts";

export async function createUser(pool: Pool, id: number, email: string) {
  await sql("INSERT INTO users (id, email, created_at) VALUES ($id, $email, now()) RETURNING id").execute(pool, { id, email });
}
```

```
info[unused_returning]: `.execute()` discards the RETURNING rows
   ╭─[src/users.ts:4:99]
 4 │   await sql("INSERT INTO users (id, email, created_at) VALUES ($id, $email, now()) RETURNING id").execute(pool, { id, email });
   ·                                                                                                   ───────
   ╰────
  help: remove RETURNING, or fetch the rows
```

In both, `--fix` removes ` RETURNING id` from the SQL string.

### Good

```rust
// No RETURNING.
sql!(pool, "INSERT INTO users (id, email, created_at) VALUES ($id, $email, now())")
    .execute()
    .await?;

// The returned row is used.
let id = sql!(pool, "INSERT INTO users (id, email, created_at) VALUES ($id, $email, now()) RETURNING id")
    .fetch_one()
    .await?;

// The query escapes to another function: the rule can't see every use.
let q = sql!(pool, "DELETE FROM posts WHERE id = $id RETURNING id");
run_and_log(q).await?;
```

```ts
// Exported: it may be fetched elsewhere.
export const deleteReturning = sql("DELETE FROM posts WHERE id = $id RETURNING id");
await deleteReturning.execute(pool, { id });
```

## Fix

`--fix` deletes the `RETURNING` keyword, its list and the whitespace before
it, keeping a trailing `;`. Removing `RETURNING` must not change what the
statement does, so the fix is offered only when all of these hold:

- there is exactly one call, the `execute()` (two `execute()` calls of the
  same query are each reported, without a fix);
- every `RETURNING` item is a plain column (`id`, `t.id`, `*`, `t.*`) or a
  plain constant. Anything else — an operator (`1 / id`), a cast
  (`id::text`), a function call (`lower(name)`, `now()`), a `CASE`, a
  subquery — can raise an error when evaluated, and removing it could make
  a failing statement succeed;
- the statement is an `INSERT`, `UPDATE` or `DELETE` (not `MERGE`) with no
  `WITH` clause, whose target and every `FROM` / `USING` item is an ordinary
  table without row-level security or rewrite rules. With row-level security
  enabled, PostgreSQL applies the table's `SELECT` policies to the rows a
  write with `RETURNING` may touch, so removing it can change which rows are
  written; a view or a rule can likewise tie the statement's effect to its
  `RETURNING`;
- no relation of the statement may have a virtual generated column, whose
  expression `RETURNING` would evaluate.

Every fix is checked once more before it is offered: the edited query must
analyze with no result columns and the same parameters. A fix that fails is
a typedpg bug; it is withheld and reported as an `internal_error` warning.

For example, this is reported but not fixed, because `lower(name)` can fail:

```rust
sql!(pool, "UPDATE users SET name = $name WHERE id = $id RETURNING lower(name)")
    .execute()
    .await?;
```

## When it stays silent

- Any call on the query is not `execute()` (it fetches the rows).
- The query may be used in a way the scan cannot see (see above).
- The statement has no `RETURNING` at the top level (a `RETURNING` inside a
  data-modifying `WITH` query is that CTE's, and is read by the statement).

## Configuration

Rust (`Cargo.toml`):

```toml
[package.metadata.typedpg.lint]
unused_returning = "warning"   # "error", "warning", "info" or "off"
fail_on = "warning"            # the severity that fails the run (default "error")
ignore = ["tests/**"]          # files whose findings are not reported
```

TypeScript (`typedpg.config.json`):

```json
{
  "lint": {
    "unusedReturning": "warning",
    "failOn": "warning",
    "ignore": ["tests/**"]
  }
}
```

To allow it for one query, put the allow comment in the SQL:

```rust
sql!(pool, "-- typedpg-lint: allow(unused_returning)\nINSERT INTO users (id, email, created_at) VALUES ($id, $email, now()) RETURNING id")
    .execute()
```

## Related rules

- [`fetch_may_return_many`](fetch_may_return_many.md),
  [`fetch_all_single_row`](fetch_all_single_row.md): the other call-site
  rules.
