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
- `parse_plpgsql` / `parse_plpgsql_with_catalog`, which are **not available
  yet**: both return an error saying so until PL/pgSQL's compiler is wired in
  (the next phase). The `Catalog` types they will consult are already here.

## How it is built

`import/import.sh` downloads the PostgreSQL release named by
`POSTGRES_VERSION`, generates its parsers with bison and flex, copies the
sources the parser needs whole into [`postgres/`](postgres), trims
`postgres/include/` to the headers they use, generates the built-in catalog
tables, the stubs for the symbols the crate does not carry and the `bindgen`
bindings of the node structs. See [`import/README.md`](import/README.md).

`build.rs` compiles `import/FILES`, `postgres/typedpg/*.c` and
`csrc/pg/*.c` (the hand-written server services and the crate's C entry
points, `shim.c`) into the library. Nothing outside `postgres/`, `csrc/` and
`import/FILES` is compiled.

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
