# `fk_type_mismatch`

A foreign key whose referencing column has a different type than the column it references.

| | |
|---|---|
| Default level | warning |
| Applies to | the final schema |
| Fix | no |

## Why

PostgreSQL accepts a foreign key between columns of different types as long as an equality operator connects them: an `integer` column can reference a `bigint` key, a `text` column a `varchar` key. The constraint works, but the mismatch shows up later:

- The referencing column may not hold every value of the referenced one. An `integer` `author_id` pointing at a `bigint` `users.id` stops working the day a user gets id 2147483648: that user can't author anything.
- Every lookup and join between the two compares across types — through a cross-type operator, or by casting one side (a `text` column against a `varchar` key compares as `text`). The same value travels as two types through your code, too.

The fix is to give both columns the same type, usually the referenced key's.

## What it reports

Every foreign key in the final schema where some referencing column's type differs from its referenced column's type, comparing base types: domains are stripped (a column of a domain over `bigint` is a `bigint`), and type modifiers are ignored (`varchar(20)` referencing `varchar(10)` is fine). `text` and `varchar` are different types and are reported. The message names the first mismatching column pair, with each type's internal name (`int4`, `int8`, `varchar`).

### Placement and `--previous-deployment`

A schema rule: it checks the catalog after the last migration and reports on the statement that created the foreign key. Changing a column's type later, in either table, changes what it sees. When the creating statement is in a migration file unchanged from `--previous-deployment <ref>` (or matching `lint.ignore`), the finding stays there and the help line points at a new migration: `in a new migration, give author_id the type int8, so the join compares without a cast`. A foreign key created by `extra_migrations` is not reported.

## Examples

### Bad

```sql
-- migrations/0001_init.sql
CREATE TABLE users (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY
);
CREATE TABLE posts (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    author_id integer NOT NULL REFERENCES users (id)
);
CREATE INDEX posts_author_id_idx ON posts (author_id);
```

```text
warning[fk_type_mismatch]: foreign key posts_author_id_fkey compares public.posts.author_id (int4) with public.users.id (int8)
   ╭─[migrations/0001_init.sql:4:1]
 4 │ CREATE TABLE posts (
   · ────────────────────
 5 │     id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
   · ───────────────────────────────────────────────────────
 6 │     author_id integer NOT NULL REFERENCES users (id)
   · ────────────────────────────────────────────────────
 7 │ );
   · ─
   ╰────
  help: give author_id the type int8, so the join compares without a cast
```

### Good

```sql
CREATE DOMAIN user_id AS bigint;
CREATE TABLE users (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    code varchar(10) NOT NULL UNIQUE
);
CREATE TABLE posts (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    -- A domain over bigint is a bigint.
    author_id user_id NOT NULL REFERENCES users (id),
    -- Only the length differs: same type.
    author_code varchar(20) REFERENCES users (code)
);
CREATE INDEX posts_author_id_idx ON posts (author_id);
CREATE INDEX posts_author_code_idx ON posts (author_code);
```

To fix an existing mismatch, change the referencing column's type in a new migration. `integer` → `bigint` rewrites the table ([`table_rewrite`](table_rewrite.md)), so on a large table use the add-column, backfill, swap pattern described there.

## When it stays silent

- The types match after stripping domains and type modifiers.
- The foreign key was created by an `extra_migrations` file.
- An allow comment on the statement that created the foreign key.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
fk_type_mismatch = "error"   # "error", "warning" (default), "info" or "off"
```

```json
// typedpg.config.json
{ "lint": { "fkTypeMismatch": "error" } }
```

To accept one, put the allow comment on the statement that creates the foreign key:

```sql
-- typedpg-lint: allow(fk_type_mismatch)
CREATE TABLE posts (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    author_id integer NOT NULL REFERENCES users (id)
);
```

See [Configuring rules](README.md#configuring-rules) for `fail_on` / `failOn`, `ignore` and `--previous-deployment`.

## Related rules

- [`fk_without_index`](fk_without_index.md)
- [`prefer_bigint_pk`](prefer_bigint_pk.md): the `int4` key on the other side of many of these.
- [`table_rewrite`](table_rewrite.md): changing the type safely.
