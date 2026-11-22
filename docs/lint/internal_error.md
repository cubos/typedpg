# `internal_error`

A typedpg bug: a rule offered a fix that would have changed the query, so the fix was withheld.

| | |
|---|---|
| Default level | warning (not configurable) |
| Applies to | queries, call sites |
| Fix | no — it reports a fix that was withheld |

## Why

Every SQL fix a rule offers (`--fix`) comes with that rule's own proof that it keeps the query's meaning. Under that proof sits a second, independent check: before the fix is offered, typedpg applies it and analyzes the edited query again. It must still analyze, and keep the same output columns — names, types, nullability and the values they are known to take, which the generated types show — the same parameters (name, type, nullability), and whether it can run as a subquery. (For [`unused_returning`](unused_returning.md), whose fix removes a `RETURNING`, the edited query must instead have no output columns.)

A fix that fails this check means a rule's proof was wrong. Rather than rewrite your query into something else, typedpg drops the fix, keeps the original finding without it, and adds this warning so the bug gets reported.

## What it reports

A finding whose fix failed the check above. The message is `typedpg bug: the <rule> fix would have changed <what>, so it was not offered`, where `<what>` is `the query text (overlapping edits)`, `the query into one that doesn't analyze`, `its output columns`, `its parameters` or `whether it can run as a subquery`, and the help line is `please report it at https://github.com/cubos/typedpg/issues with this query`.

It is placed where the original finding is: in the query text, or at the call site for a call-site rule's fix. The check runs on query fixes only; migration findings never carry fixes.

## Examples

There is no query that triggers it on purpose: each occurrence is a bug to fix in typedpg. If you see one, report it with the query (and the schema it runs against).

## Configuration

None. `internal_error` can't be given a level or turned off in either config, and can't be suppressed by an allow comment. It is a warning, so it fails the run only with `fail_on = "warning"` / `"failOn": "warning"` or lower. The original finding is reported as usual, and can be configured or allowed as usual.

See [Configuring rules](README.md#configuring-rules).

## Related rules

- [`invalid_allow`](invalid_allow.md): the other finding that can't be configured.
