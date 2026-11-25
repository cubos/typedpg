# `redundant_distinct`

A plain `DISTINCT` on rows that are already distinct.

| Category | Level | Applies to | Fix |
|---|---|---|---|
| redundant | info | queries | removes `DISTINCT` |

## Example

```sql
CREATE TABLE users (id bigint PRIMARY KEY, email text NOT NULL UNIQUE, name text);
CREATE TABLE posts (id bigint PRIMARY KEY, user_id bigint NOT NULL, views integer NOT NULL);

SELECT DISTINCT id, name FROM users;
SELECT DISTINCT p.id, u.email FROM posts p JOIN users u ON u.id = p.user_id;
SELECT DISTINCT id, 100 / views AS ratio FROM posts;
```

```
info[redundant/redundant_distinct]: DISTINCT has no effect: the rows are already distinct
   ╭─[queries.sql:2:8]
 2 │ SELECT DISTINCT id, name FROM users;
   ·        ─────────
   ╰────
  help: remove DISTINCT (a unique key of every table is in the output)
```

`--fix` gives `SELECT id, name FROM users` and
`SELECT p.id, u.email FROM posts p JOIN users u ON u.id = p.user_id`. The
third is reported without a fix (see below).

## Why

`DISTINCT` makes PostgreSQL sort or hash every row to remove duplicates
that can't exist. It also misleads: the reader looks for the join that
produces duplicates.

## Reported when

For every table the query reads, the output identifies its row:

- a primary key or unique constraint whose columns are all output columns;
- or a unique key equated to values already identified: constants,
  parameters, or the other side of a join on that table's key.

Key columns must be `NOT NULL`, and the key must be non-deferrable,
non-partial and on plain columns.

## Not reported

- Columns that aren't a key: `SELECT DISTINCT name FROM users`.
- A join that repeats rows:
  `SELECT DISTINCT u.email FROM users u JOIN posts p ON p.user_id = u.id`.
- A nullable unique column, a `DEFERRABLE` unique constraint, a partial
  unique index.
- `DISTINCT ON (…)`.
- Selects from views, subqueries, CTEs, functions, inheritance parents,
  partitioned tables or tables with row-level security.
- Volatile or set-returning calls in the output: `random()`,
  `generate_series(1, 2)`.
- Queries with `$..spread` parameters.

## Fix not offered

When an output expression can raise an error on some rows, e.g.
`100 / views`: with `DISTINCT` every row is evaluated before any is
returned, without it a `LIMIT` or a client that stops fetching may skip the
row that would divide by zero.

Nor when the output has an untyped literal in a set operation's branch or an
`INSERT`'s `SELECT`: `(SELECT DISTINCT 'y' AS c) UNION (SELECT 'a'::varchar)`
is `text` because `DISTINCT` types `'y'`, `varchar` without it.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
redundant_distinct = "warning"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "redundantDistinct": "warning" } }
```

Allow one query with `-- typedpg-lint: allow(redundant_distinct)` in its
text. See [Configuring rules](README.md#configuring-rules).

## Related

- [`redundant_group_by`](redundant_group_by.md): `GROUP BY` keys another
  key already determines.
- [`redundant_join`](redundant_join.md): a join that can't change the rows.
- [`redundant_limit`](redundant_limit.md): a `LIMIT` on a query that
  returns at most one row.
