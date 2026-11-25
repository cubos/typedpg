# `redundant_alias`

An output column alias that is the name PostgreSQL already gives the column.

| Category | Level | Applies to | Fix |
|---|---|---|---|
| redundant | info | queries | removes the alias |

## Example

```sql
CREATE TABLE users (id bigint PRIMARY KEY, email text NOT NULL UNIQUE, name text);
CREATE TABLE posts (id bigint PRIMARY KEY, user_id bigint NOT NULL, views integer NOT NULL);

SELECT u.email AS email FROM users u;
SELECT user_id, count(*) AS count FROM posts GROUP BY user_id;
UPDATE posts SET views = views + 1 WHERE id = $id RETURNING views AS views;
```

```
info[redundant/redundant_alias]: alias email is the name the column already has
   ╭─[queries.sql:2:16]
 2 │ SELECT u.email AS email FROM users u;
   ·                ────────
   ╰────
  help: remove the alias
```

`--fix` gives `SELECT u.email FROM users u`,
`SELECT user_id, count(*) FROM posts GROUP BY user_id` and
`… RETURNING views`. Column names, and so the generated types, don't change.

## Why

The alias says nothing the expression didn't, and it suggests a rename that
isn't there.

## Reported when

An alias in a select list or a `RETURNING` list (`INSERT`, `UPDATE`,
`DELETE`, `MERGE`) equals the name the expression gets without one:

| Expression | Name |
|---|---|
| `col`, `t.col` | the column name |
| `(x).field`, `arr[1]` | the field name; the array's name |
| a function call `f(…)` | the function name |
| `NULLIF`, `COALESCE`, `GREATEST`, `LEAST` | `nullif`, `coalesce`, `greatest`, `least` |
| `ARRAY[…]`, `ARRAY(SELECT …)`, `ROW(…)`, `EXISTS (…)` | `array`, `row`, `exists` |
| `CURRENT_DATE`, `CURRENT_USER`, … | the keyword, lower case |
| `CASE … ELSE x END` | `x`'s name if it has one above, else `case` |
| `x::type`, `x COLLATE c` | `x`'s name if it has one above (not `case`), else the type's |

An unquoted alias is folded to lower case (`AS COALESCE` is `coalesce`); a
quoted one is compared as written. `expr alias` without `AS` counts too.

## Not reported

- A quoted alias in other letter case: `SELECT email AS "Email" FROM users`.
- Syntax that calls another function: `trim(name)` is `btrim(name)`, so
  `AS trim` is a real rename.
- Expressions PostgreSQL names `?column?` (operators, literals):
  `SELECT id + 1 AS id FROM users`.
- Scalar subqueries (named after the subquery's own column) and `*`
  expansions.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
redundant_alias = "warning"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "redundantAlias": "warning" } }
```

Allow one query with `-- typedpg-lint: allow(redundant_alias)` in its text.
See [Configuring rules](README.md#configuring-rules).

## Related

- [`redundant_cast`](redundant_cast.md): a cast to the type the value
  already has.
- [`renaming_binding`](renaming_binding.md): a `sql!` binding that only
  renames a variable.
