//! Shared utilities for DDL interpretation.

use typedpg_pg_query::protobuf::{Node, RangeVar, TypeName, node};

use crate::ddl::DdlError;
use crate::oid::{PgCastOid, PgNamespaceOid, PgTypeOid};
use crate::pg_catalog::{
    CastContext, CastMethod, PgCast, PgCatalog, PgNamespace, oid as builtin_oid,
};
use crate::qualified_name::QualifiedName;

/// Schema an unqualified new object is created in: PG's creation namespace,
/// i.e. the first schema of the effective `search_path` that exists
/// (`RangeVarGetCreationNamespace` / `QualifiedNameGetCreationNamespace`).
pub fn creation_schema(snapshot: &PgCatalog) -> Result<String, DdlError> {
    snapshot
        .search_path
        .first()
        .and_then(|&oid| snapshot.namespace_name(oid).map(str::to_owned))
        .ok_or_else(|| DdlError::Parse("no schema has been selected to create in".into()))
}

/// Schema to report / probe for an unqualified name that does not resolve:
/// the creation namespace, or `public` when the search path is empty.
fn fallback_schema(snapshot: &PgCatalog) -> String {
    creation_schema(snapshot).unwrap_or_else(|_| "public".to_owned())
}

/// Extract the (schema, name) pair of an *existing* relation from a
/// `RangeVar`. An unqualified name is looked up along the search path (with
/// `pg_catalog` implicitly first, as `RangeVarGetRelid` does); when nothing
/// matches, the creation namespace is returned so the caller reports the
/// relation as missing.
pub fn range_var_names(rv: &RangeVar, snapshot: &PgCatalog) -> (String, String) {
    let schema = if rv.schemaname.is_empty() {
        snapshot
            .resolve_table(None, &rv.relname)
            .and_then(|c| snapshot.namespace_name(c.relnamespace).map(str::to_owned))
            .unwrap_or_else(|| fallback_schema(snapshot))
    } else {
        rv.schemaname.clone()
    };
    (schema, rv.relname.clone())
}

/// Schema of the type an unqualified `name` resolves to along the search
/// path, or the creation namespace when no such type exists.
pub fn type_lookup_schema(snapshot: &PgCatalog, name: &str) -> String {
    snapshot
        .resolve_type_by_name(None, name)
        .and_then(|t| snapshot.namespace_name(t.typnamespace).map(str::to_owned))
        .unwrap_or_else(|| fallback_schema(snapshot))
}

/// Resolve an existing relation named by a `RangeVar` to `(namespace,
/// relation)`, with PG's errors (`RangeVarGetRelidExtended`): `schema "s"
/// does not exist` for a missing qualifier, `relation "x" does not exist`
/// (qualified as written) otherwise.
pub fn lookup_relation(
    snapshot: &PgCatalog,
    rv: &RangeVar,
) -> Result<(PgNamespaceOid, crate::oid::PgClassOid), DdlError> {
    let (schema, name) = range_var_names(rv, snapshot);
    let Some(nsoid) = snapshot.namespace_oid(&schema) else {
        return Err(if rv.schemaname.is_empty() {
            DdlError::TableNotFound(format!("relation \"{name}\" does not exist"))
        } else {
            DdlError::TableNotFound(format!("schema \"{schema}\" does not exist"))
        });
    };
    match snapshot.class_by_qname.get(&(nsoid, name.clone())) {
        Some(&oid) => Ok((nsoid, oid)),
        // Deliberately not a QualifiedName: RangeVarGetRelidExtended's
        // message is `errmsg("relation \"%s.%s\" does not exist",
        // schemaname, relname)`, the raw names joined by a dot with no
        // quoting — `relation "My Schema.a"b" does not exist` — and the
        // wording must match PG's verbatim.
        None => Err(DdlError::TableNotFound(if rv.schemaname.is_empty() {
            format!("relation \"{name}\" does not exist")
        } else {
            format!("relation \"{}.{name}\" does not exist", rv.schemaname)
        })),
    }
}

