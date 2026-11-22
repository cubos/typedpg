# `non_sargable_predicate`

Reports a `WHERE` or `JOIN … ON` comparison that wraps an indexed column in a
cast, a function or arithmetic, so the column's index can't serve it.

| | |
|---|---|
| Default level | info |
| Applies to | queries |
| Fix | no |

## Why

PostgreSQL matches a btree index to a condition only when one side is the
indexed column itself (a binary-compatible relabeling such as `varchar` to
`text` aside). Wrap the column in anything — `created_at::date`,
`lower(email)`, `id + 1`, `id::text` — and the index on it no longer applies:
the planner falls back to a sequential scan, or a scan of some other index,
evaluating the expression for every row. On a table that is small in
development and large in production, that is the classic query that was
fast in tests and times out in production.

The fix is usually mechanical: compare the bare column with a converted
value (`created_at >= $day AND created_at < $day + 1`), move the arithmetic
to the other side (`id = $next - 1`), or create an expression index on the
wrapped expression.

## What it reports

A comparison with `=`, `<`, `<=`, `>`, `>=`, `BETWEEN`, `IN (…)` or `= ANY`
in a `SELECT`'s `WHERE` or `JOIN … ON`, or an `UPDATE` / `DELETE`'s
`WHERE`, where one side wraps a column:

- **A cast that converts the value**: `created_at::date = $1`,
  `id::text = $1`. A cast to the type the column already has, or to a
  binary-compatible one (`varchar` to `text`), keeps the column bare.
- **A function call with the column as an argument**: `lower(email) = $1`,
  `date_trunc('day', created_at) = $1`.
- **Arithmetic or concatenation** (`+ - * / % ||`) with the column as one
  operand and no other column: `id + 1 = $1`.
- **An implicit conversion** the other side's type forces on the column: an
  `int4` column compared with a `numeric` value (`id = 1.5`) is converted to
  `numeric` on every row.

It only reports when the comparison would otherwise have an index, which
typedpg reads from the schema the migrations build:

- the column belongs to a plain or partitioned table of the same query
  level (not a subquery, CTE, view, function or outer query);
- a non-partial btree index of that table has the column as its **first**
  key;
- the table has no expression index at all (one might match the wrapped
  expression);
- the other side is a value an index scan could compare with: it reads no
  column of the same table, runs no subquery and calls nothing volatile.

Only the top-level `AND` parts of a condition are read. The finding spans
the comparison and names the index. The help names a rewrite: a range on
the bare column for `::date` on a `timestamp` / `timestamptz` column and for
`date_trunc` with an equality, converting the other side for a cast, an
expression index for a function call, moving the computation for
arithmetic, and casting the other side for an implicit conversion.

## Examples

```sql
CREATE TABLE users (
    id bigint PRIMARY KEY,
    email text NOT NULL,
    name text,
    created_at timestamptz NOT NULL
);
CREATE INDEX users_created_at_idx ON users (created_at);

CREATE TABLE posts (
    id bigint PRIMARY KEY,
    user_id bigint NOT NULL REFERENCES users
);
CREATE INDEX posts_user_id_idx ON posts (user_id);

CREATE TABLE products (
    id int PRIMARY KEY,
    sku varchar(20) NOT NULL
);
CREATE INDEX products_sku_idx ON products (sku);
```

### Bad

```sql
SELECT id FROM users WHERE created_at::date = $day;
SELECT id FROM users WHERE date_trunc('day', created_at) = $day;
SELECT id FROM products WHERE lower(sku) = $sku;
SELECT id FROM users WHERE id::text = $id;
SELECT id FROM products WHERE id + 1 = $next;
SELECT sku FROM products WHERE id = 1.5;
DELETE FROM posts WHERE user_id::text = $user;
```

