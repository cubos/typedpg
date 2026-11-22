# `check_violation`

An `INSERT` or `UPDATE` that stores a constant a `CHECK` constraint (or the
column's domain) rejects, so the statement fails for every row it writes.

| | |
|---|---|
| Default level | warning |
| Applies to | queries |
| Fix | no |

## Why

`UPDATE posts SET status = 'publshed' WHERE id = $id` is valid SQL and
type-checks: `status` is `text`, and `'publshed'` is a string. But the
column has `CHECK (status IN ('draft', 'published', 'archived'))`, so the
statement fails with `new row for relation "posts" violates check constraint`
the first time it updates a row. If the `WHERE` rarely matches, or the code
path is rarely taken, the typo ships.

typedpg knows the table's `CHECK` constraints and the column's domain from
the migrations, and can evaluate them against the constant at build time.

## What it reports

A constant stored by:

- `INSERT … VALUES` (each row, matched to the column list, or to the table's
  columns in order when there is none);
- `UPDATE … SET col = value`;
- `INSERT … ON CONFLICT … DO UPDATE SET col = value`;

including inside a data-modifying `WITH` query, when a `CHECK` constraint of
the table, or a `CHECK` of the column's domain, rejects that value whatever
the row's other columns hold. `NOT VALID` constraints count: they do not
check existing rows, but they do check every row written after them.

A constraint that also reads another column (`CHECK (kind <> 'gift' OR note
IS NOT NULL)`) only counts when the constant alone decides it.

For a plain `INSERT … VALUES`, typedpg's analysis already fails the query
when it proves the row violates a constraint: that is a build error, not a
lint finding. This rule covers the writes the analyzer cannot reject because
they may write no row (an `UPDATE`, an `ON CONFLICT DO UPDATE`), and columns
with a type modifier such as `varchar(20)`.

The finding is placed on the value. The help line lists what the constraint
allows when typedpg can state it.

## Examples

```sql
CREATE TABLE posts (
    id bigint PRIMARY KEY,
    user_id bigint NOT NULL,
    title text NOT NULL,
    status text NOT NULL CHECK (status IN ('draft', 'published', 'archived')),
    created_at timestamptz NOT NULL
);
```

### Bad

```sql
UPDATE posts SET status = 'publshed' WHERE id = $id;
```

```
warning[check_violation]: storing this value in status violates CHECK constraint "posts_status_check": the statement fails as soon as it updates a row
   ╭─[queries.sql:1:27]
 1 │ UPDATE posts SET status = 'publshed' WHERE id = $id;
   ·                           ──────────
   ╰────
  help: the constraint allows one of 'archived', 'draft', 'published'
```

The `INSERT` part is fine; the `DO UPDATE` branch fails whenever it runs:

```sql
INSERT INTO posts (id, user_id, title, status, created_at)
VALUES ($id, $user, $title, 'draft', now())
ON CONFLICT (id) DO UPDATE SET status = 'deleted';
```

```
warning[check_violation]: storing this value in status violates CHECK constraint "posts_status_check": the statement fails as soon as it updates a row
   ╭─[queries.sql:3:41]
 3 │ ON CONFLICT (id) DO UPDATE SET status = 'deleted';
   ·                                         ─────────
   ╰────
  help: the constraint allows one of 'archived', 'draft', 'published'
```

### Good

```sql
UPDATE posts SET status = 'published' WHERE id = $id;

-- Not a constant: checked by PostgreSQL at run time.
UPDATE posts SET status = $status WHERE id = $id;
```

`SET col = NULL` is not reported (a `CHECK` passes on NULL; `NOT NULL` is a
different constraint), nor is `SET status = status`.

## Fix

None. The right value is the author's to choose; the help line lists the
allowed values when typedpg can describe them.

## When it stays silent

- The value is not a constant: a parameter, a column, an expression over
  other columns, a multi-column `SET (a, b) = (…)`.
- A `BEFORE` row trigger or a rewrite rule on the table may change the row
  before the check runs.
- The target is a view.
- The constraint's verdict depends on another column of the row.
- The column is assigned through an indirection (`SET arr[1] = …`,
  `SET comp.field = …`).

## Configuration

```toml
[package.metadata.typedpg.lint]
check_violation = "error"   # "error", "warning", "info" or "off"
```

```json
{
  "lint": { "checkViolation": "error" }
}
```

`fail_on` / `failOn` sets the severity that fails the run (default `error`),
and `ignore` lists glob patterns of files whose query findings are dropped.
For one statement:

```sql
-- typedpg-lint: allow(check_violation)
UPDATE posts SET status = 'legacy' WHERE id = $id;
```

## Related rules

- [`constant_condition`](constant_condition.md): a condition the same
  constraints decide, such as `WHERE status = 'publshed'`.