/// Extract (schema, name) of an *existing* relation or type from a list of
/// name nodes (e.g. `DROP TABLE` / `DROP TYPE` objects). Handles both
/// `["name"]` and `["schema", "name"]`; an unqualified name is looked up
/// along the search path, relations first, then types.
pub fn extract_names(names: &[Node], snapshot: &PgCatalog) -> (String, String) {
    let parts: Vec<&str> = names.iter().filter_map(node_string).collect();

    match parts.as_slice() {
        [schema, name] => ((*schema).to_owned(), (*name).to_owned()),
        [name] => {
            let found = snapshot
                .resolve_table(None, name)
                .map(|c| c.relnamespace)
                .or_else(|| {
                    snapshot
                        .resolve_type_by_name(None, name)
                        .map(|t| t.typnamespace)
                });
            let schema = found
                .and_then(|ns| snapshot.namespace_name(ns).map(str::to_owned))
                .unwrap_or_else(|| fallback_schema(snapshot));
            (schema, (*name).to_owned())
        }
        _ => ("public".to_owned(), String::new()),
    }
}

/// Extract a schema-qualified key from name nodes.
pub fn names_key(names: &[Node], snapshot: &PgCatalog) -> QualifiedName {
    let (schema, name) = extract_names(names, snapshot);
    QualifiedName::new(schema, name)
}

/// Look up (or implicitly create) the OID of the named namespace.
///
/// PG would error on a missing schema, but the analyzer historically tolerates
/// `CREATE TABLE my_schema.foo` without a prior `CREATE SCHEMA my_schema`. We
/// keep that leniency by registering the schema on demand, allocating a fresh
/// OID. Callers that *need* strict checks (e.g. `ALTER … RENAME TO`) should
/// look at [`PgCatalog::namespace_oid`] directly and surface their own errors.
pub fn ensure_namespace(interp: &mut PgCatalog, name: &str) -> Result<PgNamespaceOid, DdlError> {
    if let Some(oid) = interp.namespace_oid(name) {
        return Ok(oid);
    }
    let oid = PgNamespaceOid::from_nonzero(interp.alloc_oid()?);
    interp.insert_pg_namespace(PgNamespace {
        oid,
        nspname: name.to_owned(),
    });
    Ok(oid)
}

/// The session's temporary schema, created on first use
/// (InitTempTableNamespace).
pub fn temp_namespace(interp: &mut PgCatalog) -> Result<PgNamespaceOid, DdlError> {
    if let Some(oid) = interp.temp_namespace {
        return Ok(oid);
    }
    let oid = ensure_namespace(interp, "pg_temp_1")?;
    interp.temp_namespace = Some(oid);
    Ok(oid)
}

/// Whether `relid` lives in the temporary schema.
pub fn is_temp_relation(interp: &PgCatalog, relid: crate::oid::PgClassOid) -> bool {
    interp.temp_namespace.is_some()
        && interp.pg_class.get(&relid).map(|c| c.relnamespace) == interp.temp_namespace
}

/// LookupCreationNamespace: the schema a new object goes in must exist.
pub fn existing_namespace(interp: &PgCatalog, name: &str) -> Result<PgNamespaceOid, DdlError> {
    interp
        .namespace_oid(name)
        .ok_or_else(|| DdlError::TableNotFound(format!("schema \"{name}\" does not exist")))
}

/// The `(nspoid, name)` a new object is created as
/// (QualifiedNameGetCreationNamespace): the named schema, which must exist,
/// or the creation schema.
pub fn ensure_qualified_name(
    interp: &mut PgCatalog,
    names: &[Node],
) -> Result<(PgNamespaceOid, String), DdlError> {
    let parts: Vec<&str> = names.iter().filter_map(node_string).collect();
    let (schema, name) = match parts.as_slice() {
        [schema, name] => ((*schema).to_owned(), (*name).to_owned()),
        [name] => (creation_schema(interp)?, (*name).to_owned()),
        _ => ("public".to_owned(), String::new()),
    };
    Ok((existing_namespace(interp, &schema)?, name))
}

