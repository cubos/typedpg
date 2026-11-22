# `fk_without_index`

A foreign key whose referencing columns no index can look up.

| | |
|---|---|
| Default level | warning |
| Applies to | the final schema |
| Fix | no |

## Why

PostgreSQL indexes the referenced side of a foreign key (it must be a primary key or unique constraint) but not the referencing side. Every `DELETE` of a parent row, and every `UPDATE` of its key, must find the child rows that point at it — to reject the change, cascade it, or set them NULL. Without an index on the child's columns, that is a sequential scan of the child table for each parent row touched. Deleting a thousand users scans `posts` a thousand times, holding row locks on the parent all the while. Joins from parent to children (`JOIN posts ON posts.author_id = users.id`) need the same index.

## What it reports

Every foreign key in the schema the migrations leave behind whose referencing table has no index that can serve the lookup `col1 = $1 AND col2 = $2 …`. An index serves it when:

- it is a btree index, or a hash index for a single-column key (other access methods such as BRIN or GIN don't count);
- its leading key columns are exactly the foreign key's columns, in any order (`(b, a)` serves a key on `(a, b)`), with more columns allowed after them; an expression among the leading positions disqualifies it, while an expression or `INCLUDE` column after them does not;
- it is not partial, or its predicate only requires foreign-key columns to be `NOT NULL` (`WHERE author_id IS NOT NULL`, or an `AND` of such tests): the lookup's equality implies that. Any other predicate (`WHERE title <> ''`, `WHERE author_id IS NULL`, an `OR`) disqualifies it.

A primary key or unique constraint's index counts like any other.

### Placement and `--previous-deployment`

This is a schema rule: it runs once, over the catalog after the last migration, and puts each finding on the statement that created the foreign key — the `CREATE TABLE` with the inline `REFERENCES`, or the `ALTER TABLE … ADD CONSTRAINT`. An index added in any later migration resolves it.

A migration file that is unchanged from `--previous-deployment <ref>`, or that matches `lint.ignore`, skips the statement rules, but its schema findings are still reported on its statements — the problem is in the current schema, whichever migration created it. Since that file cannot be changed any more, the help line then points at a new migration:

```text
help: CREATE INDEX CONCURRENTLY ON public.posts (author_id), in a new migration whose first line is -- no-transaction
```

A foreign key created by an `extra_migrations` file is never reported: it is another project's schema.

## Examples

### Bad

```sql
-- migrations/0001_init.sql
CREATE TABLE users (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    email text NOT NULL
);
CREATE TABLE posts (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    author_id bigint NOT NULL REFERENCES users (id),
    title text NOT NULL
);
CREATE TABLE memberships (
    org_id bigint NOT NULL,
    user_id bigint NOT NULL REFERENCES users (id),
    PRIMARY KEY (org_id, user_id)
);
```

```text
warning[fk_without_index]: foreign key posts_author_id_fkey on public.posts (author_id) has no index
   ╭─[migrations/0001_init.sql:5:1]
 5 │ CREATE TABLE posts (
   · ────────────────────
 6 │     id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
   · ───────────────────────────────────────────────────────
 7 │     author_id bigint NOT NULL REFERENCES users (id),
   · ────────────────────────────────────────────────────
   · …
   ╰────
  help: CREATE INDEX ON public.posts (author_id)

warning[fk_without_index]: foreign key memberships_user_id_fkey on public.memberships (user_id) has no index
    ╭─[migrations/0001_init.sql:10:1]
 10 │ CREATE TABLE memberships (
    · ──────────────────────────
 11 │     org_id bigint NOT NULL,
    · ───────────────────────────
 12 │     user_id bigint NOT NULL REFERENCES users (id),
    · ──────────────────────────────────────────────────
    · …
    ╰────
   help: CREATE INDEX ON public.memberships (user_id)
```

`memberships`' primary key leads with `org_id`, so it can't find rows by `user_id`.

These indexes don't serve a key on `posts.author_id` either: a partial index with an unrelated predicate, an index where the key is not leading, and a BRIN index.

```sql
CREATE INDEX posts_author_partial ON posts (author_id) WHERE title <> '';
CREATE INDEX posts_id_author ON posts (id, author_id);
CREATE INDEX posts_author_brin ON posts USING brin (author_id);
```

### Good

```sql
CREATE TABLE posts (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    author_id bigint NOT NULL REFERENCES users (id),
    editor_id bigint REFERENCES users (id),
    reviewer_id bigint REFERENCES users (id),
    title text NOT NULL
);
-- The key's columns lead the index; more columns may follow.
CREATE INDEX posts_author_id_title_idx ON posts (author_id, title);
-- A partial index that only skips NULLs serves the lookup.
CREATE INDEX posts_editor_id_idx ON posts (editor_id) WHERE editor_id IS NOT NULL;
-- A hash index serves a single-column key.
CREATE INDEX posts_reviewer_id_idx ON posts USING hash (reviewer_id);
CREATE TABLE memberships (
    org_id bigint NOT NULL,
    user_id bigint NOT NULL REFERENCES users (id),
    -- The primary key now leads with user_id.
    PRIMARY KEY (user_id, org_id)
);
```

On a table that already has rows, build the index with `CREATE INDEX CONCURRENTLY` in a `-- no-transaction` migration ([`create_index_blocking`](create_index_blocking.md)).

## When it stays silent

- An index serves the key (above), wherever it was created.
- The foreign key was created by an `extra_migrations` file.
- An allow comment on the statement that created the foreign key.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
fk_without_index = "error"   # "error", "warning" (default), "info" or "off"
```

```json
// typedpg.config.json
{ "lint": { "fkWithoutIndex": "error" } }
```

To accept one foreign key (a tiny lookup table that is never deleted from, say), put the allow comment on the statement that creates it. The comment covers the whole statement, so every foreign key that statement creates:

```sql
-- typedpg-lint: allow(fk_without_index)
CREATE TABLE posts (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    author_id bigint NOT NULL REFERENCES users (id)
);
```

See [Configuring rules](README.md#configuring-rules) for `fail_on` / `failOn`, `ignore` and `--previous-deployment`.

## Related rules

- [`fk_type_mismatch`](fk_type_mismatch.md): the other way a foreign key's lookups go wrong.
- [`redundant_index`](redundant_index.md): the opposite problem.
- [`create_index_blocking`](create_index_blocking.md): adding the index safely.
