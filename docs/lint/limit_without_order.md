# `limit_without_order`

Reports a `LIMIT`, `OFFSET` or `DISTINCT ON` without `ORDER BY`, which picks
arbitrary rows.

| | |
|---|---|
| Default level | warning |
| Applies to | queries |
| Fix | no |

## Why

Without `ORDER BY`, PostgreSQL returns rows in whatever order the plan
produces them. That order is not stable: it changes with the plan (an index
appears, statistics change, a parallel scan kicks in), with physical row
placement after updates and `VACUUM`, and even between runs thanks to
synchronized sequential scans. So:

- `LIMIT 10` returns *some* 10 rows, not the first 10 of anything;
- `OFFSET 20` skips *some* 20 rows, so pagination repeats and skips rows;
- `DISTINCT ON (k)` keeps *some* row of each group.

Tests on a small table usually see insertion order and pass; production
returns different rows.

## What it reports

Every `SELECT` (at any nesting level: subqueries, CTEs, set operations' own
`LIMIT`) without `ORDER BY` that has:

- a `LIMIT n` or `FETCH FIRST n ROWS` (not `LIMIT ALL`, `LIMIT NULL` or
  `LIMIT 0`);
- an `OFFSET n` (not `OFFSET 0`, the optimization-fence idiom, or
  `OFFSET NULL`);
- a `DISTINCT ON (…)`.

Each clause is a separate finding. A select is skipped when the choice of rows
provably doesn't matter:

- the analyzer's cardinality proof shows it returns at most one row (ignoring
  its own `LIMIT`): a primary-key or unique-key lookup, an aggregate without
  `GROUP BY`, a `SELECT` without `FROM`, …;
- it is the subquery of an `EXISTS`, or a pure pass-through select inside one
  (existence doesn't care which row);
- it is the `IN` / `= ANY` / `ALL` subquery in the `WHERE` of an `UPDATE` or
  `DELETE`: the batching idiom
  `DELETE … WHERE id IN (SELECT id … LIMIT 1000)` deliberately takes any
  batch;
- it has a locking clause with `SKIP LOCKED` (the job-queue pattern wants any
  unlocked row);
- every output row carries the same values: a select list of constants and
  non-volatile, non-set-returning calls, or — over a single table, with no
  volatile call anywhere — bare columns a top-level `WHERE` conjunct fixes to a parameter or constant through the type's default
  equality (for types where equal values print the same: integers, `text`,
  `uuid`, dates and timestamps, `bool`, enums, …, under a deterministic
  collation; not `numeric`, floats, `interval` or `char(n)`, whose equal
  values can print differently).

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

SELECT id, title FROM posts WHERE user_id = $user_id LIMIT 10;
SELECT id, title FROM posts OFFSET 20;
SELECT DISTINCT ON (user_id) user_id, title FROM posts;
```

```
warning[limit_without_order]: LIMIT without ORDER BY returns arbitrary rows
   ╭─[queries.sql:2:54]
 2 │ SELECT id, title FROM posts WHERE user_id = $user_id LIMIT 10;
   ·                                                      ────────
   ╰────
  help: add an ORDER BY that makes the choice deterministic, or allow it with -- typedpg-lint: allow(limit_without_order)

warning[limit_without_order]: OFFSET without ORDER BY skips arbitrary rows
   ╭─[queries.sql:5:29]
 5 │ SELECT id, title FROM posts OFFSET 20;
   ·                             ─────────
   ╰────
  help: add an ORDER BY that makes the choice deterministic, or allow it with -- typedpg-lint: allow(limit_without_order)

warning[limit_without_order]: DISTINCT ON without ORDER BY keeps an arbitrary row of each group
   ╭─[queries.sql:8:8]
 8 │ SELECT DISTINCT ON (user_id) user_id, title FROM posts;
   ·        ─────────────────────
   ╰────
  help: add an ORDER BY that makes the choice deterministic, or allow it with -- typedpg-lint: allow(limit_without_order)
```

### Good

```sql
-- An ORDER BY settles it (unstable_order then checks the keys can't tie).
SELECT id, title FROM posts WHERE user_id = $user_id ORDER BY id LIMIT 10;

-- A primary-key lookup returns at most one row (redundant_limit reports the
-- LIMIT as useless instead).
SELECT id, email FROM users WHERE id = $id LIMIT 1;

-- Batching a DELETE: any 1000 rows will do.
DELETE FROM posts WHERE id IN (SELECT id FROM posts WHERE status = 'archived' LIMIT 1000);

-- A queue: any unlocked row will do.
SELECT id FROM posts WHERE status = 'draft' LIMIT 1 FOR UPDATE SKIP LOCKED;

-- EXISTS only asks whether a row exists.
SELECT EXISTS (SELECT 1 FROM posts WHERE user_id = $user_id LIMIT 1) AS has_posts;

-- Every row the LIMIT could pick projects the same values.
SELECT status FROM posts WHERE status = $status LIMIT 1;
SELECT 1 AS one FROM posts LIMIT 1;
```

## When it stays silent

- `LIMIT` / `OFFSET` given as a parameter or expression is still reported;
  only the literal `LIMIT 0`, `OFFSET 0`, `ALL` and `NULL` are exempt.
- An `IN` subquery in a `SELECT`'s `WHERE` or select list *is* reported: the
  limit changes which values the outer query sees. Only the `WHERE` of an
  `UPDATE` / `DELETE` is exempt.
- An `EXISTS` subquery's exemption passes through a `FROM` subquery only when
  the select in between adds no filter, join, grouping, aggregate, limit or
  set-returning call.
- An `ORDER BY` of any kind silences it, even one whose keys can tie: that is
  [`unstable_order`](unstable_order.md)'s finding.
- Migrations are not read: this rule runs over queries only.

## Configuration

Rust (`Cargo.toml`):

```toml
[package.metadata.typedpg.lint]
limit_without_order = "error"   # "error", "warning" (default), "info" or "off"
fail_on = "warning"             # fail the run on warnings too (default "error")
ignore = ["tests/**"]           # query files whose findings are dropped
```

TypeScript (`typedpg.config.json`):

```json
{
  "lint": {
    "limitWithoutOrder": "error",
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
-- typedpg-lint: allow(limit_without_order)
SELECT id, title FROM posts LIMIT 10;
```

## Related rules

- [`unstable_order`](unstable_order.md): an `ORDER BY` under `LIMIT` whose
  keys can tie.
- [`redundant_limit`](redundant_limit.md): a `LIMIT` on a query that returns
  at most one row.
- [`fetch_may_return_many`](fetch_may_return_many.md): `fetch_one` on a query
  not proven to return one row.
