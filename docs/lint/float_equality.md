# `float_equality`

Reports an `=`, `<>`, `IN` or `NOT IN` comparison PostgreSQL makes between
floating-point (`real` / `double precision`) values.

| | |
|---|---|
| Default level | warning |
| Applies to | queries |
| Fix | no |

## Why

`real` and `double precision` store binary fractions. Most decimals have no
exact binary form, so the value stored for `19.99` is the nearest binary
fraction, and a value computed in floating point lands on whichever binary
fraction the arithmetic rounds to: in PostgreSQL, `SELECT 0.1::float8 +
0.2::float8 = 0.3::float8` is `false`. An exact comparison against such a
value matches some rows and silently misses others, depending on how each
value was produced — a price computed as `total / qty` may never equal the
`19.99` the user typed.

This is a warning, not an info: the query is not slow, it returns the wrong
rows.

## What it reports

A comparison with `=`, `<>` / `!=`, `IN (…)` or `NOT IN (…)` whose operator,
after PostgreSQL resolves the operands' implicit conversions, compares
`real` or `double precision` values. typedpg reads the operator the
analyzer resolved, so `price = 19.99` over a `double precision` column
counts (the `numeric` literal is converted to `float8`), and
`price::numeric = 19.99` doesn't (that compares `numeric`s).

A comparison with a value that is exact in binary floating point is not
reported, because testing for one is well defined and common:

- an integer literal the type holds exactly: `score = 0`, `rating = 5`.
  `real` has 24 bits of mantissa and `double precision` 53, so every integer
  up to 2^24 (16 777 216) in magnitude is exact for `real` and up to 2^53
  for `double precision`; beyond that, only those that are a smaller
  integer times a power of two are (`33554432` is exact for `real`,
  `16777217` is not);
- a decimal literal whose value is a binary fraction that fits: `0.5`,
  `2.25`, `1e3` — but not `0.1`;
- the special values `'NaN'`, `'Infinity'` and `'-Infinity'`;
- any of those negated (`-0.5`) or cast (`2.25::float8`).

The type is `real` when either operand of the operator is `real`, and
`double precision` otherwise. An `=` / `<>` is exempt when either side is
such a value; an `IN` / `NOT IN` list only when every item is. A column, a
parameter or a computed value on both sides — a join `a.f = b.f` included —
is reported. The finding spans the comparison.

## Examples

```sql
CREATE TABLE products (
    id int PRIMARY KEY,
    price double precision NOT NULL,
    weight real,
    amount numeric
);
```

### Bad

```sql
SELECT id FROM products WHERE price = 19.99;
SELECT id FROM products WHERE price IN (0.1, 0.2);
SELECT id FROM products WHERE price = $price;
SELECT id FROM products WHERE weight = 16777217;
```

```
warning[float_equality]: = compares double precision values exactly: a floating-point value rarely equals the decimal it is written as (0.1 + 0.2 = 0.3 is false), so rows may be missed
   ╭─[queries.sql:2:31]
 2 │ SELECT id FROM products WHERE price = 19.99;
   ·                               ─────────────
   ╰────
  help: compare within a tolerance (abs(a - b) < 1e-9) or a range, or store exact values as numeric

warning[float_equality]: IN compares double precision values exactly: a floating-point value rarely equals the decimal it is written as (0.1 + 0.2 = 0.3 is false), so rows may be missed
   ╭─[queries.sql:5:31]
 5 │ SELECT id FROM products WHERE price IN (0.1, 0.2);
   ·                               ───────────────────
   ╰────
  help: compare within a tolerance (abs(a - b) < 1e-9) or a range, or store exact values as numeric

warning[float_equality]: = compares double precision values exactly: a floating-point value rarely equals the decimal it is written as (0.1 + 0.2 = 0.3 is false), so rows may be missed
   ╭─[queries.sql:8:31]
 8 │ SELECT id FROM products WHERE price = $price;
   ·                               ──────────────
   ╰────
  help: compare within a tolerance (abs(a - b) < 1e-9) or a range, or store exact values as numeric

warning[float_equality]: = compares real values exactly: a floating-point value rarely equals the decimal it is written as (0.1 + 0.2 = 0.3 is false), so rows may be missed
    ╭─[queries.sql:11:31]
 11 │ SELECT id FROM products WHERE weight = 16777217;
    ·                               ─────────────────
    ╰────
   help: compare within a tolerance (abs(a - b) < 1e-9) or a range, or store exact values as numeric
```

The last one is an integer, but `real` can't hold 16777217: a stored
`16777217` became `16777216`, which no longer equals it.

### Good

```sql
-- Within a tolerance, or a range.
SELECT id FROM products WHERE abs(price - 19.99) < 1e-9;
SELECT id FROM products WHERE price >= 19.985 AND price < 19.995;

-- Values exact in binary floating point.
SELECT id FROM products WHERE price = 0;
SELECT id FROM products WHERE price = 2.5;
SELECT id FROM products WHERE price = 'NaN';
SELECT id FROM products WHERE weight = 16777216;
SELECT id FROM products WHERE price = 9007199254740992;

-- numeric is exact: store money and other decimals there.
SELECT id FROM products WHERE amount = 19.99;
SELECT id FROM products WHERE price::numeric = 19.99;

-- IS DISTINCT FROM is not judged.
SELECT id FROM products WHERE price IS DISTINCT FROM 0.1;
```

## Fix

None: whether to compare within a tolerance (and which), compare a range,
or store the value as `numeric` is the author's choice.

## When it stays silent

- The operator compares another type: `numeric`, an integer type, or a
  float cast to `numeric` first.
- One side (or every item of an `IN` list) is a literal exact in binary
  floating point, as listed above.
- Ordering comparisons (`<`, `>=`, `BETWEEN`), `IS DISTINCT FROM`, and
  `= ANY (array)`.
- An operator written with a schema other than `pg_catalog`
  (`OPERATOR(myschema.=)`), which may be anything.

## Configuration

```toml
[package.metadata.typedpg.lint]
float_equality = "error"   # "error", "warning" (default), "info" or "off"
```

```json
{
  "lint": { "floatEquality": "error" }
}
```

`fail_on` / `failOn` sets the severity that fails the run (default `error`),
and `ignore` lists glob patterns of files whose query findings are dropped.
For one statement:

```sql
-- typedpg-lint: allow(float_equality)
SELECT id FROM products WHERE price = 19.99;
```

## Related rules

- [`integer_division`](integer_division.md): another arithmetic surprise —
  an integer division truncated before it is used as a non-integer value.
- [`null_comparison`](null_comparison.md): a comparison with `NULL`, which
  is never true.
