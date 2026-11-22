# `fetch_all_single_row`

`fetch_all` (`fetchAll` in TypeScript) on a query that returns at most one
row.

| Level | Applies to | Fix |
|---|---|---|
| info | call sites | — |

## Example

```sql
CREATE TABLE users (id bigint PRIMARY KEY, email text NOT NULL UNIQUE, name text);
```

```rust
// src/users.rs
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

In TypeScript, `sql("SELECT id, name FROM users WHERE id = $id").fetchAll(pool, { id })`
gets the same finding, with `help: use .fetchOptional() (or .fetchOne())`.

## Why

The list is always empty or a singleton, and the caller unwraps it by hand
(`rows.first()`, `rows[0]`), usually without handling the empty case.
`fetch_optional` returns what the query can produce and makes the caller
handle "not found". A `fetch_all` on a key lookup may also be a list query
with the wrong filter.

## Reported when

`fetch_all()` / `fetch_all_as()` / `fetchAll()` is called on a query that
returns at most one row: a unique key fully pinned by the `WHERE`, an
aggregate without `GROUP BY`, `LIMIT 1`, a one-row `INSERT … VALUES`, an
`UPDATE` / `DELETE` by key, and the other cases listed in
[`fetch_may_return_many`](fetch_may_return_many.md#not-reported).

Each call is reported at its method name.

## Not reported

- A query that may return several rows, or one typedpg can't bound: through
  a view, a set-returning function, a `FULL JOIN`, a `$..spread` parameter.
- `fetch_stream` / `fetchStream`.
- Calls the scan doesn't see: Rust calls must be on the `sql!(…)` expression
  or a local `let` binding of it; TypeScript calls on `sql(…)`, a local
  binding, or an exported query (from a `.sql` file or a `sql(…)` binding)
  imported by name, alias or namespace.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
fetch_all_single_row = "warning"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "fetchAllSingleRow": "warning" } }
```

Allow one query with `-- typedpg-lint: allow(fetch_all_single_row)` in its
SQL; it covers every call of that query.
See [Configuring rules](README.md#configuring-rules).

## Related

- [`fetch_may_return_many`](fetch_may_return_many.md): the opposite
  mismatch.
- [`redundant_limit`](redundant_limit.md): a `LIMIT` on a query that
  already returns at most one row.
