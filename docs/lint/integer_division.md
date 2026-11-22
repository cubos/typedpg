# `integer_division`

An integer division, which truncates, whose result is then used as a
`numeric` or floating-point value.

| Level | Applies to | Fix |
|---|---|---|
| warning | queries | — |

## Example

```sql
CREATE TABLE stats (id bigint PRIMARY KEY, done integer NOT NULL, total integer NOT NULL, ratio numeric);

SELECT id, round(done / total * 100.0, 1) AS pct FROM stats;
UPDATE stats SET ratio = done / total WHERE id = $id;
SELECT id FROM stats WHERE done / total > 0.5;
```

```
warning[integer_division]: integer division truncates before its result is used as numeric: the fraction is already lost
   ╭─[queries.sql:2:18]
 2 │ SELECT id, round(done / total * 100.0, 1) AS pct FROM stats;
   ·                  ────────────
   ╰────
  help: convert an operand before dividing: `x::numeric / y`
```

## Why

`/` between two integers truncates: `1 / 2` is `0`. Converting the result
afterwards doesn't bring the fraction back, so `round(1 / 2 * 100.0, 1)` is
`0.0`, and a progress ratio is stuck at 0 for every unfinished row.

## Reported when

Both operands of `/` are `smallint`, `integer` or `bigint` (or domains over them), and the result
is then:

- cast to `numeric`, `real` or `double precision`:
  `(done / total)::numeric`;
- combined with such a value: `done / total * 100.0`, `done / total > 0.5`;
- assigned to such a column: `SET ratio = done / total`;
- passed to `round`, `trunc`, `ceil`, `ceiling` or `floor`.

The division may be a factor of the converted value:
`(done / total * 100)::numeric` is reported too.

## Not reported

- The result stays an integer: `done / total AS whole`,
  `done / total > 1`, `avg(done / total)`.
- A term of a sum: `(done + total / 2)::numeric` may mean to halve `total`.
- An argument of any other function: `power(2, done / 10)`.
- A division by `1` or `-1`, or of two literals that divide exactly:
  `(10 / 2)::numeric`.

Convert an operand before dividing:
`round(done::numeric / total * 100, 1)`.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
integer_division = "error"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "integerDivision": "error" } }
```

Allow one query with `-- typedpg-lint: allow(integer_division)` in its text.
See [Configuring rules](README.md#configuring-rules).

## Related

- [`float_equality`](float_equality.md): an exact comparison of
  floating-point values.
- [`redundant_cast`](redundant_cast.md): a cast that does nothing.
