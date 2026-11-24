# `self_assignment`

An `UPDATE`, `ON CONFLICT DO UPDATE` or `MERGE … UPDATE` that assigns a
column its own value.

| Category | Level | Applies to | Fix |
|---|---|---|---|
| suspicious | warning | queries | removes the assignment |

## Example

```sql
CREATE TABLE users (id bigint PRIMARY KEY, email text NOT NULL UNIQUE, name text);
CREATE TABLE posts (id bigint PRIMARY KEY, user_id bigint REFERENCES users, title text NOT NULL, views integer NOT NULL DEFAULT 0);

UPDATE users SET name = name, email = $email WHERE id = $id;
UPDATE users u SET name = u.name WHERE id = $id;
-- users.name is the existing row's value: excluded.name was meant
INSERT INTO users (id, email, name) VALUES ($id, $email, $name)
    ON CONFLICT (id) DO UPDATE SET name = users.name, email = excluded.email;
MERGE INTO posts p USING users u ON p.user_id = u.id
    WHEN MATCHED THEN UPDATE SET title = p.title, views = 0;
```

```
warning[suspicious/self_assignment]: assigning name its own value changes nothing
   ╭─[queries.sql:2:18]
 2 │ UPDATE users SET name = name, email = $email WHERE id = $id;
   ·                  ───────────
   ╰────
  help: remove the assignment, or assign the value meant

warning[suspicious/self_assignment]: assigning name its own value rewrites the row with the same value
   ╭─[queries.sql:3:20]
 3 │ UPDATE users u SET name = u.name WHERE id = $id;
   ·                    ─────────────
   ╰────
  help: assign the value meant
```

`--fix` gives `UPDATE users SET email = $email WHERE id = $id`,
`… DO UPDATE SET email = excluded.email` and
`… WHEN MATCHED THEN UPDATE SET views = 0`. The second query has no fix: a
`SET` list can't be empty.

## Why

`SET name = name` beside another assignment changes nothing. Alone, it
rewrites the row as it is: a new row version and a row lock, the row
counted as updated, and nothing else. Either way it usually stands where
something else was meant: a forgotten parameter, an unqualified column of
another table, a copy-paste.

## Reported when

`SET col = col` or `SET col = t.col`, where `t` is the target table or its
alias, in an `UPDATE` (also inside a CTE), an `ON CONFLICT DO UPDATE` (the
existing row's value) or a `MERGE … WHEN MATCHED THEN UPDATE` (the matched
row's value):

- beside an assignment of another value: "changes nothing";
- when every assignment of the `SET` list is one: "rewrites the row with
  the same value". The help points an upsert to `DO NOTHING`, a `MERGE` to
  `WHEN MATCHED THEN DO NOTHING`.

## Not reported

- An `UPDATE OF col` trigger on the table or one of its partitions lists the
  column: naming it in the `SET` list fires the trigger.
- Every assignment is one, and the statement has `RETURNING`: the rewrite
  is what returns the row. `INSERT … ON CONFLICT (k) DO UPDATE SET k = t.k
  RETURNING id` is the idiom that returns the existing row's `id`, which
  `DO NOTHING` doesn't; `UPDATE … SET k = k … RETURNING` locks and returns
  the row.
- Every assignment is one, and an `UPDATE` trigger sees the rewritten rows:
  a `FOR EACH ROW` one, or a statement-level one with a `REFERENCING`
  transition table, without an `UPDATE OF` list or with one listing an
  assigned column (whatever its state or `WHEN`).
- Every assignment is one, and the target is a view, has rewrite rules or
  isn't resolved to a table: what the rewrite does there is unknown.
- A value from elsewhere: `SET title = a.title FROM archived a`,
  `SET name = excluded.name`, a `MERGE` source column, an outer query's
  column.
- An expression: `SET views = views + 1`.
- A multi-column assignment: `SET (name, email) = (name, $email)`.
- An assignment to an array element or subfield.

## Fix not offered

- Every assignment of the `SET` list is one: it can't be empty.
- The target is a view or other non-table, or has rewrite rules.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
self_assignment = "error"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "selfAssignment": "error" } }
```

Allow one query with `-- typedpg-lint: allow(self_assignment)` in its text.
See [Configuring rules](README.md#configuring-rules).

## Related

- [`unbounded_write`](unbounded_write.md): `UPDATE` without `WHERE`.
- [`check_violation`](check_violation.md): storing a constant a `CHECK`
  rejects.
