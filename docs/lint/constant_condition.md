# `constant_condition`

Reports a condition that is always true or never true.

| | |
|---|---|
| Default level | warning |
| Applies to | queries |
| Fix | yes (`--fix`), only for a whole `WHERE` of a `SELECT` (or `HAVING` with `GROUP BY`) the planner folds to true: removes it |

## Why

A condition that is always true filters nothing; one that is never true keeps
no row, or, as an `OR` argument, is dead code. Neither is usually what the
author meant:

- `status = 'publsihed'` against `CHECK (status IN ('draft', 'published',
  'archived'))` is a typo that silently returns nothing; PostgreSQL does not
  check literals against `CHECK` constraints when reading.
- `views >= 0` under `CHECK (views >= 0)` tests nothing; maybe `> 0` was
  meant.
- `name = 'a' AND name = 'b'` can never hold; maybe `OR` was meant.
- `WHERE 1 = 1` is a leftover from string-built queries.

Only typedpg's analyzer can see the first three: they need the schema's
constraints and what the surrounding conditions already established.

## What it reports

Conditions in a `WHERE` (of a `SELECT`, `UPDATE` or `DELETE`), `JOIN … ON`,
`HAVING` and aggregate `FILTER (WHERE …)`, at any nesting level. Each
argument of a top-level `AND` is judged on its own, and the analyzer's proof
also looks inside `AND` / `OR` / `NOT` and at the parts of a `CASE WHEN`
condition. Two proofs decide:

- **The planner's.** A condition (a whole one, or an argument of its
  top-level `AND`) that PostgreSQL's `eval_const_expressions` certainly folds
  to a constant: immutable operators and functions of constants (`1 = 1`,
  `'a' <> 'b'`, `true`, `false`, `NULL`). The message is
  `condition is always true: it filters nothing` or
  `condition is never true`.
- **The analyzer's.** A built-in comparison (`=`, `<>`, `<`, `<=`, `>`,
  `>=`) or `IN` list decided by what is known of its operands where it is
  evaluated: a column's `CHECK` constraints, its domain or enum, and the
  conditions holding there (an earlier `AND` argument, an enclosing `WHERE`).
  A column compared with itself (`x = x`) is always true when it is never
  `NULL`, and `x <> x` never true. An `EXISTS` whose subquery always returns
  a row (an aggregate without `GROUP BY` or `HAVING`, a `SELECT` without
  `FROM` and `WHERE`, `VALUES`, with no `LIMIT`, `OFFSET` or set-returning
  call that could drop it) is always true. The message says what decides it,
  naming the constraint or domain when one alone does.

"Never true" covers both `FALSE` and `NULL`. "Always true" needs the operands
to be never `NULL`: `kind IN ('a', 'b')` on a nullable column constrained to
those values is `TRUE` or `NULL`, and is not reported.

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

SELECT id FROM posts WHERE 1 = 1;
SELECT id FROM posts WHERE status = 'publsihed';
SELECT id FROM posts WHERE user_id = $user_id AND views >= 0;
SELECT id FROM users WHERE name = 'a' AND name = 'b';
UPDATE posts SET title = $title WHERE true;
SELECT u.id FROM users u JOIN posts p ON p.user_id = u.id AND p.status = 'deleted';
```

```
warning[constant_condition]: condition is always true: it filters nothing
   ╭─[queries.sql:2:22]
 2 │ SELECT id FROM posts WHERE 1 = 1;
   ·                      ───────────
   ╰────
  help: remove it

warning[constant_condition]: `status` is always one of 'archived', 'draft', 'published' (CHECK constraint "posts_status_check"), so this condition is never true
   ╭─[queries.sql:5:28]
 5 │ SELECT id FROM posts WHERE status = 'publsihed';
   ·                            ────────────────────
   ╰────
  help: fix the condition (a mistyped value?), or remove it

warning[constant_condition]: `views` is always at least 0 (CHECK constraint "posts_views_check"), so this condition is always true
   ╭─[queries.sql:8:51]
 8 │ SELECT id FROM posts WHERE user_id = $user_id AND views >= 0;
   ·                                                   ──────────
   ╰────
  help: remove it, or fix what it was meant to test

warning[constant_condition]: `name` is always 'a' where this is evaluated, so this condition is never true
    ╭─[queries.sql:11:43]
 11 │ SELECT id FROM users WHERE name = 'a' AND name = 'b';
    ·                                           ──────────
    ╰────
   help: fix the condition (a mistyped value?), or remove it

warning[constant_condition]: condition is always true: it filters nothing
    ╭─[queries.sql:14:33]
 14 │ UPDATE posts SET title = $title WHERE true;
    ·                                 ──────────
    ╰────
   help: remove it; to write every row on purpose, drop the WHERE and allow it with -- typedpg-lint: allow(unbounded_write)

