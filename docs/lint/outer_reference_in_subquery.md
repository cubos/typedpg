# `outer_reference_in_subquery`

Reports an unqualified column reference in a subquery that resolves to an
enclosing query's table, because no table of the subquery has that column.

| | |
|---|---|
| Default level | warning |
| Applies to | queries |
| Fix | no |

## Why

PostgreSQL resolves an unqualified column name in the innermost query level
that has it, and quietly falls back to the enclosing levels. This is what
makes correlated subqueries work, and it also turns a typo or a wrong
assumption about a table into a valid query that means something else:

```sql
SELECT id FROM users WHERE email IN (SELECT email FROM posts);
```

`posts` has no `email` column, so the inner `email` is `users.email`. The
condition becomes `users.email IN (users.email, users.email, …)`: every user
with a non-NULL email passes as long as `posts` has at least one row. No
error, and the filter is gone.

## What it reports

An unqualified column reference (a bare `name`, not `t.name`) in a subquery
that has a `FROM` clause — a sublink (`IN`, `EXISTS`, `ANY`, a scalar
subquery), a derived table, or a `LATERAL` item — when the analyzer resolved
it to a `FROM` entry of an enclosing level. The analyzer records what each
column reference bound to while it types the query, so the rule knows which
table the name came from and names it in the message.

Only references in the subquery's own clauses are read: its select list,
`WHERE`, `GROUP BY`, `HAVING`, `ORDER BY`, `DISTINCT ON`, window definitions
and `JOIN … ON` conditions.

## Examples

### Bad

```sql
CREATE TABLE users (
    id bigint PRIMARY KEY,
    email text NOT NULL UNIQUE,
    name text,
    deleted_at timestamptz
);
CREATE TABLE posts (
    id bigint PRIMARY KEY,
    user_id bigint REFERENCES users,
    title text NOT NULL,
    status text NOT NULL CHECK (status IN ('draft', 'published', 'archived')),
    views integer NOT NULL DEFAULT 0 CHECK (views >= 0)
);

SELECT id FROM users WHERE email IN (SELECT email FROM posts);
SELECT id FROM users u WHERE EXISTS (SELECT 1 FROM posts WHERE title = name);
```

```
warning[outer_reference_in_subquery]: column email is not a column of this subquery's relations: it is users.email of the outer query
   ╭─[queries.sql:2:45]
 2 │ SELECT id FROM users WHERE email IN (SELECT email FROM posts);
   ·                                             ─────
   ╰────
  help: write users.email if the outer column is meant, or name a column of the subquery

warning[outer_reference_in_subquery]: column name is not a column of this subquery's relations: it is u.name of the outer query
   ╭─[queries.sql:5:72]
 5 │ SELECT id FROM users u WHERE EXISTS (SELECT 1 FROM posts WHERE title = name);
   ·                                                                        ────
   ╰────
  help: write u.name if the outer column is meant, or name a column of the subquery
```

### Good

```sql
-- Qualified: the outer column on purpose.
SELECT id FROM users u WHERE EXISTS (SELECT 1 FROM posts p WHERE p.title = u.name);

-- user_id is a column of posts.
SELECT id FROM users WHERE id IN (SELECT user_id FROM posts);

-- A subquery without FROM has no table of its own: every reference in it is
-- meant to be outer.
SELECT id, (SELECT upper(name)) AS shout FROM users;

-- A FROM function's arguments only see the enclosing levels.
SELECT id, (SELECT count(*) FROM unnest(ARRAY[name, email]) n) AS c FROM users;
```

## When it stays silent

- Qualified references (`u.name`), whichever level they name.
- Subqueries without a `FROM` clause.
- Arguments of a function in the subquery's `FROM` (`unnest(tags)`), which
  can only see the enclosing levels.
- The top-level query, which has no outer level.
- References the analyzer didn't record a binding for (an output-column name
  in `ORDER BY`, a system column).
- When the entry has no name a query can write (a join's merged `USING`
  column, an unaliased subquery), the finding is still reported, with a
  generic message: `… it is the outer query's`.
- Migrations are not read: this rule runs over queries only.

## Configuration

Rust (`Cargo.toml`):

```toml
[package.metadata.typedpg.lint]
outer_reference_in_subquery = "error"   # "error", "warning" (default), "info" or "off"
fail_on = "warning"                     # fail the run on warnings too (default "error")
ignore = ["tests/**"]                   # query files whose findings are dropped
```

TypeScript (`typedpg.config.json`):

```json
{
  "lint": {
    "outerReferenceInSubquery": "error",
    "failOn": "warning",
    "ignore": ["tests/**"]
  }
}
```

At the default `fail_on` / `failOn` of `"error"`, a warning is printed but
does not fail the run.

To allow one query, put the comment in the query text (the `sql!` / `sql()`
string, or the statement in a `.sql` file):

```sql
-- typedpg-lint: allow(outer_reference_in_subquery)
SELECT id FROM users u WHERE EXISTS (SELECT 1 FROM posts WHERE title = name);
```

Qualifying the reference (`u.name`) is usually the better way to say the
outer column is meant.

## Related rules

- [`not_in_nullable`](not_in_nullable.md): the other classic `IN (SELECT …)`
  trap.
- [`cartesian_join`](cartesian_join.md): tables joined with no condition.
