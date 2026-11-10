//! Read-only catalog accessors for the lint crate's query, migration and
//! schema rules.
//!
//! The rules need a few facts the catalog keeps privately: whether a column
//! default is the `nextval()` of the column's own sequence (the serial
//! shorthand), the typmod a column's declared type carries through domains
//! and arrays, and whether a relation has row security or rewrite rules.
//! Everything else they read is already public.

use crate::oid::{PgClassOid, PgTypeOid};

use super::PgCatalog;

impl PgCatalog {
    /// Whether row-level security is enabled on `relid`
    /// (`pg_class.relrowsecurity`): its SELECT policies then restrict the
    /// rows a statement sees, `RETURNING` included.
    pub fn lint_relation_has_row_security(&self, relid: PgClassOid) -> bool {
        self.row_security.contains(&relid)
    }

    /// Whether `relid` has a rewrite rule (`CREATE RULE`): a statement on it
    /// may then run the rule's actions instead of, or besides, its own.
    pub fn lint_relation_has_rules(&self, relid: PgClassOid) -> bool {
        self.rules
            .get(&relid)
            .is_some_and(|rules| !rules.is_empty())
    }

    /// Whether the unique index `index` treats NULLs as equal
    /// (`indnullsnotdistinct`). `false` for a non-unique index, which has no
    /// such flag.
    pub fn lint_index_nulls_not_distinct(&self, index: PgClassOid) -> bool {
        self.index_keys
            .get(&index)
            .is_some_and(|keys| keys.nulls_not_distinct)
    }

    /// Whether the index `index` was created by a DEFERRABLE constraint
    /// (`pg_index.indimmediate = false`): its uniqueness is checked at the
    /// end of the transaction, not at each statement.
    pub fn lint_index_is_deferrable(&self, index: PgClassOid) -> bool {
        self.nonimmediate_indexes.contains(&index)
    }

    /// Resolve a `TypeName` AST node to a type OID, the way the analyzer
    /// resolves casts and column definitions.
    pub fn lint_resolve_type_name(
        &self,
        type_name: &typedpg_pg_query::protobuf::TypeName,
    ) -> Option<PgTypeOid> {
        crate::ddl::util::resolve_type_name(type_name, self)
    }

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
