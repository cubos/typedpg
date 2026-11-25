# `boolean_comparison`

A comparison of a `boolean` expression with the literal `true` or `false`:
`active = true`, `active = false`, and the same with `<>` / `!=`, the literal
on either side.

| Category | Level | Applies to | Fix |
|---|---|---|---|
| style | info | queries | writes the expression, or `NOT` it |

## Example

```sql
CREATE TABLE flags (id bigint PRIMARY KEY, active boolean NOT NULL);

SELECT id FROM flags WHERE active = true;
SELECT id FROM flags WHERE active = false;
```

```
info[style/boolean_comparison]: comparison with the literal true: it is the value itself
   ╭─[queries.sql:1:28]
 1 │ SELECT id FROM flags WHERE active = true;
   ·                            ─────────────
   ╰────
  help: write the boolean expression itself
```

`--fix` gives `SELECT id FROM flags WHERE active` and
`SELECT id FROM flags WHERE NOT active`. The four forms and their rewrites:

| written | same as |
|---|---|
| `x = true`, `x <> false` | `x` |
| `x = false`, `x <> true` | `NOT x` |

## Why

`active = true` says the same as `active`, and reads as if the column were not
a boolean. `active = false` says `NOT active`. The rewrite is safe: both forms
treat NULL the same way (`NULL = true` is NULL, as `NULL` is; `NOT NULL` is
NULL), and the result is a boolean either way.

## Not reported

- `IS TRUE`, `IS NOT TRUE`, `IS FALSE` and `IS NOT FALSE`: those turn NULL into
  a value, which the comparison's rewrite would change.
- A domain over `boolean`: the rewrite would change the result's type.
- A comparison of two literals (`true = false`), a comparison with a non-boolean
  literal (`active = 'true'`), and `active = NULL`: nothing to stand alone, or
  another rule's finding.
- A select or `RETURNING` list's own column without an alias: rewriting it would
  change the output column's name (`?column?` to the expression's own name), so
  the finding is reported without a fix. An aliased column keeps its name.
- A `CASE`'s `WHEN` condition: the analyzer proves a value set of the result
  through `x = true` that it doesn't through a bare `x`, and a fix never
  changes an output column's refinement. The finding is reported without a
  fix.

## Fix

The expression as written, wrapped in parentheses when it is an `AND` / `OR` /
`NOT` expression or an `IS [NOT] NULL` / `IS [NOT] TRUE` test — so
`(a AND b) = true` becomes `(a AND b)`, not `a AND b`, which would swallow the
code around it.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
boolean_comparison = "warning"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "booleanComparison": "warning" } }
```

Allow one query with `-- typedpg-lint: allow(boolean_comparison)` in its text.
See [Configuring rules](README.md#configuring-rules).

## Related

- [`constant_condition`](constant_condition.md): a condition that is always, or
  never, true.
- [`simplifiable_case`](simplifiable_case.md): a `CASE` that is a `COALESCE` or
  its own condition.
