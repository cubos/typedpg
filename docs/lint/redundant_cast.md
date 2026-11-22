# `redundant_cast`

Reports an explicit cast to the type, and type modifier, its operand already
has.

| | |
|---|---|
| Default level | info |
| Applies to | queries |
| Fix | yes (`--fix`): deletes the `::type` of a postfix cast |

## Why

When the operand already has the target type and modifier, PostgreSQL adds no
node for the cast at all (`coerce_type` hands the operand back): the cast does
nothing. It still costs the reader, who wonders what conversion was needed,
and if the column's type changes later, the leftover cast starts converting
silently.

## What it reports

A cast `x::type` or `CAST(x AS type)` whose operand, as typedpg's analyzer
resolved it, already has exactly the target type and type modifier. The
analyzer records this per cast while it types the query, so the rule doesn't
compare type names as text: `title::text`, `title::pg_catalog.text`,
`id::int8` on a `bigint` and `email::varchar(100)` on a `varchar(100)` all
count.

Not redundant, and never reported:

- a cast of a parameter (`$id::bigint` types the parameter);
- a cast of an untyped literal (`'x'::text`, `NULL::text`,
  `date '2020-01-01'`), which gives it its type;
- a cast to another type, or one that changes or drops the modifier
  (`email::varchar` of a `varchar(100)`, `code::bpchar` of a `char(3)`);
- a cast that types its operand (`ROW(…)::users`, `ARRAY[id::int]::bigint[]`).

An integer literal is not untyped: `1` already is an `integer`, so `1::int`
is reported.

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

SELECT title::text AS label FROM posts;
SELECT id FROM users WHERE email::text = $email;
SELECT CAST(id AS bigint) AS user_id FROM users;
SELECT 1::int;
```

```
info[redundant_cast]: cast to text has no effect: the value already is one
   ╭─[queries.sql:2:8]
 2 │ SELECT title::text AS label FROM posts;
   ·        ───────────
   ╰────
  help: remove the cast

info[redundant_cast]: cast to text has no effect: the value already is one
   ╭─[queries.sql:5:28]
 5 │ SELECT id FROM users WHERE email::text = $email;
   ·                            ───────────
   ╰────
  help: remove the cast

info[redundant_cast]: cast to bigint has no effect: the value already is one
   ╭─[queries.sql:8:8]
 8 │ SELECT CAST(id AS bigint) AS user_id FROM users;
   ·        ─────────────────
   ╰────
  help: remove the cast

info[redundant_cast]: cast to int has no effect: the value already is one
    ╭─[queries.sql:11:8]
 11 │ SELECT 1::int;
    ·        ──────
    ╰────
   help: remove the cast
```

`--fix` rewrites the first two to `SELECT title AS label FROM posts` and
`SELECT id FROM users WHERE email = $email`. The last two keep their findings
without a fix (see below).

### Good

```sql
-- A parameter's cast types the parameter.
SELECT id FROM users WHERE id = $id::bigint;

-- integer to bigint is a conversion.
SELECT views::bigint AS view_count FROM posts;

-- An untyped literal gets its type from the cast.
SELECT 'x'::text AS x;

-- text to varchar(10) truncates.
SELECT email::varchar(10) AS short FROM users;
```

## Fix

The fix deletes the `::type` of a postfix cast, modifiers and array brackets
included. It is withheld:

- for the `CAST(x AS type)` form;
- when the cast is a whole output column with no alias and the operand
  wouldn't give the column the same name: `SELECT 1::int` names its column
  `int4`, `SELECT 1` names it `?column?`. A column, function call,
  `COALESCE`, `GREATEST` or `LEAST` operand names the column itself either
  way, so `SELECT lower(name)::text FROM users` is still fixed.

Every fix is also re-analyzed before it is offered and must keep the result
columns' names, types, nullability and value refinements, and the parameters,
unchanged; a fix that doesn't is withheld and reported as an
`internal_error` (a typedpg bug).

## When it stays silent

- Any cast the analyzer has no fact for, or that it found to change something
  in any of its analyses of the site.
- Migrations are not read: this rule runs over queries only.

## Configuration

Rust (`Cargo.toml`):

```toml
[package.metadata.typedpg.lint]
redundant_cast = "off"   # "error", "warning", "info" (default) or "off"
fail_on = "error"        # the severity the run fails on (default)
ignore = ["tests/**"]    # query files whose findings are dropped
```

TypeScript (`typedpg.config.json`):

```json
{
  "lint": {
    "redundantCast": "off",
    "failOn": "error",
    "ignore": ["tests/**"]
  }
}
```

An info finding never fails the run unless `fail_on` / `failOn` is `"info"`.

To allow one query, put the comment in the query text (the `sql!` / `sql()`
string, or the statement in a `.sql` file):

```sql
-- typedpg-lint: allow(redundant_cast)
SELECT title::text AS label FROM posts;
```

## Related rules

- [`redundant_alias`](redundant_alias.md): an alias naming a column what it
  already is (`SELECT id::int AS id`).
- [`timestamp_timezone_mix`](timestamp_timezone_mix.md): an implicit
  conversion that a cast would make explicit.