/// Same as `ensure_qualified_name` but for `RangeVar` inputs
/// (RangeVarGetCreationNamespace).
pub fn ensure_range_var(
    interp: &mut PgCatalog,
    rv: &RangeVar,
) -> Result<(PgNamespaceOid, String), DdlError> {
    // A temporary relation goes in the session's temporary schema; an
    // explicit `pg_temp` makes the relation temporary.
    if rv.relpersistence == "t" || rv.schemaname == "pg_temp" {
        if !rv.schemaname.is_empty() && rv.schemaname != "pg_temp" {
            return Err(DdlError::UnsupportedDdl(
                "cannot create temporary relation in non-temporary schema".into(),
            ));
        }
        return Ok((temp_namespace(interp)?, rv.relname.clone()));
    }
    let schema = if rv.schemaname.is_empty() {
        creation_schema(interp)?
    } else {
        rv.schemaname.clone()
    };
    Ok((existing_namespace(interp, &schema)?, rv.relname.clone()))
}

/// Resolve a `TypeName` AST node to a type OID in the snapshot.
///
/// Handles:
/// - Qualified names: `pg_catalog.int4`
/// - Unqualified names: `int4`, `text`, `uuid`
/// - Array bounds: `int4[]` → array element type OID
/// - Shorthand aliases: `integer` → `int4`, `bigint` → `int8`, etc.
pub fn resolve_type_name(tn: &TypeName, snapshot: &PgCatalog) -> Option<PgTypeOid> {
    lookup_type_name(tn, snapshot).ok()
}

/// Resolve a `TypeName` the way PG's `typenameTypeId` does
/// (`src/backend/parser/parse_type.c`, `LookupTypeNameExtended`), returning
/// PG's own error when the name does not resolve.
///
/// The name is looked up verbatim: the grammar already rewrites the
/// SQL-standard keyword types (`integer`, `char(n)`, `double precision`, …)
/// to their `pg_catalog.<name>` spelling, so an unqualified `"char"` is the
/// internal single-byte type and a quoted `"integer"` does not exist — just
/// like in PG. `tbl.col%TYPE` references resolve to the column's type.
pub fn lookup_type_name(tn: &TypeName, snapshot: &PgCatalog) -> Result<PgTypeOid, DdlError> {
    if let Some(oid) = PgTypeOid::new(tn.type_oid) {
        return Ok(oid);
    }
    let parts: Vec<&str> = tn
        .names
        .iter()
        .filter_map(|n| match n.node.as_ref()? {
            node::Node::String(s) => Some(s.sval.as_str()),
            _ => None,
        })
        .collect();

    let base_oid = if tn.pct_type {
        lookup_pct_type(&parts, snapshot)?
    } else {
        let (schema, name) = match parts.as_slice() {
            [name] => (None, *name),
            [schema, name] => (Some(*schema), *name),
            // `catalog.schema.name`: PG only accepts the current database,
            // which the analyzer has no notion of.
            _ => {
                return Err(DdlError::Parse(format!(
                    "improper qualified name (too many dotted names): {}",
                    parts.join(".")
                )));
            }
        };
        if let Some(schema) = schema
            && snapshot.namespace_oid(schema).is_none()
        {
            return Err(DdlError::TypeNotFound(format!(
                "schema \"{schema}\" does not exist"
            )));
        }
        match snapshot.resolve_type_by_name(schema, name) {
            Some(t) => t.oid,
            None => {
                return Err(DdlError::TypeNotFound(format!(
                    "type \"{}\" does not exist",
                    type_name_to_string(tn)
                )));
            }
        }
    };

    if !tn.array_bounds.is_empty() {
        return snapshot.array_type_of(base_oid).ok_or_else(|| {
            DdlError::TypeNotFound(format!(
                "could not find array type for data type {}",
                format_type_for_message(snapshot, base_oid)
            ))
        });
    }
    Ok(base_oid)
}

