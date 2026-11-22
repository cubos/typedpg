# `self_assignment`

Reports an assignment of a column to its own value in an `UPDATE`, an
`INSERT … ON CONFLICT DO UPDATE` or a `MERGE … UPDATE`.

| | |
|---|---|
| Default level | warning |
| Applies to | queries |
| Fix | yes (`--fix`): removes the assignment, when that is safe |

## Why

`SET name = name` changes nothing. It usually stands where another value was
meant: a parameter that was forgotten (`SET name = $name`), a column from the
`FROM` table that was left unqualified, or a copy-paste. The statement runs,
reports its rows as updated, and the intended change never happens.

## What it reports

A single-column assignment `SET col = col` or `SET col = t.col` where the
analyzer resolved the value to the target table's own column — `t` being the
target's name or alias — in:

- an `UPDATE` (including one inside a data-modifying CTE);
- an `INSERT … ON CONFLICT DO UPDATE SET`, where `t.col` is the existing,
  conflicting row's value. `SET col = excluded.col`, the value proposed for
  insertion, is a change and is not reported. (An unqualified `col` there is
  ambiguous between the target and `excluded`, and PostgreSQL rejects it.)
- a `MERGE … WHEN MATCHED THEN UPDATE SET`, where `t.col` is the matched
  target row's own value. `SET col = s.col` from the source `s` is not
  reported.

Because the analyzer says which table each reference names, `SET x = s.x`
from a `FROM` item `s` is not reported, nor an unqualified `x` that resolves
to an outer query.

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

UPDATE users SET name = name, email = $email WHERE id = $id;
UPDATE users u SET name = u.name WHERE id = $id;
-- users.name is the existing row's name: excluded.name was meant.
INSERT INTO users (id, email, name) VALUES ($id, $email, $name) ON CONFLICT (id) DO UPDATE SET name = users.name, email = excluded.email;
-- p.title is the matched post's own title.
MERGE INTO posts p USING users u ON p.user_id = u.id WHEN MATCHED THEN UPDATE SET title = p.title, views = 0;
```

```
warning[self_assignment]: assigning name its own value changes nothing
   ╭─[queries.sql:2:18]
 2 │ UPDATE users SET name = name, email = $email WHERE id = $id;
   ·                  ───────────
   ╰────
  help: remove the assignment, or assign the value meant

warning[self_assignment]: assigning name its own value changes nothing
   ╭─[queries.sql:5:20]
 5 │ UPDATE users u SET name = u.name WHERE id = $id;
   ·                    ─────────────
   ╰────
  help: remove the assignment, or assign the value meant

warning[self_assignment]: assigning name its own value changes nothing
   ╭─[queries.sql:8:96]
 8 │ INSERT INTO users (id, email, name) VALUES ($id, $email, $name) ON CONFLICT (id) DO UPDATE SET name = users.name, email = excluded.email;
   ·                                                                                                ─────────────────
   ╰────
  help: remove the assignment, or assign the value meant

warning[self_assignment]: assigning title its own value changes nothing
    ╭─[queries.sql:11:83]
 11 │ MERGE INTO posts p USING users u ON p.user_id = u.id WHEN MATCHED THEN UPDATE SET title = p.title, views = 0;
    ·                                                                                   ───────────────
    ╰────
   help: remove the assignment, or assign the value meant
```

`--fix` rewrites the first to
`UPDATE users SET email = $email WHERE id = $id`, the third to
`… ON CONFLICT (id) DO UPDATE SET email = excluded.email` and the fourth to
`… WHEN MATCHED THEN UPDATE SET views = 0`. The second keeps its finding
without a fix: it is the only assignment, and a `SET` list can't be empty.

### Good

```sql
-- The value comes from another table.
UPDATE posts SET title = a.title FROM archived_posts a WHERE a.id = posts.id;

-- An expression of the column is a change.
UPDATE posts SET views = views + 1 WHERE id = $id;

-- excluded.name is the value proposed for insertion, not the existing one.
INSERT INTO users (id, email, name) VALUES ($id, $email, $name) ON CONFLICT (id) DO UPDATE SET name = excluded.name;
```

## Fix

The fix removes the assignment and the comma that separates it from its
neighbor, in all three statements. It is withheld unless all of these hold:

- another assignment remains;
- the target is a plain or partitioned table without rewrite rules;
- no `UPDATE OF` trigger of the table (or of a partition) lists the column.
  Such a trigger fires when the `SET` list names the column, even to assign
  the value it already has, so removing the assignment would stop the
  trigger from firing (for an `ON CONFLICT DO UPDATE` and a `MERGE` update as
  for an `UPDATE`). A trigger listing other columns doesn't matter.

Every fix is also re-analyzed before it is offered and must keep the result
columns' names, types, nullability and value refinements, and the parameters,
unchanged; a fix that doesn't is withheld and reported as an
`internal_error` (a typedpg bug).

## When it stays silent

- A multiple-column assignment `SET (a, b) = (…)`, which is not read:
  `SET (name, email) = (name, $email)` is not reported.
- An assignment to a subfield or array element (`SET a[1] = a`).
- `SET col = excluded.col` in an `ON CONFLICT DO UPDATE`, and a `MERGE`
  assignment from its source.
- A value the analyzer didn't resolve to the target's own column.
- Migrations are not read: this rule runs over queries only.

## Configuration

Rust (`Cargo.toml`):

```toml
[package.metadata.typedpg.lint]
self_assignment = "error"   # "error", "warning" (default), "info" or "off"
fail_on = "warning"         # fail the run on warnings too (default "error")
ignore = ["tests/**"]       # query files whose findings are dropped
```

TypeScript (`typedpg.config.json`):

```json
{
  "lint": {
    "selfAssignment": "error",
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
-- typedpg-lint: allow(self_assignment)
UPDATE users SET name = name WHERE id = $id;
```

## Related rules

- [`unbounded_write`](unbounded_write.md): an `UPDATE` without `WHERE`.
- [`check_violation`](check_violation.md): an `UPDATE` storing a constant a
  `CHECK` rejects.
