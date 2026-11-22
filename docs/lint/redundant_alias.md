# `redundant_alias`

Reports an output column alias that is the name PostgreSQL already gives the
column.

| | |
|---|---|
| Default level | info |
| Applies to | queries |
| Fix | yes (`--fix`): removes the alias |

## Why

`SELECT u.email AS email` and `SELECT count(*) AS count` say nothing the
query didn't already say: PostgreSQL names those columns `email` and `count`
on its own. The alias is noise, and it suggests a rename that isn't there.

## What it reports

An alias in a select list or a `RETURNING` list (of `INSERT`, `UPDATE`,
`DELETE` or `MERGE`) equal to the name PostgreSQL derives for the expression
without one (`FigureColname`). typedpg compares only the names it derives
exactly as PostgreSQL does:

| Expression | Name |
|---|---|
| a column reference, `t.col` | the column name |
| a function call, `pg_catalog.f(…)` | the function name |
| `NULLIF(…)`, `COALESCE(…)`, `GREATEST(…)`, `LEAST(…)` | `nullif`, `coalesce`, `greatest`, `least` |
| `ARRAY[…]`, `ARRAY(SELECT …)`, `ROW(…)`, `EXISTS (…)` | `array`, `row`, `exists` |
| `CASE …` | `case` |
| `x::type` | the operand's name when it is one of the above (not `case`), else the type's name |

The alias must also read back as that name: a quoted alias is compared as
written (`AS "Email"` is not `email`), an unquoted one folded to lower case
(`AS EMAIL` is `email`). Both `expr AS alias` and `expr alias` are read.

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

SELECT u.email AS email FROM users u;
SELECT user_id, count(*) AS count FROM posts GROUP BY user_id;
UPDATE posts SET views = views + 1 WHERE id = $id RETURNING views AS views;
```

```
info[redundant_alias]: alias email is the name the column already has
   ╭─[queries.sql:2:16]
 2 │ SELECT u.email AS email FROM users u;
   ·                ────────
   ╰────
  help: remove the alias

info[redundant_alias]: alias count is the name the column already has
   ╭─[queries.sql:5:26]
 5 │ SELECT user_id, count(*) AS count FROM posts GROUP BY user_id;
   ·                          ────────
   ╰────
  help: remove the alias

info[redundant_alias]: alias views is the name the column already has
   ╭─[queries.sql:8:67]
 8 │ UPDATE posts SET views = views + 1 WHERE id = $id RETURNING views AS views;
   ·                                                                   ────────
   ╰────
  help: remove the alias
```

`--fix` rewrites them to `SELECT u.email FROM users u`,
`SELECT user_id, count(*) FROM posts GROUP BY user_id` and
`… RETURNING views`.

### Good

```sql
-- A quoted alias with other letter case is a different name.
SELECT email AS "Email" FROM users;

-- trim() is btrim(): its column would be named btrim.
SELECT trim(name) AS trim FROM users;

-- An operator expression would be named ?column?.
SELECT id + 1 AS id FROM users;

-- A real rename.
SELECT email AS address FROM users;
```

## Fix

The fix deletes the alias and its `AS`, with the whitespace before them. The
result columns keep the same names, so generated types don't change. Every
fix is also re-analyzed before it is offered and must keep the result
columns' names, types, nullability and value refinements, and the parameters,
unchanged; a fix that doesn't is withheld and reported as an
`internal_error` (a typedpg bug).

## When it stays silent

- Expressions whose derived name typedpg doesn't model (scalar and other
  sublinks besides `EXISTS` and `ARRAY`, SQL-standard keywords such as
  `CURRENT_DATE`), and expressions PostgreSQL would name `?column?`
  (operators, literals).
- Special syntax that parses as another function: `trim(name)` is a call to
  `btrim`, so its column is `btrim` and `AS trim` is a real rename.
- An alias whose spelling doesn't read back as the derived name.
- Migrations are not read: this rule runs over queries only.

## Configuration

Rust (`Cargo.toml`):

```toml
[package.metadata.typedpg.lint]
redundant_alias = "off"   # "error", "warning", "info" (default) or "off"
fail_on = "error"         # the severity the run fails on (default)
ignore = ["tests/**"]     # query files whose findings are dropped
```

TypeScript (`typedpg.config.json`):

```json
{
  "lint": {
    "redundantAlias": "off",
    "failOn": "error",
    "ignore": ["tests/**"]
  }
}
```

An info finding never fails the run unless `fail_on` / `failOn` is `"info"`.

To allow one query, put the comment in the query text (the `sql!` / `sql()`
string, or the statement in a `.sql` file):

```sql
-- typedpg-lint: allow(redundant_alias)
SELECT u.email AS email FROM users u;
```

## Related rules

- [`redundant_cast`](redundant_cast.md): a cast to the type the value already
  has.
- [`renaming_binding`](renaming_binding.md): a `sql!` binding that only
  renames a variable.
