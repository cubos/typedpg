# `check_violation`

An `INSERT` or `UPDATE` that stores a constant a `CHECK` constraint or the
column's domain rejects, so it fails for every row it writes.

| Category | Level | Applies to | Fix |
|---|---|---|---|
| correctness | error | queries | — |

## Example

```sql
CREATE DOMAIN rating AS integer CHECK (VALUE BETWEEN 1 AND 5);
CREATE TABLE posts (
    id bigint PRIMARY KEY,
    title text NOT NULL,
    status text NOT NULL CHECK (status IN ('draft', 'published', 'archived')),
    score rating
);

UPDATE posts SET status = 'publshed' WHERE id = $id;
INSERT INTO posts (id, title, status) VALUES ($id, $title, 'draft') ON CONFLICT (id) DO UPDATE SET status = 'deleted';
UPDATE posts SET score = 9 WHERE id = $id;
```

```
error[correctness/check_violation]: storing this value in status violates CHECK constraint "posts_status_check": the statement fails as soon as it updates a row
   ╭─[queries.sql:2:27]
 2 │ UPDATE posts SET status = 'publshed' WHERE id = $id;
   ·                           ──────────
   ╰────
  help: the constraint allows one of 'archived', 'draft', 'published'

error[correctness/check_violation]: storing this value in score fails: value for domain rating violates check constraint "rating_check"
   ╭─[queries.sql:6:26]
 6 │ UPDATE posts SET score = 9 WHERE id = $id;
   ·                          ─
   ╰────
  help: store a value the constraint allows
```

The `ON CONFLICT … DO UPDATE` is reported on `'deleted'`: the insert is
fine, the update fails whenever it runs.

## Why

The query type-checks, so a typo in a constant ships and fails only the
first time the statement writes a row, which may be rare.

## Reported when

A constant is stored by `INSERT … VALUES`, `UPDATE … SET` or
`ON CONFLICT … DO UPDATE SET` (also inside a `WITH` query), and a `CHECK` of
the table or of the column's domain rejects it whatever the other columns
hold. `NOT VALID` constraints count: they check every new row.

A plain `INSERT … VALUES` that violates a `CHECK` is usually a compile error
already; the rule covers what can't be one: writes that may touch no row
(`UPDATE`, `DO UPDATE`), and columns with a type modifier, e.g. `'xy'` into a
`varchar(4) CHECK (code IN ('ab', 'cd'))` column.

## Not reported

- A value that isn't a constant: a parameter, a column, an expression over
  the row, a multi-column `SET (a, b) = (…)`.
- `SET col = NULL`: a `CHECK` passes on `NULL`.
- A constraint whose outcome depends on another column,
  e.g. `CHECK (kind <> 'gift' OR note IS NOT NULL)`.
- A table with a `BEFORE` row trigger or a rewrite rule that may change the
  row, and writes through a view.
- Assignments through an indirection: `SET arr[1] = …`, `SET comp.field = …`.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
check_violation = "error"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "checkViolation": "error" } }
```

Allow one query with `-- typedpg-lint: allow(check_violation)` in its text.
See [Configuring rules](README.md#configuring-rules).

## Related

- [`constant_condition`](constant_condition.md): a condition the same
  constraints decide, such as `WHERE status = 'publshed'`.
