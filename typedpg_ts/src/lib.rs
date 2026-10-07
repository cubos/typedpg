//! typedpg for TypeScript: finds the `sql("...")` queries of a project's
//! sources, analyzes them against the schema its migrations build, and
//! generates, per database, the module that types and runs them.

pub mod config;
pub mod emit;
pub mod generate;
pub mod project;
pub mod scan;
pub mod typemap;
pub mod watch;
