# `redundant_order_by`

Reports `ORDER BY` keys that can't change the order of the rows, and whole
`ORDER BY` clauses whose order nothing observes.

| | |
|---|---|
| Default level | info |
| Applies to | queries |
| Fix | yes (`--fix`): removes the keys, or the whole `ORDER BY` |

## Why

Every `ORDER BY` key is work: PostgreSQL compares it whenever the keys
before it tie, and a key no index provides can turn an index scan in order
into an explicit sort. A key that can never decide anything costs that for
nothing, and it misleads the reader: `ORDER BY email, created_at` suggests
two users can share an email.

A whole clause can be dead too. A query that returns one row has nothing to
order, and the rows of an `IN (SELECT …)` or `EXISTS (…)` subquery are only
tested for membership — PostgreSQL still sorts them if you ask.

## What it reports

A key is redundant when typedpg proves one of:

- **The keys before it determine a unique row.** The same proof
  [`unstable_order`](unstable_order.md) uses, run over the leading keys: they
  cover a unique key of every table in `FROM` (with the columns `WHERE` pins
  and the tables joined on their key), and each counted unique key is
  `NOT NULL` or `NULLS NOT DISTINCT`. No two rows tie on those keys, so a
  later key never breaks a tie.
- **`WHERE` pins it to one value.** A plain column `WHERE` equates to a
  constant or a parameter, through the column type's default btree equality
  under a deterministic collation: every row sorts it the same.
- **An earlier key already sorts by the same column.** Rows tied up to the
  second occurrence have equal values for it, whatever its direction.

The whole `ORDER BY` is reported when:

- the analyzer proves the query returns at most one row (`WHERE id = $id`,
  an aggregate without `GROUP BY`);
- it belongs to an `EXISTS`, `IN` / `= ANY` or `ALL` subquery without
  `LIMIT`, `OFFSET`, `DISTINCT ON` or a locking clause: those only test
  membership, so the order of the subquery's rows is never observed;
- every key is pinned or repeated, and the query has no `LIMIT`, `OFFSET` or
  `DISTINCT ON`.

Each redundant key is its own finding, spanning the key; a whole-clause
finding spans `ORDER BY` through its last key.

## Examples

```sql
CREATE TABLE users (
    id bigint PRIMARY KEY,
    tenant int NOT NULL,
    email text NOT NULL UNIQUE,
    nick text UNIQUE,
    name text,
    status text NOT NULL,
    created_at timestamptz NOT NULL
);

CREATE TABLE posts (
    id bigint PRIMARY KEY,
    user_id bigint NOT NULL REFERENCES users,
    title text,
    created_at timestamptz NOT NULL
);
```

### Bad

```sql
SELECT id, email FROM users ORDER BY email, created_at;
SELECT id FROM users WHERE tenant = $tenant ORDER BY tenant, created_at, id;
SELECT id, title FROM posts WHERE id = $id ORDER BY created_at;
SELECT id FROM users WHERE id IN (SELECT user_id FROM posts ORDER BY created_at);
SELECT id FROM users WHERE status = 'active' ORDER BY status;
SELECT id FROM users ORDER BY id, lower(name);
SELECT id FROM users ORDER BY created_at, name, created_at DESC;
```

```
info[redundant_order_by]: the ORDER BY key created_at can't change the order: the keys before it already determine a unique row, so it never breaks a tie
   ╭─[queries.sql:2:45]
 2 │ SELECT id, email FROM users ORDER BY email, created_at;
   ·                                             ──────────
   ╰────
  help: remove it

info[redundant_order_by]: the ORDER BY key tenant can't change the order: WHERE pins it to one value, so every row sorts it the same
   ╭─[queries.sql:5:54]
 5 │ SELECT id FROM users WHERE tenant = $tenant ORDER BY tenant, created_at, id;
   ·                                                      ──────
   ╰────
  help: remove it

info[redundant_order_by]: ORDER BY has no effect: the query returns at most one row
   ╭─[queries.sql:8:44]
 8 │ SELECT id, title FROM posts WHERE id = $id ORDER BY created_at;
   ·                                            ───────────────────
   ╰────
  help: remove the ORDER BY

info[redundant_order_by]: ORDER BY has no effect: a subquery that only tests membership never observes its rows' order
    ╭─[queries.sql:11:61]
 11 │ SELECT id FROM users WHERE id IN (SELECT user_id FROM posts ORDER BY created_at);
    ·                                                             ───────────────────
    ╰────
   help: remove the ORDER BY

info[redundant_order_by]: ORDER BY has no effect: WHERE pins every key to one value
    ╭─[queries.sql:14:46]
 14 │ SELECT id FROM users WHERE status = 'active' ORDER BY status;
    ·                                              ───────────────
    ╰────
   help: remove the ORDER BY

info[redundant_order_by]: the ORDER BY key lower(name) can't change the order: the keys before it already determine a unique row, so it never breaks a tie
    ╭─[queries.sql:17:35]
 17 │ SELECT id FROM users ORDER BY id, lower(name);
    ·                                   ───────────
    ╰────
   help: remove it

info[redundant_order_by]: the ORDER BY key created_at DESC can't change the order: an earlier ORDER BY key already sorts by the same column
    ╭─[queries.sql:20:49]
 20 │ SELECT id FROM users ORDER BY created_at, name, created_at DESC;
    ·                                                 ───────────────
    ╰────
   help: remove it
```

