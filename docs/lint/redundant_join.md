# `redundant_join`

A join that can't change the rows and whose table nothing in the query reads.

| Category | Level | Applies to | Fix |
|---|---|---|---|
| redundant | info | queries | removes the join |

## Example

```sql
CREATE TABLE users (id bigint PRIMARY KEY, email text NOT NULL UNIQUE, name text);
CREATE TABLE posts (
    id bigint PRIMARY KEY,
    user_id bigint NOT NULL REFERENCES users,
    editor_id bigint REFERENCES users,
    title text NOT NULL
);

SELECT p.id, p.title FROM posts p LEFT JOIN users e ON e.id = p.editor_id;
SELECT p.id, p.title FROM posts p JOIN users u ON u.id = p.user_id;
```

```
info[redundant/redundant_join]: the LEFT JOIN of e can't change the rows (each row matches at most one row of it, on a unique key) and none of its columns are used
   ╭─[queries.sql:2:35]
 2 │ SELECT p.id, p.title FROM posts p LEFT JOIN users e ON e.id = p.editor_id;
   ·                                   ───────────────────────────────────────
   ╰────
  help: remove the join, or read the columns it was meant to provide
```

The second is reported as `the JOIN of u can't change the rows (a foreign
key finds exactly one row of it for each row) …`. `--fix` turns both into
`SELECT p.id, p.title FROM posts p`.

## Why

The join brings in no columns and neither drops nor repeats rows, yet
PostgreSQL may still look up the joined row for every result row. Often the
query was meant to read something from it (`e.name AS editor`) and doesn't.

## Reported when

The join is in the `FROM` of a plain `SELECT`, joins a table, and no column
of that table is read anywhere in the query, and either:

- `LEFT JOIN t ON …` equates, with `AND`ed `=` between columns of the same
  type, every column of a unique key of `t` (non-partial, non-deferrable):
  each row matches at most one row of `t`;
- `[INNER] JOIN parent ON …` equates exactly the columns of a foreign key
  that is `NOT NULL`, validated and not deferrable, into a parent without
  row-level security: each row matches exactly one parent row.

## Not reported

- A column of the joined table is used:
  `SELECT p.id, p.title, e.name FROM posts p LEFT JOIN users e ON e.id = p.editor_id`.
- An inner join on a nullable foreign key, which drops rows:
  `JOIN users e ON e.id = p.editor_id`.
- A join from the referenced side, which repeats rows:
  `FROM users u LEFT JOIN posts p ON p.user_id = u.id`.
- An `ON` with an extra condition, `OR`, or only part of a composite key.
- A `DEFERRABLE` or `NOT VALID` foreign key; an inner join on a unique key
  no foreign key backs.
- `USING`, `NATURAL`, `LATERAL`, subquery and function joins.
- Queries with `WITH`, set operations, `FOR UPDATE`/`FOR SHARE`, views,
  volatile functions or `$..spread` parameters; `UPDATE … FROM` and
  `DELETE … USING`.
- A join whose removal would change a selected column's type or allowed
  values, e.g. through a `CHECK` on the parent's key.

## Fix not offered

When a comment is inside or against the join's text:
`JOIN users u /* author */ ON u.id = p.user_id`.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
redundant_join = "warning"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "redundantJoin": "warning" } }
```

Allow one query with `-- typedpg-lint: allow(redundant_join)` in its text.
See [Configuring rules](README.md#configuring-rules).

## Related

- [`unused_cte`](unused_cte.md): a `WITH` query nothing reads.
- [`left_join_nullified`](left_join_nullified.md): a `LEFT JOIN` the `WHERE`
  turns into an inner join.
- [`cartesian_join`](cartesian_join.md): a join with no condition.
