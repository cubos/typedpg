# `rls_policy_not_enforced`

Row-level security policies on a table whose row-level security is not enabled: PostgreSQL ignores them, and every role with table privileges sees and writes every row.

| | |
|---|---|
| Default level | error |
| Applies to | the final schema |
| Fix | no |

## Why

`CREATE POLICY` does nothing on its own. Policies are only applied once `ALTER TABLE … ENABLE ROW LEVEL SECURITY` has been run on the table; until then PostgreSQL accepts and stores them silently. A migration that creates a careful `USING (owner = current_user)` policy and forgets the `ALTER TABLE` looks secure in review, passes every test that doesn't check isolation, and exposes every tenant's rows in production. That is why this rule is an error by default.

## What it reports

Every ordinary or partitioned table of the final schema that has at least one policy and does not have row-level security enabled. One finding per table names all its policies. It is placed on the `CREATE POLICY` of the table's first policy (or, if typedpg did not see that statement, the table's `CREATE TABLE`), so enabling row-level security in any later migration resolves it, and disabling it in a later migration brings it back.

### Placement and `--previous-deployment`

A schema rule: it reads the catalog after the last migration. When the `CREATE POLICY` it is placed on is in a migration file unchanged from `--previous-deployment <ref>` (or matching `lint.ignore`), it is still reported there, with the help line `ALTER TABLE public.notes ENABLE ROW LEVEL SECURITY, in a new migration`. Tables and policies created by `extra_migrations` are not reported.

## Examples

### Bad

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

```text
error[rls_policy_not_enforced]: policies notes_owner, notes_read on public.notes do nothing: row-level security is not enabled on the table
   ╭─[migrations/0001_init.sql:6:1]
 6 │ CREATE POLICY notes_owner ON notes USING (owner = current_user);
   · ───────────────────────────────────────────────────────────────
   ╰────
  help: ALTER TABLE public.notes ENABLE ROW LEVEL SECURITY
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

The table's owner bypasses its policies unless `FORCE ROW LEVEL SECURITY` is also set; if your application connects as the owner, add it.

## When it stays silent

- Row-level security is enabled on the table.
- The table has no policy (then [`rls_no_policy`](rls_no_policy.md) may apply instead).
- The policy and table come from `extra_migrations`, or an allow comment is on the statement the finding is placed on.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
rls_policy_not_enforced = "warning"   # "error" (default), "warning", "info" or "off"
```

```json
// typedpg.config.json
{ "lint": { "rlsPolicyNotEnforced": "warning" } }
```

To accept it (policies prepared ahead of enabling row-level security in a later deploy), put the allow comment on the first policy's `CREATE POLICY`:

```sql
-- typedpg-lint: allow(rls_policy_not_enforced)
CREATE POLICY notes_owner ON notes USING (owner = current_user);
```

See [Configuring rules](README.md#configuring-rules) for `fail_on` / `failOn`, `ignore` and `--previous-deployment`.

## Related rules

- [`rls_no_policy`](rls_no_policy.md): the opposite mistake.
- [`security_definer_search_path`](security_definer_search_path.md): the other privilege rule.
