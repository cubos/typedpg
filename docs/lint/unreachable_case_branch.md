# `unreachable_case_branch`

Reports the branches of a searched `CASE` that are never taken.

| | |
|---|---|
| Default level | warning |
| Applies to | queries |
| Fix | no |

## Why

A branch that no row can take is dead code, and usually a bug: a mistyped
value (`WHEN status = 'deleted'` when the column can only be `'draft'`,
`'published'` or `'archived'`), a duplicated `WHEN`, or `WHEN`s in the wrong
order so that a broad condition shadows a narrower one after it
(`WHEN views >= 0 … WHEN views > 100 …`). The query runs, and the branch the
author cared about silently never fires.

## What it reports

In a searched `CASE WHEN cond THEN …` (not a simple `CASE x WHEN v …`):

- a `WHEN` whose condition is never true (`FALSE` or `NULL`): that branch;
- a `WHEN` whose condition is always true: every branch after it, `ELSE`
  included (nothing is reported when it is the last branch).

A condition is decided by the same two proofs as
[`constant_condition`](constant_condition.md):

- the planner certainly folds it to a constant (`WHEN false`, `WHEN 1 = 2`,
  `WHEN NULL`);
- the analyzer decides it from what it knows of the operands: a column's
  `CHECK` constraints, domain or enum, the conditions around the `CASE`
  (its query's `WHERE`), and the `WHEN`s before it, which are known not to
  have been taken. `AND` / `OR` / `NOT` combinations of decided parts are
  decided too.

The message says why when one constraint, domain or earlier condition
decides it.

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

SELECT id, CASE WHEN status = 'deleted' THEN 'gone' ELSE 'here' END AS state FROM posts;
SELECT id, CASE WHEN status = 'draft' THEN 1 WHEN status = 'draft' THEN 2 ELSE 3 END AS rank FROM posts;
SELECT id, CASE WHEN status IN ('draft', 'published') THEN 'live' WHEN status = 'archived' THEN 'old' ELSE 'unknown' END AS state FROM posts;
SELECT id, CASE WHEN views >= 0 THEN 'counted' WHEN views > 100 THEN 'hot' END AS heat FROM posts;
```

```
warning[unreachable_case_branch]: CASE branch is never taken: its condition is never true (`status` is always one of 'archived', 'draft', 'published' (CHECK constraint "posts_status_check"))
   ╭─[queries.sql:2:17]
 2 │ SELECT id, CASE WHEN status = 'deleted' THEN 'gone' ELSE 'here' END AS state FROM posts;
   ·                 ───────────────────────────────────
   ╰────
  help: remove the branch, or fix its condition

warning[unreachable_case_branch]: CASE branch is never taken: with the WHENs before it not taken, its condition is never true
   ╭─[queries.sql:5:46]
 5 │ SELECT id, CASE WHEN status = 'draft' THEN 1 WHEN status = 'draft' THEN 2 ELSE 3 END AS rank FROM posts;
   ·                                              ────────────────────────────
   ╰────
  help: remove the branch, or fix its condition

warning[unreachable_case_branch]: CASE branches after this WHEN are never taken: every row takes it or a branch before it
   ╭─[queries.sql:8:103]
 8 │ SELECT id, CASE WHEN status IN ('draft', 'published') THEN 'live' WHEN status = 'archived' THEN 'old' ELSE 'unknown' END AS state FROM posts;
   ·                                                                                                       ──────────────
   ╰────
  help: remove them, or replace the CASE with the result of its always-true branch

warning[unreachable_case_branch]: CASE branches after an always-true WHEN are never taken (`views` is always at least 0 (CHECK constraint "posts_views_check"))
    ╭─[queries.sql:11:48]
 11 │ SELECT id, CASE WHEN views >= 0 THEN 'counted' WHEN views > 100 THEN 'hot' END AS heat FROM posts;
    ·                                                ───────────────────────────
    ╰────
   help: remove them, or replace the CASE with the result of its always-true branch
```

### Good

```sql
-- Either branch may be taken.
SELECT id, CASE WHEN views > 100 THEN 'hot' ELSE 'cold' END AS heat FROM posts;

-- An always-true last WHEN has nothing after it to make unreachable.
SELECT id, CASE WHEN views > 100 THEN 'hot' WHEN true THEN 'cold' END AS heat FROM posts;

-- A simple CASE is not judged, even with a value the column can't have.
SELECT id, CASE status WHEN 'deleted' THEN 'gone' ELSE 'here' END AS state FROM posts;
```

## Fix

None. Every branch's result takes part in choosing the `CASE`'s result type
(PostgreSQL's `select_common_type`), so dropping a branch can change the
column's type; and what the dead branch was meant to do is for its author to
decide.

## When it stays silent

- A simple `CASE x WHEN v …`.
- Conditions that may depend on a row, a parameter or the session.
- A column of a table with a `NOT VALID` `CHECK` constraint, or under a
  nondeterministic collation (see
  [`constant_condition`](constant_condition.md)).
- When a node is analyzed more than once, it is decided only if every
  analysis agrees.
- Migrations are not read: this rule runs over queries only.

## Configuration

Rust (`Cargo.toml`):

```toml
[package.metadata.typedpg.lint]
unreachable_case_branch = "error"   # "error", "warning" (default), "info" or "off"
fail_on = "warning"                 # fail the run on warnings too (default "error")
ignore = ["tests/**"]               # query files whose findings are dropped
```

TypeScript (`typedpg.config.json`):

```json
{
  "lint": {
    "unreachableCaseBranch": "error",
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
-- typedpg-lint: allow(unreachable_case_branch)
SELECT id, CASE WHEN status = 'deleted' THEN 'gone' ELSE 'here' END AS state FROM posts;
```

## Related rules

- [`constant_condition`](constant_condition.md): the same proofs on `WHERE`,
  `ON`, `HAVING` and `FILTER` conditions, and on the parts of a `WHEN` not
  decided as a whole.
- [`case_without_else`](case_without_else.md): a `CASE` whose `WHEN`s may all
  fail.
- [`simplifiable_case`](simplifiable_case.md): a `CASE` that is a
  `COALESCE` or its own condition.
