# `case_without_else`

A `CASE` without `ELSE` whose `WHEN`s may all be false for some row, which
makes it `NULL` there.

| Level | Applies to | Fix |
|---|---|---|
| warning | queries | writes `ELSE NULL` |

## Example

```sql
CREATE TABLE posts (
    id bigint PRIMARY KEY,
    status text NOT NULL CHECK (status IN ('draft', 'published', 'archived')),
    views integer NOT NULL
);

SELECT id, CASE WHEN views > 1000 THEN 'popular' END AS badge FROM posts;
SELECT id, CASE status WHEN 'draft' THEN 1 WHEN 'published' THEN 2 END AS rank FROM posts;
```

```
warning[case_without_else]: CASE without ELSE is NULL when no WHEN matches
   ╭─[queries.sql:4:12]
 4 │ SELECT id, CASE status WHEN 'draft' THEN 1 WHEN 'published' THEN 2 END AS rank FROM posts;
   ·            ───────────────────────────────────────────────────────────
   ╰────
  help: write ELSE NULL if that is intended, or add the missing case
```

`--fix` gives `… THEN 'popular' ELSE NULL END …` and
`… WHEN 'published' THEN 2 ELSE NULL END …`. If the `NULL` is not intended,
add the missing case instead (here, `'archived'`).

## Why

The implicit `ELSE NULL` is easy to miss: a badge that is `NULL` for most
rows, a sum that skips them, a comparison that drops the row.

## Not reported

- A `CASE` with any `ELSE`, including `ELSE NULL`.
- `WHEN`s that cover every row, given `NOT NULL`, `CHECK` constraints and the
  query's `WHERE`:
  - `WHEN views >= 1000 … WHEN views < 1000 …` on a `NOT NULL` column;
  - one `WHEN` per value the `CHECK` allows, searched or simple;
  - `WHEN name IS NULL … WHEN name IS NOT NULL …`;
  - `CASE WHEN views > 1000 …` in a query with `WHERE views > 2000`.
- A `WHEN` that is always true, such as `WHEN true`.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
case_without_else = "off"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "caseWithoutElse": "off" } }
```

Allow one query with `-- typedpg-lint: allow(case_without_else)` in its text.
See [Configuring rules](README.md#configuring-rules).

## Related

- [`unreachable_case_branch`](unreachable_case_branch.md): a `CASE` branch
  that is never taken.
- [`simplifiable_case`](simplifiable_case.md): a `CASE` that is a `COALESCE`
  or its own condition.
