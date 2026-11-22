# `null_comparison`

A comparison with a `NULL` literal, which is never true.

| Level | Applies to | Fix |
|---|---|---|
| error | queries | — |

## Example

```sql
CREATE TABLE users (id bigint PRIMARY KEY, name text, deleted_at timestamptz);

SELECT id FROM users WHERE deleted_at = NULL;
SELECT id FROM users WHERE id NOT IN (1, 2, NULL);
SELECT id, CASE name WHEN NULL THEN 'anonymous' ELSE name END AS label FROM users;
```

```
error[null_comparison]: comparison with NULL is never true
   ╭─[queries.sql:2:28]
 2 │ SELECT id FROM users WHERE deleted_at = NULL;
   ·                            ─────────────────
   ╰────
  help: use IS NULL / IS NOT NULL

error[null_comparison]: NOT IN with a NULL in the list is never true
   ╭─[queries.sql:4:28]
 4 │ SELECT id FROM users WHERE id NOT IN (1, 2, NULL);
   ·                            ──────────────────────
   ╰────
  help: remove the NULL from the list

error[null_comparison]: CASE ... WHEN NULL never matches
   ╭─[queries.sql:6:22]
 6 │ SELECT id, CASE name WHEN NULL THEN 'anonymous' ELSE name END AS label FROM users;
   ·                      ─────────
   ╰────
  help: use CASE WHEN x IS NULL
```

## Why

`x = NULL` is `NULL` for every row, never true, so the query silently
matches nothing. `NOT IN (…, NULL)` is never true for the same reason, and
`CASE x WHEN NULL` never matches, not even when `x` is `NULL`. Use
`IS [NOT] NULL` or `IS [NOT] DISTINCT FROM`.

## Reported when

Anywhere in the query, including subqueries, CTEs and `RETURNING`:

- `=`, `<>`, `!=`, `<`, `>`, `<=` or `>=` (also as `OPERATOR(pg_catalog.=)`)
  with a `NULL` literal on either side;
- `NOT IN (…)` whose list contains a `NULL` literal;
- a simple `CASE x WHEN NULL`.

A cast of `NULL` that keeps it `NULL` still counts: `id = NULL::int`.

## Not reported

- A parameter or a nullable column, even when it may be `NULL` at run time:
  `deleted_at = $deleted_at`.
- `IN (1, 2, NULL)`, `= ANY (…)`, `NULLIF(x, NULL)`, `COALESCE(x, NULL)`.
- A cast that may turn `NULL` into a value (a cast function not declared
  `STRICT`) or a cast to a domain.
- A comparison through an operator declared `CALLED ON NULL INPUT`, which may
  accept `NULL`.
- `NOT IN (SELECT …)`: see [`not_in_nullable`](not_in_nullable.md).

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
null_comparison = "warning"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "nullComparison": "warning" } }
```

Allow one query with `-- typedpg-lint: allow(null_comparison)` in its text.
See [Configuring rules](README.md#configuring-rules).

## Related

- [`not_in_nullable`](not_in_nullable.md): `NOT IN (SELECT …)` where a side
  may be `NULL`.
- [`redundant_null_check`](redundant_null_check.md): `IS [NOT] NULL` on a
  value that is never `NULL`.
- [`constant_condition`](constant_condition.md): other conditions that are
  always or never true.