/// `rel.col%TYPE` / `schema.rel.col%TYPE`: the referenced column's type.
/// Mirrors the `typeName->pct_type` branch of `LookupTypeNameExtended`.
fn lookup_pct_type(parts: &[&str], snapshot: &PgCatalog) -> Result<PgTypeOid, DdlError> {
    let (schema, rel, col) = match parts {
        [rel, col] => (None, *rel, *col),
        [schema, rel, col] => (Some(*schema), *rel, *col),
        [_db, schema, rel, col] => (Some(*schema), *rel, *col),
        _ => {
            return Err(DdlError::Parse(format!(
                "improper %TYPE reference (too few dotted names): {}",
                parts.join(".")
            )));
        }
    };
    if let Some(schema) = schema
        && snapshot.namespace_oid(schema).is_none()
    {
        return Err(DdlError::TypeNotFound(format!(
            "schema \"{schema}\" does not exist"
        )));
    }
    let class = snapshot.resolve_table(schema, rel).ok_or_else(|| {
        let shown = match schema {
            Some(s) => QualifiedName::new(s, rel).to_string(),
            None => rel.to_owned(),
        };
        DdlError::TableNotFound(format!("relation \"{shown}\" does not exist"))
    })?;
    snapshot
        .attribute_by_name(class.oid, col)
        .map(|a| a.atttypid)
        .ok_or_else(|| {
            DdlError::Parse(format!(
                "column \"{col}\" of relation \"{rel}\" does not exist"
            ))
        })
}

/// PG's `TypeNameToString`: the dotted name list (unquoted), `%TYPE` for
/// column references and `[]` when array bounds are present.
pub fn type_name_to_string(tn: &TypeName) -> String {
    let mut out = tn
        .names
        .iter()
        .filter_map(node_string)
        .collect::<Vec<_>>()
        .join(".");
    if tn.pct_type {
        out.push_str("%TYPE");
    }
    if !tn.array_bounds.is_empty() {
        out.push_str("[]");
    }
    out
}

/// Normalize PostgreSQL type name aliases to their canonical form, the way
/// the SQL grammar would for a bare keyword. Only meaningful for type names
/// that went through no grammar at all (e.g. the text of a `regtype`
/// literal); `TypeName` nodes from `typedpg_pg_query` are already canonical.
pub(crate) fn normalize_type_name(name: &str) -> &str {
    match name {
        "integer" | "int" => "int4",
        "smallint" => "int2",
        "bigint" => "int8",
        "real" => "float4",
        "double precision" | "double" => "float8",
        "boolean" => "bool",
        "character varying" | "varchar" => "varchar",
        "character" | "char" => "bpchar",
        "decimal" | "numeric" => "numeric",
        "serial" => "int4",
        "bigserial" => "int8",
        "smallserial" => "int2",
        other => other,
    }
}

/// Render a type OID into PG's user-facing name for diagnostic messages.
///
/// Mirrors `format_type_extended` in `src/backend/utils/adt/format_type.c`
/// (PG 18) with `flags = 0` (no typmod, no force-qualify):
///
/// 1. Arrays: when the type's `typelem` points at a real array element
///    type, render as `<element>[]` (skipping pseudo-arrays / plain-storage
///    arrays like `oidvector`, same check PG does).
/// 2. Special-case the SQL-standard built-ins (`BOOL` → `boolean`,
///    `INT4` → `integer`, `TIMESTAMP` → `timestamp without time zone`, etc.).
/// 3. Otherwise: render the catalog name, qualified with the schema if the
///    type isn't visible on the search path (`pg_catalog` builtins stay
///    unqualified). `QualifiedName::Display` handles the PG identifier
///    quoting rules.
pub fn format_type_for_message(snapshot: &PgCatalog, oid: PgTypeOid) -> String {
    let Some(t) = snapshot.pg_type.get(&oid) else {
        return format!("oid={oid}");
    };

    // Array deconstruction. PG checks `IsTrueArrayType(typeform) &&
    // typeform->typstorage != TYPSTORAGE_PLAIN` — we approximate by
    // requiring the element's canonical `typarray` to point back at this
    // type, which excludes `oidvector`/`int2vector` (plain-storage
    // pseudo-arrays that PG renders by their own name, e.g.
    // `function to_char(oidvector) does not exist`).
    if t.typcategory == crate::pg_catalog::TypCategory::Array
        && let Some(elem_oid) = t.typelem
        && snapshot.array_type_of(elem_oid) == Some(oid)
    {
        return format!("{}[]", format_type_for_message(snapshot, elem_oid));
    }

    // Special-case the SQL-standard built-ins. Mirrors the big `switch
    // (type_oid)` block in `format_type_extended`.
    let aliased = match t.typname.as_str() {
        "bool" => Some("boolean"),
        // The internal single-byte type (OID 18) — PG always renders it
        // double-quoted as `"char"` to distinguish it from SQL `char`/`bpchar`.
        "char" => Some("\"char\""),
        "int2" => Some("smallint"),
        "int4" => Some("integer"),
        "int8" => Some("bigint"),
        "float4" => Some("real"),
        "float8" => Some("double precision"),
        "bpchar" => Some("character"),
        "varchar" => Some("character varying"),
        "varbit" => Some("bit varying"),
        "bit" => Some("bit"),
        "timestamp" => Some("timestamp without time zone"),
        "timestamptz" => Some("timestamp with time zone"),
        "time" => Some("time without time zone"),
        "timetz" => Some("time with time zone"),
        "interval" => Some("interval"),
        "numeric" => Some("numeric"),
        "json" => Some("json"),
        _ => None,
    };
    if let Some(name) = aliased {
        return name.to_string();
    }

    // Default handling: catalog name, qualified iff the type isn't visible
    // on the search path. `pg_catalog` types are always visible without
    // qualification; for everything else we ask the lookup.
    let visible = snapshot
        .resolve_type_by_name(None, &t.typname)
        .map(|found| found.oid == oid)
        .unwrap_or(false);
    if !visible && let Some(ns) = snapshot.namespace_name(t.typnamespace) {
        return QualifiedName::new(ns, &t.typname).to_string();
    }
    t.typname.clone()
}

