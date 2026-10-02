//! Query analysis test binary.
//!
//! Each submodule tests a specific SQL/Postgres feature by:
//! 1. Building a minimal `PgCatalog` with only the schema it needs
//! 2. Analyzing a query
//! 3. Asserting output columns, parameters, or a specific error variant
//!
//! Compare with the `ddl` binary, which tests schema-state changes (DDL
//! application and its resulting snapshot), not query analysis.

#[macro_use]
mod common;

// ── Feature files ────────────────────────────────────────────────────────────
#[path = "query/aggregates.rs"]
mod aggregates;
#[path = "query/array_element_nullability.rs"]
mod array_element_nullability;
#[path = "query/assignments.rs"]
mod assignments;
#[path = "query/call.rs"]
mod call;
#[path = "query/casts_and_coercion.rs"]
mod casts_and_coercion;
#[path = "query/check_reasoning.rs"]
mod check_reasoning;
#[path = "query/constraint_narrowing.rs"]
mod constraint_narrowing;
#[path = "query/copy_in.rs"]
mod copy_in;
#[path = "query/cte_rules.rs"]
mod cte_rules;
#[path = "query/ctes.rs"]
mod ctes;
#[path = "query/dml.rs"]
mod dml;
#[path = "query/dml_clause_kinds.rs"]
mod dml_clause_kinds;
#[path = "query/dml_returning_narrowing.rs"]
mod dml_returning_narrowing;
#[path = "query/expression_facts.rs"]
mod expression_facts;
#[path = "query/expressions.rs"]
mod expressions;
#[path = "query/function_aggregate_narrowing.rs"]
mod function_aggregate_narrowing;
#[path = "query/join_rules.rs"]
mod join_rules;
#[path = "query/joins.rs"]
mod joins;
#[path = "query/json_table.rs"]
mod json_table;
#[path = "query/literal_input.rs"]
mod literal_input;
#[path = "query/merge.rs"]
mod merge;
#[path = "query/named_args.rs"]
mod named_args;
#[path = "query/null_narrowing.rs"]
mod null_narrowing;
#[path = "query/nullability_soundness.rs"]
mod nullability_soundness;
#[path = "query/params.rs"]
mod params;
#[path = "query/records.rs"]
mod records;
#[path = "query/select.rs"]
mod select;
#[path = "query/select_rules.rs"]
mod select_rules;
#[path = "query/set_operations.rs"]
mod set_operations;
#[path = "query/set_returning_functions.rs"]
mod set_returning_functions;
#[path = "query/special.rs"]
mod special;
#[path = "query/sql_json.rs"]
mod sql_json;
#[path = "query/subqueries.rs"]
mod subqueries;
#[path = "query/typmod.rs"]
mod typmod;
#[path = "query/user_types.rs"]
mod user_types;
#[path = "query/utility_stmts.rs"]
mod utility_stmts;
#[path = "query/view_dml.rs"]
mod view_dml;
#[path = "query/where_clause.rs"]
mod where_clause;
#[path = "query/xml.rs"]
mod xml;

// ── Coverage gaps (empty; populate as features get covered) ──────────────────
#[path = "query/aggregate_filter.rs"]
mod aggregate_filter;
#[path = "query/arrays.rs"]
mod arrays;
#[path = "query/collation.rs"]
mod collation;
#[path = "query/full_text_search.rs"]
mod full_text_search;
#[path = "query/function_nullability.rs"]
mod function_nullability;
#[path = "query/function_resolution.rs"]
mod function_resolution;
#[path = "query/grouping_sets.rs"]
mod grouping_sets;
#[path = "query/json_operators.rs"]
mod json_operators;
#[path = "query/recursive_ctes.rs"]
mod recursive_ctes;
#[path = "query/returning_old_new.rs"]
mod returning_old_new;
#[path = "query/virtual_generated.rs"]
mod virtual_generated;
#[path = "query/window_calls.rs"]
mod window_calls;
#[path = "query/window_functions.rs"]
mod window_functions;
