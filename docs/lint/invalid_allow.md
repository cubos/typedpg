# `invalid_allow`

A `typedpg-lint:` comment that is malformed or names a rule that doesn't exist.

| | |
|---|---|
| Default level | error (not configurable) |
| Applies to | queries, migrations |
| Fix | no |

## Why

An allow comment is a promise that a finding was looked at and accepted. A typo in it (`allow(unbounded_writes)`) would otherwise suppress nothing while looking like it does, and the finding it was meant for would reappear — or worse, a later reader would trust the comment. typedpg treats the comment as code and rejects what it can't read.

## What it reports

Any SQL comment (`--` or `/* … */`) whose text, after the comment marker and whitespace, starts with `typedpg-lint:`, when it is not exactly `typedpg-lint: allow(rule, …)` with nothing after the closing parenthesis, or when a name in the list is not a configurable rule. One finding per unknown name; the message lists every valid name.

Valid names in the same comment still suppress their rules. An unknown name suppresses nothing.

The comments are validated in queries and in migration files, including files that only run the schema rules (unchanged from `--previous-deployment`, or matching `lint.ignore`). Files from `extra_migrations` are not linted at all.

## Examples

### Bad

```sql
-- migrations/0002_x.sql
-- typedpg-lint: allow(create_index_blocking, missing_lock_timeot)
CREATE INDEX posts_title_idx ON posts (title);
-- typedpg-lint: allow create_index_blocking
CREATE INDEX posts_views_idx ON posts (views);
```

```text
error[invalid_allow]: unknown lint rule `missing_lock_timeot` in a typedpg-lint allow comment: expected one of null_comparison, unbounded_write, not_in_nullable, create_index_blocking, constraint_validating, set_not_null_scan, table_rewrite, fk_without_index, redundant_index, redundant_binding, renaming_binding, redundant_null_check, limit_without_order, redundant_limit, redundant_distinct, constant_condition, unreachable_case_branch, case_without_else, redundant_cast, redundant_alias, outer_reference_in_subquery, cartesian_join, insert_without_columns, self_assignment, unused_cte, simplifiable_case, like_unescaped_param, redundant_join, scalar_subquery_may_return_many, timestamp_timezone_mix, integer_division, left_join_nullified, check_violation, unstable_order, fk_type_mismatch, missing_lock_timeout, drop_index_blocking, blocking_rebuild, detach_partition_blocking, validate_in_same_transaction, attach_partition_scan, missing_primary_key, rls_policy_not_enforced, rls_no_policy, security_definer_search_path, prefer_timestamptz, prefer_identity, prefer_text, prefer_bigint_pk, prefer_jsonb, fetch_may_return_many, fetch_all_single_row, unused_returning, breaks_deployed_queries
   ╭─[migrations/0002_x.sql:1:1]
 1 │ -- typedpg-lint: allow(create_index_blocking, missing_lock_timeot)
   · ──────────────────────────────────────────────────────────────────
   ╰────

error[invalid_allow]: malformed typedpg-lint comment: expected `typedpg-lint: allow(rule, ...)` and nothing after it
   ╭─[migrations/0002_x.sql:3:1]
 3 │ -- typedpg-lint: allow create_index_blocking
   · ────────────────────────────────────────────
   ╰────
```

The first comment still suppresses `create_index_blocking` on its statement, but not `missing_lock_timeout`, which is reported there; the second suppresses nothing, so the second `CREATE INDEX` is reported by `create_index_blocking`.

### Good

```sql
-- typedpg-lint: allow(create_index_blocking, missing_lock_timeout)
CREATE INDEX posts_title_idx ON posts (title);
/* typedpg-lint: allow(create_index_blocking, missing_lock_timeout) */
CREATE INDEX posts_views_idx ON posts (views);
```

A comment that doesn't start with `typedpg-lint:` is an ordinary comment, and a `typedpg-lint:` inside a string literal is not a comment.

## Configuration

None. `invalid_allow` can't be given a level in `[package.metadata.typedpg.lint]` or `typedpg.config.json` (the key is rejected as unknown), can't be suppressed by an allow comment, and is always an error, so it always fails the run whatever `fail_on` / `failOn` says. Fix the comment.

See [Configuring rules](README.md#configuring-rules) for how allow comments work.

## Related rules

- [`internal_error`](internal_error.md): the other finding that can't be configured.
