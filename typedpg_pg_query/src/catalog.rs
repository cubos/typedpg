//! The catalog PL/pgSQL's compiler will read.
//!
//! PL/pgSQL's compiler resolves types (of arguments, the result and declared
//! variables), schemas and, for `%TYPE` / `%ROWTYPE`, relations and columns.
//! Outside a server, PostgreSQL has no catalog to consult, so
//! [`parse_plpgsql_with_catalog`](crate::parse_plpgsql_with_catalog) takes a
//! [`Catalog`] and the compiler resolves everything — with PostgreSQL's own
//! errors — against it.
//!
//! PL/pgSQL parsing is not available yet (see the crate docs); the types stay
//! here for the phase that brings it back.

/// A `pg_type` row, with the columns the parser and the PL/pgSQL compiler
/// read. OIDs of `0` stand for none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogType {
    pub oid: u32,
    pub name: String,
    pub namespace: u32,
    /// `typlen`: a fixed size, `-1` for varlena, `-2` for C strings.
    pub len: i16,
    pub by_val: bool,
    /// `typtype`: `b`, `c`, `d`, `e`, `p`, `r` or `m`.
    pub typtype: u8,
    pub category: u8,
    pub preferred: bool,
    /// `typalign`: `c`, `s`, `i` or `d`.
    pub align: u8,
    pub relid: u32,
    /// `typsubscript`: the subscripting handler function.
    pub subscript: u32,
    pub elem: u32,
    pub array: u32,
    pub base_type: u32,
    pub typmod: i32,
    pub not_null: bool,
    pub collation: u32,
    /// `typisdefined`: `false` for a shell type.
    pub is_defined: bool,
}

/// A `pg_attribute` row, with the columns `%TYPE` reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogAttribute {
    pub number: i16,
    pub type_oid: u32,
    pub typmod: i32,
    pub collation: u32,
}

/// A PostgreSQL catalog, as the PL/pgSQL compiler consults it.
pub trait Catalog {
    fn type_by_oid(&self, oid: u32) -> Option<CatalogType>;
    fn type_by_name(&self, namespace: u32, name: &str) -> Option<u32>;
    fn namespace_by_name(&self, name: &str) -> Option<u32>;
    fn namespace_name(&self, namespace: u32) -> Option<String>;
    /// The schemas an unqualified name is looked up in, in order — the
    /// effective search path, `pg_catalog` included where it applies.
    fn search_path(&self) -> Vec<u32>;
    fn relation_by_name(&self, namespace: u32, name: &str) -> Option<u32>;
    /// `pg_class.reltype` (`None` for a relkind without a row type).
    fn relation_type(&self, relation: u32) -> Option<u32>;
    fn attribute_by_name(&self, relation: u32, name: &str) -> Option<CatalogAttribute>;
    fn attribute_by_number(&self, relation: u32, number: i16) -> Option<CatalogAttribute>;
}
