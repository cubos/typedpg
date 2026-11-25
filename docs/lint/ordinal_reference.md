# `ordinal_reference`

`ORDER BY 1`, `GROUP BY 1` or `DISTINCT ON (1)`: a clause that picks an output
column by its position in the select list.

| Category | Level | Applies to | Fix |
|---|---|---|---|
| style | info | queries | names the column or expression |

## Example

```sql
CREATE TABLE users (id bigint PRIMARY KEY, name text NOT NULL);

SELECT id, name FROM users ORDER BY 1;
```

```
info[style/ordinal_reference]: ORDER BY 1 refers to the output column `id` by its position: name it instead
   ╭─[queries.sql:1:37]
 1 │ SELECT id, name FROM users ORDER BY 1;
   ·                                     ─
   ╰────
  help: write the column or its alias instead of its position
```

`--fix` gives `SELECT id, name FROM users ORDER BY id`. `ORDER BY 2` of
`SELECT id, name …` gives `ORDER BY name`.

## Why

`ORDER BY 1` means the first output column — until someone adds a column in
front of it, or reorders the list, and the query silently sorts by something
else. A name survives that edit, and says what it sorts by.

`GROUP BY 1` and `DISTINCT ON (1)` have the same problem, with a second one:
PostgreSQL resolves a *name* in `GROUP BY` to an input column before an output
column, so the alias that reads well in the select list may not name what the
position named.

## Reported when

- `ORDER BY <n>`, `GROUP BY <n>` or `DISTINCT ON (<n>)` where `<n>` is a plain
  integer constant that names a target of the same `SELECT`.
- A `GROUP BY <n>` whose target is an aliased expression, when no input column
  of the same `SELECT` has the alias's name: the help names the alias, which
  works there.

## Not reported

- A position outside the select list doesn't analyze, so it is never linted.
- `ORDER BY 1 + 0` is an expression, not a position.
- The `ORDER BY` of a `UNION`/`INTERSECT`/`EXCEPT`, or of a `VALUES` list,
  names the output column of a list that isn't a `SELECT`'s target list here.
- A `SELECT *` (or `t.*`): it expands to columns the parse tree doesn't list,
  so the positions after it map to no target, and one of its columns may take
  the name a target has. The select list's positions are not reported.
- A `GROUP BY` position whose output column's alias is also an input column's
  name: `GROUP BY` resolves a bare name to the input column first, so with
  `r.bucket` in `FROM`, `GROUP BY bucket` would group by `r.bucket`, not by
  `CASE … END AS bucket`. Repeating the whole expression is worse than the
  position, which is then the only short form. The same holds when a `FROM`
  item's columns can't be enumerated (a CTE, a function without a covering
  column alias list): a collision can't be ruled out, so the position is left
  alone.

## Fix

Only when the replacement is provably the same expression:

- `ORDER BY`: the target's alias, which `ORDER BY` resolves before an input
  column, or — without one — the target's column reference. An unqualified
  column is written qualified there (`ORDER BY users.id`) when another output
  column may carry the same name, since `ORDER BY` resolves a bare name to an
  output column first. Any other expression is reported without a fix:
  writing a volatile function a second time would evaluate it twice.
- `GROUP BY` and `DISTINCT ON`: the target's expression when it is a plain
  column reference, never its alias (see [Why](#why)).
- When the key it would write is ambiguous among the output columns, or a
  `DISTINCT ON` must keep matching it, the finding is reported without a fix.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
ordinal_reference = "warning"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "ordinalReference": "warning" } }
```

Allow one query with `-- typedpg-lint: allow(ordinal_reference)` in its text.
See [Configuring rules](README.md#configuring-rules).

## Related

- [`unstable_order`](unstable_order.md): `ORDER BY` keys that can tie under
  `LIMIT`, `OFFSET` or `DISTINCT ON`.
- [`redundant_order_by`](redundant_order_by.md): `ORDER BY` keys that can't
  change the order.
