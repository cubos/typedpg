# `outer_reference_in_subquery`

An unqualified column in a subquery that resolves to an outer query, because
no table of the subquery has it.

| Category | Level | Applies to | Fix |
|---|---|---|---|
| suspicious | warning | queries | — |

## Example

```sql
CREATE TABLE users (id bigint PRIMARY KEY, email text NOT NULL, name text);
CREATE TABLE posts (id bigint PRIMARY KEY, user_id bigint NOT NULL, title text NOT NULL);
CREATE INDEX ON posts (user_id);

SELECT id FROM users WHERE email IN (SELECT email FROM posts);
SELECT id FROM users u WHERE EXISTS (SELECT 1 FROM posts WHERE title = name);
```

```
warning[suspicious/outer_reference_in_subquery]: column email is not a column of this subquery's relations: it is users.email of the outer query
   ╭─[queries.sql:2:45]
 2 │ SELECT id FROM users WHERE email IN (SELECT email FROM posts);
   ·                                             ─────
   ╰────
  help: write users.email if the outer column is meant, or name a column of the subquery

warning[suspicious/outer_reference_in_subquery]: column name is not a column of this subquery's relations: it is u.name of the outer query
   ╭─[queries.sql:4:72]
 4 │ SELECT id FROM users u WHERE EXISTS (SELECT 1 FROM posts WHERE title = name);
   ·                                                                        ────
   ╰────
  help: write u.name if the outer column is meant, or name a column of the subquery
```

## Why

PostgreSQL silently falls back to outer queries for a name the subquery's
tables don't have. `posts` has no `email`, so the first query is
`users.email IN (users.email, …)`: every user passes as soon as `posts` has
a row. A typo or a wrong assumption about a table becomes a valid query
with no filter.

## Reported when

A bare column name (not `t.col`) in a subquery with a `FROM` clause (`IN`,
`EXISTS`, `ANY`, a scalar subquery, a derived table, a `LATERAL` item)
belongs to an outer query's table. Read in the subquery's select list,
`WHERE`, `GROUP BY`, `HAVING`, `ORDER BY`, `DISTINCT ON`, windows and
`JOIN … ON`.

When the outer table has no writable name (a `USING` merged column, an
unaliased subquery), the message ends with `it is the outer query's`.

## Not reported

- Qualified references: `WHERE p.title = u.name`.
- A subquery without `FROM`: `(SELECT upper(name))`.
- Arguments of a function in the subquery's `FROM`:
  `(SELECT count(*) FROM unnest(ARRAY[name, email]) n)`.
- Output-column names in `ORDER BY`, system columns.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
outer_reference_in_subquery = "error"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "outerReferenceInSubquery": "error" } }
```

Allow one query with `-- typedpg-lint: allow(outer_reference_in_subquery)`
in its text; qualifying the column (`u.name`) is usually better.
See [Configuring rules](README.md#configuring-rules).

## Related

- [`not_in_nullable`](not_in_nullable.md): the other classic `IN (SELECT …)`
  trap.
- [`cartesian_join`](cartesian_join.md): tables joined with no condition.
