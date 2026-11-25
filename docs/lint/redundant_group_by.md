# `redundant_group_by`

`GROUP BY` keys that can't split a group: the other keys determine them, or
`WHERE` pins them to one value.

| Category | Level | Applies to | Fix |
|---|---|---|---|
| redundant | info | queries | removes the keys |

## Example

```sql
CREATE TABLE users (id bigint PRIMARY KEY, email text NOT NULL UNIQUE, nick text UNIQUE, name text, status text NOT NULL);
CREATE TABLE posts (id bigint PRIMARY KEY, user_id bigint NOT NULL, status text NOT NULL);

SELECT u.id, u.name, count(p.id) AS n FROM users u LEFT JOIN posts p ON p.user_id = u.id GROUP BY u.id, u.name;
SELECT email, count(*) AS n FROM users GROUP BY email, name;
SELECT user_id, count(*) AS n FROM posts WHERE status = $status GROUP BY status, user_id;
```

```
info[redundant/redundant_group_by]: the GROUP BY key name can't split a group: the other keys determine it, or WHERE pins it to one value
   ╭─[queries.sql:4:56]
 4 │ SELECT email, count(*) AS n FROM users GROUP BY email, name;
   ·                                                        ────
   ╰────
  help: remove it
```

`--fix` gives `… GROUP BY u.id`, `… GROUP BY email` and `… GROUP BY user_id`.

## Why

Every key is hashed or sorted and compared for every input row; a wide
`text` key makes that slower. Extra keys also hide what a group is:
`GROUP BY email, name` reads as one group per email and name, when it is one
group per user.

## Reported when

A plain column key is:

- determined by the other keys: they cover a `NOT NULL` unique key of its
  table, so the table's row is one per group;
- or equated by `WHERE` to a constant, a parameter or an outer query's
  value.

If the query reads the key after grouping (select list, `HAVING`,
`ORDER BY`, `DISTINCT ON`, a window, `*`), it is reported only when the
table's primary key stays grouped, as PostgreSQL requires.

## Not reported

- Keys that don't determine each other:
  `GROUP BY status, name`.
- A nullable, deferrable or partial unique key:
  `GROUP BY nick, name` (users with a `NULL` nick share a group).
- A key read after grouping that only a non-primary key determines, or that
  `WHERE` pins: `SELECT email, name FROM users GROUP BY email, name`.
- The last remaining key: no `GROUP BY` would turn no rows into one group.
- A key determined only through a join:
  `… JOIN users u ON u.id = p.user_id GROUP BY p.user_id, u.name`.
- Expression keys, `GROUPING SETS`, `ROLLUP`, `CUBE`, `GROUP BY DISTINCT`.
- Grouping over views, subqueries, CTEs, functions, partitioned or
  inheritance tables, and queries with `$..spread` parameters.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
redundant_group_by = "warning"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "redundantGroupBy": "warning" } }
```

Allow one query with `-- typedpg-lint: allow(redundant_group_by)` in its
text. See [Configuring rules](README.md#configuring-rules).

## Related

- [`redundant_distinct`](redundant_distinct.md): `DISTINCT` on rows already
  distinct.
- [`redundant_order_by`](redundant_order_by.md): `ORDER BY` keys that can't
  change the order.
