# `null_comparison`

Reports a comparison with a `NULL` literal, which is never true.

| | |
|---|---|
| Default level | error |
| Applies to | queries |
| Fix | no |

## Why

In SQL, `NULL` means "unknown", and almost every operator returns `NULL` when
one operand is `NULL`. `deleted_at = NULL` is therefore `NULL` for every row,
never `TRUE`, so a `WHERE deleted_at = NULL` returns no rows, and a
`WHERE deleted_at <> NULL` returns no rows either. The query runs without an
error and returns an empty result, so the bug is easy to miss.

The same applies to two shapes that compare with `=` internally:

- `x NOT IN (1, 2, NULL)` is `x <> 1 AND x <> 2 AND x <> NULL`. The last term
  is never true, so the whole condition is never true and the query keeps no
  row.
- `CASE x WHEN NULL THEN …` compares `x = NULL`, so that branch is never
  taken, not even when `x` is `NULL`.

To test for `NULL`, use `IS NULL`, `IS NOT NULL` or
`IS [NOT] DISTINCT FROM`.

## What it reports

Anywhere in the query (the select list, `WHERE`, `JOIN … ON`, `HAVING`,
subqueries, CTEs, data-modifying CTEs, `RETURNING`):

- an operator expression using `=`, `<>`, `!=`, `<`, `>`, `<=` or `>=`
  (`OPERATOR(pg_catalog.=)` included) with a `NULL` literal on either side;
- `x NOT IN (…)` whose value list contains a `NULL` literal;
- a simple `CASE x WHEN NULL …`.

A `NULL` literal under casts still counts (`id = NULL::int`) when typedpg can
prove the cast keeps the value `NULL`: the bare `NULL` literal cast to a type,
a binary-coercible or I/O cast, a cast function declared `STRICT`, or a
conversion between built-in types. It does not count when the cast goes
through a function that is not `STRICT` (`CALLED ON NULL INPUT`, which may
turn `NULL` into a value), when the target is a domain (its constraints run on
the result), or when the cast can't be resolved.

The rule reads the catalog for the operators' implementations: if any operator
with that name (`=`, `<`, …) has a non-strict implementation function, a
comparison through that operator name may legitimately accept `NULL`, and the
rule doesn't report it.

## Examples

### Bad

```sql
CREATE TABLE users (
    id bigint PRIMARY KEY,
    email text NOT NULL UNIQUE,
    name text,
    deleted_at timestamptz
);

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
   ╭─[queries.sql:5:28]
 5 │ SELECT id FROM users WHERE id NOT IN (1, 2, NULL);
   ·                            ──────────────────────
   ╰────
  help: remove the NULL from the list

error[null_comparison]: CASE ... WHEN NULL never matches
   ╭─[queries.sql:8:22]
 8 │ SELECT id, CASE name WHEN NULL THEN 'anonymous' ELSE name END AS label FROM users;
   ·                      ─────────
   ╰────
  help: use CASE WHEN x IS NULL
```

### Good

```sql
-- The NULL tests.
SELECT id FROM users WHERE deleted_at IS NULL;
SELECT id FROM users WHERE deleted_at IS NOT DISTINCT FROM $deleted_at;

-- A parameter is not a NULL literal: it is not reported, even though it may
-- be NULL at run time.
SELECT id FROM users WHERE deleted_at = $deleted_at;

-- IN with a NULL still matches 1 and 2: only NOT IN is never true.
SELECT id FROM users WHERE id IN (1, 2, NULL);

-- Instead of CASE name WHEN NULL.
SELECT id, coalesce(name, 'anonymous') AS label FROM users;
```

## When it stays silent

- Only a `NULL` *literal* is reported. A parameter, a nullable column or an
  expression that may be `NULL` is not: `deleted_at = $deleted_at` with a
  `NULL` argument also matches nothing, and typedpg can't know the argument.
- `IS [NOT] DISTINCT FROM`, `IN (…, NULL)`, `= ANY (…)`, `NULLIF(x, NULL)`
  and `COALESCE(x, NULL)` are not reported.
- `NOT IN (SELECT …)` over a subquery that may return `NULL` is
  [`not_in_nullable`](not_in_nullable.md)'s, not this rule's.
- A cast that may turn `NULL` into a value, or one into a domain (see above).
- A comparison through an operator name that has a non-strict implementation
  anywhere in the catalog: one user-defined non-strict `=` (on any type)
  silences the rule for every `=` comparison.
- Migrations are not read: this rule runs over queries only.

## Configuration

Rust (`Cargo.toml`):

```toml
[package.metadata.typedpg.lint]
null_comparison = "warning"   # "error" (default), "warning", "info" or "off"
fail_on = "error"             # the severity the run fails on (default)
ignore = ["tests/**"]         # query files whose findings are dropped
```

TypeScript (`typedpg.config.json`):

```json
{
  "lint": {
    "nullComparison": "warning",
    "failOn": "error",
    "ignore": ["tests/**"]
  }
}
```

The run fails when a finding reaches `fail_on` / `failOn`; at its default,
`"error"`, this rule fails the run unless you lower its level.

To allow one query, put the comment in the query text (the `sql!` / `sql()`
string, or the statement in a `.sql` file):

```sql
-- typedpg-lint: allow(null_comparison)
SELECT id FROM users WHERE deleted_at = NULL;
```

## Related rules

- [`not_in_nullable`](not_in_nullable.md): `NOT IN (SELECT …)` where a side
  may be `NULL`.
- [`redundant_null_check`](redundant_null_check.md): `IS [NOT] NULL` on a
  value that is never `NULL`.
- [`constant_condition`](constant_condition.md): other conditions that are
  always or never true (it leaves `NULL`-literal comparisons to this rule).
