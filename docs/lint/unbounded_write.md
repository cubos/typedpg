# `unbounded_write`

Reports an `UPDATE` or `DELETE` with no `WHERE` clause.

| | |
|---|---|
| Default level | error |
| Applies to | queries |
| Fix | no |

## Why

An `UPDATE` or `DELETE` without `WHERE` writes every row of the table. It is
almost always a mistake: a condition that was forgotten, lost in a refactor,
or left in a string that was never concatenated. PostgreSQL runs it without
complaint, and the damage is done in one statement. Requiring the intent to be
written down (an allow comment) makes the rare deliberate case visible in
review.

## What it reports

Every `UPDATE` and `DELETE` statement in the query whose `WHERE` clause is
absent, including one inside a data-modifying CTE
(`WITH gone AS (DELETE FROM posts RETURNING id) SELECT …`). The message names
the table as written (`users`, `public.users`, `"My Users"`).

This is a syntactic check: it looks only at whether a `WHERE` is present, not
at what it says.

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

UPDATE users SET deleted_at = now();
DELETE FROM posts;
```

```
error[unbounded_write]: UPDATE without WHERE changes every row of users
   ╭─[queries.sql:2:1]
 2 │ UPDATE users SET deleted_at = now();
   · ────────────
   ╰────
  help: add a WHERE clause, or allow it with -- typedpg-lint: allow(unbounded_write)

error[unbounded_write]: DELETE without WHERE removes every row of posts
   ╭─[queries.sql:5:1]
 5 │ DELETE FROM posts;
   · ─────────────────
   ╰────
  help: add a WHERE clause, or allow it with -- typedpg-lint: allow(unbounded_write)
```

### Good

```sql
UPDATE users SET deleted_at = now() WHERE id = $id;

-- DELETE … USING still needs its WHERE, and has one.
DELETE FROM posts USING users
WHERE posts.user_id = users.id AND users.deleted_at IS NOT NULL;

-- Writing every row on purpose: say so.
-- typedpg-lint: allow(unbounded_write)
DELETE FROM posts;
```

`UPDATE … WHERE CURRENT OF cursor` has a `WHERE` and is not reported either.

## When it stays silent

- Any `WHERE` silences it, even one that is always true. `WHERE true` or
  `WHERE 1 = 1` is reported by
  [`constant_condition`](constant_condition.md) instead, whose help points
  back to this rule's allow comment.
- `TRUNCATE`, `MERGE` and `INSERT … ON CONFLICT DO UPDATE` are not read.
- Migrations are not read: this rule runs over queries only.

## Configuration

Rust (`Cargo.toml`):

```toml
[package.metadata.typedpg.lint]
unbounded_write = "warning"   # "error" (default), "warning", "info" or "off"
fail_on = "error"             # the severity the run fails on (default)
ignore = ["tests/**"]         # query files whose findings are dropped
```

TypeScript (`typedpg.config.json`):

```json
{
  "lint": {
    "unboundedWrite": "warning",
    "failOn": "error",
    "ignore": ["tests/**"]
  }
}
```

At the default `fail_on` / `failOn` of `"error"`, a finding of this rule fails
the run unless you lower its level.

To allow one statement, put the comment in the query text (the `sql!` /
`sql()` string, or the statement in a `.sql` file, after its `/* @name */`
block):

```sql
-- typedpg-lint: allow(unbounded_write)
DELETE FROM posts;
```

## Related rules

- [`constant_condition`](constant_condition.md): a `WHERE` that is always
  true, which writes every row just the same.
- [`self_assignment`](self_assignment.md): an `UPDATE … SET a = a`.
