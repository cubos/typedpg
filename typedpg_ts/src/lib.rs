//! typedpg for TypeScript: finds the `sql("...")` queries of a project's
//! sources and the queries of its `.sql` files, analyzes them against the
//! schema its migrations build, and generates the modules that type and run
//! them — or compiles a `.sql` file alone, for a loader's cache (`build`)
//! or TypeScript's content mapper (`serve`).

pub mod cache;
pub mod config;
pub mod emit;
pub mod generate;
pub mod project;
pub mod scan;
pub mod serve;
pub mod sqlfile;
pub mod sqlmodule;
pub mod typemap;
pub mod watch;
