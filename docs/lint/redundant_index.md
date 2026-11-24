# `redundant_index`

An index another index on the same table already covers.

| Category | Level | Applies to | Fix |
|---|---|---|---|
| performance | warning | the final schema | — |

## Example

```sql
-- migrations/001_init.sql
CREATE TABLE users (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    email text NOT NULL,
    name text NOT NULL
);
CREATE INDEX users_email_idx ON users (email);
CREATE INDEX users_email_name_idx ON users (email, name);
CREATE INDEX users_id_idx ON users (id);
```

```
warning[performance/redundant_index]: index users_email_idx is redundant with users_email_name_idx
   ╭─[migrations/001_init.sql:6:1]
 6 │ CREATE INDEX users_email_idx ON users (email);
   · ─────────────────────────────────────────────
   ╰────
  help: drop the index; users_email_name_idx serves the same lookups

warning[performance/redundant_index]: index users_id_idx is redundant with users_pkey
   ╭─[migrations/001_init.sql:8:1]
 8 │ CREATE INDEX users_id_idx ON users (id);
   · ───────────────────────────────────────
   ╰────
  help: drop the index; users_pkey serves the same lookups
```

When the migration is unchanged from `--previous-deployment` or matched by
`ignore`, the help puts the drop in a new migration instead:
`help: DROP INDEX CONCURRENTLY users_email_idx, in a new migration whose first line is -- no-transaction`.

## Why

Every index slows every write and takes disk and cache. A btree on
`(email, name)` serves every lookup one on `(email)` can.

## Reported when

Index A is redundant with index B on the same table when:

- both use the same access method, have plain column keys and no `WHERE`;
- B's leading key columns are A's, in order, with the same operator classes,
  collations and sort orders (`ASC NULLS LAST` spelled out equals the
  default);
- A is neither unique nor has `INCLUDE` columns, unless the two are
  identical;
- both are immediate, or both deferrable.

A unique index is also redundant with an identical unique
`NULLS NOT DISTINCT` one. Every redundant index is reported in one run, and
each finding names an index that is not itself reported, so dropping all of
them leaves one that covers the rest.

## Not reported

- A unique index next to a wider one: `UNIQUE (kind)` and `(kind, happened_at)`.
- Another sort order, operator class or access method:
  `(kind, happened_at DESC)`, `(kind text_pattern_ops)`, `USING hash (kind)`.
- Covering indexes: `(happened_at) INCLUDE (kind)`.
- Partial and expression indexes: `(kind) WHERE payload IS NULL`,
  `(lower(kind))`.
- A deferrable unique constraint next to an immediate unique index.
- An index backing a constraint (primary key, unique, exclusion): the
  standalone index it duplicates is reported instead, even if older.
- Indexes created by `extra_migrations`.

To drop a redundant index from a table with rows, see
[`drop_index_blocking`](drop_index_blocking.md).

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
redundant_index = "warning"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "redundantIndex": "warning" } }
```

Keep one index by putting `-- typedpg-lint: allow(redundant_index)` before
the statement that creates it in the migration file.
See [Configuring rules](README.md#configuring-rules).

## Related

- [`fk_without_index`](fk_without_index.md): an index that is missing.
- [`drop_index_blocking`](drop_index_blocking.md): dropping an index without
  blocking writes.
