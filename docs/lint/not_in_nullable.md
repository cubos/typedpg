# `not_in_nullable`

Reports a `NOT IN (SELECT …)` or `<> ALL (SELECT …)` whose left side or
subquery output may be `NULL`, which makes it return no rows.

| | |
|---|---|
| Default level | warning |
| Applies to | queries |
| Fix | no |

## Why

`x NOT IN (SELECT y FROM t)` is `x <> y1 AND x <> y2 AND …`. Two things
follow from SQL's three-valued logic:

- If the subquery returns a single `NULL`, one term is `x <> NULL`, which is
  `NULL`. The `AND` can then never be `TRUE`, so the condition keeps **no
  row at all**. A nullable foreign key column (`posts.user_id`) in the
  subquery is the classic case: the day one post has no author, "users
  without posts" becomes empty.
- If `x` itself is `NULL`, every term is `NULL` as soon as the subquery
  returns any row, so that row is dropped. Only against an empty subquery is
  the condition `TRUE`.

`x <> ALL (SELECT y FROM t)` is the same `AND` of `<>` terms, with the same
trap.

The query keeps working in tests with clean data and silently breaks later.
`NOT EXISTS (SELECT 1 FROM t WHERE t.y = x)` has none of these problems: it
is true exactly when no matching row exists.

## What it reports

Two forms of the same comparison, with a single (not row-valued) left side:

- a `NOT (x IN (SELECT …))` — what `x NOT IN (SELECT …)` parses to — with the
  plain `=` operator;
- an `x <> ALL (SELECT …)` (or `!= ALL`), not itself negated:
  `NOT (x <> ALL (SELECT …))` is `TRUE` when some comparison is `FALSE`, so
  it can keep rows.

It is reported when the analyzer proves that the left side may be `NULL`,
that some subquery output column may be `NULL`, or both, and every comparison
the sublink resolved to is strict (the operator's function and the implicit
casts of its operands are `STRICT`, so a `NULL` makes that comparison
`NULL`). The message names the form as written and says which side:

| Left side may be NULL | Subquery may return NULL | Message |
|---|---|---|
| no | yes | `NOT IN (SELECT …) returns no rows when the subquery returns a NULL` |
| yes | no | `NOT IN (SELECT …) returns no rows for a NULL left side when the subquery returns any row` |
| yes | yes | `… returns no rows for a NULL left side when the subquery returns any row, or when the subquery returns a NULL` |

For the other form the message starts with `<> ALL (SELECT …)` instead.

Nullability is typedpg's inference, the same that types query results:
column `NOT NULL` constraints, null-extension by outer joins, and narrowing by
conditions such as `WHERE user_id IS NOT NULL` inside the subquery. When both
sides are proven `NOT NULL`, nothing is reported.

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

-- posts.user_id is nullable.
SELECT id FROM users WHERE id NOT IN (SELECT user_id FROM posts);
-- users.name is nullable.
SELECT id FROM users WHERE name NOT IN (SELECT title FROM posts);
-- The same comparison, written as <> ALL.
SELECT id FROM users WHERE id <> ALL (SELECT user_id FROM posts);
```

```
warning[not_in_nullable]: NOT IN (SELECT …) returns no rows when the subquery returns a NULL
   ╭─[queries.sql:2:28]
 2 │ SELECT id FROM users WHERE id NOT IN (SELECT user_id FROM posts);
   ·                            ─────────────────────────────────────
   ╰────
  help: use NOT EXISTS (SELECT 1 FROM … WHERE …), or make both sides NOT NULL

warning[not_in_nullable]: NOT IN (SELECT …) returns no rows for a NULL left side when the subquery returns any row
   ╭─[queries.sql:5:28]
 5 │ SELECT id FROM users WHERE name NOT IN (SELECT title FROM posts);
   ·                            ─────────────────────────────────────
   ╰────
  help: use NOT EXISTS (SELECT 1 FROM … WHERE …), or make both sides NOT NULL

warning[not_in_nullable]: <> ALL (SELECT …) returns no rows when the subquery returns a NULL
   ╭─[queries.sql:8:28]
 8 │ SELECT id FROM users WHERE id <> ALL (SELECT user_id FROM posts);
   ·                            ─────────────────────────────────────
   ╰────
  help: use NOT EXISTS (SELECT 1 FROM … WHERE …), or make both sides NOT NULL
```

### Good

```sql
-- NOT EXISTS: the anti-join that means what it says.
SELECT id FROM users u WHERE NOT EXISTS (SELECT 1 FROM posts p WHERE p.user_id = u.id);

-- The subquery's WHERE proves its output NOT NULL, and users.id is NOT NULL.
SELECT id FROM users WHERE id NOT IN (SELECT user_id FROM posts WHERE user_id IS NOT NULL);
SELECT id FROM users WHERE id <> ALL (SELECT user_id FROM posts WHERE user_id IS NOT NULL);

-- Row-valued NOT IN compares field by field: one NULL field does not drop a
-- row whose other fields differ, so the scalar reasoning doesn't apply.
SELECT id FROM users WHERE (id, email) NOT IN (SELECT user_id, title FROM posts);
```

## When it stays silent

- When the analyzer has no nullability fact for the sublink, it reports
  nothing rather than guess.
- A row-valued left side `(a, b) NOT IN (SELECT …)`.
- A `NOT IN` through an operator other than `=`, an `ALL` through an operator
  other than `<>`, any other `ANY` / `ALL` form, and a negated
  `NOT (x <> ALL (SELECT …))`.
- A comparison that resolves to a non-strict operator (a user-defined one
  declared `CALLED ON NULL INPUT`, which may decide a `NULL` either way), or
  whose operands reach it through a non-strict implicit cast.
- `NOT IN (1, 2, NULL)` with a literal list is
  [`null_comparison`](null_comparison.md)'s.
- Migrations are not read: this rule runs over queries only.

## Configuration

Rust (`Cargo.toml`):

```toml
[package.metadata.typedpg.lint]
not_in_nullable = "error"   # "error", "warning" (default), "info" or "off"
fail_on = "warning"         # fail the run on warnings too (default "error")
ignore = ["tests/**"]       # query files whose findings are dropped
```

TypeScript (`typedpg.config.json`):

```json
{
  "lint": {
    "notInNullable": "error",
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
-- typedpg-lint: allow(not_in_nullable)
SELECT id FROM users WHERE id NOT IN (SELECT user_id FROM posts);
```

## Related rules

- [`null_comparison`](null_comparison.md): `NOT IN` with a `NULL` literal in
  its list.
- [`outer_reference_in_subquery`](outer_reference_in_subquery.md): the other
  classic `IN (SELECT …)` trap.
