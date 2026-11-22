# `integer_division`

An integer division, which truncates, whose result is then used as a
`numeric` or floating-point value.

| | |
|---|---|
| Default level | warning |
| Applies to | queries |
| Fix | no |

## Why

In PostgreSQL, `/` between two integers (`smallint`, `integer`, `bigint`) is
integer division: `7 / 2` is `3`, `1 / 2` is `0`. The fraction is gone before
anything else happens. Converting the result afterwards does not bring it
back:

```sql
SELECT round(1 / 2 * 100.0, 1);   -- 0.0, not 50.0
SELECT (done / total)::numeric;   -- 0 for every unfinished row
```

The query type-checks, the column comes out as `numeric`, and it is wrong
for every row where `done < total`. The usual result is a progress bar stuck
at 0% or a ratio that is always 0 or 1.

## What it reports

A division whose two operands are integers (by the types the analyzer
inferred, domains over integers included), when the integer result is then
converted to `numeric`, `real` or `double precision`:

- cast: `(done / total)::numeric`, `CAST(big / small AS double precision)`;
- combined with a fractional value: `done / total * 100.0`,
  `done / total > 0.5`, `COALESCE(done / total, 0.5)`;
- assigned to a `numeric` or floating-point column: `SET ratio = done /
  total`;
- passed to a rounding function: `round`, `trunc`, `ceil`, `ceiling`,
  `floor`. These have nothing to round in an integer, so the call shows the
  fraction was expected.

The division may also be a factor of the converted value: in
`(done / total * 100)::numeric` the truncation happens before the
multiplication. Factors of a product and the operand of a negation are
followed; terms of a sum are not (`a + b / 2` may mean to halve `b`).

The finding is placed on the division.

## Examples

```sql
CREATE TABLE stats (
    id bigint PRIMARY KEY,
    done integer NOT NULL,
    total integer NOT NULL,
    ratio numeric
);
```

### Bad

```sql
SELECT id, round(done / total * 100.0, 1) AS pct FROM stats;
```

```
warning[integer_division]: integer division truncates before its result is used as numeric: the fraction is already lost
   ╭─[queries.sql:1:18]
 1 │ SELECT id, round(done / total * 100.0, 1) AS pct FROM stats;
   ·                  ────────────
   ╰────
  help: convert an operand before dividing: `x::numeric / y`
```

```sql
UPDATE stats SET ratio = done / total WHERE id = $id;
```

```
warning[integer_division]: integer division truncates before its result is used as numeric: the fraction is already lost
   ╭─[queries.sql:1:26]
 1 │ UPDATE stats SET ratio = done / total WHERE id = $id;
   ·                          ────────────
   ╰────
  help: convert an operand before dividing: `x::numeric / y`
```

```sql
SELECT id FROM stats WHERE done / total > 0.5;
```

```
warning[integer_division]: integer division truncates before its result is used as numeric: the fraction is already lost
   ╭─[queries.sql:1:28]
 1 │ SELECT id FROM stats WHERE done / total > 0.5;
   ·                            ────────────
   ╰────
  help: convert an operand before dividing: `x::numeric / y`
```

### Good

```sql
-- Convert an operand before dividing.
SELECT id, round(done::numeric / total * 100, 1) AS pct FROM stats;

-- The integer result stays an integer: truncation may be intended.
SELECT id, done / total AS whole FROM stats;

-- Another function's argument: stepping may be on purpose.
SELECT id, power(2, done / 10) AS steps FROM stats;

-- Two literals that divide exactly lose nothing.
SELECT (10 / 2)::numeric AS five;
```

A division by the literal `1` or `-1` is not reported either, nor is an
integer division whose result is only aggregated (`avg(done / total)`,
`sum(done / total)`) or compared with another integer (`done / total > 1`).

## Fix

None. Which operand to convert, and to which type, is the author's choice;
the help line shows the usual pattern, `x::numeric / y`.

## When it stays silent

- The division's result is used as an integer.
- It is a term of a sum or difference rather than a factor of the value
  that is converted.
- It is an argument of any function other than the five rounding functions
  (`power`, `sqrt`, your own functions).
- An operand is not an integer type (`numeric`, `double precision`).

## Configuration

```toml
[package.metadata.typedpg.lint]
integer_division = "error"   # "error", "warning", "info" or "off"
```

```json
{
  "lint": { "integerDivision": "error" }
}
```

`fail_on` / `failOn` sets the severity that fails the run (default `error`),
and `ignore` lists glob patterns of files whose query findings are dropped.
For one statement:

```sql
-- typedpg-lint: allow(integer_division)
SELECT (done / total)::numeric AS r FROM stats;
```

## Related rules

- [`redundant_cast`](redundant_cast.md): a cast that does nothing.
