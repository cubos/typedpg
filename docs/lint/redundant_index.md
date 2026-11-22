# `redundant_index`

An index that another index on the same table already covers: every query it can serve, the other serves too.

| | |
|---|---|
| Default level | info |
| Applies to | the final schema |
| Fix | no |

## Why

Every index costs on every write — each `INSERT`, and each `UPDATE` that can't be done in place, adds an entry to it — and it takes disk and cache space. A btree index on `(email, name)` can do every lookup an index on `(email)` can, since `email` leads it; the single-column index only slows writes down. Exact duplicates are common too: an explicit `CREATE INDEX` on a column that already has a primary key or unique constraint.

## What it reports

Index `A` is reported as redundant with index `B` on the same table when all of these hold:

- both use the same access method, and both have plain column keys (no expressions) and no `WHERE` predicate;
- `B` has at least as many key columns, and its first ones are `A`'s key columns in the same order, with the same operator classes, collations and sort orders (`ASC` / `DESC`, `NULLS FIRST` / `LAST`; the defaults spelled out count as the defaults);
- `A` adds nothing: it is not unique and has no `INCLUDE` columns — unless the two indexes are identical;
- both are immediate, or both are deferrable (a `DEFERRABLE` unique constraint checks at commit, an immediate unique index at each statement: neither replaces the other).

A unique index is also reported when an otherwise identical unique `NULLS NOT DISTINCT` index exists: the stricter index enforces everything it does. Of two exact duplicates, the one created later is reported (and the earlier one too, when a third index covers both). An index that backs a constraint (a primary key, unique or exclusion constraint) is never the one reported, since it cannot be dropped with `DROP INDEX` and `ON CONFLICT ON CONSTRAINT` depends on it: when it duplicates a standalone index, the standalone index is reported, even if it came first.

Only indexes created by your migrations are reported; an index from `extra_migrations` can make one of yours redundant, but is never reported itself.

### Placement and `--previous-deployment`

A schema rule: it looks at the indexes after the last migration and reports on the statement that created the redundant index. Normally the finding has no help line. When that statement is in a migration file unchanged from `--previous-deployment <ref>` (or matching `lint.ignore`), it is still reported there, with a help line that drops the index in a new migration:

```text
help: DROP INDEX CONCURRENTLY users_email_idx, in a new migration whose first line is -- no-transaction
```

## Examples

### Bad

```sql
-- migrations/0001_init.sql
CREATE TABLE users (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    email text NOT NULL,
    name text NOT NULL
);
CREATE INDEX users_email_idx ON users (email);
CREATE INDEX users_email_name_idx ON users (email, name);
CREATE INDEX users_id_idx ON users (id);
```

```text
info[redundant_index]: index users_email_idx is redundant with users_email_name_idx
   ╭─[migrations/0001_init.sql:6:1]
 6 │ CREATE INDEX users_email_idx ON users (email);
   · ─────────────────────────────────────────────
   ╰────

info[redundant_index]: index users_id_idx is redundant with users_pkey
   ╭─[migrations/0001_init.sql:8:1]
 8 │ CREATE INDEX users_id_idx ON users (id);
   · ───────────────────────────────────────
   ╰────
```

Every redundant index is reported in one run, and each finding names an index that is not reported itself, so dropping every reported index leaves one that covers them all. With two exact duplicates and a wider index (`(email)`, `(email)` again, `(email, name)`), both `(email)` indexes are reported as redundant with `(email, name)`; with `(a)`, `(a, b)` and `(a, b, c)`, both narrower ones name `(a, b, c)`. When several indexes would do, the finding names an exact duplicate first, then the oldest.

### Good

None of these is redundant with another:

```sql
CREATE TABLE events (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    kind text NOT NULL,
    happened_at timestamptz NOT NULL,
    payload jsonb
);
-- Unique: enforces something (kind, happened_at) doesn't.
CREATE UNIQUE INDEX events_kind_key ON events (kind);
CREATE INDEX events_kind_happened_at_idx ON events (kind, happened_at);
-- A different sort order serves a different ORDER BY.
CREATE INDEX events_kind_happened_at_desc_idx ON events (kind, happened_at DESC);
-- Covering indexes serve index-only scans the other can't.
CREATE INDEX events_happened_at_idx ON events (happened_at) INCLUDE (kind);
CREATE INDEX events_happened_at_payload_idx ON events (happened_at) INCLUDE (payload);
-- A different operator class serves different operators (LIKE 'abc%').
CREATE INDEX events_kind_pattern_idx ON events (kind text_pattern_ops);
```

To remove a redundant index from a table with rows, use `DROP INDEX CONCURRENTLY` in a `-- no-transaction` migration ([`drop_index_blocking`](drop_index_blocking.md)).

## When it stays silent

- Either index is partial or has an expression key: the rule doesn't compare predicates or expressions.
- The covering index differs in access method, operator class, collation, sort order or deferrability.
- The redundant candidate is unique or covering, and not an exact duplicate.
- The candidate backs a constraint, or was created by an `extra_migrations` file.
- An allow comment on the statement that created the redundant index.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
redundant_index = "warning"   # "error", "warning", "info" (default) or "off"
```

```json
// typedpg.config.json
{ "lint": { "redundantIndex": "warning" } }
```

To keep one index on purpose, put the allow comment on the statement that created it:

```sql
-- typedpg-lint: allow(redundant_index)
CREATE INDEX users_email_idx ON users (email);
```

See [Configuring rules](README.md#configuring-rules) for `fail_on` / `failOn`, `ignore` and `--previous-deployment`.

## Related rules

- [`fk_without_index`](fk_without_index.md): the index that is missing rather than extra.
- [`drop_index_blocking`](drop_index_blocking.md): dropping it safely.
