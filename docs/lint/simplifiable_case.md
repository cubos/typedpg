# `simplifiable_case`

A `CASE` that is `COALESCE(x, y)`, or just its own condition.

| Level | Applies to | Fix |
|---|---|---|
| info | queries | rewrites it to `COALESCE` or the condition |

## Example

```sql
CREATE TABLE users (id bigint PRIMARY KEY, email text NOT NULL, name text);
CREATE TABLE posts (id bigint PRIMARY KEY, user_id bigint NOT NULL, score int);

SELECT CASE WHEN name IS NULL THEN email ELSE name END AS display FROM users;
SELECT id, CASE WHEN user_id > 10 THEN true ELSE false END AS big FROM posts;
SELECT id, CASE WHEN score > 10 THEN true ELSE false END AS popular FROM posts;
```

```
info[simplifiable_case]: this CASE is COALESCE(name, email)
   ╭─[queries.sql:2:8]
 2 │ SELECT CASE WHEN name IS NULL THEN email ELSE name END AS display FROM users;
   ·        ───────────────────────────────────────────────
   ╰────
  help: write COALESCE(name, email)

info[simplifiable_case]: CASE WHEN … THEN true ELSE false END is its condition
   ╭─[queries.sql:6:12]
 6 │ SELECT id, CASE WHEN score > 10 THEN true ELSE false END AS popular FROM posts;
   ·            ─────────────────────────────────────────────
   ╰────
  help: write (score > 10) IS TRUE: the CASE is false where the condition is NULL
```

`--fix` gives `SELECT COALESCE(name, email) AS display FROM users` and
`SELECT id, (user_id > 10) AS big FROM posts`. The third query keeps its
finding without a fix.

## Why

The longhand makes the reader check that both `x`s match and that the
branches are `true` then `false`. The boolean form also hides that a NULL
condition becomes `false`; `c IS TRUE` says so.

## Reported when

A `CASE` with one `WHEN` and an `ELSE`, of either shape:

- `CASE WHEN x IS NULL THEN y ELSE x END`, where `x` is the same column or
  parameter (possibly cast) in both places, and not a whole row.
- `CASE WHEN c THEN true ELSE false END`. When `c` may be NULL (`score` is
  nullable), the help suggests `(c) IS TRUE` instead.

## Not reported

- A simple `CASE x WHEN …`, or more than one `WHEN`.
- `x` is an expression: `CASE WHEN lower(name) IS NULL THEN email ELSE lower(name) END`.
- Other shapes: `CASE WHEN name IS NOT NULL THEN name ELSE email END`,
  `THEN false ELSE true`, `ELSE NULL` (see
  [`case_without_else`](case_without_else.md)).
- The two `x`s resolve to different columns (same name in two scopes).

## Fix not offered

- The condition may be NULL.
- The `CASE` is a whole output column without an alias: its column is named
  `case`, and the rewrite would rename it. Add `AS` and the fix applies.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
simplifiable_case = "warning"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "simplifiableCase": "warning" } }
```

Allow one query with `-- typedpg-lint: allow(simplifiable_case)` in its
text. See [Configuring rules](README.md#configuring-rules).

## Related

- [`redundant_null_check`](redundant_null_check.md): `IS NULL` or `COALESCE`
  on a value that is never NULL.
- [`case_without_else`](case_without_else.md): a `CASE` that may fall
  through to NULL.
- [`unreachable_case_branch`](unreachable_case_branch.md): a branch that is
  never taken.
