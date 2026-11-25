# `redundant_count_argument`

`count(expr)` over a value that is never `NULL`, which counts every row:
that is `count(*)`.

| Category | Level | Applies to | Fix |
|---|---|---|---|
| redundant | info | queries | rewrites the argument to `*` |

## Example

```sql
CREATE TABLE users (id bigint PRIMARY KEY, email text NOT NULL, name text);
CREATE TABLE posts (id bigint PRIMARY KEY, user_id bigint NOT NULL REFERENCES users);

SELECT count(1) FROM posts;
SELECT u.id, count(u.email) FROM users u LEFT JOIN posts p ON p.user_id = u.id GROUP BY u.id;
SELECT count(name) FROM users WHERE name IS NOT NULL;
SELECT count(id / 2) FROM users;
```

```
info[redundant/redundant_count_argument]: count(u.email) counts every row: its argument is never NULL here
   ╭─[queries.sql:4:14]
 4 │ SELECT u.id, count(u.email) FROM users u LEFT JOIN posts p ON p.user_id = u.id GROUP BY u.id;
   ·              ──────────────
   ╰────
  help: use count(*)
```

All four are reported. `--fix` rewrites the first three to `count(*)`; the
last keeps its finding without a fix.

## Why

`count(expr)` differs from `count(*)` only by skipping rows where `expr` is
`NULL`. When it never is, the argument misleads: a reader of `count(u.email)`
wonders which users have no email. The mistake to keep apart is
`count(p.id)` on the nullable side of a `LEFT JOIN`, which counts matches and
is not reported.

## Reported when

`count` with one argument (also with `FILTER` or `OVER`) whose argument is
never `NULL`:

- a `NOT NULL` column of a table no outer join null-extends;
- a non-`NULL` literal: `count(1)`, `count('x')`;
- an expression of those that can't be `NULL`: `count(id / 2)`;
- a nullable column the call's `FILTER (WHERE … IS NOT NULL)`, the query's
  `WHERE` or an inner join's `ON` rules out as `NULL`.

## Not reported

- An argument that may be `NULL`: a nullable column, a column of a
  null-extended table (`count(p.id)` above), a parameter, `NULL::int`.
- `count(DISTINCT …)`, `count(x ORDER BY …)`, `count(*)`.
- A row or composite argument: `count(u)`, a composite column.
- A `count` of an outer query's columns written inside a subquery: it counts
  the outer rows.
- A condition that doesn't filter the counted rows: an outer join's `ON`, a
  `HAVING`, another query level's `WHERE`, or
  `WHERE name IS NOT NULL OR id > 0`.

## Fix not offered

When the argument is anything but a plain column or a constant, such as
`count(id / 2)`: `count(*)` would no longer evaluate it, nor raise the errors
it may raise.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
redundant_count_argument = "warning"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "redundantCountArgument": "warning" } }
```

Allow one query with `-- typedpg-lint: allow(redundant_count_argument)` in its text.
See [Configuring rules](README.md#configuring-rules).

## Related

- [`redundant_null_check`](redundant_null_check.md): `IS [NOT] NULL` or
  `COALESCE` on a value that is never `NULL`.
- [`left_join_nullified`](left_join_nullified.md): an outer join the `WHERE`
  turns into an inner join.
