# `redundant_null_check`

An `IS [NOT] NULL` test or `COALESCE` on a value that is never `NULL`, and
an `IS [NOT] NULL` test the rest of its condition already settles.

| Level | Applies to | Fix |
|---|---|---|
| warning | queries | `COALESCE` only: drops the unused arguments |

## Example

```sql
CREATE TABLE users (id bigint PRIMARY KEY, email text NOT NULL UNIQUE, name text);
CREATE TABLE posts (id bigint PRIMARY KEY, user_id bigint, title text NOT NULL, views integer NOT NULL);

SELECT id FROM posts WHERE title IS NOT NULL;
SELECT id, COALESCE(title, 'untitled') AS label FROM posts;
SELECT COALESCE(name, email, 'unknown') AS label FROM users;
SELECT id FROM users WHERE name <> '' AND name IS NOT NULL;
SELECT id FROM posts WHERE user_id = $user_id AND user_id IS NULL;
```

```
warning[redundant_null_check]: `title` IS NOT NULL is always true: it is never NULL
   ╭─[queries.sql:2:28]
 2 │ SELECT id FROM posts WHERE title IS NOT NULL;
   ·                            ─────────────────
   ╰────
  help: remove the check, or find why the value was expected to be NULL (an outer join?)

warning[redundant_null_check]: `user_id` IS NULL makes the condition never true: the rest of it is never true when `user_id` is NULL
    ╭─[queries.sql:10:51]
 10 │ SELECT id FROM posts WHERE user_id = $user_id AND user_id IS NULL;
    ·                                                   ───────────────
    ╰────
   help: the condition keeps no row: fix the test or the rest of the condition
```

`--fix` gives `SELECT id, title AS label FROM posts` and
`SELECT COALESCE(name, email) AS label FROM users`. The `IS [NOT] NULL`
findings have no fix.

## Why

The check does nothing, and usually means the author's model of the data
differs from the schema: an outer join since made inner, a column since made
`NOT NULL`, or a typo in the rest of the condition.

## Reported when

- `x IS NULL` / `x IS NOT NULL` where `x` is never `NULL`: a `NOT NULL`
  column on the non-nullable side of every join, `count(*)`, a literal, and
  so on. Also in a select list.
- `x IS [NOT] NULL` as an `AND` argument of a `WHERE`, `JOIN … ON` or
  `HAVING` whose other arguments are never true for a `NULL` `x`
  (`x <> ''`, `x = $p`, `lower(x) = 'a'`): the row is dropped either way.
- `COALESCE(a, b, …)` where an argument before the last is never `NULL`:
  the arguments after it are never used.

## Not reported

- A nullable value: `SELECT id FROM users WHERE name IS NOT NULL`.
- A column an outer join null-extends:
  `… LEFT JOIN posts p ON p.user_id = u.id WHERE p.title IS NULL`.
- An aggregate over possibly no rows: `COALESCE(max(views), 0)`.
- A test under `OR` or `NOT`, in a `CASE WHEN` or a `FILTER`.
- Other arguments that can be true for `NULL`:
  `name IS DISTINCT FROM '' AND name IS NOT NULL`.
- Row and composite tests: `ROW(id, name) IS NULL`.

## Fix not offered

The fix deletes the arguments after the first never-`NULL` one; when that is
the first, the call goes: `COALESCE(views + 1, 0) * 2` becomes
`(views + 1) * 2`. It is withheld when:

- the kept argument doesn't have the call's result type, type modifier
  included: `COALESCE('1', views)` is `integer`, `'1'` alone is `text`;
  `COALESCE(code, 'x')` of a `varchar(10)` is a plain `varchar`;
- a dropped argument contains a parameter (`COALESCE(id, $fallback)`), a
  column reference, a `COLLATE`, or a function call outside a subquery;
- the `COALESCE` is a whole output column without an alias, named
  `coalesce`: `SELECT id, COALESCE(title, 'untitled') FROM posts`, also
  under a cast or as a `CASE`'s `ELSE`: `COALESCE(title, 'untitled')::varchar`.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
redundant_null_check = "info"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "redundantNullCheck": "info" } }
```

Allow one query with `-- typedpg-lint: allow(redundant_null_check)` in its
text. See [Configuring rules](README.md#configuring-rules).

## Related

- [`null_comparison`](null_comparison.md): comparing with a `NULL` literal.
- [`left_join_nullified`](left_join_nullified.md): a `WHERE` that rejects an
  outer join's null-extended rows.
- [`count_not_null`](count_not_null.md): `count(expr)` over a never-`NULL`
  value.
- [`simplifiable_case`](simplifiable_case.md): a `CASE` that is a
  `COALESCE`.
