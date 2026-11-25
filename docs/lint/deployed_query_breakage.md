# `deployed_query_breakage`

A new migration breaks a query of the code deployed now, which keeps running
while the migration is applied and until the new code has rolled out.

| Category | Level | Applies to | Fix |
|---|---|---|---|
| correctness | error | migrations, with `--previous-deployment <ref>` only | — |

## Example

```sh
cargo typedpg lint --previous-deployment v1.4.0   # or origin/main
npx typedpg lint --previous-deployment v1.4.0
```

```sql
-- migrations/0001_init.sql (at v1.4.0)
CREATE TYPE user_status AS ENUM ('active', 'blocked');
CREATE TABLE users (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, email text NOT NULL,
                    name varchar(80) NOT NULL, status user_status NOT NULL DEFAULT 'active');

-- queries.sql (at v1.4.0)
/* @name userEmail */
SELECT email, name FROM users WHERE id = $id;
/* @name userStatuses */
SELECT id, status FROM users;
/* @name renameUser */
UPDATE users SET name = $name WHERE id = $id;
/* @name blockUser */
UPDATE users SET name = 'anonymous', status = 'blocked' WHERE id = $id;

-- migrations/0002_users.sql (new; the branch also renames its own query's column)
SET LOCAL lock_timeout = '5s';
ALTER TABLE users RENAME COLUMN email TO email_address;
ALTER TYPE user_status ADD VALUE 'deleted';
ALTER TABLE users ALTER COLUMN name TYPE varchar(40);
ALTER TABLE users ADD CONSTRAINT users_status_active CHECK (status = 'active') NOT VALID;
```

```
error[correctness/deployed_query_breakage]: this migration breaks the deployed query at queries.sql:2 (v1.4.0): it no longer runs: column "email" does not exist
   ╭─[migrations/0002_users.sql:2:1]
 2 │ ALTER TABLE users RENAME COLUMN email TO email_address;
   · ──────────────────────────────────────────────────────
   ╰────
  help: the code deployed at v1.4.0 keeps running while the migration is applied and during the rollout: first deploy code that works with both schemas, then the migration (expand/contract)
```

The same run also reports:

```
error[correctness/deployed_query_breakage]: this migration breaks the deployed query at queries.sql:4 (v1.4.0): result column "status" may now be 'deleted', a value the deployed code does not know
error[correctness/deployed_query_breakage]: this migration breaks the deployed query at queries.sql:6 (v1.4.0): its UPDATE of users writes column name, now character varying(40) instead of character varying(80): a longer value fails
error[correctness/deployed_query_breakage]: this migration breaks the deployed query at queries.sql:8 (v1.4.0): its UPDATE of users writes column status, which CHECK constraint "users_status_active" now limits it to 'active': writing another value fails
```

Line 8 is not reported for `name`: `'anonymous'` fits a `varchar(40)`.
Other message endings, from similar runs:

```
result column "id" changes from bigint to integer
its result columns become (id, balance, email, plan) instead of (id, balance, email), and the deployed code checks them
parameter $id changes from bigint to integer
result column "name" may now be NULL
its INSERT into users leaves out column tenant_id, now NOT NULL without a default, so inserting any row fails
its INSERT into archive writes column other, now numeric(4,2) instead of numeric(10,2): a value with more than 2 digits before the decimal point fails
its UPDATE of active_accounts (a view of accounts) writes column balance, which CHECK constraint "balance_positive" now limits it to at least 0: writing another value fails
it no longer runs as role "app": permission denied for table audit
its INSERT into audit fails as role "app": row-level security applies to the role and no policy lets it INSERT
its INSERT into orders may fail as role "app": row-level security now checks the rows it writes against policies the deployed schema didn't, and a row they reject fails
its UPDATE of users keeps the stored value of column kind, which the new NOT VALID CHECK constraint "users_kind" reads: updating a row already stored that breaks it fails
its INSERT into users leaves out column kind, written its default, which CHECK constraint "users_kind" now limits it to 'b': writing another value fails
it calls function account_name(integer), a statement of whose body now fails: column "name" does not exist
its INSERT fires trigger "accounts_audit" on accounts (function audit_account()), a statement of whose body now fails: relation "audit" does not exist
its UPDATE fires trigger "accounts_ro" on accounts (function read_only()), which now fails for every row it writes: accounts are read-only
its INSERT fires trigger "no_long" on accounts (function no_long()), which may now raise an error: a row it rejects fails
its INSERT into orders may fail: foreign key "orders_customer_fk" now requires column customer_id to match a row of customers, and a row with no match fails
its DELETE from customers may fail: foreign key "orders_customer_fk" of orders (ON DELETE NO ACTION) now rejects deleting a row still referenced
```

## Why