warning[constant_condition]: `p.status` is always one of 'archived', 'draft', 'published' (CHECK constraint "posts_status_check"), so this condition is never true
    ╭─[queries.sql:17:63]
 17 │ SELECT u.id FROM users u JOIN posts p ON p.user_id = u.id AND p.status = 'deleted';
    ·                                                               ────────────────────
    ╰────
   help: fix the condition (a mistyped value?), or remove it
```

`--fix` rewrites only the first one, to `SELECT id FROM posts`.

### Good

```sql
-- A value the CHECK allows, or a parameter: it may vary.
SELECT id FROM posts WHERE status = 'published';
SELECT id FROM posts WHERE status = $status;

-- The earlier condition doesn't settle the later one.
SELECT id FROM users WHERE name = 'a' OR name = 'b';

-- The ON true a LATERAL join needs is not reported.
SELECT u.id, s.n FROM users u
LEFT JOIN LATERAL (SELECT count(*) AS n FROM posts p WHERE p.user_id = u.id) s ON true;

-- views >= 0 allows views = 0: not always true.
SELECT id FROM posts WHERE views > 0;

-- user_id is nullable: NULL = NULL is NULL, so this is not always true
-- (it filters out the NULLs).
SELECT id FROM posts WHERE user_id = user_id;
```

## Fix

Only one case has a fix: a whole `WHERE` of a `SELECT` that the planner
folds to true, or a whole `HAVING` folded to true in a query with `GROUP BY`.
The fix deletes the clause keyword and the condition.

Everything else is reported without a fix:

- an always-true argument of an `AND` among others;
- a `HAVING` without `GROUP BY`: it makes the query grouped (one row for the
  whole input), so removing it would change the result;
- the `WHERE` of an `UPDATE` or `DELETE`: without it the statement writes
  every row, which [`unbounded_write`](unbounded_write.md) reports — the help
  says to drop the `WHERE` and allow that rule if this is intended;
- a `JOIN … ON` or `FILTER` condition;
- a condition that is never true: what the query meant is not what it does;
- anything only the analyzer's facts decide.

Every fix is also re-analyzed before it is offered and must keep the result
columns' names, types, nullability and value refinements, and the parameters,
unchanged; a fix that doesn't is withheld and reported as an
`internal_error` (a typedpg bug).

## When it stays silent

- Anything that may depend on a row, a parameter or the session
  (`random() > 0.5`, `now() > '2020-01-01'`, `$1 = 1`).
- A column of a table with a `NOT VALID` `CHECK` constraint: rows older than
  the constraint may hold any value.
- A column under a nondeterministic collation, where `'A'` may equal `'a'`.
- An enum compared with one of its own labels.
- The bare `ON true` of a join.
- A comparison with a `NULL` literal: [`null_comparison`](null_comparison.md)
  reports it.
- `IS [NOT] NULL` tests: [`redundant_null_check`](redundant_null_check.md)
  reports those.
- A `CASE WHEN` condition decided as a whole:
  [`unreachable_case_branch`](unreachable_case_branch.md) reports the branch.
  This rule reports only the decided parts of a `WHEN` that is not decided as
  a whole (`WHEN id = $1 OR status = 'x'`).
- When a node is analyzed more than once (a CTE read twice), it is decided
  only if every analysis agrees.
- Migrations are not read: this rule runs over queries only.

## Configuration

Rust (`Cargo.toml`):

```toml
[package.metadata.typedpg.lint]
constant_condition = "error"   # "error", "warning" (default), "info" or "off"
fail_on = "warning"            # fail the run on warnings too (default "error")
ignore = ["tests/**"]          # query files whose findings are dropped
```

TypeScript (`typedpg.config.json`):

```json
{
  "lint": {
    "constantCondition": "error",
    "failOn": "warning",
    "ignore": ["tests/**"]
  }
}
```

At the default `fail_on` / `failOn` of `"error"`, a warning is printed but
does not fail the run.

To allow one query, put the comment in the query text (the `sql!` / `sql()`
string, or the statement in a `.sql` file):

```sql
-- typedpg-lint: allow(constant_condition)
SELECT id FROM posts WHERE 1 = 1;
```

## Related rules

- [`unreachable_case_branch`](unreachable_case_branch.md): a `CASE` branch
  whose condition is decided the same way.
- [`null_comparison`](null_comparison.md): comparisons with a `NULL` literal.
- [`redundant_null_check`](redundant_null_check.md): `IS [NOT] NULL` on a
  value never `NULL`.
- [`unbounded_write`](unbounded_write.md): an `UPDATE` / `DELETE` without
  `WHERE`.
- [`check_violation`](check_violation.md): writing a constant a `CHECK`
  rejects.
- [`cartesian_join`](cartesian_join.md): a `JOIN … ON` that doesn't relate
  its sides.
