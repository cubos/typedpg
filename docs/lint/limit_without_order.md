# `limit_without_order`

A `LIMIT`, `OFFSET` or `DISTINCT ON` without `ORDER BY`, which picks
arbitrary rows.

| Category | Level | Applies to | Fix |
|---|---|---|---|
| correctness | error | queries | — |

## Example

```sql
CREATE TABLE posts (id bigint PRIMARY KEY, user_id bigint NOT NULL, title text NOT NULL);
CREATE INDEX ON posts (user_id);

SELECT id, title FROM posts WHERE user_id = $user_id LIMIT 10;
SELECT id, title FROM posts OFFSET 20;
SELECT DISTINCT ON (user_id) user_id, title FROM posts;
```

```
error[correctness/limit_without_order]: LIMIT without ORDER BY returns arbitrary rows
   ╭─[queries.sql:2:54]
 2 │ SELECT id, title FROM posts WHERE user_id = $user_id LIMIT 10;
   ·                                                      ────────
   ╰────
  help: add an ORDER BY that makes the choice deterministic, or allow it with -- typedpg-lint: allow(limit_without_order)

error[correctness/limit_without_order]: DISTINCT ON without ORDER BY keeps an arbitrary row of each group
   ╭─[queries.sql:6:8]
 6 │ SELECT DISTINCT ON (user_id) user_id, title FROM posts;
   ·        ─────────────────────
   ╰────
  help: add an ORDER BY that makes the choice deterministic, or allow it with -- typedpg-lint: allow(limit_without_order)
```

## Why

Without `ORDER BY`, rows come in whatever order the plan produces, which
changes with indexes, statistics, updates and even between runs. `LIMIT 10`
returns some 10 rows, `OFFSET` pagination repeats and skips rows, and
`DISTINCT ON` keeps some row per group. Tests on small tables pass;
production returns different rows.

## Reported when

A `SELECT` at any level (subqueries, CTEs, the `LIMIT` of a `UNION`) has no
`ORDER BY` and has, each reported separately:

- `LIMIT n` or `FETCH FIRST n ROWS ONLY` (`FETCH FIRST ROW ONLY` without a count
  is `LIMIT 1`), also with a parameter: `LIMIT $n`;
- `OFFSET n`;
- `DISTINCT ON (…)`.

An `IN (SELECT … LIMIT 5)` inside a `SELECT` is reported: the limit changes
which values the outer query sees.

## Not reported

- `LIMIT 0` (the whole select), `LIMIT ALL`, `LIMIT NULL`, `OFFSET 0`,
  `OFFSET NULL`.
- Any `ORDER BY`, even one whose keys can tie: see
  [`unstable_order`](unstable_order.md).
- A query that returns at most one row anyway:
  `WHERE id = $id LIMIT 1` (see [`redundant_limit`](redundant_limit.md)).
  For `DISTINCT ON`, the rows before it count: `DISTINCT ON (user_id) …
  WHERE user_id = $u` returns one row, but an arbitrary one of the user's.
- An `EXISTS` subquery: `EXISTS (SELECT 1 FROM posts WHERE … LIMIT 1)`.
- Batching in an `UPDATE` / `DELETE`:
  `DELETE FROM posts WHERE id IN (SELECT id FROM posts WHERE … LIMIT 1000)`.
- `FOR UPDATE SKIP LOCKED`, the job-queue pattern.
- Every row would produce the same output:
  - the select list reads no column and calls nothing volatile:
    `SELECT 1 AS one FROM posts LIMIT 1`, `now()`, but not `random()`;
  - over tables and joins of them, it outputs only columns `WHERE` fixes to
    a parameter or constant: `SELECT status FROM posts WHERE status =
    $status LIMIT 1`, also with `JOIN users u ON …`. A join multiplies rows,
    never changes those values.
    Not for types whose equal values can print differently (`numeric`,
    floats, `interval`, `char(n)`) or under a nondeterministic collation.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
limit_without_order = "error"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "limitWithoutOrder": "error" } }
```

Allow one query with `-- typedpg-lint: allow(limit_without_order)` in its
text. See [Configuring rules](README.md#configuring-rules).

## Related

- [`unstable_order`](unstable_order.md): an `ORDER BY` under `LIMIT` whose
  keys can tie.
- [`redundant_limit`](redundant_limit.md): a `LIMIT` on a query that returns
  at most one row.
- [`fetch_one_multiple_rows`](fetch_one_multiple_rows.md): `fetch_one` on a query
  not proven to return one row.
