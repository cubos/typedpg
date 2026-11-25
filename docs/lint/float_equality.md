# `float_equality`

An `=`, `<>`, `IN` or `NOT IN` comparison of `real` or `double precision`
values.

| Category | Level | Applies to | Fix |
|---|---|---|---|
| suspicious | warning | queries | — |

## Example

```sql
CREATE TABLE products (id int PRIMARY KEY, price double precision NOT NULL, weight real, amount numeric);

SELECT id FROM products WHERE price = 19.99;
SELECT id FROM products WHERE weight = 16777217;
SELECT id FROM products WHERE price IN (0.1, 0.2);
SELECT id FROM products WHERE price = $price;
```

```
warning[suspicious/float_equality]: = compares double precision values exactly: a floating-point value rarely equals the decimal it is written as (0.1 + 0.2 = 0.3 is false), so rows may be missed
   ╭─[queries.sql:2:31]
 2 │ SELECT id FROM products WHERE price = 19.99;
   ·                               ─────────────
   ╰────
  help: compare within a tolerance (abs(a - b) < 1e-9) or a range, or store exact values as numeric

warning[suspicious/float_equality]: = compares real values exactly: a floating-point value rarely equals the decimal it is written as (0.1 + 0.2 = 0.3 is false), so rows may be missed
   ╭─[queries.sql:4:31]
 4 │ SELECT id FROM products WHERE weight = 16777217;
   ·                               ─────────────────
   ╰────
  help: compare within a tolerance (abs(a - b) < 1e-9) or a range, or store exact values as numeric
```

`16777217` is an integer, but `real` can't hold it: a stored `16777217`
became `16777216`.

## Why

Most decimals have no exact binary form, and floating-point arithmetic
rounds: `0.1::float8 + 0.2::float8 = 0.3::float8` is `false`. An exact
comparison matches some rows and silently misses others, depending on how
each value was produced.

## Reported when

The comparison's operator compares `real` or `double precision` after
implicit conversions: `price = 19.99` counts (the literal becomes
`double precision`), as do a column, a parameter or an expression on both
sides, joins included.

## Not reported

- One side is a literal the type holds exactly (for `IN`, every item):
  - an integer up to 2^24 in magnitude for `real`, 2^53 for
    `double precision`, or a larger one that is such an integer times a
    power of two: `price = 0`, `weight = 16777216`;
  - a binary fraction that fits: `0.5`, `2.25`, `1e3` (not `0.1`);
  - `'NaN'`, `'Infinity'`, `'-Infinity'`;
  - any of those negated or cast: `-0.5`, `2.25::float8`.
- The operator compares another type: `amount = 19.99` on `numeric`,
  `price::numeric = 19.99`.
- `<`, `>=`, `BETWEEN`, `IS DISTINCT FROM`, `= ANY (array)`. A range or a
  tolerance is the usual replacement:
  `abs(price - 19.99) < 1e-9`.
- An operator in a schema other than `pg_catalog`: `OPERATOR(myschema.=)`.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
float_equality = "error"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "floatEquality": "error" } }
```

Allow one query with `-- typedpg-lint: allow(float_equality)` in its text.
See [Configuring rules](README.md#configuring-rules).

## Related

- [`integer_division`](integer_division.md): an integer division truncated
  before it is used as a non-integer value.
