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
cargo build -p typedpg_wasm --target wasm32-unknown-unknown --profile wasm  # npm runtime's helpers
cargo nextest run --release                          # all tests (no Docker needed)
cargo nextest run --release -p typedpg_core        # core crate only
cargo nextest run --release -p typedpg_macros      # macro crate only
cargo nextest run --release -p typedpg_analyzer    # analyzer crate only
cargo nextest run --release -p typedpg_lint        # lint crate only
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

### TypeScript

`typedpg_ts` is the TypeScript CLI (`typedpg gen|check|build|serve|migrate`;
it needs a newer Rust than the workspace's MSRV, for oxc), `typedpg_npm` the
npm runtime package (the query runtime, the migration runner and the `.sql`
loaders a production install runs from the cache), `typedpg_npm_cli` the npm
development package (the `typedpg` command and the bundler plugins, with the
platform binaries as optional dependencies), `typedpg_vscode` the editor
extension (SQL highlighting, `.sql` files' errors), `typedpg_ts_example` the
project the end-to-end run uses (`loaders/` in it imports a `.sql` file
itself).

`@cubos/typedpg` carries no binary: its loaders run from the cache `typedpg
build` fills, and the CLI — a devDependency — compiles on demand. The
asynchronous loaders (Bun's `onLoad`, the bundler plugins) compile through a
persistent `typedpg serve` per binary, which keeps the schema in memory and
exits after an idle timeout; the Node hook is synchronous and runs `typedpg
build`. The CLI imports the runtime's cache and binary lookup through
`@cubos/typedpg/internal`, which is not a public API.

The npm runtime and the Rust runner must agree on what a migration is: the
runner's whole logic — the advisory lock and its backoff, the tracking table,
drift checks, the apply/revert/accept flows, every message — is one sans-I/O
state machine in `typedpg_core::migrations::machine`. The Rust runner
(`typedpg/src/migrate/runner.rs`) and the npm one (`typedpg_npm/src/migrate.ts`)
are drivers: they perform each `Step` on their own client and reply. The pure
helpers (statement splitting, drift normalization and hash, ordering,
file-name and tracking-table validation, error and drift messages) live in
the same module. `typedpg_wasm` exports the machine (with handles, so runs
can interleave across awaits) plus those helpers, and the `.sql` loaders'
cache recipe (`typedpg_core::loader_cache`), to the npm runtime. Building
`typedpg_npm` (`npm run build` there) runs `scripts/wasm.mjs` (cargo, wasm32)
and embeds the module in a generated `src/wasm.generated.ts`, so building or
testing the package needs Rust and the wasm target:

```bash
rustup target add wasm32-unknown-unknown
```

The loaders' cache key is one recipe in `typedpg_core::loader_cache`, used by
`typedpg_ts/src/cache.rs` and, through `typedpg_wasm`, by
`typedpg_npm/src/sqlfiles.ts`: change it once, in core, and bump `FORMAT` when
the output must change (the e2e runs the Node hook on a cache `typedpg build`
filled, without the binary). The recipe's actual key material must not change
casually: an existing cache stays valid.

```bash
cargo nextest run --release -p typedpg_ts        # scanner, codegen, incremental project
(cd typedpg_npm && npm ci && npm test)           # runtime unit tests (codecs, wasm bridge)
(cd typedpg_npm_cli && npm ci && npm test)       # command package (after typedpg_npm builds)
(cd typedpg_vscode && npm ci && npm test)        # editor grammars (VS Code's TextMate engine)
scripts/run-ts-e2e.sh                            # everything, on a PostgreSQL container
BLESS=1 scripts/run-ts-e2e.sh                    # rewrite the errors fixtures' expected outputs
```

After changing `typedpg_npm` or `typedpg_npm_cli`, build both (`npm run
build` in each, the runtime first) and `npm install` in `typedpg_ts_example`:
it installs them as copies (`install-links`), as from the registry.

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
typedpg_cli (binary: `cargo typedpg migrate …`, `cargo typedpg lint`)
    ├── typedpg (runtime: Pool, Executor, migrate)
    │       ├── typedpg_core (shared config and migration helpers — kept small so runtime does not pull pg_query)
    │       └── typedpg_macros (proc macro: sql!)
    │               ├── typedpg_core
    │               └── typedpg_analyzer (compile-time only: lexer, param types, query_info, type_map, static SQL analyzer)
    │                       └── typedpg_pg_query (PostgreSQL parser, built from PostgreSQL's own sources)
    └── typedpg_lint (the `typedpg lint` rules: queries, migrations and the final schema)
            ├── typedpg_analyzer
            ├── typedpg_core
            └── typedpg_pg_query

typedpg_wasm (the same migration helpers, compiled to WebAssembly for typedpg_npm)
    └── typedpg_core
```

`typedpg_pg_query` compiles PostgreSQL's own sources, imported into
`typedpg_pg_query/postgres/` from the release named by `POSTGRES_VERSION`
(see `typedpg_pg_query/import/README.md`; `cargo run -p typedpg_pg_query_codegen`
regenerates the AST types, walkers and the C-tree converter from the imported
headers). Nothing is compiled from libpg_query any more, and the repository no
longer carries it as a submodule. The imported tree is portable: the
configure-generated `pg_config.h` ends with `import/pg_config_overrides.h`
(per-platform defines, documented there), `import/pg_config_os.h` dispatches to
`port/linux.h`, `port/darwin.h` or `port/win32.h`, and `import/FILES.windows`
names the sources only the Windows/MSVC build compiles. All seven release
targets (glibc and musl Linux on x86_64/aarch64, macOS on x86_64/aarch64,
Windows x64) build the same imported tree.

PL/pgSQL is compiled by PostgreSQL's own compiler (`pl_comp.c`, `pl_gram.c`),
driven by `csrc/plpgsql/` (libpg_query's PL/pgSQL driver, BSD-3-Clause), and
each compiled function is converted straight from its C structs into the
hand-written `typedpg_pg_query::plpgsql` types (`src/plpgsql/convert.rs`, over
the bindgen bindings of `plpgsql.h`). A
`CREATE FUNCTION … LANGUAGE plpgsql` builds a fake `pg_proc` row, registers it
in the syscache and calls `plpgsql_compile` exactly as the validator does; a
`DO` block calls `plpgsql_compile_inline`. Catalog lookups go through the
syscache: `csrc/pg/syscache.c` answers TYPEOID / TYPENAMENSP / NAMESPACEOID /
NAMESPACENAME / RELNAMENSP / RELOID / ATTNAME / ATTNUM / PROCOID from the
caller's `Catalog` while `parse_plpgsql_with_catalog` runs (the search path is
handed to `assign_search_path`, and installing or removing the catalog replays
the syscache invalidation callbacks), and from the release's `postgres.bki`
rows (`postgres/typedpg/builtin_catalog.c`: `pg_type`, `pg_namespace` and the
built-in `pg_collation`s) otherwise. Parameter `DEFAULT` expressions are not
transformed (the expression parser is not carried; the analyzer validates them
itself), and nothing is executed.

### Compile-time pipeline (`sql!` macro)

1. Parse macro input: `sql!(executor, "SQL with $params", name = value)`
2. Lex SQL via `typedpg_analyzer::lexer::lex()` — rewrites `$name` → `$1`, extracts `$..spread`
3. Load config from `[package.metadata.typedpg]` in consumer's `Cargo.toml`
4. Build schema snapshot from seed + migrations via DDL interpreter (in-memory, no Docker)
5. Static analysis: parse SQL with `typedpg_pg_query` (PostgreSQL's parser), resolve types and nullability against snapshot
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
(`typedpg_analyzer/tests/fuzz/main.rs`, `#[ignore]`d) generates queries to surface
new disagreements automatically.

**Catalog state.** After every migration both sides accept, the mirror also
compares the system catalogs (`pg_sanity/catalog_state.rs`, on by default in
`scripts/run-pg-sanity.sh`; `PG_SANITY_CATALOG_LOG=<file>` logs instead of
panicking): the server's user rows, exported like `seed.json`
(`typedpg_analyzer::catalog_export`), must equal the analyzer's, OIDs
mapped to names; what is compared and what is deliberately normalized is
listed there. `tests/catalog_oracle` feeds it migration corpora
(`CATALOG_CORPUS`) and random DDL, and minimizes a divergence.

**Nullability soundness.** Describe says nothing about nullability, so the
mirror also *executes* every accepted query, in a rolled-back transaction,
over adversarial rows it seeds into every table (NULL in each nullable
column, a second fully-filled row, foreign keys leaving parents unmatched,
then rows mixing NULL and filled columns so each side of a CHECK tying
columns together has one)
and fails if a value inferred NOT NULL — a column, a `Some(false)` array
element, a non-nullable record field — comes back NULL
(`typedpg_analyzer/src/pg_sanity/soundness.rs`). A divergence there is a
soundness bug in the analyzer: fix the inference, never the expectation.

**Lint soundness.** `scripts/run-lint-oracle.sh` runs every lint finding over the
same scenarios (`typedpg_lint/src/rules/oracle.rs`): a `--fix` must keep rows, row
count, table contents and SQLSTATE, and a claim (never NULL, always true, at most one
row, …) runs as an assertion or equivalent query. A broken claim is a rule bug.
It also runs `breaks_deployed_queries` against PG (`typedpg_lint/src/deployed_oracle.rs`,
its own tests and the `deployed_oracle_corpus` test): deployed queries are called with
sampled values before and after the new migrations, over seeded rows; a call that breaks
with no finding, or that runs where a finding says it can't, is a rule bug.

**Migration rules.** Under `pg_sanity`, every `lint_migrations` call is replayed
statement by statement on PG (`typedpg_lint/src/migration_oracle.rs`), recording the
locks gained (`pg_locks`), rewrites and index builds (`relfilenode`) and scans
(`pg_stat_xact_*`, `DEBUG1` "verifying table"). A message must match what PG did, and a
write-blocking lock without `lock_timeout`, a rewrite, an index build or a scan under a
write-blocking lock on an existing table must be reported. `scripts/run-migration-oracle.sh`
runs its corpus (docs, the analyzer suite's migrations, generated `ALTER TABLE` & co.).

**Declared constraints are trusted.** Nullability inference may rely on
what the schema states — a foreign key's referenced row exists, a CHECK
holds for every row — including `NOT VALID` and deferrable constraints;
only `NOT ENFORCED` ones are ignored. The exceptions are what PG itself
doesn't enforce or what can hide rows: row security on a referenced table,
`DISABLE TRIGGER ALL` (which turns foreign-key checks off), foreign keys
under row locking (EvalPlanQual re-joins a re-fetched row), constraints an
inheritance child doesn't carry (foreign keys, `NO INHERIT`), and a NOT
NULL domain on a column without its own NOT NULL (PG stores a NULL already
of the domain type unchecked).

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

One deliberate exception goes further: writing a literal `NULL` (or
`DEFAULT` where the column has no default) into a NOT NULL column (or NOT
NULL domain) is rejected at compile time in UPDATE and
MERGE too, although PG fails only the executions that touch a row (an UPDATE
or MERGE matching nothing succeeds). Such a statement is a bug whenever it
does anything, so typedpg reports it; those tests call `skip_pg_sanity`,
since the oracle's execute fallback runs against empty tables. A column's
own NOT NULL is checked only after the BEFORE ROW triggers
(`ExecConstraints`), so the NULL is accepted when a BEFORE ROW trigger for
the event — on the table, on a partition the row is routed to, or (UPDATE)
a BEFORE ROW INSERT one on a partition a row may move to — can replace it.
A NOT NULL domain fails as the row is built, before any trigger, and stays
rejected. A NULL given for an identity column under `OVERRIDING USER VALUE`
is never stored (the sequence's value is) and is accepted too. Likewise, an
expression that fails every time it is evaluated — a multidimensional
`ARRAY[ARRAY[a], NULL]` whose sub-arrays can never agree in dimensions — is
rejected even where no row reaches it.

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
