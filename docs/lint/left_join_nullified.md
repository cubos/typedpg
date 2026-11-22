# `left_join_nullified`

A `WHERE` condition that rejects the NULL-extended rows of an outer join, so
the `LEFT JOIN` behaves as an inner join.

| | |
|---|---|
| Default level | warning |
| Applies to | queries |
| Fix | no |

## Why

`users u LEFT JOIN posts p ON …` keeps every user. A user without posts
comes out once, with every `p` column set to NULL: a *NULL-extended* row.
Then `WHERE p.status = 'published'` runs on the joined rows. For a
NULL-extended row it compares NULL with `'published'`, which is not true, so
the row is dropped. Every user without a matching post disappears, which is
exactly what the `LEFT JOIN` was written to prevent. The query is an inner
join in disguise.

PostgreSQL knows this too: its planner (`reduce_outer_joins`) quietly turns
such a join into an inner join. Nothing tells the author, who usually meant
one of two different things:

- keep every user, and only join their published posts: the condition
  belongs in `ON`;
- only users with a published post: write `JOIN`, which also makes the `p`
  columns non-nullable in the generated types.

## What it reports

A `WHERE` condition, or one `AND`-ed part of one, that typedpg proves is
never true when an outer join's nullable side is NULL-extended. It uses the
same strictness reasoning as PostgreSQL's planner:

- a strict operator or function applied to a column of the nullable side
  (`p.status = 'x'`, `p.score > 0`, `lower(p.status) = 'x'`);
- `p.col IS NOT NULL`;
- an `OR` only when every arm rejects (`p.score = 1 OR p.score = 2`).

`LEFT JOIN` and `RIGHT JOIN` are reported as behaving like an inner join. A
condition on one side of a `FULL JOIN` makes it behave as a `LEFT` or
`RIGHT` join, and the message says which.

The finding is placed on the condition. The message names the nullable side
by its alias (or name), and the help line depends on the condition: for
`IS NOT NULL` it suggests an inner join or dropping the condition; for
anything else, moving it into `ON` or writing an inner join.

## Examples

```sql
CREATE TABLE users (
    id bigint PRIMARY KEY,
    email text NOT NULL UNIQUE,
    name text,
    created_at timestamptz NOT NULL
);

CREATE TABLE posts (
    id bigint PRIMARY KEY,
    user_id bigint NOT NULL REFERENCES users,
    title text NOT NULL,
    status text NOT NULL CHECK (status IN ('draft', 'published', 'archived')),
    score int,
    created_at timestamptz NOT NULL
);
```

### Bad

```sql
SELECT u.id, p.title
FROM users u
LEFT JOIN posts p ON p.user_id = u.id
WHERE p.status = 'published';
```

```
warning[left_join_nullified]: this condition is never true for the NULL-extended rows of p, on the nullable side of a LEFT JOIN: the join behaves as an INNER JOIN
   ╭─[queries.sql:4:7]
 4 │ WHERE p.status = 'published';
   ·       ──────────────────────
   ╰────
  help: to keep the rows without a match in p, move the condition into the join's ON clause; to drop them, write an INNER JOIN
```

```sql
SELECT u.id
FROM users u
LEFT JOIN posts p ON p.user_id = u.id
WHERE p.id IS NOT NULL;
```

```
warning[left_join_nullified]: this condition is never true for the NULL-extended rows of p, on the nullable side of a LEFT JOIN: the join behaves as an INNER JOIN
   ╭─[queries.sql:4:7]
 4 │ WHERE p.id IS NOT NULL;
   ·       ────────────────
   ╰────
  help: to keep only the rows with a match in p, write an INNER JOIN; to keep the unmatched rows, drop the condition
```

### Good

```sql
-- The condition filters which posts join; users without one are kept.
SELECT u.id, p.title
FROM users u
LEFT JOIN posts p ON p.user_id = u.id AND p.status = 'published';

-- The anti-join: users with no posts at all.
SELECT u.id
FROM users u
LEFT JOIN posts p ON p.user_id = u.id
WHERE p.id IS NULL;

-- COALESCE is not strict: true for the NULL-extended rows.
SELECT u.id
FROM users u
LEFT JOIN posts p ON p.user_id = u.id
WHERE COALESCE(p.score, 0) = 0;

-- One arm of the OR keeps the NULL-extended rows.
SELECT u.id
FROM users u
LEFT JOIN posts p ON p.user_id = u.id
WHERE p.score = 1 OR p.id IS NULL;

-- Every post has a user (NOT NULL foreign key), so this join never
-- NULL-extends: nothing is lost.
SELECT p.id
FROM posts p
LEFT JOIN users u ON u.id = p.user_id
WHERE u.name = 'a';
```

`IS DISTINCT FROM`, conditions on the preserved side (`u.name = 'a'` in the
first example's shape) and conditions under an inner join are not reported
either. A parameter that may be NULL keeps the rows too:
`($1::int IS NULL OR p.score = $1)`.

## Fix

None. Moving the condition into `ON` keeps the unmatched rows (with NULLs);
writing `INNER JOIN` drops them and makes the columns non-nullable. Either
changes the query, and which one was meant is for its author to decide.

## When it stays silent

- The condition is not strict in the nullable side's columns (`COALESCE`,
  `IS NULL`, `IS DISTINCT FROM`, an `OR` with an arm that can be true for a
  NULL-extended row).
- The join never NULL-extends that side: a `NOT NULL` foreign key's parent,
  or `ON true` to a row that is always there.
- The condition is in `ON` or `HAVING`, not in `WHERE`.

## Configuration

```toml
[package.metadata.typedpg.lint]
left_join_nullified = "error"   # "error", "warning", "info" or "off"
```

```json
{
  "lint": { "leftJoinNullified": "error" }
}
```

`fail_on` / `failOn` sets the severity that fails the run (default `error`),
and `ignore` lists glob patterns of files whose query findings are dropped.
For one statement:

```sql
-- typedpg-lint: allow(left_join_nullified)
SELECT u.id FROM users u LEFT JOIN posts p ON p.user_id = u.id WHERE p.score > 0;
```

## Related rules

- [`redundant_join`](redundant_join.md): a join that changes nothing at all.
- [`null_comparison`](null_comparison.md): a comparison with `NULL` that is
  never true.