A deploy runs the migrations, then replaces the code instance by instance.
Until then the old code runs against the new schema: a renamed column fails
every request; a new enum label or nullable column reaches code that can't
decode it. Expand/contract avoids it: add what the new code needs, deploy
code that works with both schemas, remove the old parts in a later deploy.

## How it runs

`<ref>` is what production runs, e.g. a release tag or `origin/main`
(formerly `--ignore-migrations-from`). Configuration, migrations and queries
(`sql!`, `copy_in!`, `sql()`, `copyIn()`, `.sql` files) are read at the ref,
so a query your branch changed or deleted is still checked. Migration files
identical at the ref are applied; the rest are new. Each deployed query is
compared between the schema at the ref and the schema after all new
migrations; the finding goes on the first new statement after which the
change shows (an `extra_migrations`-only change: line 1 of the first new
file).

## Reported when

Against the new schema, a deployed query:

- no longer analyzes; PostgreSQL's error is quoted;
- loses a result column, or one changes type (type modifiers and collations
  aside, and a domain added or removed over the same type when the type
  mapping — Rust `[types]`, TypeScript `types` — gives both sides the same
  type) or may now be `NULL` (also array elements and record fields);
- returns a different column list (`SELECT *` gaining a column);
- may return a value the code doesn't know: a new enum label, a `CHECK`
  admitting more values or a wider range (also in arrays and composites).
  A new value is reported where the code's type narrows to the old ones:
  in TypeScript, the generated literal union (`"active" | "blocked"`),
  which the runtime passes any text through unchecked; in Rust, a type
  `[types]` maps — an enum decoding into its Rust enum fails on the new
  label ("a label the deployed code's crate::Status does not have:
  decoding it fails"), a mapped domain may reject a new value;
- has a parameter (or `$..spread` field) that changes type, no longer
  accepts `NULL`, or no longer accepts an enum label;
- `INSERT`s (a `MERGE`'s `INSERT` action too) or `COPY`s without a column
  now `NOT NULL` with no default — a `NOT NULL` domain counts for an
  `INSERT`, not for a `COPY`, which doesn't check it on the NULL (an insert
  through a view uses the view's defaults first) —, or `COPY`s other
  columns (a `COPY` without a column list gains every new one);
- writes a column that now accepts fewer values (below);
- `UPDATE`s (also `ON CONFLICT DO UPDATE`, a `MERGE`'s `UPDATE`) a table
  with a new `NOT VALID` `CHECK` or `NOT NULL` reading a column it doesn't
  write: a row already stored that breaks it fails whatever is written;
- calls a function, or fires a trigger (its `INSERT`, `UPDATE`, `DELETE`,
  `MERGE`, `COPY`), with a statement in its body — a `LANGUAGE sql` string
  or PL/pgSQL, through the functions it calls too — that no longer
  analyzes: a table it reads renamed, a column dropped. PostgreSQL fails
  the statement when it runs;
- fires a trigger whose function may now raise an error: a new trigger, or
  a function replaced, with a `RAISE EXCEPTION`, `ASSERT`, `INTO STRICT` or
  dynamic `EXECUTE` the deployed one didn't have ("may raise"). One that
  raises before anything depending on the row (`BEGIN RAISE EXCEPTION
  '…'; END`) fails every row a plain `INSERT`, `UPDATE`, `DELETE` or `COPY`
  of its table writes, a statement trigger every run — under a `WHEN`, `ON
  CONFLICT`, a `MERGE` or a partition's trigger, it "may raise";
- writes a column a new foreign key covers (or one `NOT ENFORCED` at the
  ref): a row with no match fails ("may fail"). Not when every row's key is
  certainly `NULL` — a `NULL` written, a column left out without a default;
  one key column for `MATCH SIMPLE`, all for `MATCH FULL` — nor for an
  `UPDATE` not writing the key;
- `DELETE`s from a table, or `UPDATE`s its referenced columns, where a new
  foreign key (or one whose action changed) now rejects that for a row
  still referenced ("may fail"): `NO ACTION`, `RESTRICT`, `SET NULL` into a
  column that holds no `NULL`, `SET DEFAULT`, also reached through an `ON
  DELETE` / `ON UPDATE CASCADE`;
- runs as a role (`role = "…"` at the call site, or the database's `roles`)
  that lost a privilege, or that row-level security now denies: enabled
  with no policy for the role, a policy dropped, or `USING (false)`; or
  writes a table (directly) whose row-level security now checks the rows
  it writes against more: newly enabled, a new or changed restrictive
  policy, a permissive one dropped or changed — unless a permissive policy
  and every restrictive one are certainly true.

A write (`INSERT`, `ON CONFLICT DO UPDATE`, `UPDATE`, `MERGE`, `COPY`)
breaks on a shorter `varchar(n)` / `char(n)` / `varbit(n)`, another
`bit(n)`, a `numeric` with fewer digits (before the point a value fails,
after it one is rounded), a new `NOT NULL` (the column's or its domain's),
or a new or narrower `CHECK` of the table or the column's domain (`NOT
VALID` included), depending on the value written — a column an insert
leaves out, or `SET c = DEFAULT`, writes its default (NULL without one),
checked against every constraint when the migration changes the default:

- a parameter or a copied field: always reported;
- a constant, a `CASE` of constants, or an `INSERT … SELECT` column whose
  type or `CHECK`s bound it: reported only if it may not fit, compared as the
  column stores it (`'2021-05-05'` as a `date`). A `numeric(10,2)` column
  with `CHECK (amount BETWEEN 0 AND 99.99)` fits a `numeric(4,2)`;
- a `CHECK` over several columns or functions (`length(name) > 1`,
  `lo <= hi`) is evaluated over the constants each row writes; a column left
  to its stored or default value, a parameter or a `BEFORE ROW` trigger
  makes it "may reject";
- a default: as a constant (`DEFAULT 'a'` meets `CHECK (kind = 'b')`), and
  for `SET c = DEFAULT` also when the column becomes `NOT NULL` without a
  default, or loses its default while `NOT NULL`.

A write or a `DELETE` through an automatically updatable view is checked
against the table below it — its constraints, triggers and the foreign keys
referencing it — unless an `INSTEAD OF` trigger or `DO INSTEAD` rule for
that command takes it over (then only the view's own columns are checked).

## Not reported

- No new migration; a deployed query already broken at the ref.
- Changes the deployed code doesn't see: a new nullable column it doesn't
  select, a new table or index, a parameter that accepts more values (a
  deployed `UPDATE users SET name = $name` after `DROP NOT NULL`).
- A narrower column the value written certainly fits; a `CHECK` renamed, or
  dropped and re-added with the same expression; a `CHECK` that is true or
  `NULL` for the constants every row writes.
- A new `CHECK` the deployed schema's `CHECK`s imply, letting through every
  row they did: `kind IN ('a', 'b')` widened to `kind IN ('a', 'b', 'c')`,
  or a `CASE` over one column widened by a branch —
  `CASE type WHEN 'a' THEN payload ? 'x' ELSE false END` to the same with
  `WHEN 'b' THEN true` — or with a branch's body made `true`. The `CASE`s are
  compared value by value (simple or searched `WHEN type = 'a'`, constants
  of a type whose equal values print the same): each branch the old one
  takes must imply the new one's, equal or an old `false`. An `AND` that
  lost a term, an `OR` that gained one, or either reordered implies the
  same, at any depth (`content IS NOT NULL AND result IS NULL` to
  `content IS NOT NULL`).
- In Rust, a new enum label or `CHECK` value of a column the code decodes
  into a `String` (an enum or domain `[types]` doesn't map, a `text`): any
  value decodes.
- A new `UNIQUE`, primary key or exclusion constraint (depends on data); a
  timestamp, time or interval precision change (rounds); a length or
  precision change of a column only read.
- A ref with no typedpg configuration, or whose migrations don't build.
- Of a body, what only running it shows: a value it computes failing a
  cast, the constraints of what it writes, a dynamic `EXECUTE`'s text; a new
  trigger with nothing that raises (`NEW.updated_at := now()`). A `BEGIN
  ATOMIC` body names its objects when created: a migration can't break it
  without dropping it, which the call then reports.
- A foreign key validated (`VALIDATE CONSTRAINT`: a `NOT VALID` one already
  checks every row written), one whose `CASCADE`, or `SET NULL` into a
  nullable column, deletes or changes referencing rows (the statement
  runs).
- Row-level security narrowing the rows a read or an `UPDATE` sees (short of
  denying all of them), and the policies of a table written through a view
  (the view owner's).
- SQL built at run time, other services, ad-hoc scripts.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
deployed_query_breakage = "error"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "deployedQueryBreakage": "error" } }
```

Accept a break (the code using the column is gone everywhere) with
`-- typedpg-lint: allow(deployed_query_breakage)` before the statement in
the migration file, or in the deployed query's SQL at the ref.
`--previous-deployment` also makes migration files unchanged at the ref skip
the statement rules. See [Configuring rules](README.md#configuring-rules)
and [`--previous-deployment`](README.md#--previous-deployment-ref).

## Related

- [`alter_table_rewrite`](alter_table_rewrite.md): the lock an `ALTER COLUMN … TYPE` holds.
- [`check_violation`](check_violation.md): a current query writing a constant a `CHECK` rejects.
- [`rls_without_policy`](rls_without_policy.md): row-level security with no policy.