```
info[non_sargable_predicate]: this comparison can't use the index users_created_at_idx on created_at: the column is cast to date
   ╭─[queries.sql:2:28]
 2 │ SELECT id FROM users WHERE created_at::date = $day;
   ·                            ───────────────────────
   ╰────
  help: compare a range on the column itself: created_at >= $day AND created_at < $day + 1

info[non_sargable_predicate]: this comparison can't use the index users_created_at_idx on created_at: the column is an argument of date_trunc()
   ╭─[queries.sql:5:28]
 5 │ SELECT id FROM users WHERE date_trunc('day', created_at) = $day;
   ·                            ────────────────────────────────────
   ╰────
  help: compare a range on the column itself: created_at >= $day AND created_at < $day + interval '1 day'

info[non_sargable_predicate]: this comparison can't use the index products_sku_idx on sku: the column is an argument of lower()
   ╭─[queries.sql:8:31]
 8 │ SELECT id FROM products WHERE lower(sku) = $sku;
   ·                               ─────────────────
   ╰────
  help: compare the column itself, or create an expression index on (lower(sku))

info[non_sargable_predicate]: this comparison can't use the index users_pkey on id: the column is cast to text
    ╭─[queries.sql:11:28]
 11 │ SELECT id FROM users WHERE id::text = $id;
    ·                            ──────────────
    ╰────
   help: compare the column itself, converting the other side to int8 instead

info[non_sargable_predicate]: this comparison can't use the index products_pkey on id: the column is an operand of +
    ╭─[queries.sql:14:31]
 14 │ SELECT id FROM products WHERE id + 1 = $next;
    ·                               ──────────────
    ╰────
   help: move the computation to the other side so the column stands alone

info[non_sargable_predicate]: this comparison can't use the index products_pkey on id: the column is converted from int4 to numeric to match the other side's type
    ╭─[queries.sql:17:32]
 17 │ SELECT sku FROM products WHERE id = 1.5;
    ·                                ────────
    ╰────
   help: cast the other side to int4 so the column is compared as stored

info[non_sargable_predicate]: this comparison can't use the index posts_user_id_idx on user_id: the column is cast to text
    ╭─[queries.sql:20:25]
 20 │ DELETE FROM posts WHERE user_id::text = $user;
    ·                         ─────────────────────
    ╰────
   help: compare the column itself, converting the other side to int8 instead
```

### Good

```sql
-- A range on the bare column.
SELECT id FROM users WHERE created_at >= $day::date AND created_at < $day::date + 1;

-- The bare column; varchar to text is binary-compatible, so the index applies.
SELECT id FROM products WHERE sku = $sku;
SELECT id FROM products WHERE sku::text = $sku;

-- A cross-type operator of the index's operator family: int4 = int8 uses
-- the index without converting the column.
SELECT id FROM products WHERE id = 5::bigint;

-- No index leads with name.
SELECT id FROM users WHERE lower(name) = $name;

-- <> is not a comparison a btree index serves.
SELECT id FROM users WHERE lower(email) <> $email;

-- Not a top-level AND part of WHERE.
SELECT id FROM users WHERE created_at::date = $day OR id = 2;

-- In the select list, not a filter.
SELECT created_at::date = $day FROM users;

-- The other side reads the same table: no index scan could use it anyway.
SELECT id FROM products WHERE lower(sku) = sku;
```

```sql
-- With an expression index on the table, the rule stays silent for every
-- wrapped column of it: the expression index may be the one that matches.
CREATE INDEX products_lower_sku_idx ON products (lower(sku));
SELECT id FROM products WHERE lower(sku) = $sku;
```

## Fix

None: whether to rewrite the comparison, convert the parameter or add an
expression index is the author's choice, and a range rewrite of `::date`
must pick the time zone the day is in. The help names the rewrite.

## When it stays silent

- No non-partial btree index of the table has the column as its first key
  (an index on `(tenant, email)` doesn't make `lower(email)` reportable).
- The table has any expression index.
- The column is a subquery's, CTE's, view's or function's output, or an
  outer query's.
- The other side reads a column of the same table, contains a subquery, or
  calls a volatile function.
- A comparison under `OR` or `NOT`, a `<>`, `LIKE`, `IS DISTINCT FROM` and
  other operators.
- A function call with more than one column argument, an aggregate or a
  window call, or arithmetic between two columns.
- Comparisons in the select list, `HAVING`, `ORDER BY` or an
  `INSERT … ON CONFLICT`.

## Configuration

```toml
[package.metadata.typedpg.lint]
non_sargable_predicate = "warning"   # "error", "warning", "info" (default) or "off"
```

```json
{
  "lint": { "nonSargablePredicate": "warning" }
}
```

`fail_on` / `failOn` sets the severity that fails the run (default `error`;
an info finding only fails it with `"info"`), and `ignore` lists glob
patterns of files whose query findings are dropped. For one statement:

```sql
-- typedpg-lint: allow(non_sargable_predicate)
SELECT id FROM users WHERE created_at::date = $day;
```

## Related rules

- [`timestamp_timezone_mix`](timestamp_timezone_mix.md): a `timestamp` /
  `timestamptz` conversion that depends on the session's `TimeZone`, which a
  `::date` comparison or its range rewrite can involve.
- [`fk_without_index`](fk_without_index.md): a foreign key whose columns
  have no index at all.
