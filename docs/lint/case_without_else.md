# `case_without_else`

Reports a `CASE` without `ELSE` whose `WHEN`s may all fail for some row,
which makes it `NULL` there.

| | |
|---|---|
| Default level | warning |
| Applies to | queries |
| Fix | yes (`--fix`): writes `ELSE NULL` before the `END` |

## Why

A `CASE` without `ELSE` has an implicit `ELSE NULL`. When no `WHEN` matches,
the result is `NULL`, which is easy to miss when reading the query: a badge
column that is `NULL` for most rows, a sum that silently skips them, a
comparison that turns `NULL` and drops the row. Writing `ELSE NULL` makes the
choice visible; otherwise, add the case that was forgotten.

## What it reports

Every `CASE` (searched `CASE WHEN …` or simple `CASE x WHEN …`) without
`ELSE`, when the analyzer finds that its implicit `NULL` may be reached:
no `WHEN` certainly folds to `TRUE`, and the analyzer can't prove that the
`WHEN`s cover every row. The proof uses the constraints and conditions around
the `CASE`: `WHEN views >= 1000 … WHEN views < 1000 …` over a `NOT NULL`
column, `WHEN x IS NULL … WHEN x IS NOT NULL …`, or one `WHEN` per value a
`CHECK` allows leave nothing to the `ELSE`.

## Examples

### Bad

```sql
CREATE TABLE users (
    id bigint PRIMARY KEY,
    email text NOT NULL UNIQUE,
    name text,
    deleted_at timestamptz
);
CREATE TABLE posts (
    id bigint PRIMARY KEY,
    user_id bigint REFERENCES users,
    title text NOT NULL,
    status text NOT NULL CHECK (status IN ('draft', 'published', 'archived')),
    views integer NOT NULL DEFAULT 0 CHECK (views >= 0)
);

SELECT id, CASE WHEN views > 1000 THEN 'popular' END AS badge FROM posts;
SELECT id, CASE status WHEN 'draft' THEN 1 WHEN 'published' THEN 2 END AS rank FROM posts;
```

```
warning[case_without_else]: CASE without ELSE is NULL when no WHEN matches
   ╭─[queries.sql:2:12]
 2 │ SELECT id, CASE WHEN views > 1000 THEN 'popular' END AS badge FROM posts;
   ·            ─────────────────────────────────────────
   ╰────
  help: write ELSE NULL if that is intended, or add the missing case

warning[case_without_else]: CASE without ELSE is NULL when no WHEN matches
   ╭─[queries.sql:5:12]
 5 │ SELECT id, CASE status WHEN 'draft' THEN 1 WHEN 'published' THEN 2 END AS rank FROM posts;
   ·            ───────────────────────────────────────────────────────────
   ╰────
  help: write ELSE NULL if that is intended, or add the missing case
```

`--fix` rewrites them to `… THEN 'popular' ELSE NULL END …` and
`… WHEN 'published' THEN 2 ELSE NULL END …`.

### Good

```sql
-- The NULL is written out.
SELECT id, CASE WHEN views > 1000 THEN 'popular' ELSE NULL END AS badge FROM posts;

-- views is NOT NULL: one of the two WHENs always matches.
SELECT id, CASE WHEN views >= 1000 THEN 'popular' WHEN views < 1000 THEN 'quiet' END AS badge FROM posts;

-- One WHEN per value the CHECK allows, searched or simple.
SELECT id, CASE WHEN status = 'draft' THEN 1 WHEN status = 'published' THEN 2 WHEN status = 'archived' THEN 3 END AS rank FROM posts;
SELECT id, CASE status WHEN 'draft' THEN 1 WHEN 'published' THEN 2 WHEN 'archived' THEN 3 END AS rank FROM posts;

-- NULL or not NULL: always one.
SELECT id, CASE WHEN name IS NULL THEN 'anonymous' WHEN name IS NOT NULL THEN name END AS label FROM users;
```

## Fix

The fix inserts `ELSE NULL ` before the `CASE`'s `END`. An explicit `NULL`
literal is the same untyped input as the implicit one, so the result type and
every row stay the same. Every fix is also re-analyzed before it is offered
and must keep the result columns' names, types, nullability and value
refinements, and the parameters, unchanged; a fix that doesn't is withheld and
reported as an `internal_error` (a typedpg bug).

## When it stays silent

- A `CASE` with any `ELSE`, including `ELSE NULL`.
- A `CASE` the analyzer proves exhaustive (see above), or with a `WHEN` that
  folds to `TRUE`.
- A `CASE` the analyzer has no fact for.
- Migrations are not read: this rule runs over queries only.

## Configuration

Rust (`Cargo.toml`):

```toml
[package.metadata.typedpg.lint]
case_without_else = "off"   # "error", "warning" (default), "info" or "off"
fail_on = "error"           # the severity the run fails on (default)
ignore = ["tests/**"]       # query files whose findings are dropped
```

TypeScript (`typedpg.config.json`):

```json
{
  "lint": {
    "caseWithoutElse": "off",
    "failOn": "error",
    "ignore": ["tests/**"]
  }
}
```

At the default `fail_on` / `failOn` of `"error"`, a warning is printed but
does not fail the run.

To allow one query, put the comment in the query text (the `sql!` / `sql()`
string, or the statement in a `.sql` file):

```sql
-- typedpg-lint: allow(case_without_else)
SELECT id, CASE WHEN views > 1000 THEN 'popular' END AS badge FROM posts;
```

## Related rules

- [`unreachable_case_branch`](unreachable_case_branch.md): a `CASE` branch
  that is never taken.
- [`simplifiable_case`](simplifiable_case.md): a `CASE` that is a
  `COALESCE` or its own condition.
