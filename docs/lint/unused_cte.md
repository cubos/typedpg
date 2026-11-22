# `unused_cte`

A `WITH` query that nothing in the statement references.

| Level | Applies to | Fix |
|---|---|---|
| warning | queries | removes the `WITH` item |

## Example

```sql
CREATE TABLE users (id bigint PRIMARY KEY);
CREATE TABLE posts (id bigint PRIMARY KEY, title text NOT NULL, status text NOT NULL, created_at timestamptz NOT NULL);

WITH recent AS (SELECT id FROM posts WHERE created_at > now() - interval '1 day')
SELECT id, title FROM posts;

WITH r AS (SELECT random() AS x), s AS (SELECT 1 AS y)
SELECT id FROM users;
```

```
warning[unused_cte]: WITH query recent is never referenced
   ╭─[queries.sql:2:6]
 2 │ WITH recent AS (SELECT id FROM posts WHERE created_at > now() - interval '1 day')
   ·      ──────
   ╰────
  help: remove it, or read it where it was meant to be
```

`--fix` gives `SELECT id, title FROM posts` and
`WITH r AS (SELECT random() AS x) SELECT id FROM users`: `r` and `s` are
both reported, but `r` calls a volatile function (see below).

## Why

PostgreSQL never runs a `SELECT` CTE nobody reads, so the text is dead. More
often it is a bug: `WITH recent AS (…) SELECT … FROM posts` looks like it
returns recent posts but returns all of them.

## Reported when

A `WITH` item whose body is a `SELECT`, `VALUES` or set operation, and whose
name appears nowhere in the statement as a table reference. Nested `WITH`
clauses are checked too.

## Not reported

- Data-modifying CTEs (`INSERT`, `UPDATE`, `DELETE`, `MERGE`): they run
  whether or not they are read.
  `WITH archived AS (UPDATE posts SET status = 'archived' … RETURNING id) SELECT count(*) AS n FROM users`.
- A name referenced anywhere, even where a nested CTE of the same name may
  be the one read, or by the recursive CTE itself.

## Fix not offered

When the body:

- uses a parameter (`WITH x AS (SELECT $n::int AS v) …`): removing it would
  change the query's parameters;
- calls a function that may be volatile (`random()`, `nextval()`) or that
  typedpg can't find;
- has a `SEARCH` or `CYCLE` clause.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
unused_cte = "error"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "unusedCte": "error" } }
```

Allow one query with `-- typedpg-lint: allow(unused_cte)` in its text. See
[Configuring rules](README.md#configuring-rules).

## Related

- [`redundant_join`](redundant_join.md): a joined table nothing reads.
- [`outer_reference_in_subquery`](outer_reference_in_subquery.md): a
  subquery reading an outer column it doesn't look like it reads.
