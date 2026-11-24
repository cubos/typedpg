# `unbounded_write`

An `UPDATE` or `DELETE` without a `WHERE` clause.

| Category | Level | Applies to | Fix |
|---|---|---|---|
| suspicious | warning | queries | — |

## Example

```sql
CREATE TABLE users (id bigint PRIMARY KEY, deleted_at timestamptz);
CREATE TABLE posts (id bigint PRIMARY KEY, user_id bigint REFERENCES users);

UPDATE users SET deleted_at = now();
WITH gone AS (DELETE FROM posts RETURNING id) SELECT count(*) AS n FROM gone;
```

```
warning[suspicious/unbounded_write]: UPDATE without WHERE changes every row of users
   ╭─[queries.sql:2:1]
 2 │ UPDATE users SET deleted_at = now();
   · ────────────
   ╰────
  help: add a WHERE clause, or allow it with -- typedpg-lint: allow(unbounded_write)

warning[suspicious/unbounded_write]: DELETE without WHERE removes every row of posts
   ╭─[queries.sql:4:15]
 4 │ WITH gone AS (DELETE FROM posts RETURNING id) SELECT count(*) AS n FROM gone;
   ·               ─────────────────
   ╰────
  help: add a WHERE clause, or allow it with -- typedpg-lint: allow(unbounded_write)
```

## Why

Writing every row is almost always a lost or forgotten condition, and the
damage is done in one statement. Requiring an allow comment makes the rare
deliberate case visible in review.

## Reported when

Any `UPDATE` or `DELETE` without `WHERE`, including one inside a CTE, on a
table that may hold more than one row. Only the presence of `WHERE` is
checked, not what it says.

## Not reported

- Any `WHERE`, including `WHERE CURRENT OF`, `DELETE … USING … WHERE …`, and
  conditions that are always true: `WHERE true` is reported by
  [`constant_condition`](constant_condition.md) instead.
- A table that provably holds at most one row, written without `WHERE` by
  design: a primary key or unique key whose every column is `NOT NULL` and
  fixed to one value by a validated `CHECK` (`id boolean PRIMARY KEY CHECK
  (id)`, `id int PRIMARY KEY CHECK (id = 1)`), on a plain table with no
  inheritance children and no rewrite rules.
- `TRUNCATE`, `MERGE`, `INSERT … ON CONFLICT DO UPDATE`.
- Migrations.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
unbounded_write = "warning"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "unboundedWrite": "warning" } }
```

Write every row on purpose with `-- typedpg-lint: allow(unbounded_write)` in
the query's text. See [Configuring rules](README.md#configuring-rules).

## Related

- [`constant_condition`](constant_condition.md): a `WHERE` that is always
  true.
- [`self_assignment`](self_assignment.md): `SET a = a`.
