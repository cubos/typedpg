# `policy_without_rls`

Row-level security policies on a table that does not enable row-level
security, so PostgreSQL ignores them.

| Category | Level | Applies to | Fix |
|---|---|---|---|
| security | error | final schema | — |

## Example

```sql
-- migrations/0001_init.sql
CREATE TABLE notes (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    owner text NOT NULL DEFAULT current_user,
    body text NOT NULL
);
CREATE POLICY notes_owner ON notes USING (owner = current_user);
CREATE POLICY notes_read ON notes FOR SELECT USING (true);
```

```
error[security/policy_without_rls]: policies notes_owner, notes_read on public.notes do nothing: row-level security is not enabled on the table
   ╭─[migrations/0001_init.sql:6:1]
 6 │ CREATE POLICY notes_owner ON notes USING (owner = current_user);
   · ───────────────────────────────────────────────────────────────
   ╰────
  help: ALTER TABLE public.notes ENABLE ROW LEVEL SECURITY
```

## Why

`CREATE POLICY` is accepted and stored, but has no effect until
`ALTER TABLE … ENABLE ROW LEVEL SECURITY`. The migration looks secure in
review while every role with table privileges reads and writes every row.

## Reported when

- One finding per table, naming all its policies, placed on the table's
  first `CREATE POLICY`.
- The check runs on the schema after the last migration: enabling row-level
  security in a later migration clears the finding, disabling it brings the
  finding back.
- When the statement is in a migration already deployed (unchanged from
  `--previous-deployment`) or ignored, the help says to enable it in a new
  migration.

## Not reported

- Row-level security is enabled. Note that the table owner still bypasses
  the policies unless `FORCE ROW LEVEL SECURITY` is also set.
- The table has no policy (see [`rls_without_policy`](rls_without_policy.md)).
- Tables and policies from `extra_migrations`.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
policy_without_rls = "warning"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "policyWithoutRls": "warning" } }
```

Allow one table with `-- typedpg-lint: allow(policy_without_rls)`
before its first `CREATE POLICY`, e.g. when row-level security is enabled in
a later deploy. See [Configuring rules](README.md#configuring-rules).

## Related

- [`rls_without_policy`](rls_without_policy.md): row-level security enabled with no
  policy.
- [`security_definer_without_search_path`](security_definer_without_search_path.md):
  `SECURITY DEFINER` without `SET search_path`.
