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
  help: compare a range on the column itself: `created_at >= $day::date::timestamptz AND created_at < ($day::date + 1)::timestamptz` reads days in the session's TimeZone as the cast does, and selects the same rows but around a daylight-saving change at midnight; for days of a fixed zone instead (other rows, unless the session is in that zone), convert each bound with `::timestamp AT TIME ZONE 'UTC'`

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
   help: cast the other side to int4 so the column is compared as stored (the same rows only where that cast is exact: a value it rounds or truncates compares differently)
```

The help names the rewrite: a range on the bare column (`::date`,
`date_trunc`), converting the other side (a cast or implicit conversion), an
expression index (a function), or moving the computation (arithmetic).

A range is named only where one selects the same rows, and it is one no
other rule reports:

- `timestamp` column, `col::date` compared with a `date`: the date's
  midnight is the bound (`col >= $day::date AND col < $day::date + 1`),
  exactly the same rows. `<`, `<=`, `>`, `>=` get their one bound,
  `BETWEEN` its two (`col < $to::date + 1` for the upper one).
- `timestamptz` column: `::date` reads the day in the session's `TimeZone`,
  and so do the bounds, converted explicitly (`$day::date::timestamptz`)
  rather than compared as a `date`, which
  [`timestamp_timezone_mix`](timestamp_timezone_mix.md) reports. The rows
  differ only around a daylight-saving change at midnight (a skipped or
  repeated midnight). Days of a fixed zone
  (`$day::date::timestamp AT TIME ZONE 'UTC'`) stop depending on the
  session, but select other rows unless the session is in that zone.
- `date_trunc('unit', col) = $t`: `col >= $t AND col < $t + interval '1
  unit'`, the same rows when `$t` is itself truncated (`date_trunc` never
  equals anything else).
- A `date_trunc` with a time zone argument, a unit with no interval, or the
  other side of another type (`col::date = localtimestamp`) gets no range;
  a `::date` cast compared with a list (`IN`, `= ANY`) gets the advice to
  compare each day as a range, not to convert the other side (which would
  compare instants with each day's midnight).

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

- [`timestamp_timezone_mix`](timestamp_timezone_mix.md): a range on a
  `timestamptz` column with bare `date` bounds converts them in the
  session's `TimeZone` implicitly; the suggested range converts them
  explicitly.
- [`fk_without_index`](fk_without_index.md): a foreign key with no index at
  all.
