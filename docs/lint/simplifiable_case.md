# `simplifiable_case`

A `CASE` that is just `COALESCE(x, y)`, or just its own condition.

| | |
|---|---|
| Default level | info |
| Applies to | queries |
| Fix | yes (`--fix`): rewrites it to `COALESCE(x, y)` or to the condition, when that is provably the same value |

## Why

`CASE WHEN x IS NULL THEN y ELSE x END` and `CASE WHEN c THEN true ELSE false
END` are longhand for things SQL already has a word for. The longer form is
harder to read: a reader has to check that both `x`s are the same and that
the branches really are `true` / `false`, in that order.

The boolean form also hides a NULL rule. When `c` is NULL, the `CASE` goes
to its `ELSE` and returns `false`, while `c` itself would be NULL. So the
`CASE` is only "its condition" when the condition can never be NULL. When it
can, the honest spelling is `c IS TRUE`, and the rule says so instead of
offering a rewrite that would change results.

## What it reports

A searched `CASE` (not `CASE x WHEN …`) with exactly one `WHEN` and an
`ELSE`, of one of two shapes:

1. **`CASE WHEN x IS NULL THEN y ELSE x END`**, which is `COALESCE(x, y)`.
   `x` must be a column reference or a parameter (possibly cast, as in
   `$1::text`), written with the same tokens in the `WHEN` and the `ELSE`
   and, for a column, resolving to the same column of the same relation in
   both places. It must not be a row: `row IS NULL` is true for a row whose
   fields are all NULL, while `COALESCE` tests the row value itself. Both
   forms evaluate `y` only when `x` is NULL, and PostgreSQL resolves their
   result type from the same two expressions.
2. **`CASE WHEN c THEN true ELSE false END`**, with the literals `true` and
   `false` in that order. The rule asks the analyzer whether `c` can be NULL
   at that point (from column nullability, `NOT NULL` constraints and the
   conditions around it):
   - never NULL: the `CASE` is `c`, and the fix rewrites it;
   - may be NULL: the finding is still reported, with a help line that
     suggests `(c) IS TRUE` and no fix.

The finding spans the whole `CASE … END`.

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
    score int,
    created_at timestamptz NOT NULL
);
```

### Bad

```sql
SELECT CASE WHEN name IS NULL THEN email ELSE name END AS display FROM users;
```

```
info[simplifiable_case]: this CASE is COALESCE(name, email)
   ╭─[queries.sql:1:8]
 1 │ SELECT CASE WHEN name IS NULL THEN email ELSE name END AS display FROM users;
   ·        ───────────────────────────────────────────────
   ╰────
  help: write COALESCE(name, email)
```

`--fix` writes `SELECT COALESCE(name, email) AS display FROM users;`.

`user_id` is `NOT NULL`, so `user_id > 10` is never NULL:

```sql
SELECT id, CASE WHEN user_id > 10 THEN true ELSE false END AS big FROM posts;
```

```
info[simplifiable_case]: CASE WHEN … THEN true ELSE false END is its condition
   ╭─[queries.sql:1:12]
 1 │ SELECT id, CASE WHEN user_id > 10 THEN true ELSE false END AS big FROM posts;
   ·            ───────────────────────────────────────────────
   ╰────
  help: write user_id > 10
```

`--fix` writes `SELECT id, (user_id > 10) AS big FROM posts;`.

`score` is nullable, so the `CASE` maps NULL to `false` and is not the same as
`score > 10`. It is reported without a fix:

```sql
SELECT id, CASE WHEN score > 10 THEN true ELSE false END AS popular FROM posts;
```

```
info[simplifiable_case]: CASE WHEN … THEN true ELSE false END is its condition
   ╭─[queries.sql:1:12]
 1 │ SELECT id, CASE WHEN score > 10 THEN true ELSE false END AS popular FROM posts;
   ·            ─────────────────────────────────────────────
   ╰────
  help: write (score > 10) IS TRUE: the CASE is false where the condition is NULL
```

### Good

```sql
SELECT COALESCE(name, email) AS display FROM users;

SELECT id, (score > 10) IS TRUE AS popular FROM posts;

-- IS NOT NULL with the branches swapped: not one of the two shapes.
SELECT CASE WHEN name IS NOT NULL THEN name ELSE email END AS display FROM users;

-- The tested value is an expression, not a column or a parameter.
SELECT CASE WHEN lower(name) IS NULL THEN email ELSE lower(name) END AS display
FROM users;
```

Also not reported: a `CASE` with more than one `WHEN`, `THEN false ELSE true`,
an `ELSE` that is a different column, `ELSE NULL` (that one is for
[`case_without_else`](case_without_else.md) to discuss), and
`CASE WHEN u IS NULL THEN … ELSE u END` where `u` is a whole row.

## Fix

The `COALESCE` shape is replaced by `COALESCE(x, y)`, with `x` and `y`
copied as written. The boolean shape is replaced by the condition as written,
wrapped in parentheses unless it is a single column, function call or
constant.

The fix is withheld, and only the finding is reported, when:

- the boolean condition may be NULL (the help suggests `IS TRUE` instead);
- the `CASE` is a whole output column with no alias. PostgreSQL names that
  column `case`; the rewrite would rename it to `coalesce` or to the
  condition's name. Add an `AS` alias and the fix applies.

Every fix is checked once more before it is offered: the edited query must
analyze to the same result columns (names, types, nullability and value
refinements) and parameters. A fix that fails is a typedpg bug; it is
withheld and reported as an `internal_error` warning.

## When it stays silent

- A simple `CASE x WHEN … END`.
- `x` is anything other than a column reference or a parameter (a function
  call, an operator), or is written differently in the two places.
- The two `x` column references resolve to different columns (for example
  the same name in two scopes).

## Configuration

```toml
[package.metadata.typedpg.lint]
simplifiable_case = "warning"   # "error", "warning", "info" or "off"
```

```json
{
  "lint": { "simplifiableCase": "warning" }
}
```

`fail_on` / `failOn` sets the severity that fails the run (default `error`),
and `ignore` lists glob patterns of files whose query findings are dropped.
For one statement:

```sql
-- typedpg-lint: allow(simplifiable_case)
SELECT CASE WHEN name IS NULL THEN email ELSE name END AS display FROM users;
```

## Related rules

- [`redundant_null_check`](redundant_null_check.md): `IS NULL` or `COALESCE`
  on a value that is never NULL.
- [`case_without_else`](case_without_else.md),
  [`unreachable_case_branch`](unreachable_case_branch.md): other `CASE`
  problems.
