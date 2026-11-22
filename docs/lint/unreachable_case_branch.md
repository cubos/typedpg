# `unreachable_case_branch`

A `CASE` branch that is never taken.

| Level | Applies to | Fix |
|---|---|---|
| warning | queries | — |

## Example

```sql
CREATE TABLE posts (
    id bigint PRIMARY KEY,
    status text NOT NULL CHECK (status IN ('draft', 'published', 'archived')),
    views integer NOT NULL DEFAULT 0 CHECK (views >= 0)
);

SELECT id, CASE WHEN status = 'deleted' THEN 'gone' ELSE 'here' END AS state FROM posts;
SELECT id, CASE WHEN views >= 0 THEN 'counted' WHEN views > 100 THEN 'hot' END AS heat FROM posts;
SELECT id, CASE WHEN status = 'draft' THEN 1 WHEN status = 'draft' THEN 2 ELSE 3 END AS rank FROM posts;
SELECT id, CASE status WHEN 'draft' THEN 1 WHEN 'published' THEN 2 WHEN 'archived' THEN 3 ELSE 4 END AS rank FROM posts;
```

```
warning[unreachable_case_branch]: CASE branch is never taken: its condition is never true (`status` is always one of 'archived', 'draft', 'published' (CHECK constraint "posts_status_check"))
   ╭─[queries.sql:2:17]
 2 │ SELECT id, CASE WHEN status = 'deleted' THEN 'gone' ELSE 'here' END AS state FROM posts;
   ·                 ───────────────────────────────────
   ╰────
  help: remove the branch, or fix its condition

warning[unreachable_case_branch]: CASE branches after an always-true WHEN are never taken (`views` is always at least 0 (CHECK constraint "posts_views_check"))
   ╭─[queries.sql:4:48]
 4 │ SELECT id, CASE WHEN views >= 0 THEN 'counted' WHEN views > 100 THEN 'hot' END AS heat FROM posts;
   ·                                                ───────────────────────────
   ╰────
  help: remove them, or replace the CASE with the result of its always-true branch
```

The duplicated `WHEN status = 'draft'` and the `ELSE 4` (every status is
covered) are reported too.

## Why

A dead branch is usually a bug: a mistyped value, a duplicated `WHEN`, or a
broad condition shadowing a narrower one after it. The query runs and the
branch the author cared about never fires.

## Reported when

- A `WHEN` that is never true: that branch.
- A `WHEN` that is always true, or that with the earlier `WHEN`s covers every
  value: every branch after it, `ELSE` included.

Conditions are decided from constants (`WHEN 1 = 2`, `WHEN NULL`), the
columns' `CHECK` constraints, domains and enums, the query's `WHERE`
(`CASE WHEN status = 'published' …` under `WHERE status = 'draft'`) and the
earlier `WHEN`s, combined through `AND` / `OR` / `NOT`. The message names
the constraint or condition when one decides it.

A simple `CASE x WHEN v` is judged as `CASE WHEN x = v`, when `x` is a plain
column and `v` a constant of its type.

## Not reported

- Conditions that depend on the row or a parameter:
  `CASE WHEN views > $n THEN 'hot' ELSE 'cold' END`.
- An always-true last `WHEN`, with nothing after it:
  `CASE WHEN views > 100 THEN 'hot' WHEN true THEN 'cold' END`.
- A simple `CASE` over an expression, a parameter, a cast or `COLLATE`d
  column, a value of another type, or a non-built-in `=`:
  `CASE status::text COLLATE "C" WHEN 'deleted' …`.
- Columns with a `NOT VALID` `CHECK`, or under a nondeterministic collation.

No fix is offered: every branch's result takes part in choosing the
`CASE`'s type, and what the branch was meant to do is for its author to
decide.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
unreachable_case_branch = "error"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "unreachableCaseBranch": "error" } }
```

Allow one query with `-- typedpg-lint: allow(unreachable_case_branch)` in
its text. See [Configuring rules](README.md#configuring-rules).

## Related

- [`constant_condition`](constant_condition.md): `WHERE`, `ON`, `HAVING` and
  `FILTER` conditions that are always or never true.
- [`case_without_else`](case_without_else.md): a `CASE` whose `WHEN`s may all
  be false.
- [`simplifiable_case`](simplifiable_case.md): a `CASE` that is a `COALESCE`
  or its own condition.
