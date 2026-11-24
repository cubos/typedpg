# `redundant_cast`

A cast to the type, and type modifier, the value already has.

| Level | Applies to | Fix |
|---|---|---|
| info | queries | removes the `::type` |

## Example

```sql
CREATE TABLE users (id bigint PRIMARY KEY, email text NOT NULL UNIQUE, name text);
CREATE TABLE posts (id bigint PRIMARY KEY, title text NOT NULL, views integer NOT NULL);

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
```

`--fix` gives `SELECT title AS label FROM posts` and
`SELECT id FROM users WHERE email = $email`. The last two are reported
without a fix (see below).

## Why

The cast converts nothing. The reader still wonders what conversion was
needed, and if the column's type changes later, the leftover cast starts
converting silently.

## Reported when

The operand already has exactly the target type and modifier, however the
type is spelled: `title::pg_catalog.text`, `id::int8` on a `bigint`,
`email::varchar(100)` on a `varchar(100)`. An integer literal already is an
`integer`, so `1::int` and `WHERE id = 5::int` are reported.

## Not reported

- A cast of a parameter, which types it: `$id::bigint`.
- A cast of an untyped literal, which gives it its type: `'x'::text`,
  `NULL::text`, `date '2020-01-01'`.
- A cast to another type, or one that changes or drops the modifier:
  `views::bigint` of an `integer`, `email::varchar` of a `varchar(100)`.
- A cast that types its operand: `ROW(…)::users`, `ARRAY[id::int]::bigint[]`,
  a subquery's row `s::users`; and `s::record`, which forgets the row's fields.

## Fix not offered

- For the `CAST(x AS type)` form.
- When the cast names an output column without an alias (as the whole
  item, or the `ELSE` of a `CASE` that is) and removing it would rename the
  column: `SELECT 1::int` names it `int4`, `SELECT 1`
  names it `?column?`. A column, function call, `COALESCE`, `GREATEST` or
  `LEAST` operand keeps the name, so `SELECT lower(name)::text FROM users`
  is fixed.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
redundant_cast = "warning"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "redundantCast": "warning" } }
```

Allow one query with `-- typedpg-lint: allow(redundant_cast)` in its text.
See [Configuring rules](README.md#configuring-rules).

## Related

- [`redundant_alias`](redundant_alias.md): an alias naming a column what it
  is already named.
- [`timestamp_timezone_mix`](timestamp_timezone_mix.md): an implicit
  conversion a cast would make explicit.
