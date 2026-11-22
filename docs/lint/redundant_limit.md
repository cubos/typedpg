# `redundant_limit`

Reports a `LIMIT` or `FETCH FIRST` on a query typedpg proves returns at most
one row.

| | |
|---|---|
| Default level | info |
| Applies to | queries |
| Fix | yes (`--fix`): removes the clause |

## Why

`SELECT … WHERE id = $id LIMIT 1` reads as if the lookup could match several
rows and the author wanted one of them. When `id` is the primary key it
can't, and the `LIMIT` only misleads the reader about the data model (and
hides the real question if the key ever stops being unique). Removing it
states what the schema already guarantees.

## What it reports

A `SELECT` with a literal positive `LIMIT n` / `FETCH FIRST|NEXT n ROWS
ONLY` (no `OFFSET`, no `WITH TIES`) that the analyzer's cardinality proof
shows returns at most one row without the limit. The proof accepts, among
others:

- every `FROM` table's primary key or unique key equated, column by column,
  to constants, parameters, outer columns or columns of already-determined
  tables (through the column type's default equality);
- an aggregate or `HAVING` without `GROUP BY`, or `GROUP BY` keys that are
  each one value;
- a `SELECT` without `FROM`, or a single-row `VALUES`;
- single-row CTEs, `FROM` subqueries and non-set-returning `FROM` functions.

It gives up (and the rule stays silent) on set operations, set-returning
calls, `FULL JOIN`, views other than trusted `pg_catalog` ones, inheritance
parents, foreign tables, and anything else it can't see through.

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

SELECT id, email FROM users WHERE id = $id LIMIT 1;
SELECT count(*) AS n FROM posts FETCH FIRST 1 ROWS ONLY;
SELECT id FROM posts WHERE id = $id AND 100 / views > 1 LIMIT 1;
```

```
info[redundant_limit]: LIMIT 1 has no effect: the query returns at most one row
   ╭─[queries.sql:2:44]
 2 │ SELECT id, email FROM users WHERE id = $id LIMIT 1;
   ·                                            ───────
   ╰────
  help: remove it

info[redundant_limit]: FETCH FIRST 1 has no effect: the query returns at most one row
   ╭─[queries.sql:5:33]
 5 │ SELECT count(*) AS n FROM posts FETCH FIRST 1 ROWS ONLY;
   ·                                 ───────────────────────
   ╰────
  help: remove it

info[redundant_limit]: LIMIT 1 has no effect: the query returns at most one row
   ╭─[queries.sql:8:57]
 8 │ SELECT id FROM posts WHERE id = $id AND 100 / views > 1 LIMIT 1;
   ·                                                         ───────
   ╰────
  help: remove it
```

`--fix` rewrites the first two to `SELECT id, email FROM users WHERE id =
$id` and `SELECT count(*) AS n FROM posts`. The third keeps its finding
without a fix (see below).

### Good

```sql
-- user_id is not unique: the LIMIT picks a row.
SELECT id FROM posts WHERE user_id = $user_id ORDER BY id LIMIT 1;

-- A parameter count, or an OFFSET next to the LIMIT, is left alone.
SELECT id FROM users WHERE id = $id LIMIT $n;
SELECT id FROM users WHERE id = $id LIMIT 1 OFFSET $skip;

-- The LIMIT of a set operation applies to the combined rows.
SELECT id FROM users WHERE email = $a UNION SELECT id FROM users WHERE email = $b LIMIT 1;
```

`LIMIT 0`, `LIMIT ALL`, `LIMIT NULL` and `FETCH … WITH TIES` are not reported
either.

## Fix

The fix deletes the clause (`LIMIT n`, or the whole `FETCH FIRST n ROWS
ONLY`) and the whitespace before it, keeping a newline that ends a `--`
comment so the text after it doesn't join the comment.

It is withheld when the select is not proven error-free. With the `LIMIT`,
the scan may stop before reading rows that the query without it evaluates;
an expression that can raise (the division `100 / views` raises on
`views = 0`) could then fail on a row it never reached before. The proof
accepts only expressions that can't raise and `FROM` items that are stored
tables.

Every fix is also re-analyzed before it is offered and must keep the result
columns' names, types, nullability and value refinements, and the parameters,
unchanged; a fix that doesn't is withheld and reported as an
`internal_error` (a typedpg bug).

## When it stays silent

- Whenever the cardinality proof can't show at most one row, including
  queries with `$..spread` parameters.
- `LIMIT` with an `OFFSET`, a non-literal count, `0`, `ALL` or `NULL`.
- Migrations are not read: this rule runs over queries only.

## Configuration

Rust (`Cargo.toml`):

```toml
[package.metadata.typedpg.lint]
redundant_limit = "off"   # "error", "warning", "info" (default) or "off"
fail_on = "error"         # the severity the run fails on (default)
ignore = ["tests/**"]     # query files whose findings are dropped
```

TypeScript (`typedpg.config.json`):

```json
{
  "lint": {
    "redundantLimit": "off",
    "failOn": "error",
    "ignore": ["tests/**"]
  }
}
```

An info finding never fails the run unless `fail_on` / `failOn` is `"info"`.

To allow one query, put the comment in the query text (the `sql!` / `sql()`
string, or the statement in a `.sql` file):

```sql
-- typedpg-lint: allow(redundant_limit)
SELECT id, email FROM users WHERE id = $id LIMIT 1;
```

## Related rules

- [`limit_without_order`](limit_without_order.md): a `LIMIT` without
  `ORDER BY` on a query that may return many rows.
- [`fetch_all_single_row`](fetch_all_single_row.md): `fetch_all` on a query
  that returns at most one row (same proof).
- [`redundant_distinct`](redundant_distinct.md): a `DISTINCT` on rows that
  are already distinct.
