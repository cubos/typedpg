# `cartesian_join`

Reports `FROM` items of one `SELECT` that no condition connects, so every row
of one pairs with every row of the other.

| | |
|---|---|
| Default level | warning |
| Applies to | queries |
| Fix | no |

## Why

`FROM users u, posts p` without a condition relating `u` and `p` is a
Cartesian product: 10,000 users and 100,000 posts make a billion rows. It is
nearly always a forgotten join condition, often after a refactor moved
conditions around, or a `JOIN … ON` whose condition only filters one side
(`ON p.status = 'published'`). On small test data it may even return
plausible results; in production it exhausts memory or runs for hours. When
every combination is really wanted, `CROSS JOIN` says so.

## What it reports

For each `SELECT` with two or more `FROM` items (comma-separated or joined),
the rule groups the items connected by:

- a `WHERE`, `JOIN … ON` or `HAVING` condition — each argument of a
  top-level `AND` on its own — that references both, directly or
  transitively through other items (a correlated subquery in the condition
  counts too);
- `USING (…)` or `NATURAL`;
- an explicit `CROSS JOIN`;
- a `LATERAL` subquery or `FROM` function referencing items to its left.

Each group after the first that contains an item worth reporting is a
finding, placed on that group's first such item and naming the first item of
the first group. What each column reference names comes from the analyzer, so
an unqualified column counts for the table that has it.

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

SELECT u.email, p.title FROM users u, posts p WHERE p.status = 'published';
SELECT u.email, p.title FROM users u JOIN posts p ON p.status = 'published';
```

```
warning[cartesian_join]: p is joined to u with no condition connecting them: every row of one pairs with every row of the other
   ╭─[queries.sql:2:39]
 2 │ SELECT u.email, p.title FROM users u, posts p WHERE p.status = 'published';
   ·                                       ───────
   ╰────
  help: add the join condition, or write CROSS JOIN if every combination is meant

warning[cartesian_join]: p is joined to u with no condition connecting them: every row of one pairs with every row of the other
   ╭─[queries.sql:5:43]
 5 │ SELECT u.email, p.title FROM users u JOIN posts p ON p.status = 'published';
   ·                                           ───────
   ╰────
  help: add the join condition, or write CROSS JOIN if every combination is meant
```

### Good

```sql
-- Connected by ON or WHERE.
SELECT u.email, p.title FROM users u JOIN posts p ON p.user_id = u.id;
SELECT u.email, p.title FROM users u, posts p WHERE p.user_id = u.id;

-- Every combination, on purpose.
SELECT u.email, p.title FROM users u CROSS JOIN posts p;

-- A function in FROM multiplies rows deliberately.
SELECT u.email, d.day FROM users u, generate_series(1, 7) AS d(day);

-- An item of at most one row costs nothing to pair with.
SELECT u.email, t.total FROM users u, (SELECT count(*) AS total FROM posts) t;

-- A LATERAL item referencing its left side.
SELECT u.email, l.title FROM users u,
LATERAL (SELECT title FROM posts p WHERE p.user_id = u.id ORDER BY p.id DESC LIMIT 1) l;
```

## When it stays silent

Items whose product is cheap or meant are never reported:

- an item proven to return at most one row: a subquery the analyzer's
  cardinality proof accepts (an aggregate without `GROUP BY`, a one-row
  `VALUES`, a key lookup) or with a literal `LIMIT 0` / `LIMIT 1` (or
  `FETCH FIRST 1`, not `WITH TIES`), or a reference to such a `WITH` query;
- a function in `FROM` (`generate_series`, `unnest`, `json_table`, …):
  whether it returns a set isn't resolved, and a set-returning one is the
  usual way to multiply rows on purpose.

The rule also says nothing about a level when:

- a condition references something it can't place on an item — a join's
  merged `USING` column, an entry with no name — which may connect anything;
- a `FROM` item is of a kind it doesn't model.

Only a `SELECT`'s `FROM` is read: `UPDATE … FROM` and `DELETE … USING` are
not. Migrations are not read: this rule runs over queries only.

## Configuration

Rust (`Cargo.toml`):

```toml
[package.metadata.typedpg.lint]
cartesian_join = "error"   # "error", "warning" (default), "info" or "off"
fail_on = "warning"        # fail the run on warnings too (default "error")
ignore = ["tests/**"]      # query files whose findings are dropped
```

TypeScript (`typedpg.config.json`):

```json
{
  "lint": {
    "cartesianJoin": "error",
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
-- typedpg-lint: allow(cartesian_join)
SELECT u.email, p.title FROM users u, posts p WHERE p.status = 'published';
```

Writing `CROSS JOIN` is usually the better way to say the product is meant.

## Related rules

- [`constant_condition`](constant_condition.md): a `JOIN … ON` that is always
  true or never true.
- [`outer_reference_in_subquery`](outer_reference_in_subquery.md): a subquery
  column that silently resolves to the outer query.
- [`redundant_join`](redundant_join.md): a join that can't change the rows.
