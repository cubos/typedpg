//! `CREATE COLLATION` handler.
//!
//! Two PG variants:
//! 1. `CREATE COLLATION name (LOCALE = '…' [, PROVIDER = 'libc'|'icu' …])` —
//!    a fresh collation built from locale parameters. We don't model the
//!    parameters; only the name + namespace round-trip.
//! 2. `CREATE COLLATION name FROM existing_collation` — clones an existing
//!    collation. We resolve the source name to confirm it exists; the new
//!    row inherits its encoding.
//!
//! Either way the analyzer just needs a valid `pg_collation` row so
//! subsequent `COLLATE "name"` references resolve.

use typedpg_pg_query::protobuf::{DefineStmt, node};

use super::DdlError;
use super::util::ensure_qualified_name;
use crate::oid::PgCollationOid;
use crate::pg_catalog::{PgCatalog, PgCollation};

pub fn define_collation(interp: &mut PgCatalog, stmt: &DefineStmt) -> Result<(), DdlError> {
    let (nsoid, name) = ensure_qualified_name(interp, &stmt.defnames)?;

    // PG: `collation "<name>" already exists`. Names are unique per schema.
    if interp
        .collation_by_qname
        .contains_key(&(nsoid, name.clone()))
    {
        if stmt.if_not_exists {
            return Ok(());
        }
        return Err(DdlError::DuplicateObject(format!(
            "collation \"{name}\" for encoding \"UTF8\" already exists"
        )));
    }

    // `CREATE COLLATION x FROM y` parses as a `DefElem { defname = "from",
    // arg = List([String("y")]) }`; we use it to resolve the source and
    // inherit its encoding. Option-style `(LOCALE = …, PROVIDER = …)` parses
    // as a list of DefElems we don't read today — collencoding stays `-1`.
    let mut collencoding: i32 = -1;
    for opt in &stmt.definition {
        let Some(node::Node::DefElem(de)) = opt.node.as_ref() else {
            continue;
        };
        if de.defname.eq_ignore_ascii_case("from")
            && let Some(arg) = de.arg.as_deref()
        {
            let parts: Vec<&str> = match arg.node.as_ref() {
                Some(node::Node::List(list)) => list
                    .items
                    .iter()
                    .filter_map(|n| match n.node.as_ref()? {
                        node::Node::String(s) => Some(s.sval.as_str()),
                        _ => None,
                    })
                    .collect(),
                Some(node::Node::String(s)) => vec![s.sval.as_str()],
                _ => Vec::new(),
            };
            let (schema, src_name) = match parts.as_slice() {
                [n] => (None, *n),
                [s, n] => (Some(*s), *n),
                _ => {
                    return Err(DdlError::Parse("malformed collation source name".into()));
                }
            };
            let source = interp.resolve_collation(schema, src_name).ok_or_else(|| {
                DdlError::DependencyError(format!(
                    "collation \"{src_name}\" for encoding \"UTF8\" does not exist"
                ))
            })?;
            collencoding = source.collencoding;
        }
    }

    let oid = PgCollationOid::from_nonzero(interp.alloc_oid()?);
    interp.insert_pg_collation(PgCollation {
        oid,
        collname: name,
        collnamespace: nsoid,
        collencoding,
    });
    Ok(())
}
