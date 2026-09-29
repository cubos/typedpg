# typedpg_pg_query

PostgreSQL parser bindings for typedpg, built on
[libpg_query](https://github.com/pganalyze/libpg_query) — PostgreSQL's own
grammar extracted into a C library, with the parse tree handed over as
protobuf.

The crate compiles libpg_query from the `libpg_query/` git submodule (pinned
to a release tag) and exposes:

- `parse`, `scan`, `deparse` and `parse_plpgsql`;
- `protobuf`, the AST types, generated from libpg_query's `pg_query.proto`;
- `NodeRef` / `NodeMut`, a view of a node by kind, and `nodes()` /
  `nodes_mut()`, every node of a tree (breadth-first, with its depth);
- `parse_plpgsql_with_catalog`, which compiles a PL/pgSQL function against a
  caller-supplied `Catalog` (see below).

## PL/pgSQL and the catalog

SQL parsing is pure grammar, but PL/pgSQL's compiler looks things up: the
types of the arguments, the result and declared variables, schemas, and — for
`%TYPE` / `%ROWTYPE` — relations and columns. Outside a server, libpg_query
answers those lookups from mocks that only know the built-in types.

`parse_plpgsql_with_catalog` installs a `Catalog` for the duration of the
call, and the lookups read it instead (`csrc/catalog.c` plus the redirecting
patches in `patches.rs`): user-defined types resolve with their real kind,
and missing objects get PostgreSQL's own errors (`type "x" does not exist`,
`relation "t" does not exist`, `column "c" of relation "t" does not exist`).
PostgreSQL 18.4's `%TYPE` / `%ROWTYPE` code, which libpg_query replaces with a
text-only stand-in, is restored while a catalog is installed.

`PG_VERSION` / `PG_VERSION_NUM` name the PostgreSQL release the grammar comes
from; they are read from the vendored `pg_query.h` at build time.

## Moving to a new libpg_query release

1. Check out the new tag in the submodule:
   `git -C typedpg_pg_query/libpg_query checkout <tag>`.
2. Regenerate the Rust sources: `cargo run -p typedpg_pg_query_codegen`
   (writes `src/protobuf.rs` and `src/node.rs`; the
   `generated_sources_are_up_to_date` test fails while they are stale).
3. Build. A patch in `patches.rs` whose spot changed upstream fails the build
   with the patch's description: re-derive it, or drop it if upstream fixed
   the problem.
4. Adapt typedpg_analyzer to the AST changes, regenerate the seed
   (`cargo run -p typedpg_seed` — the seed stores protobuf-encoded ASTs), and
   run the test suites, including `scripts/run-pg-sanity.sh` against the
   matching PostgreSQL release.

## Patches

`patches.rs` lists exact-text replacements applied to libpg_query's sources
at build time: the build copies each patched file into `OUT_DIR` and compiles
the copy. They fall in three groups:

- **Catalog hooks** — route the mocked catalog lookups to an installed
  `Catalog`, and restore PostgreSQL's `%TYPE` / `%ROWTYPE` code while one is.
- **PL/pgSQL JSON dump fixes** — libpg_query 18.0.0 wrote a trigger
  function's `TG_*` promise datums as empty objects (invalid JSON), and left
  out `retvarno`, a record variable's declared type and the type name as
  written.
- **Mock fixes**, for compiling without a catalog: the mock `pg_type` rows
  had no `typelem` / `typsubscript` (every `VARIADIC` function was rejected),
  schemas other than `pg_catalog` / `public` failed with "Not implemented",
  an array of a user-defined type became the pseudo-type `record[]`, and the
  written type name was never kept.

## License

The Rust code is MIT OR Apache-2.0. The bundled libpg_query is BSD-3-Clause
(`libpg_query/LICENSE`), and the PostgreSQL sources it embeds are under the
PostgreSQL License.
