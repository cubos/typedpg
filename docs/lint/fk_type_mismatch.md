# `fk_type_mismatch`

A foreign key whose referencing column has a different type than the column
it references.

| Category | Level | Applies to | Fix |
|---|---|---|---|
| performance | warning | final schema | — |

## Example

```sql
-- migrations/0001_init.sql
CREATE TABLE users (id bigint PRIMARY KEY, code varchar(10) NOT NULL UNIQUE);
CREATE TABLE posts (id bigint PRIMARY KEY, author_id integer NOT NULL REFERENCES users (id));
CREATE INDEX ON posts (author_id);
```

```
warning[performance/fk_type_mismatch]: foreign key posts_author_id_fkey compares public.posts.author_id (int4) with public.users.id (int8)
   ╭─[migrations/0001_init.sql:2:1]
 2 │ CREATE TABLE posts (id bigint PRIMARY KEY, author_id integer NOT NULL REFERENCES users (id));
   · ────────────────────────────────────────────────────────────────────────────────────────────
   ╰────
  help: give author_id the type int8, so the join compares without a cast
```

## Why

PostgreSQL accepts the key, but:

- the referencing column may not hold every referenced value: this
  `author_id` breaks the day a user gets id 2147483648;
- every join between the two compares across types, and the same value
  travels as two types through your code.

## Reported when

Some referencing column's type differs from its referenced column's,
ignoring domains (a domain over `bigint` is `bigint`) and type modifiers
(`varchar(20)` referencing `varchar(10)` is fine). `text` referencing
`varchar` is reported. The message names the first mismatching pair.

The finding is placed on the statement that created the foreign key; a later
`ALTER COLUMN … TYPE` on either side that makes the types match resolves it.
When that statement's file can't change any more (unchanged from
`--previous-deployment`, or matched by `ignore`), the help starts with
`in a new migration, give author_id …`.

Changing `integer` to `bigint` rewrites the table
([`alter_table_rewrite`](alter_table_rewrite.md)); on a large table, add a new column,
backfill it and swap.

## Not reported

- Types that match once domains and type modifiers are ignored.
- Foreign keys created by `extra_migrations` / `extraMigrations`.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
fk_type_mismatch = "error"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "fkTypeMismatch": "error" } }
```

Allow one foreign key with `-- typedpg-lint: allow(fk_type_mismatch)` before
the statement that created it in the migration file.
See [Configuring rules](README.md#configuring-rules).

## Related

- [`fk_without_index`](fk_without_index.md): a foreign key with no index on
  its referencing columns.
- [`alter_table_rewrite`](alter_table_rewrite.md): the rewrite a type change causes.
