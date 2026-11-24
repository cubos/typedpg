# `not_in_nullable`

A `NOT IN (SELECT …)` or `<> ALL (SELECT …)` whose left side or subquery
may be `NULL`, which makes it return no rows.

| Category | Level | Applies to | Fix |
|---|---|---|---|
| correctness | error | queries | — |

## Example

```sql
CREATE TABLE users (id bigint PRIMARY KEY, email text NOT NULL, name text);
CREATE TABLE posts (id bigint PRIMARY KEY, user_id bigint REFERENCES users, title text NOT NULL);

SELECT id FROM users WHERE id NOT IN (SELECT user_id FROM posts);
SELECT id FROM users WHERE name NOT IN (SELECT title FROM posts);
SELECT id FROM users WHERE name <> ALL (SELECT user_id::text FROM posts);
```

```
error[correctness/not_in_nullable]: NOT IN (SELECT …) returns no rows when the subquery returns a NULL
   ╭─[queries.sql:2:28]
 2 │ SELECT id FROM users WHERE id NOT IN (SELECT user_id FROM posts);
   ·                            ─────────────────────────────────────
   ╰────
  help: use NOT EXISTS (SELECT 1 FROM … WHERE …), or make both sides NOT NULL

error[correctness/not_in_nullable]: NOT IN (SELECT …) returns no rows for a NULL left side when the subquery returns any row
   ╭─[queries.sql:4:28]
 4 │ SELECT id FROM users WHERE name NOT IN (SELECT title FROM posts);
   ·                            ─────────────────────────────────────
   ╰────
  help: use NOT EXISTS (SELECT 1 FROM … WHERE …), or make both sides NOT NULL

error[correctness/not_in_nullable]: <> ALL (SELECT …) returns no rows for a NULL left side when the subquery returns any row, or when the subquery returns a NULL
   ╭─[queries.sql:6:28]
 6 │ SELECT id FROM users WHERE name <> ALL (SELECT user_id::text FROM posts);
   ·                            ─────────────────────────────────────────────
   ╰────
  help: use NOT EXISTS (SELECT 1 FROM … WHERE …), or make both sides NOT NULL
```

## Why

`x NOT IN (SELECT y …)` is `x <> y1 AND x <> y2 AND …`. One `NULL` from the
subquery makes the whole condition `NULL`, so no row is kept: the day one
post has no author, "users without posts" comes back empty. A `NULL` `x`
drops its row whenever the subquery returns anything. `NOT EXISTS (SELECT 1
FROM posts p WHERE p.user_id = u.id)` has neither problem.

## Reported when

- `x NOT IN (SELECT …)` or `x <> ALL (SELECT …)` with a single-value `x`;
- and `x` or the subquery's output may be `NULL`, judged by the same
  nullability that types query results (`NOT NULL` columns, outer joins,
  `WHERE … IS NOT NULL` in the subquery).

The message says which side may be `NULL`.

## Not reported

- Both sides proven `NOT NULL`:
  `id NOT IN (SELECT user_id FROM posts WHERE user_id IS NOT NULL)`.
- A row-valued left side: `(id, email) NOT IN (SELECT user_id, title FROM posts)`.
- `NOT (x <> ALL (SELECT …))`, other operators with `ANY` / `ALL`.
- A comparison through an operator declared `CALLED ON NULL INPUT`.
- `NOT IN (1, 2, NULL)` with a literal list: see
  [`null_comparison`](null_comparison.md).

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
not_in_nullable = "error"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "notInNullable": "error" } }
```

Allow one query with `-- typedpg-lint: allow(not_in_nullable)` in its text.
See [Configuring rules](README.md#configuring-rules).

## Related

- [`null_comparison`](null_comparison.md): `NOT IN` with a `NULL` literal in
  its list.
- [`outer_reference_in_subquery`](outer_reference_in_subquery.md): the other
  classic `IN (SELECT …)` trap.
