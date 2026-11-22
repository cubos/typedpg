# `security_definer_search_path`

A `SECURITY DEFINER` function or procedure without `SET search_path`.

| Level | Applies to | Fix |
|---|---|---|
| warning | final schema | — |

## Example

```sql
-- migrations/0001_init.sql
CREATE TABLE users (id bigint PRIMARY KEY, name text NOT NULL);
CREATE FUNCTION count_users() RETURNS bigint
    SECURITY DEFINER LANGUAGE sql
    AS $$ SELECT count(*) FROM users $$;
```

```
warning[security_definer_search_path]: SECURITY DEFINER function public.count_users has no SET search_path: it runs with its owner's privileges but resolves names along the caller's search_path, where the caller can shadow them
   ╭─[migrations/0001_init.sql:2:1]
 2 │ CREATE FUNCTION count_users() RETURNS bigint
   · ────────────────────────────────────────────
 3 │     SECURITY DEFINER LANGUAGE sql
   · ─────────────────────────────────
 4 │     AS $$ SELECT count(*) FROM users $$;
   · ───────────────────────────────────────
   ╰────
  help: add SET search_path = pg_catalog, pg_temp to the function, and qualify the objects it uses with their schema
```

## Why

The routine runs with its owner's privileges, but a `$$ … $$` body looks up
unqualified names along the caller's `search_path`. A caller who can create
objects in a schema on that path (or in `pg_temp`, which every session can)
can shadow `users` or a function the body calls, and have it run as the
owner.

Pin the path with `SET search_path = pg_catalog, pg_temp` (`pg_temp` last)
and schema-qualify the objects the body uses.

## Reported when

- The routine is `SECURITY DEFINER`, its body is a string (`AS $$ … $$`, in
  any language), and it has no `search_path` setting of its own after the
  last migration.
- The finding is placed on the last `CREATE [OR REPLACE]` that defined the
  routine. A later `ALTER FUNCTION … SET search_path` clears it.
- When that statement is in a migration already deployed (unchanged from
  `--previous-deployment`) or ignored, the help suggests an
  `ALTER FUNCTION … SET search_path` in a new migration.

## Not reported

- Any `search_path` is set, at `CREATE` or with a later `ALTER`. The value is
  not checked.
- A SQL-standard body, resolved at `CREATE`:
  `LANGUAGE sql BEGIN ATOMIC SELECT count(*) FROM users; END`.
- `SECURITY INVOKER` routines (the default).
- Routines from `extra_migrations`.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
security_definer_search_path = "error"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "securityDefinerSearchPath": "error" } }
```

Allow one routine with `-- typedpg-lint: allow(security_definer_search_path)`
before its `CREATE FUNCTION` or `CREATE PROCEDURE`. See
[Configuring rules](README.md#configuring-rules).

## Related

- [`rls_policy_not_enforced`](rls_policy_not_enforced.md): policies on a
  table without row-level security.
- [`rls_no_policy`](rls_no_policy.md): row-level security with no policy.
