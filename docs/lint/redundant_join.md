# `redundant_join`

A join that cannot change the result rows and whose table nothing in the
statement reads.

| | |
|---|---|
| Default level | info |
| Applies to | queries |
| Fix | yes (`--fix`): deletes the join |

## Why

A join does two things: it brings in columns, and it can drop or repeat rows.
When neither happens — every row of the left side matches at most one row
(for a `LEFT JOIN`) or exactly one row (for an inner join), and no column of
the joined table is used — the join is dead text. PostgreSQL still has to
plan it and, unless its planner proves the join removable on its own, look
up the joined row for every result row.

It is also often a sign of an unfinished query: the join was added to read
something (`e.name AS editor`) that never made it into the select list.

Proving that a join changes no rows takes schema knowledge a text-based
linter does not have: unique keys, foreign keys, their nullability and
validation state, and row-level security. typedpg reads all of them from the
migrations.

## What it reports

Only joins in the `FROM` of a plain `SELECT` are considered: one statement
with no `WITH`, set operation, `VALUES`, `INTO`, `FOR UPDATE` / `FOR SHARE`,
volatile function (views count as volatile there), `USING` or `NATURAL` join,
and no `$..spread` parameter. The joined relation must be a plain or
partitioned table, on the right-hand side of the join. Two shapes are
proven:

1. **`LEFT JOIN t ON …` on a unique key.** The `ON` condition is a
   conjunction (`AND`) of equalities, each between a column of `t` and a
   column of the same type of another table, written as qualified names
   (`t.k = a.c`), under deterministic collations. Together they must cover
   every column of a unique key of `t` that is non-partial, not deferrable
   and on plain columns, compared with that index's own equality operator.
   Each left row then matches at most one row of `t`, and a `LEFT JOIN`
   keeps the left rows that match none: the row count is unchanged.
2. **`[INNER] JOIN parent ON parent.pk = child.fk` through a foreign key.**
   The condition must equate exactly the columns of a foreign key from the
   left table to `parent`, and that foreign key must be `NOT NULL` on every
   column, enforced, validated (not `NOT VALID`) and `NOT DEFERRABLE`, into
   a parent without row-level security. Each child row then has exactly one
   parent row. This proof is abandoned for the whole database once a
   migration may let rows skip the foreign-key triggers (`DISABLE TRIGGER`,
   `session_replication_role`, and similar).

On top of that, nothing anywhere in the statement may possibly read the
joined table: a column qualified by its name or alias, a whole-row reference
(`SELECT u`), `u.*`, an unqualified `*`, or an unqualified column name that
one of its columns has.

Finally, typedpg analyzes the query with the join cut out and keeps the
finding only if the result columns (names, types, nullability, value
refinements) and parameters come out the same. A join through a foreign key
can refine a column — the child's column takes only the values a `CHECK` on
the parent's key allows — and such a join is not reported while that column
is selected.

The message names the joined table by its alias (or its name when it has
none). The finding spans the join, from its first keyword to the end of its
`ON` condition.

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
    editor_id bigint REFERENCES users,
    title text NOT NULL,
    created_at timestamptz NOT NULL
);
```

### Bad

```sql
SELECT p.id, p.title
FROM posts p
LEFT JOIN users e ON e.id = p.editor_id;
```

```
info[redundant_join]: the LEFT JOIN of e can't change the rows (each row matches at most one row of it, on a unique key) and none of its columns are used
   ╭─[queries.sql:3:1]
 3 │ LEFT JOIN users e ON e.id = p.editor_id;
   · ───────────────────────────────────────
   ╰────
  help: remove the join, or read the columns it was meant to provide
```

`posts.user_id` is a `NOT NULL`, validated, immediate foreign key to
`users`, so every post has exactly one author:

```sql
SELECT p.id, p.title
FROM posts p
JOIN users u ON u.id = p.user_id;
```

```
info[redundant_join]: the JOIN of u can't change the rows (a foreign key finds exactly one row of it for each row) and none of its columns are used
   ╭─[queries.sql:3:1]
 3 │ JOIN users u ON u.id = p.user_id;
   · ────────────────────────────────
   ╰────
  help: remove the join, or read the columns it was meant to provide
```

`--fix` deletes the join in both, leaving `SELECT p.id, p.title FROM posts p;`.

### Good

```sql
-- A column of the joined table is used.
SELECT p.id, p.title, e.name
FROM posts p
LEFT JOIN users e ON e.id = p.editor_id;

-- editor_id is nullable: the inner join drops posts without an editor.
SELECT p.id, p.title
FROM posts p
JOIN users e ON e.id = p.editor_id;

-- The referencing side: a user has many posts, so the join repeats users.
SELECT u.id
FROM users u
LEFT JOIN posts p ON p.user_id = u.id;
```

Also not reported: an `ON` that covers only part of a composite key, adds a
non-key condition (`AND u.name = 'x'`) or uses `OR`; a key compared with a
column of another type; a unique index that is partial or on an expression;
a `DEFERRABLE` or `NOT VALID` foreign key; a parent with row-level security
enabled (for the inner-join shape; a `LEFT JOIN` on a unique key is still
reported, since security can only hide rows); an inner join on a unique key
that no foreign key backs; `LATERAL`, subquery and function joins; `USING`;
`SELECT *`.

## Fix

`--fix` deletes the join from the end of its left-hand side through the end
of its `ON` condition, so `FROM posts p LEFT JOIN users e ON …` becomes
`FROM posts p`. The fix is withheld when a comment lies inside or against
the text it would delete.

The fix is checked once more before it is offered: the edited query must
analyze to the same result columns (names, types, nullability and value
refinements) and parameters. A fix that fails is a typedpg bug; it is
withheld and reported as an `internal_error` warning.

## When it stays silent

Whenever the proof cannot see through something, the join is kept: any
statement that is not a plain `SELECT` (an `UPDATE … FROM`, a `DELETE …
USING`, a query with `WITH`), a view or volatile function anywhere in the
statement, row locking, an unqualified column name that the joined table also
has (it might be the one read), or a join whose removal changes a column's
refinement.

## Configuration

```toml
[package.metadata.typedpg.lint]
redundant_join = "warning"   # "error", "warning", "info" or "off"
```

```json
{
  "lint": { "redundantJoin": "warning" }
}
```

`fail_on` / `failOn` sets the severity that fails the run (default `error`),
and `ignore` lists glob patterns of files whose query findings are dropped.
For one statement:

```sql
-- typedpg-lint: allow(redundant_join)
SELECT p.id, p.title FROM posts p JOIN users u ON u.id = p.user_id;
```

## Related rules

- [`unused_cte`](unused_cte.md): a `WITH` query nothing reads.
- [`left_join_nullified`](left_join_nullified.md): a `LEFT JOIN` the `WHERE`
  turns into an inner join.
- [`cartesian_join`](cartesian_join.md): a join with no condition at all.
