//! Static seed loading.
//!
//! The seed is a pre-exported snapshot of a clean PostgreSQL 19 instance —
//! every built-in row from `pg_namespace`, `pg_type`, `pg_class`,
//! `pg_attribute`, `pg_proc`, `pg_operator`, `pg_cast`, `pg_extension`, and
//! the supporting `pg_enum`/`pg_range`/`pg_aggregate`/`pg_depend` tables. It
//! is embedded at compile time via `include_str!` and deserialized into a
//! [`PgCatalogSeed`] DTO before being moved into a fresh [`crate::PgCatalog`].

use crate::error::AnalyzeError;
use crate::pg_catalog::PgCatalogSeed;

const SEED_JSON: &str = include_str!("seed.json");

/// Load the embedded seed (clean PostgreSQL 19 catalog).
///
/// The seed is bundled with the analyzer crate and validated by the
/// regenerator (`cargo run -p typedpg_seed`); a malformed seed surfaces as
/// an [`AnalyzeError::Serde`] rather than a panic so callers using
/// [`crate::PgCatalog::try_new`] can capture it.
pub(crate) fn load_seed() -> Result<PgCatalogSeed, AnalyzeError> {
    serde_json::from_str(SEED_JSON).map_err(AnalyzeError::from)
}
