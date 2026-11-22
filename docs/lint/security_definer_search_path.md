# `security_definer_search_path`

A `SECURITY DEFINER` function or procedure without `SET search_path`: it runs with its owner's privileges, but resolves table, function and operator names along the caller's `search_path`.

| | |
|---|---|
| Default level | warning |
| Applies to | the final schema |
| Fix | no |

## Why

A `SECURITY DEFINER` routine runs as the role that owns it, typically to let less privileged roles do one specific privileged thing. But a body written as text (`AS $$ … $$`, in `sql` or `plpgsql`) looks up unqualified names when it runs, using the `search_path` of the session that called it. A caller who can create objects in any schema on its own `search_path` — or in `pg_temp`, which every session can write to — can create a table, function or operator with the same name as one the body uses, and the body then runs the caller's object with the owner's privileges. This is a privilege escalation, and PostgreSQL's documentation for `SECURITY DEFINER` warns about exactly this.

Pinning the routine's `search_path` (`SET search_path = pg_catalog, pg_temp`, with `pg_temp` last) and schema-qualifying the objects it uses closes the hole.

## What it reports

Every function and procedure created by your migrations that is `SECURITY DEFINER`, has no `search_path` setting of its own (no `SET search_path` clause, neither at `CREATE` nor added later with `ALTER FUNCTION … SET search_path`), and has a body resolved at run time. A SQL-standard body (`BEGIN ATOMIC … END` or `RETURN …`) is resolved when the routine is created, so it is not reported.

The rule only checks that a `search_path` is set; it does not judge the value.

### Placement and `--previous-deployment`

A schema rule: it reads the routines after the last migration and reports on the last `CREATE [OR REPLACE] FUNCTION` / `PROCEDURE` that defined the routine — an `ALTER FUNCTION … SET search_path` in a later migration resolves it, and a `CREATE OR REPLACE … SECURITY DEFINER` of an existing function is reported on the replacing statement. When that statement is in a migration file unchanged from `--previous-deployment <ref>` (or matching `lint.ignore`), the finding stays there with the help line `in a new migration, ALTER FUNCTION public.count_users SET search_path = pg_catalog, pg_temp, and qualify the objects it uses with their schema`. Routines from `extra_migrations` are not reported.

## Examples

### Bad

```sql
-- migrations/0001_init.sql
CREATE TABLE users (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    email text NOT NULL,
    name text NOT NULL
);
CREATE FUNCTION count_users() RETURNS bigint
    SECURITY DEFINER
    LANGUAGE sql
    AS $$ SELECT count(*) FROM users $$;
CREATE PROCEDURE touch_users() SECURITY DEFINER
    LANGUAGE plpgsql AS $$ BEGIN UPDATE users SET name = name; END $$;
```

```text
warning[security_definer_search_path]: SECURITY DEFINER function public.count_users has no SET search_path: it runs with its owner's privileges but resolves names along the caller's search_path, where the caller can shadow them
   ╭─[migrations/0001_init.sql:6:1]
 6 │ CREATE FUNCTION count_users() RETURNS bigint
   · ────────────────────────────────────────────
 7 │     SECURITY DEFINER
   · ────────────────────
 8 │     LANGUAGE sql
   · ────────────────
 9 │     AS $$ SELECT count(*) FROM users $$;
   · ───────────────────────────────────────
   ╰────
  help: add SET search_path = pg_catalog, pg_temp to the function, and qualify the objects it uses with their schema

warning[security_definer_search_path]: SECURITY DEFINER procedure public.touch_users has no SET search_path: it runs with its owner's privileges but resolves names along the caller's search_path, where the caller can shadow them
    ╭─[migrations/0001_init.sql:10:1]
 10 │ CREATE PROCEDURE touch_users() SECURITY DEFINER
    · ───────────────────────────────────────────────
 11 │     LANGUAGE plpgsql AS $$ BEGIN UPDATE users SET name = name; END $$;
    · ─────────────────────────────────────────────────────────────────────
    ╰────
   help: add SET search_path = pg_catalog, pg_temp to the procedure, and qualify the objects it uses with their schema
```

### Good

```sql
-- Pinned search_path, qualified table.
CREATE FUNCTION count_users() RETURNS bigint
    SECURITY DEFINER
    SET search_path = pg_catalog, pg_temp
    LANGUAGE sql
    AS $$ SELECT count(*) FROM public.users $$;
-- A SQL-standard body is resolved at CREATE.
CREATE FUNCTION count_users_atomic() RETURNS bigint
    SECURITY DEFINER
    LANGUAGE sql
    BEGIN ATOMIC SELECT count(*) FROM users; END;
-- SECURITY INVOKER (the default) runs with the caller's own privileges.
CREATE FUNCTION count_users_invoker() RETURNS bigint
    LANGUAGE sql
    AS $$ SELECT count(*) FROM users $$;
-- A search_path set afterwards counts.
CREATE FUNCTION count_users_later() RETURNS bigint
    SECURITY DEFINER
    LANGUAGE sql
    AS $$ SELECT count(*) FROM public.users $$;
ALTER FUNCTION count_users_later() SET search_path = pg_catalog, pg_temp;
```

## When it stays silent

- The routine is not `SECURITY DEFINER`, sets any `search_path`, or has a SQL-standard body.
- The routine comes from `extra_migrations`, or an allow comment is on the statement that defined it.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
security_definer_search_path = "error"   # "error", "warning" (default), "info" or "off"
```

```json
// typedpg.config.json
{ "lint": { "securityDefinerSearchPath": "error" } }
```

For one routine, put the allow comment on its `CREATE FUNCTION`:

```sql
-- typedpg-lint: allow(security_definer_search_path)
CREATE FUNCTION count_users() RETURNS bigint
    SECURITY DEFINER
    LANGUAGE sql
    AS $$ SELECT count(*) FROM users $$;
```

See [Configuring rules](README.md#configuring-rules) for `fail_on` / `failOn`, `ignore` and `--previous-deployment`.

## Related rules

- [`rls_policy_not_enforced`](rls_policy_not_enforced.md), [`rls_no_policy`](rls_no_policy.md): the other privilege rules.
