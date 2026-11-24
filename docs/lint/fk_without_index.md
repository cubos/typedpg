# `fk_without_index`

A foreign key whose referencing columns no index can look up.

| Category | Level | Applies to | Fix |
|---|---|---|---|
| performance | warning | final schema | — |

## Example

```sql
-- migrations/0001_init.sql
CREATE TABLE users (id bigint PRIMARY KEY, email text NOT NULL);
CREATE TABLE posts (id bigint PRIMARY KEY, author_id bigint NOT NULL REFERENCES users (id), title text NOT NULL);
CREATE TABLE memberships (org_id bigint NOT NULL, user_id bigint NOT NULL REFERENCES users (id), PRIMARY KEY (org_id, user_id));
```

```
warning[performance/fk_without_index]: foreign key posts_author_id_fkey on public.posts (author_id) has no index
   ╭─[migrations/0001_init.sql:2:1]
 2 │ CREATE TABLE posts (id bigint PRIMARY KEY, author_id bigint NOT NULL REFERENCES users (id), title text NOT NULL);
   · ────────────────────────────────────────────────────────────────────────────────────────────────────────────────
   ╰────
  help: CREATE INDEX ON public.posts (author_id)
```

`memberships` is reported too: its primary key leads with `org_id`, so it
can't find rows by `user_id`.

## Why

PostgreSQL indexes the referenced side of a foreign key, not the
referencing side. Every `DELETE` of a parent row, or `UPDATE` of its key,
must find the child rows: without an index, that is a full scan of the
child table per parent row. Joins from parent to children need the same
index.

## Reported when

No index on the referencing table serves `col1 = $1 AND col2 = $2 …` for the
key's columns. An index serves it when:

- it is a btree or a hash index;
- its leading columns are exactly the key's columns, in any order, or some
  of them that reference columns unique by themselves in the referenced
  table; more columns, expressions or `INCLUDE` columns may follow;
- it is not partial, or its predicate only requires key columns to be
  `IS NOT NULL`.

So these don't serve a key on `posts.author_id`:

```sql
CREATE INDEX ON posts (author_id) WHERE title <> '';   -- other predicate
CREATE INDEX ON posts (id, author_id);                 -- key not leading
CREATE INDEX ON posts USING brin (author_id);          -- not btree/hash
```

In a multi-tenant schema, a key `(exercise_id, tenant_id)` referencing
`exercises (id, tenant_id)` is served by an index on `exercise_id` when
`exercises.id` is the primary key: every row with that `exercise_id` belongs
to one exercise. An index leading with `tenant_id` doesn't serve it.

The finding is placed on the statement that created the foreign key; an
index added by any later migration resolves it. When that statement's file
can't change any more (unchanged from `--previous-deployment`, or matched by
`ignore`), the help points to a new migration:

```
  help: CREATE INDEX CONCURRENTLY ON public.posts (author_id), in a new migration whose first line is -- no-transaction
```

## Not reported

- An index that serves the key, created anywhere: `(author_id, title)`,
  `(editor_id) WHERE editor_id IS NOT NULL`, `USING hash (reviewer_id)`, a
  primary key `(user_id, org_id)`.
- Foreign keys created by `extra_migrations` / `extraMigrations`.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
fk_without_index = "error"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "fkWithoutIndex": "error" } }
```

Allow one foreign key with `-- typedpg-lint: allow(fk_without_index)` before
the statement that created it in the migration file.
See [Configuring rules](README.md#configuring-rules).

## Related

- [`create_index_blocking`](create_index_blocking.md): build the index
  `CONCURRENTLY` on a table that has rows.
- [`redundant_index`](redundant_index.md): an index another index covers.
- [`fk_type_mismatch`](fk_type_mismatch.md): a foreign key between columns
  of different types.