`--fix` rewrites them to:

```sql
SELECT id, email FROM users ORDER BY email;
SELECT id FROM users WHERE tenant = $tenant ORDER BY created_at, id;
SELECT id, title FROM posts WHERE id = $id;
SELECT id FROM users WHERE id IN (SELECT user_id FROM posts);
SELECT id FROM users WHERE status = 'active';
SELECT id FROM users ORDER BY id, lower(name);   -- reported, not fixed (see Fix)
SELECT id FROM users ORDER BY created_at, name;
```

### Good

```sql
-- created_at can tie: id breaks the tie.
SELECT id, email FROM users ORDER BY created_at, id;

-- nick is UNIQUE but nullable: rows with a NULL nick tie, so id still matters.
SELECT id FROM users ORDER BY nick, id;

-- The null-extended rows of the outer join all tie on u.id.
SELECT p.id FROM posts p LEFT JOIN users u ON u.id = p.user_id ORDER BY u.id, p.id;

-- tenant > 3 is not an equality: tenant still varies.
SELECT id FROM users WHERE tenant > 3 ORDER BY tenant, id;

-- A COLLATE key is a different order.
SELECT id FROM users ORDER BY name, name COLLATE "C";

-- Every key pinned under LIMIT: the order is arbitrary, which is
-- unstable_order's finding, not this one's.
SELECT id FROM users WHERE status = 'active' ORDER BY status LIMIT 10;

-- The leading keys of DISTINCT ON must match its expressions.
SELECT DISTINCT ON (id, created_at) id FROM users ORDER BY id, created_at;

-- A FROM subquery, a CTE or ARRAY(…): their order may be observed in practice.
SELECT * FROM (SELECT id FROM users ORDER BY created_at) s;
SELECT ARRAY(SELECT id FROM users ORDER BY created_at);

-- A membership subquery whose LIMIT picks rows by the order.
SELECT id FROM users WHERE id IN (SELECT user_id FROM posts ORDER BY created_at DESC LIMIT 10);

-- The order of a set operation.
SELECT id FROM users UNION SELECT id FROM posts ORDER BY 1;
```

## Fix

The fix deletes each redundant key with its comma, or the whole `ORDER BY`
clause when every key goes. It only removes keys that are a plain column,
an output column's name (an alias) or an ordinal (`ORDER BY 1`): those
refer to values the query computes anyway. Any other key — `lower(name)`,
`1 / (tenant - 1)`, `random()` — is reported without a fix, because removing
it could drop an error or a volatile call it evaluates. A whole-clause
finding has a fix only when every key in it is removable.

Every fix is also re-analyzed before it is offered and must keep the result
columns' names, types, nullability and value refinements, and the
parameters, unchanged; a fix that doesn't is withheld and reported as an
[`internal_error`](internal_error.md) (a typedpg bug).

## When it stays silent

- A key that might break a tie: the leading keys aren't proven unique (a
  nullable unique column, a table on the null-extended side of an outer
  join, a relation the proof can't see through — a subquery, a CTE, a
  function, a partitioned or inheritance table).
- A `WHERE` condition that is not a plain equality on the column
  (`lower(status) = 'a'`, `tenant > 3`, `tenant = 3 OR tenant = 4`), or an
  equality in a `LEFT JOIN`'s `ON`, which pins nothing on null-extended rows.
- The leading keys of a `DISTINCT ON`.
- A query with `LIMIT`, `OFFSET` or `DISTINCT ON` whose every key is pinned
  or repeated: [`unstable_order`](unstable_order.md) reports it instead.
- The `ORDER BY` of a `FROM` subquery, a CTE, `ARRAY(…)`, or a set operation
  (`UNION` …).
- A membership subquery with `LIMIT`, `OFFSET`, `DISTINCT ON` or
  `FOR UPDATE` / `FOR SHARE`.
- Queries with `$..spread` parameters.

## Configuration

```toml
[package.metadata.typedpg.lint]
redundant_order_by = "warning"   # "error", "warning", "info" (default) or "off"
```

```json
{
  "lint": { "redundantOrderBy": "warning" }
}
```

`fail_on` / `failOn` sets the severity that fails the run (default `error`;
an info finding only fails it with `"info"`), and `ignore` lists glob
patterns of files whose query findings are dropped. For one statement:

```sql
-- typedpg-lint: allow(redundant_order_by)
SELECT id, email FROM users ORDER BY email, created_at;
```

## Related rules

- [`unstable_order`](unstable_order.md): the opposite problem — `ORDER BY`
  keys under `LIMIT` that can tie, so a tiebreaker is missing.
- [`redundant_group_by`](redundant_group_by.md): the same proof applied to
  `GROUP BY` keys.
- [`redundant_limit`](redundant_limit.md): a `LIMIT` on a query that returns
  at most one row.
