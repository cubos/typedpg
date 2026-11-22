# `invalid_allow`

A `typedpg-lint:` comment that is malformed or names an unknown rule.

| Level | Applies to | Fix |
|---|---|---|
| error, always | queries, migrations | — |

## Example

```sql
-- migrations/0002_indexes.sql
-- typedpg-lint: allow create_index_blocking
CREATE INDEX posts_views_idx ON posts (views);

-- queries.sql
-- typedpg-lint: allow(unbounded_writes)
DELETE FROM posts;
```

```
error[invalid_allow]: malformed typedpg-lint comment: expected `typedpg-lint: allow(rule, ...)` and nothing after it
   ╭─[migrations/0002_indexes.sql:1:1]
 1 │ -- typedpg-lint: allow create_index_blocking
   · ────────────────────────────────────────────
   ╰────

error[invalid_allow]: unknown lint rule `unbounded_writes` in a typedpg-lint allow comment: expected one of null_comparison, unbounded_write, not_in_nullable, create_index_blocking, constraint_validating, set_not_null_scan, table_rewrite, fk_without_index, redundant_index, redundant_binding, renaming_binding, redundant_null_check, limit_without_order, redundant_limit, redundant_distinct, constant_condition, unreachable_case_branch, case_without_else, redundant_cast, redundant_alias, outer_reference_in_subquery, cartesian_join, insert_without_columns, self_assignment, unused_cte, simplifiable_case, like_unescaped_param, redundant_join, scalar_subquery_may_return_many, timestamp_timezone_mix, integer_division, left_join_nullified, check_violation, unstable_order, redundant_order_by, redundant_group_by, count_not_null, non_sargable_predicate, float_equality, fk_type_mismatch, missing_lock_timeout, drop_index_blocking, blocking_rebuild, detach_partition_blocking, validate_in_same_transaction, attach_partition_scan, missing_primary_key, rls_policy_not_enforced, rls_no_policy, security_definer_search_path, prefer_timestamptz, prefer_identity, prefer_text, prefer_bigint_pk, prefer_jsonb, fetch_may_return_many, fetch_all_single_row, unused_returning, breaks_deployed_queries
   ╭─[queries.sql:2:1]
 2 │ -- typedpg-lint: allow(unbounded_writes)
   · ────────────────────────────────────────
   ╰────
```

Neither comment suppresses anything: `unbounded_write` and
`create_index_blocking` still report their statements.

## Why

A typo in an allow comment suppresses nothing while looking like it does.
typedpg rejects what it can't read instead of ignoring it.

## Reported when

A `--` or `/* */` comment starting with `typedpg-lint:`:

- is not exactly `typedpg-lint: allow(rule, …)` with nothing after `)`;
- or names something that is not a configurable rule (one finding per
  name). The valid names in the same comment still apply.

Checked in queries and in every migration file, including files unchanged
since `--previous-deployment` or matched by `ignore`; not in
`extra_migrations`.

## Not reported

- Ordinary comments, and `typedpg-lint:` inside a string literal.

## What to do

Fix the comment. The finding can't be configured (an `invalidAllow` /
`invalid_allow` key is rejected), can't be allowed, and always fails the
run, whatever `fail_on` says.
See [Allow comments](README.md#allow-comments).

## Related

- [`internal_error`](internal_error.md): the other finding that can't be
  configured.