/// PG's `NAMEDATALEN - 1`: the longest identifier, in bytes.
const MAX_IDENTIFIER_BYTES: usize = 63;

/// Truncate `s` to at most `max` bytes on a char boundary (`pg_mbcliplen`).
fn clip_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// PG's `makeObjectName` (`indexcmds.c`): `name1[_name2][_label]`, with the
/// longer of `name1` / `name2` shortened until the result fits in an
/// identifier.
pub fn make_object_name(name1: &str, name2: &str, label: &str) -> String {
    let mut overhead = 0;
    if !name2.is_empty() {
        overhead += 1;
    }
    if !label.is_empty() {
        overhead += label.len() + 1;
    }
    let avail = MAX_IDENTIFIER_BYTES.saturating_sub(overhead);
    let (mut n1, mut n2) = (name1.len(), name2.len());
    while n1 + n2 > avail {
        if n1 > n2 {
            n1 -= 1;
        } else {
            n2 -= 1;
        }
    }
    let mut out = clip_bytes(name1, n1).to_owned();
    if !name2.is_empty() {
        out.push('_');
        out.push_str(clip_bytes(name2, n2));
    }
    if !label.is_empty() {
        out.push('_');
        out.push_str(label);
    }
    out
}

/// PG's `ChooseRelationName`: [`make_object_name`], appending a counter to
/// the label (`t_a_key1`, `t_a_key2`, …) until no relation in the schema
/// has the name.
pub fn choose_relation_name(
    snapshot: &PgCatalog,
    nsoid: PgNamespaceOid,
    name1: &str,
    name2: &str,
    label: &str,
) -> String {
    let mut pass = 0;
    loop {
        let modlabel = if pass == 0 {
            label.to_owned()
        } else {
            format!("{label}{pass}")
        };
        let name = make_object_name(name1, name2, &modlabel);
        if !snapshot.class_by_qname.contains_key(&(nsoid, name.clone())) {
            return name;
        }
        pass += 1;
    }
}

/// PG's `ChooseRelationName` with `isconstraint`: the name of a
/// constraint's index must be free as a relation name and as a constraint
/// name in the schema.
pub fn choose_constraint_index_name(
    snapshot: &PgCatalog,
    nsoid: PgNamespaceOid,
    name1: &str,
    name2: &str,
    label: &str,
) -> String {
    let constraint_taken = |name: &str| {
        snapshot.pg_constraint.values().any(|c| {
            c.conname == name
                && snapshot.pg_class.get(&c.conrelid).map(|r| r.relnamespace) == Some(nsoid)
        })
    };
    let mut pass = 0;
    loop {
        let modlabel = if pass == 0 {
            label.to_owned()
        } else {
            format!("{label}{pass}")
        };
        let name = make_object_name(name1, name2, &modlabel);
        if !snapshot.class_by_qname.contains_key(&(nsoid, name.clone())) && !constraint_taken(&name)
        {
            return name;
        }
        pass += 1;
    }
}

