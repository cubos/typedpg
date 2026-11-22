# `internal_error`

A typedpg bug: a rule's `--fix` would have changed what the query means, so
the fix was withheld.

| Level | Applies to | Fix |
|---|---|---|
| warning, always | queries, call sites | — |

## What it means

Before a fix is offered, typedpg applies it and checks the edited query
still analyzes with the same output columns (names, types, nullability,
known values), the same parameters, and the same ability to run as a
subquery. A fix that fails is dropped: the original finding is reported
without it, and this warning is added next to it.

The message reads `typedpg bug: the <rule> fix would have changed <what>,
so it was not offered`, where `<what>` is one of:

- `the query text (overlapping edits)`;
- `the query into one that doesn't analyze`;
- `its output columns`;
- `its parameters`;
- `whether it can run as a subquery`.

Its help line asks for a report at
<https://github.com/cubos/typedpg/issues>.

No valid query triggers it on purpose, so this page has no example output.

## What to do

- Report it with the query and the schema it runs against.
- Your query is unchanged; fix the original finding by hand, or allow it as
  usual.

`internal_error` can't be configured or allowed. As a warning, it fails the
run only with `fail_on` / `failOn` at `"warning"` or `"info"`.
See [`--fix`](README.md#--fix).

## Related

- [`invalid_allow`](invalid_allow.md): the other finding that can't be
  configured.
