//! DDL application test binary.
//!
//! Each submodule tests a DDL feature by:
//! 1. Applying a sequence of migrations (via `build` or `try_apply`)
//! 2. Asserting the resulting schema snapshot state, or the specific
//!    `DdlError` variant when the migration is expected to fail.
//!
//! Compare with the `query` binary, which tests query analysis semantics
//! against a prepared schema — not schema construction itself.

#[macro_use]
mod common;

// ── Feature files ────────────────────────────────────────────────────────────
#[path = "ddl/alter_table.rs"]
mod alter_table;
#[path = "ddl/casts.rs"]
mod casts;
#[path = "ddl/constraints.rs"]
mod constraints;
#[path = "ddl/create_table.rs"]
mod create_table;
#[path = "ddl/drop.rs"]
mod drop_objects;
#[path = "ddl/extensions.rs"]
mod extensions;
#[path = "ddl/functions.rs"]
mod functions;
#[path = "ddl/generated.rs"]
mod generated;
#[path = "ddl/misc.rs"]
mod misc;
#[path = "ddl/not_null.rs"]
mod not_null;
#[path = "ddl/operators.rs"]
mod operators;
#[path = "ddl/procedures.rs"]
mod procedures;
#[path = "ddl/rename.rs"]
mod rename;
#[path = "ddl/schemas.rs"]
mod schemas;
#[path = "ddl/types.rs"]
mod types;
#[path = "ddl/views.rs"]
mod views;

// ── Coverage gaps (empty; populate as features get covered) ──────────────────
#[path = "ddl/indexes.rs"]
mod indexes;
#[path = "ddl/inheritance.rs"]
mod inheritance;
#[path = "ddl/sequences.rs"]
mod sequences;
#[path = "ddl/triggers.rs"]
mod triggers;
