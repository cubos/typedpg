# `unused_returning`

`execute()` on an `INSERT`, `UPDATE`, `DELETE` or `MERGE` whose `RETURNING`
rows it discards.

| Level | Applies to | Fix |
|---|---|---|
| info | call sites | removes the `RETURNING` clause |

## Example

```sql
CREATE TABLE users (id bigint PRIMARY KEY, email text NOT NULL UNIQUE, name text);
```

```ts
// src/users.ts
import { sql } from "./db.ts";

export async function createUser(pool: Pool, id: number, email: string) {
  await sql("INSERT INTO users (id, email) VALUES ($id, $email) RETURNING id").execute(pool, { id, email });
}

export async function rename(pool: Pool, id: number, name: string) {
  await sql("UPDATE users SET name = $name WHERE id = $id RETURNING lower(name)").execute(pool, { id, name });
}
```

```
info[unused_returning]: `.execute()` discards the RETURNING rows
   ╭─[src/users.ts:4:80]
 4 │   await sql("INSERT INTO users (id, email) VALUES ($id, $email) RETURNING id").execute(pool, { id, email });
   ·                                                                                ───────
   ╰────
  help: remove RETURNING, or fetch the rows
```

`--fix` turns the first query into
`INSERT INTO users (id, email) VALUES ($id, $email)`. The second is reported
without a fix: `lower(name)` is not a plain column. The same applies to
`sql!(…).execute()` in Rust.

## Why

PostgreSQL still evaluates and sends the `RETURNING` rows, and typedpg
generates a row type for them. It is usually a leftover, or a sign the code
meant to fetch the new id and forgot.

## Reported when

The statement has a top-level `RETURNING` and every call on the query is
`execute()`. Each call is reported. The query must be used where every use
is visible:

- Rust: methods called on the `sql!(…)` expression or on a local `let`
  binding of it.
- TypeScript: inline `sql(…).execute(…)` or a local, non-exported binding.

## Not reported

- Any call that reads rows (`fetch_one`, `fetchOne`, `fetch_stream`, …).
- A query passed to a function, borrowed, or with a method call typedpg
  doesn't recognize.
- An exported `sql(…)` binding, and every `.sql` file query (each is an
  export usable anywhere).
- A `RETURNING` inside a data-modifying CTE (the statement reads it).

## Fix not offered

- The query is executed in more than one place.
- A `RETURNING` item is anything but a column (`id`, `t.id`, `*`, `t.*`) or a
  plain constant: an expression, cast or function call can raise an error,
  and removing it could make a failing statement succeed.
- The statement is a `MERGE` or has a `WITH` clause.
- Its target or a `FROM` / `USING` item is not an ordinary table (a view, a
  partitioned table, …), or has row-level security, rewrite rules or a
  virtual generated column.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
unused_returning = "warning"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "unusedReturning": "warning" } }
```

Allow one query with `-- typedpg-lint: allow(unused_returning)` in its SQL.
See [Configuring rules](README.md#configuring-rules).

## Related

- [`fetch_may_return_many`](fetch_may_return_many.md): `fetch_one` on a
  query that may return many rows.
- [`fetch_all_single_row`](fetch_all_single_row.md): `fetch_all` on a
  single-row query.
