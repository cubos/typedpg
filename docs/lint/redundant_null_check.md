# `redundant_null_check`

Reports an `IS [NOT] NULL` test or a `COALESCE` on a value typedpg proves is
never `NULL`.

| | |
|---|---|
| Default level | warning |
| Applies to | queries |
| Fix | yes (`--fix`), for `COALESCE` only: drops the arguments it never uses |

## Why

A `NULL` check on a value that can't be `NULL` does nothing: `title IS NOT
NULL` is always true, `title IS NULL` never true, and `COALESCE(title,
'untitled')` always returns `title`. At best it is noise. Often it is a sign
that the author's model of the data differs from the schema: they expected
the value to be `NULL` somewhere, for example on the nullable side of an
outer join that was later changed to an inner join, or on a column since made
`NOT NULL`. The help line asks you to check which is true.

## What it reports

Nullability comes from typedpg's analyzer, the same inference that types the
query's result columns: column `NOT NULL` constraints, primary keys,
null-extension by outer joins, function and aggregate rules (`count(*)` is
never `NULL`; `max(x)` over no rows is), scalar subqueries (`NULL` when they
return no row), `CASE` without `ELSE`, and so on.

- **`x IS NULL` / `x IS NOT NULL`** when the analyzer proves `x` is never
  `NULL` where it is evaluated. The argument's own nullability is used: a
  `WHERE nullable IS NOT NULL` does not prove itself redundant. A row or
  composite argument (`ROW(a, b) IS NULL`, `t IS NULL`), whose `IS NULL`
  semantics differ, is never reported.
- **`COALESCE(a, b, …)`** when some argument before the last is proven never
  `NULL`: the arguments after it are never evaluated. The message names that
  argument, or its position when its text is longer than 40 characters.

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

SELECT id FROM posts WHERE title IS NOT NULL;
SELECT id, COALESCE(title, 'untitled') AS label FROM posts;
SELECT id, COALESCE(title, 'untitled') FROM posts;
SELECT COALESCE(name, email, 'unknown') AS label FROM users;
```

```
warning[redundant_null_check]: `title` IS NOT NULL is always true: it is never NULL
   ╭─[queries.sql:2:28]
 2 │ SELECT id FROM posts WHERE title IS NOT NULL;
   ·                            ─────────────────
   ╰────
  help: remove the check, or find why the value was expected to be NULL (an outer join?)

warning[redundant_null_check]: COALESCE never uses the arguments after `title`: it is never NULL
   ╭─[queries.sql:5:12]
 5 │ SELECT id, COALESCE(title, 'untitled') AS label FROM posts;
   ·            ───────────────────────────
   ╰────

warning[redundant_null_check]: COALESCE never uses the arguments after `title`: it is never NULL
   ╭─[queries.sql:8:12]
 8 │ SELECT id, COALESCE(title, 'untitled') FROM posts;
   ·            ───────────────────────────
   ╰────

warning[redundant_null_check]: COALESCE never uses the arguments after `email`: it is never NULL
    ╭─[queries.sql:11:8]
 11 │ SELECT COALESCE(name, email, 'unknown') AS label FROM users;
    ·        ────────────────────────────────
    ╰────
```

`--fix` rewrites the second query to `SELECT id, title AS label FROM posts`
and the fourth to `SELECT COALESCE(name, email) AS label FROM users`. The
third keeps its finding without a fix: its unaliased column is named
`coalesce`, and dropping the call would rename it to `title`.

### Good

```sql
-- users.name is nullable.
SELECT id FROM users WHERE name IS NOT NULL;

-- posts.title is NOT NULL, but the LEFT JOIN null-extends it: this is the
-- anti-join, and the test is meaningful.
SELECT u.id FROM users u LEFT JOIN posts p ON p.user_id = u.id WHERE p.title IS NULL;

-- max() over no rows is NULL.
SELECT COALESCE(max(views), 0) AS most FROM posts;

-- Every argument but the last may be NULL.
SELECT COALESCE(name, email) AS label FROM users;
```

## Fix

`IS [NOT] NULL` findings have no fix: removing a test from a `WHERE` changes
which rows are kept, and what was meant is for the author to decide.

For `COALESCE`, the fix deletes the arguments after the first never-`NULL`
one. When that is the first argument, the call itself goes:
`COALESCE(x, 0)` becomes `x`, and `COALESCE(a + b, 0) * 2` becomes
`(a + b) * 2`, keeping the grouping the call's parentheses gave. The fix is
withheld (the finding stays) when:

- the arguments don't all share the call's result type, or the kept argument
  isn't typed as the result: `COALESCE('1', int_col)` is `integer`, `'1'`
  alone would be `text`;
- a dropped argument contains a function call outside a subquery: an
  aggregate, window or set-returning function shapes the rows even when
  never evaluated (`COALESCE(1, max(v))` is one row, `1` is one per row);
- a dropped argument contains a `COLLATE` clause or a column reference,
  either of which can decide the result's collation;
- the `COALESCE` is a whole output column with no alias, named `coalesce`.

Every fix is also re-analyzed before it is offered and must keep the result
columns' names, types, nullability and value refinements, and the parameters,
unchanged; a fix that doesn't is withheld and reported as an
`internal_error` (a typedpg bug).

## When it stays silent

- Whenever the analyzer has no fact for the site, or the value may be `NULL`.
- A condition does not narrow its own test: `WHERE nullable > 0 AND nullable
  IS NOT NULL` is not reported, although the first conjunct already rejects
  `NULL`.
- Row and composite `IS NULL` tests.
- `IS DISTINCT FROM NULL` and other forms: only `IS [NOT] NULL` and
  `COALESCE` are read.
- Migrations are not read: this rule runs over queries only.

## Configuration

Rust (`Cargo.toml`):

```toml
[package.metadata.typedpg.lint]
redundant_null_check = "info"   # "error", "warning" (default), "info" or "off"
fail_on = "error"               # the severity the run fails on (default)
ignore = ["tests/**"]           # query files whose findings are dropped
```

TypeScript (`typedpg.config.json`):

```json
{
  "lint": {
    "redundantNullCheck": "info",
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
-- typedpg-lint: allow(redundant_null_check)
SELECT id FROM posts WHERE title IS NOT NULL;
```

## Related rules

- [`null_comparison`](null_comparison.md): comparing with a `NULL` literal.
- [`left_join_nullified`](left_join_nullified.md): a `WHERE` that rejects an
  outer join's null-extended rows.
- [`simplifiable_case`](simplifiable_case.md): a `CASE` that is a
  `COALESCE`.
