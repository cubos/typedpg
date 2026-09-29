//! The catalog as libpg_query's PL/pgSQL compiler consults it
//! ([`typedpg_pg_query::Catalog`]): types, schemas, the search path,
//! relations and columns, read from the snapshot.

use typedpg_pg_query::{Catalog, CatalogAttribute, CatalogType};

use super::{PgAttribute, PgCatalog};
use crate::oid::{PgClassOid, PgNamespaceOid, PgTypeOid};

/// The PG char a catalog enum serializes as (`typtype`, `typcategory`).
fn pg_char<T: serde::Serialize>(value: &T) -> u8 {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().and_then(|s| s.bytes().next()))
        .unwrap_or(0)
}

fn attribute(a: &PgAttribute) -> CatalogAttribute {
    CatalogAttribute {
        number: a.attnum,
        type_oid: a.atttypid.get(),
        typmod: a.atttypmod.unwrap_or(-1),
        collation: a.attcollation.map_or(0, |c| c.get()),
    }
}

impl Catalog for PgCatalog {
    fn type_by_oid(&self, oid: u32) -> Option<CatalogType> {
        let t = self.pg_type.get(&PgTypeOid::new(oid)?)?;
        Some(CatalogType {
            oid,
            name: t.typname.clone(),
            namespace: t.typnamespace.get(),
            len: t.typlen,
            by_val: t.typbyval,
            typtype: pg_char(&t.typtype),
            category: pg_char(&t.typcategory),
            preferred: t.typispreferred,
            align: t.typalign.as_char(),
            relid: t.typrelid.map_or(0, |o| o.get()),
            subscript: t.typsubscript.map_or(0, |o| o.get()),
            elem: t.typelem.map_or(0, |o| o.get()),
            array: t.typarray.map_or(0, |o| o.get()),
            base_type: t.typbasetype.map_or(0, |o| o.get()),
            typmod: t.typtypmod.unwrap_or(-1),
            not_null: t.typnotnull,
            collation: t.typcollation.map_or(0, |o| o.get()),
            is_defined: t.typisdefined,
        })
    }

    fn type_by_name(&self, namespace: u32, name: &str) -> Option<u32> {
        self.type_by_qname
            .get(&(PgNamespaceOid::new(namespace)?, name.to_owned()))
            .map(|o| o.get())
    }

    fn namespace_by_name(&self, name: &str) -> Option<u32> {
        self.namespace_oid(name).map(|o| o.get())
    }

    fn namespace_name(&self, namespace: u32) -> Option<String> {
        PgCatalog::namespace_name(self, PgNamespaceOid::new(namespace)?).map(str::to_owned)
    }

    fn search_path(&self) -> Vec<u32> {
        self.schemas_for_lookup(None)
            .into_iter()
            .map(|o| o.get())
            .collect()
    }

    fn relation_by_name(&self, namespace: u32, name: &str) -> Option<u32> {
        self.class_by_qname
            .get(&(PgNamespaceOid::new(namespace)?, name.to_owned()))
            .map(|o| o.get())
    }

    fn relation_type(&self, relation: u32) -> Option<u32> {
        self.pg_class
            .get(&PgClassOid::new(relation)?)?
            .reltype
            .map(|o| o.get())
    }

    fn attribute_by_name(&self, relation: u32, name: &str) -> Option<CatalogAttribute> {
        self.attributes_of(PgClassOid::new(relation)?)
            .iter()
            .find(|a| a.attname == name)
            .map(attribute)
    }

    fn attribute_by_number(&self, relation: u32, number: i16) -> Option<CatalogAttribute> {
        self.attributes_of(PgClassOid::new(relation)?)
            .iter()
            .find(|a| a.attnum == number)
            .map(attribute)
    }
}
