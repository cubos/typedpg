# `rls_no_policy`

Row-level security is enabled on a table that has no policy, so PostgreSQL denies every row to every role it applies to.

| | |
|---|---|
| Default level | info |
| Applies to | the final schema |
| Fix | no |

## Why

With row-level security enabled and no policy, PostgreSQL applies a default-deny policy: queries see no rows, and `INSERT`, `UPDATE` and `DELETE` affect none (inserts fail). Only the roles that bypass row-level security are unaffected: superusers, roles with `BYPASSRLS`, and the table's owner — unless `FORCE ROW LEVEL SECURITY` is set, which subjects the owner too.

That can be the intent (a table only the owner should touch), which is why the rule is informational. But more often the policies were forgotten or live in a migration that hasn't been written yet, and the symptom — an application role that silently gets empty results — is confusing to debug.

## What it reports

Every ordinary or partitioned table of the final schema with row-level security enabled and no policy. The message lists who is exempt: "its owner, superusers and BYPASSRLS roles", or "superusers and BYPASSRLS roles" when `FORCE ROW LEVEL SECURITY` is set.

### Placement and `--previous-deployment`

A schema rule: it reads the catalog after the last migration, and reports on the last `ALTER TABLE … ENABLE ROW LEVEL SECURITY` of the table (or its `CREATE TABLE` if typedpg did not see one). A policy created in any later migration resolves it. When that statement is in a migration file unchanged from `--previous-deployment <ref>` (or matching `lint.ignore`), the finding stays there and the help line asks for the `CREATE POLICY` in a new migration. Tables from `extra_migrations` are not reported.

## Examples

### Bad

```sql
-- migrations/0001_init.sql
CREATE TABLE notes (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    owner text NOT NULL,
    body text NOT NULL
);
ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
CREATE TABLE secrets (id bigint PRIMARY KEY, value text NOT NULL);
ALTER TABLE secrets ENABLE ROW LEVEL SECURITY, FORCE ROW LEVEL SECURITY;
```

```text
info[rls_no_policy]: row-level security is enabled on public.notes, which has no policy: every role but its owner, superusers and BYPASSRLS roles sees no rows and can write none
   ╭─[migrations/0001_init.sql:6:1]
 6 │ ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
   · ───────────────────────────────────────────
   ╰────
  help: CREATE POLICY for the roles that use public.notes, or leave it as is if denying them is the intent

info[rls_no_policy]: row-level security is enabled on public.secrets, which has no policy: every role but superusers and BYPASSRLS roles sees no rows and can write none
   ╭─[migrations/0001_init.sql:8:1]
 8 │ ALTER TABLE secrets ENABLE ROW LEVEL SECURITY, FORCE ROW LEVEL SECURITY;
   · ───────────────────────────────────────────────────────────────────────
   ╰────
  help: CREATE POLICY for the roles that use public.secrets, or leave it as is if denying them is the intent
```

### Good

```sql
CREATE TABLE notes (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    owner text NOT NULL DEFAULT current_user,
    body text NOT NULL
);
ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
CREATE POLICY notes_owner ON notes USING (owner = current_user);
```

Also not reported: a table whose row-level security was enabled and then disabled again.

## When it stays silent

- The table has a policy, or row-level security is not enabled.
- The table comes from `extra_migrations`, or an allow comment is on the statement the finding is placed on.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
rls_no_policy = "warning"   # "error", "warning", "info" (default) or "off"
```

```json
// typedpg.config.json
{ "lint": { "rlsNoPolicy": "warning" } }
```

When denying everyone is the intent, say so on the statement:

```sql
-- typedpg-lint: allow(rls_no_policy)
ALTER TABLE secrets ENABLE ROW LEVEL SECURITY, FORCE ROW LEVEL SECURITY;
```

See [Configuring rules](README.md#configuring-rules) for `fail_on` / `failOn`, `ignore` and `--previous-deployment`.

## Related rules

- [`rls_policy_not_enforced`](rls_policy_not_enforced.md): policies without row-level security.
