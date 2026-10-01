# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Test Commands

Always use `cargo nextest run --release` instead of `cargo test` — nextest
aggregates results across test binaries into a single summary, and `--release`
runs *much* faster end-to-end on this workspace (the analyzer's DDL/query test
suite is heavy on parsing + interpretation; the optimized binary saves more
than the extra build time costs).

```bash
cargo build                                          # build all crates
cargo build -p typedpg                             # build specific crate
cargo nextest run --release                          # all tests (no Docker needed)
cargo nextest run --release -p typedpg_core        # core crate only
cargo nextest run --release -p typedpg_macros      # macro crate only
cargo nextest run --release -p typedpg_analyzer    # analyzer crate only
cargo nextest run --release -p typedpg             # runtime crate only
cargo nextest run --release --test migrate_integration  # integration tests (requires Docker)
cargo nextest run --release test_name                # run a single test by name
```

All compile-time tests run without Docker. Integration tests for the runtime migration runner use `testcontainers` and require a running Docker daemon.

Note: doctests are not supported by nextest — for those, fall back to `cargo test --doc`.

### Compile-fail snapshots

Every compile-time error the macros report is pinned by
`typedpg_compile_fail`: each binary under `typedpg_compile_fail/fixture/src/bin/`
is one case, and its rendered errors must equal
`fixture/expected/<case>.stderr` (`pass_*` cases must compile). After a
deliberate wording change, re-bless and review the diff:

```bash
cargo nextest run --release -p typedpg_compile_fail           # check
BLESS=1 cargo nextest run --release -p typedpg_compile_fail   # rewrite snapshots
```

## Regenerating `seed.json`

Never hand-migrate `typedpg_analyzer/src/seed.json` (e.g. with a Python
script) when changing catalog struct shapes. The seed loader tolerates empty
or stale seeds — `typedpg_seed` exports the catalog from a live PG via
testcontainers and overwrites the file. Always regenerate by running:

```bash
cargo run -p typedpg_seed   # requires Docker; takes ~10 seconds
```

Hand-rewriting the seed risks subtle FK drift (e.g. an old aggfinaltype
field repurposed as aggfinalfn would store pg_type oids where pg_proc oids
are expected).

## Architecture

Workspace with crates:

```
typedpg_cli (binary: `cargo typedpg migrate up/down/status/create`)
    └── typedpg (runtime: Pool, Executor, migrate)
            ├── typedpg_core (shared config only — kept small so runtime does not pull pg_query)
            └── typedpg_macros (proc macro: sql!)
                    ├── typedpg_core
                    └── typedpg_analyzer (compile-time only: lexer, param types, query_info, type_map, static SQL analyzer)
                            └── typedpg_pg_query (PostgreSQL parser: our libpg_query binding)
```

`typedpg_pg_query` compiles libpg_query from a git submodule pinned to a
release tag (`git submodule update --init` after cloning). Its AST types and
walkers are generated from libpg_query's `pg_query.proto` by
`typedpg_pg_query_codegen` (`cargo run -p typedpg_pg_query_codegen`), and a
few fixes to libpg_query's sources are applied at build time from
`typedpg_pg_query/patches.rs`. See `typedpg_pg_query/README.md` for moving to
a new PostgreSQL release.

### Compile-time pipeline (`sql!` macro)

1. Parse macro input: `sql!(executor, "SQL with $params", name = value)`
2. Lex SQL via `typedpg_analyzer::lexer::lex()` — rewrites `$name` → `$1`, extracts `$..spread`
3. Load config from `[package.metadata.typedpg]` in consumer's `Cargo.toml`
4. Build schema snapshot from seed + migrations via DDL interpreter (in-memory, no Docker)
5. Static analysis: parse SQL with `typedpg_pg_query` (libpg_query), resolve types and nullability against snapshot
6. `codegen::generate()` — emit anonymous output struct, typed query builder, `.fetch_all()/.fetch_one()/.fetch_optional()/.execute()` methods

### Runtime

- `Pool` wraps `deadpool-postgres`, constructed from a connection URL
- `Executor` trait implemented for `Pool`, `&Pool`, `Client`, `Transaction<'_>`
- Migration runner uses advisory locks (`pg_advisory_lock`) and per-migration transactions (opt-out via `-- no-transaction` first line)

## Configuration

Users configure via `[package.metadata.typedpg]` in their `Cargo.toml`:

```toml
[package.metadata.typedpg.database]
migrations = "./migrations"

[package.metadata.typedpg.migrations]
table = "public._migrations"
lock_id = 713705
use_transaction = true

# Single unified type map. The `sql!` macro infers the (de)serialization
# strategy from each PG type's kind: JSONB domain, enum, composite, or scalar.
[package.metadata.typedpg.types]
user_preferences = "crate::domains::UserPreferences"  # JSONB domain
post_status = "crate::PostStatus"                     # enum
"public.address" = "crate::Address"                   # composite type
```

