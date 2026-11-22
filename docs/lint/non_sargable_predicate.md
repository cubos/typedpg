# `non_sargable_predicate`

A comparison that wraps an indexed column in a cast, function or arithmetic,
so the column's index can't serve it.

| Level | Applies to | Fix |
|---|---|---|
| info | queries | — |

## Example

```sql
CREATE TABLE users (id bigint PRIMARY KEY, email text NOT NULL, name text, created_at timestamptz NOT NULL);
CREATE INDEX users_created_at_idx ON users (created_at);
CREATE TABLE products (id int PRIMARY KEY, sku varchar(20) NOT NULL);
CREATE INDEX products_sku_idx ON products (sku);

SELECT id FROM users WHERE created_at::date = $day;
SELECT id FROM users WHERE date_trunc('day', created_at) = $day;
SELECT id FROM products WHERE lower(sku) = $sku;
SELECT id FROM users WHERE id::text = $id;
SELECT id FROM products WHERE id + 1 = $next;
SELECT sku FROM products WHERE id = 1.5;
```

```
info[non_sargable_predicate]: this comparison can't use the index users_created_at_idx on created_at: the column is cast to date
   ╭─[queries.sql:2:28]
 2 │ SELECT id FROM users WHERE created_at::date = $day;
   ·                            ───────────────────────
   ╰────
  help: compare a range on the column itself: created_at >= $day AND created_at < $day + 1

info[non_sargable_predicate]: this comparison can't use the index products_sku_idx on sku: the column is an argument of lower()
   ╭─[queries.sql:6:31]
 6 │ SELECT id FROM products WHERE lower(sku) = $sku;
   ·                               ─────────────────
   ╰────
  help: compare the column itself, or create an expression index on (lower(sku))

info[non_sargable_predicate]: this comparison can't use the index products_pkey on id: the column is converted from int4 to numeric to match the other side's type
    ╭─[queries.sql:12:32]
 12 │ SELECT sku FROM products WHERE id = 1.5;
    ·                                ────────
    ╰────
   help: cast the other side to int4 so the column is compared as stored
```

The help names the rewrite: a range on the bare column (`::date`,
`date_trunc`), converting the other side (a cast or implicit conversion), an
expression index (a function), or moving the computation (arithmetic).

## Why

An index matches only the bare column. `created_at::date`, `lower(email)`,
`id + 1` evaluate an expression for every row instead, usually in a
sequential scan: fast on a small development table, a timeout in
production.

## Reported when

A comparison (`=`, `<`, `<=`, `>`, `>=`, `BETWEEN`, `IN (…)`, `= ANY`) in a
top-level `AND` part of a `WHERE` or `JOIN … ON` (`SELECT`, `UPDATE`,
`DELETE`) has one side that wraps a column:

- a cast that converts it: `created_at::date`, `id::text`;
- a function with it as an argument: `lower(sku)`,
  `date_trunc('day', created_at)`;
- arithmetic or `||` with it and no other column: `id + 1`;
- an implicit conversion forced by the other side: `int4` column `= 1.5`.

And the index would otherwise apply:

- the column is a table's of this query level;
- a non-partial btree index has it as its first key;
- the other side reads no column of the same table, runs no subquery and
  calls nothing volatile.

## Not reported

- A cast to the column's own or a binary-compatible type:
  `sku::text = $sku` on `varchar`.
- A cross-type comparison the index supports: `id = 5::bigint` on `int4`.
- No btree index leads with the column: `lower(name) = $name`.
- The table has any expression index, which might match.
- Under `OR` or `NOT`; `<>`, `LIKE`, `IS DISTINCT FROM`.
- The other side reads the same table: `lower(sku) = sku`.
- A function of two columns, aggregates, window calls, arithmetic between
  columns.
- The select list, `HAVING`, `ORDER BY`, `ON CONFLICT`; columns of
  subqueries, CTEs, views, functions or an outer query.

A range on the bare column passes:
`created_at >= $start AND created_at < $start + interval '1 day'`.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
non_sargable_predicate = "warning"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "nonSargablePredicate": "warning" } }
```

Allow one query with `-- typedpg-lint: allow(non_sargable_predicate)` in its
text. See [Configuring rules](README.md#configuring-rules).

## Related

- [`timestamp_timezone_mix`](timestamp_timezone_mix.md): the `::date` range
  rewrite with a `date` value on a `timestamptz` column depends on the
  session's `TimeZone`.
- [`fk_without_index`](fk_without_index.md): a foreign key with no index at
  all.
