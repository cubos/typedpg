# `redundant_group_by`

Reports `GROUP BY` keys that can't split a group: the other keys determine
them, or `WHERE` pins them to one value.

| | |
|---|---|
| Default level | info |
| Applies to | queries |
| Fix | yes (`--fix`): removes the keys |

## Why

Older PostgreSQL versions required every selected column in `GROUP BY`, so
`GROUP BY u.id, u.name, u.email` is a common habit. Since 9.1, grouping by a
table's primary key lets the query select its other columns, and the extra
keys are pure cost: every one is hashed or sorted and compared for every
input row, and a wide `text` column makes the grouping slower and its hash
table bigger. They also hide what the groups are: `GROUP BY email, name`
reads as "one group per email and name", when it is one group per user.

## What it reports

A `GROUP BY` item typedpg proves can be dropped without changing the groups
or making PostgreSQL reject the query:

- **Determined by the other keys.** A plain column of a table whose other
  grouped columns cover a unique key of it: within a group, that table's row
  (or its null extension) is a single row, so the column has one value. The
  unique key must be `NOT NULL` (or `NULLS NOT DISTINCT` on a table no outer
  join null-extends), compared with the column type's default btree
  equality under a deterministic collation — otherwise two rows with a
  `NULL` key would share a group.
- **Pinned by `WHERE`.** A plain column `WHERE` equates to a constant, a
  parameter or an outer query's value.

PostgreSQL only lets the select list, `HAVING`, `ORDER BY`, `DISTINCT ON` or
a window read an ungrouped column when its table's **primary key** is
grouped. So a key read there is only reported when the primary key stays
grouped: `GROUP BY u.id, u.name` with `SELECT u.name` becomes
`GROUP BY u.id`, but a column determined by another unique key, or pinned by
`WHERE`, is kept when the query reads it. A `*` reads every column.

Keys are tried from the last one against those still kept, so of two
columns that determine each other only one goes. One key always stays:
dropping the last `GROUP BY` key would turn "no rows, no groups" into one
group. Each redundant key is its own finding, spanning the key.

## Examples

```sql
CREATE TABLE users (
    id bigint PRIMARY KEY,
    email text NOT NULL UNIQUE,
    nick text UNIQUE,
    name text,
    status text NOT NULL
);

CREATE TABLE posts (
    id bigint PRIMARY KEY,
    user_id bigint NOT NULL REFERENCES users,
    status text NOT NULL
);
```

### Bad

```sql
SELECT u.id, u.name, count(p.id) FROM users u LEFT JOIN posts p ON p.user_id = u.id GROUP BY u.id, u.name;
SELECT email, count(*) FROM users GROUP BY email, name;
SELECT user_id, count(*) FROM posts WHERE status = $status GROUP BY status, user_id;
```

```
info[redundant_group_by]: the GROUP BY key u.name can't split a group: the other keys determine it, or WHERE pins it to one value
   ╭─[queries.sql:2:100]
 2 │ SELECT u.id, u.name, count(p.id) FROM users u LEFT JOIN posts p ON p.user_id = u.id GROUP BY u.id, u.name;
   ·                                                                                                    ──────
   ╰────
  help: remove it

info[redundant_group_by]: the GROUP BY key name can't split a group: the other keys determine it, or WHERE pins it to one value
   ╭─[queries.sql:5:51]
 5 │ SELECT email, count(*) FROM users GROUP BY email, name;
   ·                                                   ────
   ╰────
  help: remove it

info[redundant_group_by]: the GROUP BY key status can't split a group: the other keys determine it, or WHERE pins it to one value
   ╭─[queries.sql:8:69]
 8 │ SELECT user_id, count(*) FROM posts WHERE status = $status GROUP BY status, user_id;
   ·                                                                     ──────
   ╰────
  help: remove it
```

`--fix` rewrites them to:

```sql
SELECT u.id, u.name, count(p.id) FROM users u LEFT JOIN posts p ON p.user_id = u.id GROUP BY u.id;
SELECT email, count(*) FROM users GROUP BY email;
SELECT user_id, count(*) FROM posts WHERE status = $status GROUP BY user_id;
```

The second one is reported because `name` is not read after grouping;
`email`, a `NOT NULL` unique column, determines it.

### Good

```sql
-- Neither key determines the other.
SELECT status, name, count(*) FROM users GROUP BY status, name;

-- nick is UNIQUE but nullable: every user with a NULL nick shares one group.
SELECT nick, name, count(*) FROM users GROUP BY nick, name;

-- name is selected, and only email (not the primary key) determines it:
-- PostgreSQL rejects the query without name in GROUP BY.
SELECT email, name FROM users GROUP BY email, name;

-- status is pinned, but selected: PostgreSQL needs it grouped.
SELECT status, user_id, count(*) FROM posts WHERE status = $status GROUP BY status, user_id;

-- The last key stays: without GROUP BY, no rows would still be one group.
SELECT count(*) FROM posts WHERE status = $status GROUP BY status;

-- p.user_id equals u.id, but GROUP BY holds p.user_id, not users' own key.
SELECT p.user_id, u.name FROM posts p JOIN users u ON u.id = p.user_id GROUP BY p.user_id, u.name;

-- GROUPING SETS, ROLLUP, CUBE and GROUP BY DISTINCT are never touched.
SELECT id, name FROM users GROUP BY ROLLUP (id, name);
```

## Fix

The fix deletes each redundant key with its comma. Every reported key is a
plain column, so removing it can't drop an error or a volatile call.

Every fix is also re-analyzed before it is offered and must keep the result
columns' names, types, nullability and value refinements, and the
parameters, unchanged; a fix that doesn't is withheld and reported as an
[`internal_error`](internal_error.md) (a typedpg bug).

## When it stays silent

- A key that is an expression rather than a plain column.
- A unique key that is nullable (and not `NULLS NOT DISTINCT`), deferrable,
  partial or on expressions, or compared under a nondeterministic
  collation.
- A key the query reads after grouping, unless the table's primary key
  stays grouped.
- A key determined only through another table (`p.user_id = u.id` does not
  make `p.user_id` users' key).
- The last remaining key.
- `GROUPING SETS`, `ROLLUP`, `CUBE`, `GROUP BY ()` and `GROUP BY DISTINCT`.
- A `FROM` the proof can't see through: a view, subquery, CTE, function,
  partitioned or inheritance table.
- Queries with `$..spread` parameters.

## Configuration

```toml
[package.metadata.typedpg.lint]
redundant_group_by = "warning"   # "error", "warning", "info" (default) or "off"
```

```json
{
  "lint": { "redundantGroupBy": "warning" }
}
```

`fail_on` / `failOn` sets the severity that fails the run (default `error`;
an info finding only fails it with `"info"`), and `ignore` lists glob
patterns of files whose query findings are dropped. For one statement:

```sql
-- typedpg-lint: allow(redundant_group_by)
SELECT email, count(*) FROM users GROUP BY email, name;
```

## Related rules

- [`redundant_distinct`](redundant_distinct.md): a `DISTINCT` on rows a
  unique key already makes distinct — the same kind of proof.
- [`redundant_order_by`](redundant_order_by.md): `ORDER BY` keys that can't
  change the order.
