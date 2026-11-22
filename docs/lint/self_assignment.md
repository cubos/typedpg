# `self_assignment`

An `UPDATE`, `ON CONFLICT DO UPDATE` or `MERGE … UPDATE` that assigns a
column its own value.

| Level | Applies to | Fix |
|---|---|---|
| warning | queries | removes the assignment |

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
warning[self_assignment]: assigning name its own value changes nothing
   ╭─[queries.sql:2:18]
 2 │ UPDATE users SET name = name, email = $email WHERE id = $id;
   ·                  ───────────
   ╰────
  help: remove the assignment, or assign the value meant
```

`--fix` gives `UPDATE users SET email = $email WHERE id = $id`,
`… DO UPDATE SET email = excluded.email` and
`… WHEN MATCHED THEN UPDATE SET views = 0`. The second query is reported
without a fix (see below).

## Why

`SET name = name` changes nothing, yet the statement runs and reports rows
updated. It usually stands where something else was meant: a forgotten
parameter, an unqualified column of another table, a copy-paste.

## Reported when

`SET col = col` or `SET col = t.col`, where `t` is the target table or its
alias, in an `UPDATE` (also inside a CTE), an `ON CONFLICT DO UPDATE` (the
existing row's value) or a `MERGE … WHEN MATCHED THEN UPDATE` (the matched
row's value).

## Not reported

- A value from elsewhere: `SET title = a.title FROM archived a`,
  `SET name = excluded.name`, a `MERGE` source column, an outer query's
  column.
- An expression: `SET views = views + 1`.
- A multi-column assignment: `SET (name, email) = (name, $email)`.
- An assignment to an array element or subfield.

## Fix not offered

- It is the only assignment: a `SET` list can't be empty.
- The target is a view or other non-table, or has rewrite rules.
- An `UPDATE OF col` trigger on the table or one of its partitions lists the
  column: it fires whenever the `SET` list names the column, so removing the
  assignment would stop it firing.

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
