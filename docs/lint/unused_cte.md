# `unused_cte`

A `WITH` query that nothing in the statement references.

| | |
|---|---|
| Default level | warning |
| Applies to | queries |
| Fix | yes (`--fix`): removes the `WITH` item (and the `WITH` keyword when it was the only one) |

## Why

PostgreSQL never runs a `SELECT` (or `VALUES`) `WITH` query that no part of
the statement reads: the planner drops it. The text is dead. More often than
not it is a bug rather than leftover code: the query was meant to read the
CTE and reads the base table instead. `WITH recent AS (…) SELECT … FROM posts`
looks like it returns recent posts, but it returns all of them.

A data-modifying `WITH` query (`INSERT`, `UPDATE`, `DELETE`, `MERGE`) is
different: PostgreSQL runs it exactly once whether or not anything reads its
`RETURNING` rows, so writing one that nothing references is a legitimate way
to chain writes. The rule leaves those alone.

## What it reports

A `WITH` item whose body is a `SELECT` (including `VALUES` and set
operations) and whose name appears nowhere in the statement as an
unqualified table reference. Every `WITH` clause is checked, including ones
nested in subqueries.

Any table reference with that name anywhere in the statement counts as a
use: a recursive query's reference to itself, a reference inside another
CTE, a reference a nested `WITH` of the same name may answer. Only a name
that appears nowhere is reported. The finding is placed on the CTE's name.

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
    title text NOT NULL,
    status text NOT NULL CHECK (status IN ('draft', 'published', 'archived')),
    created_at timestamptz NOT NULL
);
```

### Bad

```sql
WITH recent AS (
  SELECT id FROM posts WHERE created_at > now() - interval '1 day'
)
SELECT id, title FROM posts;
```

```
warning[unused_cte]: WITH query recent is never referenced
   ╭─[queries.sql:1:6]
 1 │ WITH recent AS (
   ·      ──────
   ╰────
  help: remove it, or read it where it was meant to be
```

`--fix` rewrites it to `SELECT id, title FROM posts;`.

A body that calls a volatile function is still reported, but without a fix
(see [Fix](#fix)):

```sql
WITH r AS (SELECT random() AS x)
SELECT id FROM users;
```

```
warning[unused_cte]: WITH query r is never referenced
   ╭─[queries.sql:1:6]
 1 │ WITH r AS (SELECT random() AS x)
   ·      ─
   ╰────
  help: remove it, or read it where it was meant to be
```

### Good

```sql
-- The CTE is read.
WITH recent AS (
  SELECT id FROM posts WHERE created_at > now() - interval '1 day'
)
SELECT id, title FROM posts WHERE id IN (SELECT id FROM recent);

-- A data-modifying WITH query runs whether or not it is read: not reported.
WITH archived AS (
  UPDATE posts SET status = 'archived'
  WHERE created_at < now() - interval '1 year'
  RETURNING id
)
SELECT count(*) FROM users;
```

## Fix

`--fix` deletes the item from its name through the `)` closing its body,
together with its separating comma. When it is the only item, the whole
`WITH [RECURSIVE] … AS (…)` prefix goes, up to the statement that follows.

Removing a `SELECT` CTE nobody reads cannot change what runs, but the fix is
withheld (the finding stays) when the body:

- binds a parameter (`WITH x AS (SELECT $n::int) …`): removing it would
  change the statement's parameters, and the generated types with them;
- calls a function any overload of which is volatile (`random()`,
  `nextval()`, …), or a function typedpg cannot find;
- has a `SEARCH` or `CYCLE` clause.

Like every fix, it is checked once more before it is offered: the edited
query must analyze to the same result columns (names, types, nullability and
value refinements) and the same parameters. A fix that fails that check is a
typedpg bug; it is withheld and reported as an `internal_error` warning.

## When it stays silent

- The CTE is data-modifying (`INSERT`, `UPDATE`, `DELETE`, `MERGE`).
- Its name appears as an unqualified table reference anywhere in the
  statement, even in a place where another, nested CTE of the same name is
  the one actually read. The check is by name, so it never reports a CTE
  that may be used.

## Configuration

Change its level or turn it off in `Cargo.toml`:

```toml
[package.metadata.typedpg.lint]
unused_cte = "error"      # "error", "warning", "info" or "off"
fail_on = "warning"       # the severity that fails the run (default "error")
ignore = ["tests/**"]     # files whose query findings are not reported
```

or in `typedpg.config.json` for the TypeScript CLI:

```json
{
  "lint": {
    "unusedCte": "error",
    "failOn": "warning",
    "ignore": ["tests/**"]
  }
}
```

To silence it for one statement, put an allow comment in the query:

```sql
-- typedpg-lint: allow(unused_cte)
WITH recent AS (SELECT id FROM posts) SELECT id, title FROM posts;
```

## Related rules

- [`outer_reference_in_subquery`](outer_reference_in_subquery.md): another
  query that reads something other than what it looks like it reads.
- [`redundant_join`](redundant_join.md): a joined table nothing reads.
