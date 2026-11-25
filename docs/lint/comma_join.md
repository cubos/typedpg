# `comma_join`

A `FROM` list that joins a relation to an earlier one with a comma instead of
`JOIN … ON`.

| Category | Level | Applies to | Fix |
|---|---|---|---|
| style | info | queries | — |

## Example

```sql
CREATE TABLE users (id bigint PRIMARY KEY, name text NOT NULL);
CREATE TABLE posts (id bigint PRIMARY KEY, user_id bigint NOT NULL REFERENCES users,
                    title text NOT NULL);

SELECT u.name, p.title FROM users u, posts p WHERE u.id = p.user_id;
```

```
info[style/comma_join]: p is joined to u with a comma: write JOIN … ON
   ╭─[queries.sql:1:38]
 1 │ SELECT u.name, p.title FROM users u, posts p WHERE u.id = p.user_id;
   ·                                      ───────
   ╰────
  help: move the condition tying them into an ON clause (CROSS JOIN when every combination is meant)
```

## Why

A comma between two relations is a cross join: the condition that really ties
them sits somewhere in the `WHERE` clause, away from either. A reader skimming
the `FROM` clause can't tell whether the product is meant; and a condition
deleted or moved later turns the query into a silent cross join. `JOIN … ON`
says which condition joins which pair, next to the pair.

## Not reported

- A single relation: there is no comma join.
- A `FROM` function: `FROM t, unnest(t.arr) x`, `jsonb_array_elements(…)`,
  `generate_series(…)`, `JSON_TABLE(…)`. The comma is the idiomatic way to
  write the lateral join a set-returning function forms.
- A `LATERAL` subquery: `FROM t, LATERAL (SELECT …) s` is that lateral join
  too.
- A comma between two functions, or before the first relation of the list: no
  two relations are joined by it.

A `VALUES` list (`VALUES (1, 2), (3, 4)`) has no `FROM` clause and is not
reported. A `(VALUES …)` subquery in `FROM` is a relation like any other, so a
comma joining it to another relation is reported.

## Fix not offered

Rewriting the pair as `JOIN … ON` means moving the conditions that tie them out
of `WHERE`; which conditions those are, and in what order, is the author's
call. [`cartesian_join`](cartesian_join.md) reports the pairs no condition
connects at all.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
comma_join = "error"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "commaJoin": "error" } }
```

Allow one query with `-- typedpg-lint: allow(comma_join)` in its text.
See [Configuring rules](README.md#configuring-rules).

## Related

- [`cartesian_join`](cartesian_join.md): `FROM` items no condition connects.
- [`redundant_join`](redundant_join.md): a join that can't change the rows.
