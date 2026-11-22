# `rls_no_policy`

Row-level security enabled on a table that has no policy, so every role it
applies to sees no rows.

| Level | Applies to | Fix |
|---|---|---|
| info | the final schema | — |

## Example

```sql
-- migrations/001_init.sql
CREATE TABLE notes (id bigint PRIMARY KEY, owner text NOT NULL, body text NOT NULL);
ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
CREATE TABLE secrets (id bigint PRIMARY KEY, value text NOT NULL);
ALTER TABLE secrets ENABLE ROW LEVEL SECURITY, FORCE ROW LEVEL SECURITY;
```

```
info[rls_no_policy]: row-level security is enabled on public.notes, which has no policy: every role but its owner, superusers and BYPASSRLS roles sees no rows and can write none
   ╭─[migrations/001_init.sql:2:1]
 2 │ ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
   · ───────────────────────────────────────────
   ╰────
  help: CREATE POLICY for the roles that use public.notes, or leave it as is if denying them is the intent
```

With `FORCE ROW LEVEL SECURITY`, the owner is subject too, and the message for
`secrets` reads `every role but superusers and BYPASSRLS roles …`. When the
migration is unchanged from `--previous-deployment`, the help asks for the
`CREATE POLICY` `in a new migration`.

## Why

With row-level security on and no policy, PostgreSQL denies everything:
selects return no rows, updates and deletes touch none, inserts fail. That
can be intended (a table only the owner uses), hence info. More often the
policies were forgotten, and the symptom, an application role silently
getting empty results, is hard to debug.

## Reported when

A table has row-level security enabled after the last migration and no
policy. The finding is placed on the table's last
`ALTER TABLE … ENABLE ROW LEVEL SECURITY`.

## Not reported

- A table with a policy, created in the same or any later migration:
  `CREATE POLICY notes_owner ON notes USING (owner = current_user)`.
- Row-level security enabled and then disabled again.
- Tables from `extra_migrations`.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
rls_no_policy = "warning"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "rlsNoPolicy": "warning" } }
```

When denying everyone is the intent, put
`-- typedpg-lint: allow(rls_no_policy)` before the `ENABLE ROW LEVEL
SECURITY` statement in the migration file.
See [Configuring rules](README.md#configuring-rules).

## Related

- [`rls_policy_not_enforced`](rls_policy_not_enforced.md): policies on a
  table without row-level security.
