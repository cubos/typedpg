//! The analyzer fuzzer's query generators, shared with the lint oracle's
//! corpus run: the modules `use super::*` for what `main.rs` imports.

pub(crate) use rand::rngs::StdRng;
pub(crate) use rand::{RngExt, SeedableRng};

#[path = "../../../typedpg_analyzer/tests/fuzz/generators.rs"]
pub(crate) mod generators;
#[path = "../../../typedpg_analyzer/tests/fuzz/narrowing.rs"]
pub(crate) mod narrowing;
#[path = "../../../typedpg_analyzer/tests/fuzz/schema.rs"]
pub(crate) mod schema;

pub(crate) use generators::*;
pub(crate) use narrowing::*;
pub(crate) use schema::*;
