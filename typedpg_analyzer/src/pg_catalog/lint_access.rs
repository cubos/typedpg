//! Read-only catalog accessors for the lint crate's migration and schema
//! rules.
//!
//! The rules need a few facts the catalog keeps privately: whether a column
//! default is the `nextval()` of the column's own sequence (the serial
//! shorthand), and the typmod a column's declared type carries through
//! domains and arrays. Everything else they read is already public.

use crate::oid::{PgClassOid, PgTypeOid};

use super::PgCatalog;

impl PgCatalog {
    /// Whether the default of column `attnum` of `relid` is `nextval()` of a
    /// sequence owned by that column, i.e. the column was written with the
    /// `serial` / `bigserial` / `smallserial` shorthand.
    pub fn lint_column_is_serial(&self, relid: PgClassOid, attnum: i16) -> bool {
        matches!(
            self.attr_default_exprs.get(&(relid, attnum)),
            Some(crate::ddl::tables::check_inherit::StoredExpr::Serial(..))
        )
    }

    /// The typmod a column's declared type carries, through domains and
    /// arrays, exactly as the analyzer resolves it.
    pub fn lint_effective_typmod(&self, oid: PgTypeOid, typmod: Option<i32>) -> Option<i32> {
        self.effective_typmod(oid, typmod)
    }
}
