# typedpg_pg_query

PostgreSQL parser bindings for typedpg, built on PostgreSQL's own, unmodified
sources: the parser, the grammar and the scanner of the release named by
`POSTGRES_VERSION` (PostgreSQL 18.7), imported into [`postgres/`](postgres)
by [`import/`](import).

The crate exposes:

- `parse`, `parse_type_name` and `scan`;
- `protobuf`, the AST types (the name is historical: they are plain Rust
  records), generated from PostgreSQL's headers by
  `typedpg_pg_query_codegen`;
- `NodeRef` / `NodeMut`, a view of a node by kind, and `nodes()` /
  `nodes_mut()`, every node of a tree (breadth-first, with its depth);
- `Equal`, PostgreSQL's `equal()` over the AST: every field compared except
  source locations and `equal_ignore` fields, both read from the PostgreSQL
  headers;
- `parse_plpgsql` / `parse_plpgsql_with_catalog`, which compile `DO` blocks
  and `CREATE FUNCTION … LANGUAGE plpgsql` statements with PostgreSQL's own
  PL/pgSQL compiler and return the compiled functions as JSON (see below).

## PL/pgSQL

The compiler is PostgreSQL's (`postgres/src/pl/plpgsql/src/pl_comp.c`,
`pl_gram.c`, `pl_funcs.c`, `pl_scanner.c`, `pl_handler.c`). The driver lives in
[`csrc/plpgsql/`](csrc/plpgsql): libpg_query's `pg_query_parse_plpgsql.c` and
`pg_query_json_plpgsql.c` — BSD-3-Clause, its [`LICENSE`](csrc/plpgsql/LICENSE)
sits next to them — adapted to this crate. The JSON patches typedpg used to
apply at build time are applied to the source directly:

- a block's `initvarnos` (the variables it initializes on entry) are dumped;
- a trigger function's `TG_*` promise datums are dumped as the variables they
  are (older libpg_query emitted them as empty objects, invalid JSON);
- `retvarno` is dumped for `RETURN` / `RETURN NEXT` of a row or set variable;
- `PLpgSQL_type` carries the type name as written (`origtypname`), all that is
  left of a named composite type for re-lookup;
- a record variable's declared type is dumped.

Compiling a `CREATE FUNCTION` builds the `pg_proc` row PostgreSQL's validator
would read — parameter types (the `proargtypes` oidvector and the OUT/VARIADIC
arrays), the result type, the source text — as a real heap tuple, registers it
in the syscache under a fake OID and calls `plpgsql_compile`, so the compiler
runs on its normal path. A `DO` block calls `plpgsql_compile_inline`. The
result is rendered by `plpgsqlToJSON`.

Without a catalog, every lookup the compiler and the parser helpers make is
answered from the release's built-in `pg_catalog` rows
([`postgres/typedpg/builtin_catalog.c`](postgres/typedpg/builtin_catalog.c),
generated from `postgres.bki`): an empty database, where an unknown type or
relation is an error. `parse_plpgsql_with_catalog` installs the caller's
[`Catalog`](src/catalog.rs) for the call instead:
[`csrc/pg/syscache.c`](csrc/pg/syscache.c) builds the `pg_type`,
`pg_namespace`, `pg_class` and `pg_attribute` rows from it, the search path is
handed to PostgreSQL's `assign_search_path`, and installing or removing the
catalog replays the syscache invalidation callbacks so namespace.c rebuilds
its caches. A panic in a catalog callback never unwinds through C: it is
stored, the lookup reports "not found", and the panic resumes once the call
returns.

What compilation does not do:

- parameter `DEFAULT` expressions are not transformed (that needs the whole
  expression parser; [`csrc/pg/parse_defaults.c`](csrc/pg/parse_defaults.c)
  keeps transformExpr's identity, as libpg_query's extraction did). The
  analyzer validates defaults itself;
- nothing is executed: functions and triggers are compiled, never run, and
  `SPI` is not carried;
- a composite type's `TupleDesc` is built from the attributes the catalog
  exposes (the trait has no names), enough for `build_datatype`'s "is it
  composite" check; compile-time never reads the fields.

## How it is built

`import/import.sh` downloads the PostgreSQL release named by
`POSTGRES_VERSION`, generates its parsers with bison and flex, copies the
sources the parser needs whole into [`postgres/`](postgres), trims
`postgres/include/` to the headers they use, generates the built-in catalog
tables, the stubs for the symbols the crate does not carry and the `bindgen`
bindings of the node structs. See [`import/README.md`](import/README.md).

`build.rs` compiles `import/FILES`, `postgres/typedpg/*.c`, `csrc/*.c` (the
catalog bridge), `csrc/pg/*.c` (the hand-written server services and the
crate's C entry points, `shim.c`) and `csrc/plpgsql/*.c` into the library.
Nothing outside `postgres/`, `csrc/` and `import/FILES` is compiled.

`cargo run -p typedpg_pg_query_codegen` regenerates the AST, the walkers,
`equal()` and the C-tree converter from `postgres/include` and
`postgres/src/backend/parser/gram.h`. `tests/up_to_date.rs` fails when the
committed files are stale.

## Moving to a new release

1. Set `POSTGRES_VERSION` and `POSTGRES_SHA256` to the release and its
   tarball's hash.
2. Run `typedpg_pg_query/import/import.sh` (with bison, flex, bindgen and
   clang on `PATH`; see `import/README.md`), then `import/check.sh`.
3. Run `cargo run -p typedpg_pg_query_codegen` and commit the generated
   sources with the imported tree.
4. Pin `typedpg_test_support`'s image to the release (its test servers check
   it), regenerate the seed (`cargo run -p typedpg_seed`) and run the
   suites, including `scripts/run-pg-sanity.sh`.

A PostgreSQL patch release changes no grammar; a major release may add node
fields or enum values, which the codegen picks up from the headers. The
`typedpg_pg_query` crate keeps libpg_query's AST shape (see the module docs
of `typedpg_pg_query_codegen::ast`) so stored trees and the analyzer keep
working.