/// PG's `ChooseIndexColumnNames` + `ChooseIndexNameAddition`: the index
/// columns' names (`expr` for expressions, deduplicated with a counter),
/// joined with `_`.
pub fn index_name_addition(colnames: &[String]) -> String {
    let mut chosen: Vec<String> = Vec::new();
    for base in colnames {
        let mut name = base.clone();
        let mut i = 0;
        while chosen.contains(&name) {
            i += 1;
            name = format!("{base}{i}");
        }
        chosen.push(name);
    }
    let mut out = String::new();
    for name in &chosen {
        if !out.is_empty() {
            out.push('_');
        }
        out.push_str(name);
        if out.len() > MAX_IDENTIFIER_BYTES {
            break;
        }
    }
    out
}

/// `heap_create_with_catalog`'s name checks for a new relation: no relation
/// of that name may exist in the schema, and neither may a type (every
/// relation with a row type claims the name in `pg_type` too). An
/// auto-generated array type is not a conflict — PG renames it out of the
/// way.
pub fn check_relation_name_free(
    snapshot: &PgCatalog,
    nsoid: PgNamespaceOid,
    name: &str,
) -> Result<(), DdlError> {
    if snapshot
        .class_by_qname
        .contains_key(&(nsoid, name.to_owned()))
    {
        return Err(DdlError::DuplicateObject(format!(
            "relation \"{name}\" already exists"
        )));
    }
    if let Some(t) = snapshot
        .type_by_qname
        .get(&(nsoid, name.to_owned()))
        .and_then(|oid| snapshot.pg_type.get(oid))
    {
        let is_auto_array = t.typcategory == crate::pg_catalog::TypCategory::Array
            && t.typelem
                .is_some_and(|e| snapshot.array_type_of(e) == Some(t.oid));
        if !is_auto_array {
            return Err(DdlError::DuplicateObject(format!(
                "type \"{name}\" already exists"
            )));
        }
    }
    Ok(())
}

/// PG's `format_type_with_typemod`: [`format_type_for_message`] plus the
/// type modifier, e.g. `character varying(5)`, `numeric(10,2)`,
/// `timestamp(3) without time zone`.
pub fn format_type_with_typmod(
    snapshot: &PgCatalog,
    oid: PgTypeOid,
    typmod: Option<i32>,
) -> String {
    use crate::typmod::DecodedTypmod;
    let base = format_type_for_message(snapshot, oid);
    let modifier = match crate::typmod::decode(snapshot, oid, typmod) {
        DecodedTypmod::None => return base,
        DecodedTypmod::Length(n) | DecodedTypmod::Precision(n) | DecodedTypmod::VectorDim(n) => {
            format!("({n})")
        }
        DecodedTypmod::Numeric { precision, scale } => format!("({precision},{scale})"),
        DecodedTypmod::Other(n) => format!("({n})"),
    };
    // The datetime types carry the modifier before their zone suffix.
    for (head, tail) in [
        ("timestamp", " without time zone"),
        ("timestamp", " with time zone"),
        ("time", " without time zone"),
        ("time", " with time zone"),
    ] {
        if base == format!("{head}{tail}") {
            return format!("{head}{modifier}{tail}");
        }
    }
    format!("{base}{modifier}")
}

/// Extract a string value from a Node.
pub fn node_string(n: &Node) -> Option<&str> {
    match n.node.as_ref()? {
        node::Node::String(s) => Some(s.sval.as_str()),
        _ => None,
    }
}

/// Register the implicit `composite_oid → record` cast PG creates for every
/// composite type. Used by the operator resolver so that `composite =
/// composite` reaches the polymorphic `record = record` (record_eq) operator
/// via cast lookup.
pub fn register_composite_to_record_cast(
    interp: &mut PgCatalog,
    composite_oid: PgTypeOid,
) -> Result<(), DdlError> {
    let cast_oid = PgCastOid::from_nonzero(interp.alloc_oid()?);
    interp.insert_pg_cast(PgCast {
        oid: cast_oid,
        castsource: composite_oid,
        casttarget: builtin_oid::RECORD,
        castcontext: CastContext::Implicit,
        castmethod: CastMethod::Binary,
        castfunc: None,
    });
    Ok(())
}
