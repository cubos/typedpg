# `cartesian_join`

`FROM` items of one `SELECT` that no condition connects, so every row of one
pairs with every row of the other.

| Category | Level | Applies to | Fix |
|---|---|---|---|
| suspicious | warning | queries | — |

## Example

```sql
CREATE TABLE users (id bigint PRIMARY KEY, email text NOT NULL UNIQUE);
CREATE TABLE posts (id bigint PRIMARY KEY, user_id bigint NOT NULL REFERENCES users,
                    title text NOT NULL, status text NOT NULL);

SELECT u.email, p.title FROM users u, posts p WHERE p.status = 'published';
SELECT u.email, p.title FROM users u JOIN posts p ON p.status = 'published';
```

```
warning[suspicious/cartesian_join]: p is joined to u with no condition connecting them: every row of one pairs with every row of the other
   ╭─[queries.sql:2:39]
 2 │ SELECT u.email, p.title FROM users u, posts p WHERE p.status = 'published';
   ·                                       ───────
   ╰────
  help: add the join condition, or write CROSS JOIN if every combination is meant
```

The `JOIN … ON` is reported the same way: its condition only filters `p`.

## Why

10,000 users and 100,000 posts make a billion rows. It is nearly always a
forgotten join condition; small test data may hide it, production data
won't.

## Reported when

Two groups of `FROM` items have no connection between them. Items are
connected by:

- a `WHERE`, `ON` or `HAVING` condition (or one `AND` argument of it) that
  references both, directly or through other items;
- two `AND` arguments equating a column of each to the same parameter or
  outer column: `a.tenant_id = $t AND s.tenant_id = $t`;
- `USING (…)` or `NATURAL`;
- a `LATERAL` subquery or `FROM` function referencing items to its left.

An unqualified column counts for the table it belongs to.

## Not reported

- `CROSS JOIN`: the product is meant.
- An item that returns at most one row:
  `users u, (SELECT count(*) AS total FROM posts) t`, a subquery with
  `LIMIT 1`, a one-row `VALUES`, a `WITH` query of that kind (an
  `UPDATE … WHERE id = $id RETURNING …` too), or a table whose unique key
  the conditions on it alone pin: `projects p, users u WHERE p.id = $p AND
  u.email = $e`, `JOIN users u ON u.email = $e`. The conditions are `WHERE`
  and the `ON` of an inner join or of an outer join's nullable side.
- A function in `FROM`: `users u, generate_series(1, 7) AS d(day)`.
- A condition on a merged `USING` column, which may connect anything.
- `UPDATE … FROM` and `DELETE … USING`.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
cartesian_join = "error"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "cartesianJoin": "error" } }
```

Allow one query with `-- typedpg-lint: allow(cartesian_join)` in its text,
though `CROSS JOIN` says the same thing in SQL.
See [Configuring rules](README.md#configuring-rules).

## Related

- [`constant_condition`](constant_condition.md): a `JOIN … ON` that is
  always or never true.
- [`redundant_join`](redundant_join.md): a join that can't change the rows.
- [`outer_reference_in_subquery`](outer_reference_in_subquery.md): a
  subquery column that resolves to the outer query.
