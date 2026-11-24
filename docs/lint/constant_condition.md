# `constant_condition`

A condition that is always true or never true.

| Category | Level | Applies to | Fix |
|---|---|---|---|
| suspicious | warning | queries | removes a whole `WHERE` / `HAVING` that is a constant true |

## Example

```sql
CREATE TABLE users (id bigint PRIMARY KEY, name text);
CREATE TABLE posts (
    id bigint PRIMARY KEY,
    user_id bigint REFERENCES users,
    title text NOT NULL,
    status text NOT NULL CHECK (status IN ('draft', 'published', 'archived')),
    views integer NOT NULL CHECK (views >= 0)
);

SELECT id FROM posts WHERE 1 = 1;
SELECT id FROM posts WHERE status = 'publsihed';
SELECT id FROM posts WHERE user_id = $user_id AND views >= 0;
SELECT id FROM users WHERE name = 'a' AND name = 'b';
```

```
warning[suspicious/constant_condition]: condition is always true: it filters nothing
   ╭─[queries.sql:2:22]
 2 │ SELECT id FROM posts WHERE 1 = 1;
   ·                      ───────────
   ╰────
  help: remove it

warning[suspicious/constant_condition]: `status` is always one of 'archived', 'draft', 'published' (CHECK constraint "posts_status_check"), so this condition is never true
   ╭─[queries.sql:4:28]
 4 │ SELECT id FROM posts WHERE status = 'publsihed';
   ·                            ────────────────────
   ╰────
  help: fix the condition (a mistyped value?), or remove it

warning[suspicious/constant_condition]: `name` is always 'a' where this is evaluated, so this condition is never true
   ╭─[queries.sql:8:43]
 8 │ SELECT id FROM users WHERE name = 'a' AND name = 'b';
   ·                                           ──────────
   ╰────
  help: fix the condition (a mistyped value?), or remove it
```

`views >= 0` is reported as always true (`CHECK constraint
"posts_views_check"`). `--fix` rewrites only the first query, to
`SELECT id FROM posts`.

## Why

An always-true condition filters nothing; a never-true one keeps no row, or
is dead code inside an `OR`. Usually a typo (`'publsihed'`), a wrong operator
(`>=` for `>`, `AND` for `OR`) or a leftover `1 = 1`. PostgreSQL checks none
of it against the schema.

## Reported when

In a `WHERE` (of `SELECT`, `UPDATE`, `DELETE`), `ON`, `HAVING` or
`FILTER (WHERE …)`, including parts of `AND` / `OR` / `NOT` and of a
`CASE WHEN` condition:

- an expression of constants: `1 = 1`, `'a' <> 'b'`, `true`, `false`;
- a comparison (`=`, `<>`, `<`, `<=`, `>`, `>=`) or `IN` list decided by a
  `CHECK` constraint, a domain or an enum, or by an earlier condition in the
  same `AND` or an enclosing `WHERE`. The message names the one that alone
  decides it — `(enum "mech")` for `m = 'oauth'` on a one-label enum,
  `(CHECK constraint "…")`, `(domain "…")` — and says "where this is
  evaluated" when none does alone; a `CHECK` that says nothing of the column
  is never named;
- `x = x` on a column that is never `NULL` (always true);
- `EXISTS` over a subquery that always returns a row, such as
  `EXISTS (SELECT count(*) FROM users)`.

"Never true" includes `NULL`. "Always true" requires non-`NULL` operands:
`kind IN ('a', 'b')` on a nullable column constrained to those values is not
reported.

## Not reported

- Anything that depends on a row, a parameter or the session:
  `status = $status`, `random() > 0.5`, `now() > '2020-01-01'`.
- `x = x` on a nullable column: it filters out the `NULL`s.
- Columns of a table that has a `NOT VALID` `CHECK` constraint.
- A column with a nondeterministic collation.
- An enum compared with one of its labels, when the column can hold another.
- The bare `ON true` of a join, e.g. `LEFT JOIN LATERAL (…) s ON true`.
- Comparisons with a `NULL` literal ([`null_comparison`](null_comparison.md))
  and `IS [NOT] NULL` ([`redundant_null_check`](redundant_null_check.md)).
- A whole `CASE WHEN` condition that is constant
  ([`unreachable_case_branch`](unreachable_case_branch.md)).

## Fix not offered

- An always-true argument of an `AND` among others.
- A `HAVING` without `GROUP BY`: removing it would change the result.
- The `WHERE` of an `UPDATE` or `DELETE`: without it the statement writes
  every row. The help says to drop it and allow
  [`unbounded_write`](unbounded_write.md) if that is meant.
- `ON` and `FILTER` conditions.
- Never-true conditions, and anything decided by constraints or other
  conditions rather than constants alone.
- A condition that reads a parameter, as in a branch folded away
  (`CASE WHEN 1 = 1 THEN true ELSE name = $name END`): removing it would
  change the query's parameters.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
constant_condition = "error"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "constantCondition": "error" } }
```

Allow one query with `-- typedpg-lint: allow(constant_condition)` in its
text. See [Configuring rules](README.md#configuring-rules).

## Related

- [`unreachable_case_branch`](unreachable_case_branch.md): a `CASE` branch
  decided the same way.
- [`check_violation`](check_violation.md): writing a constant a `CHECK`
  rejects.
- [`null_comparison`](null_comparison.md): comparisons with a `NULL`
  literal.
- [`redundant_null_check`](redundant_null_check.md): `IS [NOT] NULL` on a
  value never `NULL`.
- [`unbounded_write`](unbounded_write.md): an `UPDATE` / `DELETE` without
  `WHERE`.
