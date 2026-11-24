# `left_join_nullified`

A `WHERE` condition that rejects the NULL-extended rows of an outer join, so
the join behaves as an inner join.

| Category | Level | Applies to | Fix |
|---|---|---|---|
| suspicious | warning | queries | — |

## Example

```sql
CREATE TABLE users (id bigint PRIMARY KEY, name text);
CREATE TABLE posts (id bigint PRIMARY KEY, user_id bigint NOT NULL REFERENCES users, status text NOT NULL, score int);

SELECT u.id, p.status
FROM users u
LEFT JOIN posts p ON p.user_id = u.id
WHERE p.status = 'published';
SELECT u.id FROM users u LEFT JOIN posts p ON p.user_id = u.id WHERE p.id IS NOT NULL;
```

```
warning[suspicious/left_join_nullified]: this condition is never true for the NULL-extended rows of p, on the nullable side of a LEFT JOIN: the join behaves as an INNER JOIN
   ╭─[queries.sql:5:7]
 5 │ WHERE p.status = 'published';
   ·       ──────────────────────
   ╰────
  help: to keep the rows without a match in p, move the condition into the join's ON clause; to drop them, write an INNER JOIN

warning[suspicious/left_join_nullified]: this condition is never true for the NULL-extended rows of p, on the nullable side of a LEFT JOIN: the join behaves as an INNER JOIN
   ╭─[queries.sql:7:70]
 7 │ SELECT u.id FROM users u LEFT JOIN posts p ON p.user_id = u.id WHERE p.id IS NOT NULL;
   ·                                                                      ────────────────
   ╰────
  help: to keep only the rows with a match in p, write an INNER JOIN; to keep the unmatched rows, drop the condition
```

## Why

A user without posts comes out of the `LEFT JOIN` with every `p` column
NULL, and `p.status = 'published'` is not true for that row, so it is
dropped: exactly the rows the `LEFT JOIN` was written to keep. Either the
condition belongs in `ON` (keep every user, join only published posts), or
the join should be `JOIN` (which also makes the `p` columns non-nullable in
the generated types).

## Reported when

A `WHERE` condition of a `SELECT`, or one `AND`-ed part of it, is never true
when the nullable side is NULL-extended:

- an operator or function that returns NULL for NULL input, over a column of
  that side: `p.score > 0`, `lower(p.status) = 'x'`;
- `p.col IS NOT NULL`;
- an `OR` whose every arm rejects: `p.score = 1 OR p.score = 2`.

On a `FULL JOIN`, a condition on one side makes it behave as a `LEFT` or
`RIGHT` join, and the message says which. The message names the join as
written; when a foreign key already keeps one side from being NULL-extended
(`users u FULL JOIN posts p ON p.user_id = u.id` with `user_id` a `NOT NULL`
foreign key is a `LEFT JOIN`), it also says what the join behaves as before
the condition: `on the nullable side of a FULL JOIN, which already behaves as
a LEFT JOIN: the join behaves as an INNER JOIN`.

## Not reported

- The condition keeps NULL-extended rows: `p.id IS NULL` (the anti-join),
  `COALESCE(p.score, 0) = 0`, `IS DISTINCT FROM`,
  `p.score = 1 OR p.id IS NULL`, `($s::int IS NULL OR p.score = $s)`.
- The condition is in `ON` or `HAVING`, or on the preserved side.
- The join never NULL-extends that side: `posts p LEFT JOIN users u ON
  u.id = p.user_id` with `user_id` a `NOT NULL` foreign key.
- The `WHERE` of an `UPDATE` or `DELETE`.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
left_join_nullified = "error"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "leftJoinNullified": "error" } }
```

Allow one query with `-- typedpg-lint: allow(left_join_nullified)` in its
text. See [Configuring rules](README.md#configuring-rules).

## Related

- [`redundant_join`](redundant_join.md): a join that changes nothing.
- [`null_comparison`](null_comparison.md): a comparison with `NULL` that is
  never true.
