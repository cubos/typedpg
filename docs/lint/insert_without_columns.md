# `insert_without_columns`

An `INSERT … VALUES` or `INSERT … SELECT` without a column list.

| Level | Applies to | Fix |
|---|---|---|
| warning | queries | writes the column list the values go to now |

## Example

```sql
CREATE TABLE users (id bigint PRIMARY KEY, email text NOT NULL, name text, deleted_at timestamptz);
CREATE TABLE old_users (id bigint PRIMARY KEY, email text NOT NULL, name text, deleted_at timestamptz);

INSERT INTO users VALUES ($id, $email, $name);
INSERT INTO users SELECT * FROM old_users;
```

```
warning[insert_without_columns]: INSERT into users without a column list depends on the table's column order
   ╭─[queries.sql:2:1]
 2 │ INSERT INTO users VALUES ($id, $email, $name);
   · ─────────────────
   ╰────
  help: list the columns the values go to
```

`--fix` gives `INSERT INTO users (id, email, name) VALUES ($id, $email, $name)`;
`deleted_at`, left out, still takes its default. The `SELECT *` one keeps its
finding without a fix.

## Why

Without a column list, values go to the table's columns by position. A
migration that adds a column or recreates the table in another order
silently moves them, and fails only when the types disagree: two `text`
columns swapped insert wrong data without an error.

## Reported when

Any `INSERT` with `VALUES` or a `SELECT` (set operations included) and no
column list, also inside a data-modifying CTE.

## Not reported

- `INSERT … DEFAULT VALUES`.
- An `INSERT` with a column list.

## Fix not offered

- The number of values isn't plain from the text: a `*` or `(row).*` in the
  select list or a `VALUES` row.
- The table name has a database qualifier, or the table has fewer columns
  than the row has values.

The fix lists the table's first N columns for N values, quoted when needed,
after the table's alias if any. Generated and identity columns are listed
too: they accept only `DEFAULT`, as before.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
insert_without_columns = "error"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "insertWithoutColumns": "error" } }
```

Allow one query with `-- typedpg-lint: allow(insert_without_columns)` in its
text. See [Configuring rules](README.md#configuring-rules).

## Related

- [`check_violation`](check_violation.md): an `INSERT` storing a constant a
  `CHECK` constraint rejects.
- [`breaks_deployed_queries`](breaks_deployed_queries.md): a migration that
  breaks a deployed `INSERT`.