## Implementation Status

The `sql!` macro is wired end-to-end with static analysis (no Docker needed at compile time). For high-level context see `PROJECT_GOAL.md` and `README.md`.

## Differential testing (`pg_sanity`) & the error-message contract

The `pg_sanity` feature mirrors every `apply_sql` / `analyze` onto a real
PostgreSQL and asserts they agree (see `typedpg_analyzer/src/pg_sanity.rs`,
run via `scripts/run-pg-sanity.sh`). A differential fuzzer
(`typedpg_analyzer/tests/fuzz.rs`, `#[ignore]`d) generates queries to surface
new disagreements automatically.

**Nullability soundness.** Describe says nothing about nullability, so the
mirror also *executes* every accepted query, in a rolled-back transaction,
over adversarial rows it seeds into every table (NULL in each nullable
column, a second fully-filled row, foreign keys leaving parents unmatched)
and fails if a value inferred NOT NULL — a column, a `Some(false)` array
element, a non-nullable record field — comes back NULL
(`typedpg_analyzer/src/pg_sanity/soundness.rs`). A divergence there is a
soundness bug in the analyzer: fix the inference, never the expectation.

**Error-message contract — single-error fidelity only.** When the analyzer
rejects a query, its message must *start with* PG's server-side message
verbatim (extra trailing detail / hints are fine). This contract applies to
queries with a **single** error: there, the analyzer must report the *same*
error PG would.

**SQLSTATE contract.** `AnalyzeError` variants map 1:1 to PG error codes via
`AnalyzeError::sqlstate()` (a pure variant → code mapping — never derive a
code from message text). When it returns `Some`, the oracle also asserts the
code matches the live server's `DbError::code()`. New PG-verbatim wordings
go through a `pgmsg` constructor that picks the variant carrying the right
code; multi-code buckets (`Invalid`, `InvalidLiteral`, `TypeMismatch`)
return `None` and are compared on wording only.

**Errors that every execution raises.** A query PG prepares but can never
execute successfully (e.g. `INSERT INTO` a materialized view: PREPARE
succeeds, `CheckValidResultRel` fails every execution, whatever the rows)
is rejected at compile time with the error the execution raises. That is
stricter than PREPARE, and correct: the oracle's execute fallback runs such
a query and compares against the runtime error.

One deliberate exception goes further: writing a literal `NULL` into a NOT
NULL column (or NOT NULL domain) is rejected at compile time in UPDATE and
MERGE too, although PG fails only the executions that touch a row (an UPDATE
or MERGE matching nothing succeeds). Such a statement is a bug whenever it
does anything, so typedpg reports it; those tests call `skip_pg_sanity`,
since the oracle's execute fallback runs against empty tables.

When a query has **multiple simultaneous errors**, we deliberately do **not**
require the analyzer to pick the *same* error PG reports first. PG's
error-reporting order follows its own parse/transform sequence (it resolves an
expression's functions/operators/types before applying clause-placement rules
like "aggregate not allowed in WHERE", and processes clauses in its own order),
and matching that ordering everywhere is neither tractable nor valuable. So:

- A divergence where both sides reject but pick a *different* error on a
  multi-error query is **not a bug** — don't chase it.
- The fuzzer encodes this: its **single-fault** mode (one mutation over a
  known-valid query) produces single-error cases whose reports *must* match —
  those findings are high-signal. Multi-fault findings are tagged separately
  and treated as likely error-ordering noise.

## Coding conventions

### Rendering qualified PostgreSQL names

Never format a qualified PG identifier with `format!("{schema}.{name}")` or
`format!("\"{schema}\".\"{name}\"")`. PG's quoting rules are non-trivial
(quoting only when necessary, escaping `"` as `""`), and bare concatenation
loses the round-trip guarantee — `"foo.bar".baz` and `foo."bar.baz"` would
collide on a plain `format!`.

Always use `typedpg_core::QualifiedName::new(schema, name).to_string()`
(or pass the `QualifiedName` directly to `format!("{}", qn)`). The
`Display` impl handles the quoting and is the canonical way to render
these names — including in error messages, whenever PG quotes them.

The exception is an error message PG itself builds from the raw names: the
verbatim contract wins, so render exactly what PG's `errmsg` does. E.g.
`RangeVarGetRelidExtended` reports `relation "%s.%s" does not exist` with
neither quoting nor escaping (`"My Schema"."a""b"` → `relation "My
Schema.a"b" does not exist`). Cite the PG `errmsg` in a comment where you
do this.
