# `self_assignment`

Reports an `UPDATE` assignment of a column to its own value.

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

A single-column assignment `SET col = col` or `SET col = t.col` in an
`UPDATE` (including one inside a data-modifying CTE), where the analyzer
resolved the value to the target table's own column — `t` being the target's
name or alias. Because the analyzer says which table each reference names,
`SET x = s.x` from a `FROM` item `s` is not reported, nor an unqualified `x`
that resolves to an outer query.

## Examples

### Bad

```sql
CREATE TABLE users (
    id bigint PRIMARY KEY,
    email text NOT NULL UNIQUE,
    name text,
    deleted_at timestamptz
);

UPDATE users SET name = name, email = $email WHERE id = $id;
UPDATE users u SET name = u.name WHERE id = $id;
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
```

`--fix` rewrites the first to
`UPDATE users SET email = $email WHERE id = $id`. The second keeps its
finding without a fix: it is the only assignment, and a `SET` list can't be
empty.

### Good

```sql
-- The value comes from another table.
UPDATE posts SET title = a.title FROM archived_posts a WHERE a.id = posts.id;

-- An expression of the column is a change.
UPDATE posts SET views = views + 1 WHERE id = $id;
```

## Fix

The fix removes the assignment and the comma that separates it from its
neighbor. It is withheld unless all of these hold:

- another assignment remains;
- the target is a plain or partitioned table without rewrite rules;
- no `UPDATE OF` trigger of the table (or of a partition) lists the column.
  Such a trigger fires when the `SET` list names the column, even to assign
  the value it already has, so removing the assignment would stop the
  trigger from firing. A trigger listing other columns doesn't matter.

Every fix is also re-analyzed before it is offered and must keep the result
columns' names, types, nullability and value refinements, and the parameters,
unchanged; a fix that doesn't is withheld and reported as an
`internal_error` (a typedpg bug).

## When it stays silent

- A multiple-column assignment `SET (a, b) = (…)`, which is not read:
  `SET (name, email) = (name, $email)` is not reported.
- An assignment to a subfield or array element (`SET a[1] = a`).
- `INSERT … ON CONFLICT DO UPDATE SET` and `MERGE … UPDATE SET` are not read.
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
