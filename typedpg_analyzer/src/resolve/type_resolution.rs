use super::*;

// ──────────────────────────────────────────────────────────────────────────────
// Type resolution
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn build_column(
    rc: RawColumn,
    snapshot: &PgCatalog,
) -> Result<AnalyzedColumn, AnalyzeError> {
    let pg_type = resolve_type_with_shape(
        rc.type_oid,
        rc.typmod,
        rc.collation,
        rc.record_fields.as_deref(),
        snapshot,
    )?;

    // Handle nullability annotations (! and ?).
    let (name, nullable) = parse_nullability_annotation(&rc.name, rc.nullable);

    Ok(AnalyzedColumn {
        name,
        pg_type,
        nullable,
    })
}

/// Like [`resolve_type`] but lets the caller override the structural shape
/// when the OID is the pseudo `record` type (typmod -1 in PG terms). When
/// `shape` is `Some` and the OID is `record`, we build a `Type::AnonymousRecord`
/// from the shape recursively. Otherwise falls through to the OID-only path.
fn resolve_type_with_shape(
    type_oid: PgTypeOid,
    typmod: Option<i32>,
    collation: Option<crate::oid::PgCollationOid>,
    shape: Option<&[crate::expr::RecordField]>,
    snapshot: &PgCatalog,
) -> Result<Type, AnalyzeError> {
    if type_oid == oid::RECORD
        && let Some(fields) = shape
    {
        let mut out = Vec::with_capacity(fields.len());
        for f in fields {
            out.push(crate::types::RecordField {
                name: f.name.clone(),
                ty: resolve_type_with_shape(
                    f.ty.type_oid,
                    f.ty.typmod,
                    f.ty.collation,
                    f.ty.record_fields.as_deref(),
                    snapshot,
                )?,
                nullable: f.ty.nullable,
            });
        }
        return Ok(Type::AnonymousRecord { fields: out });
    }
    resolve_type(type_oid, typmod, collation, snapshot)
}

/// The [`Type`] of a table column: its type with the column's typmod and
/// collation.
pub(crate) fn column_type(
    type_oid: PgTypeOid,
    typmod: Option<i32>,
    collation: Option<crate::oid::PgCollationOid>,
    snapshot: &PgCatalog,
) -> Result<Type, AnalyzeError> {
    resolve_type(type_oid, typmod, collation, snapshot)
}

pub(crate) fn build_param_info(
    type_oid: PgTypeOid,
    nullable: bool,
    snapshot: &PgCatalog,
) -> Result<ParamInfo, AnalyzeError> {
    // Params have no analyzer-visible typmod (they're typeless until
    // bound, and the cast-introduced typmod is consumed at the cast site
    // itself). Match PG's `pg_param_collator`/`exec_describe_params` which
    // does not include atttypmod for parameters.
    let pg_type = resolve_type(type_oid, None, None, snapshot)?;
    Ok(ParamInfo { pg_type, nullable })
}

/// Resolve a [`PgCollationOid`] to a printable name, suppressing the
/// database default ("default", oid 100). Used when materializing
/// `Type::Basic` / `Type::Domain` from a column or expression site.
fn collation_name(
    collation: Option<crate::oid::PgCollationOid>,
    snapshot: &PgCatalog,
) -> Option<String> {
    let oid = collation?;
    let row = snapshot.pg_collation.get(&oid)?;
    if row.collname == "default" {
        None
    } else {
        Some(row.collname.clone())
    }
}

/// Build the PG-facing [`Type`] for an OID, recursing through Domain/Array
/// wrappers. Unknown OIDs (pseudo `UNKNOWN` included) are surfaced as
/// [`Type::Basic`] named `pg_catalog.unknown` so consumers can fall back to
/// `String` without the analyzer having to know about Rust.
///
/// `typmod` is the column / expression-level modifier observed in the
/// outer scope; resolve_type will surface it on the matching variant. The
/// inner recursion clears `typmod` for nested types (e.g. domain's base
/// type, array's element) since the seed-level `typtypmod` already tracks
/// per-type defaults.
///
/// `collation` is the `pg_collation.oid` resolved on the column /
/// expression site. When present, the database default ("default", oid
/// 100) is suppressed and any other collation is rendered as its name on
/// `Type::Basic` / `Type::Domain`.
fn resolve_type(
    type_oid: PgTypeOid,
    typmod: Option<i32>,
    collation: Option<crate::oid::PgCollationOid>,
    snapshot: &PgCatalog,
) -> Result<Type, AnalyzeError> {
    let coll_name = collation_name(collation, snapshot);
    if let Some(te) = snapshot.get_type(type_oid) {
        let schema = snapshot
            .namespace_name(te.typnamespace)
            .map(str::to_owned)
            .unwrap_or_else(|| "pg_catalog".to_owned());
        let name = te.typname.clone();
        let extension = snapshot.extension_of_type(type_oid).map(str::to_owned);

        // Arrays first: in PG, `_int4` is typtype=Base + typcategory=Array +
        // typelem=int4. They aren't a separate `typtype`.
        if te.typcategory == TypCategory::Array
            && let Some(elem) = te.typelem
        {
            // Array columns store the modifier on the element type
            // (`varchar(20)[]` → element typmod = 24, array typmod = -1).
            // Collation propagates through to the element — PG attaches it
            // to the element type, not the array wrapper.
            let element = resolve_type(elem, typmod, collation, snapshot)?;
            return Ok(Type::Array {
                element: Box::new(element),
            });
        }

        match te.typtype {
            TypType::Domain => {
                let base_oid = te.typbasetype.ok_or_else(|| {
                    AnalyzeError::UndefinedType(format!(
                        "internal: domain (OID {}) has no base type",
                        type_oid.get()
                    ))
                })?;
                // Domains inherit their base typmod when the column didn't
                // pin one. Recurse without re-applying so the base sees its
                // own seed-level value (or `None`).
                let base = resolve_type(base_oid, None, None, snapshot)?;
                let effective_typmod = typmod.or(te.typtypmod);
                return Ok(Type::Domain {
                    schema,
                    name,
                    base: Box::new(base),
                    extension,
                    typmod: effective_typmod,
                    collation: coll_name,
                });
            }
            TypType::Enum => {
                let labels = snapshot
                    .enum_labels_of(type_oid)
                    .into_iter()
                    .map(str::to_owned)
                    .collect();
                return Ok(Type::Enum {
                    schema,
                    name,
                    labels,
                    extension,
                });
            }
            TypType::Range | TypType::Multirange => {
                let multirange = te.typtype == TypType::Multirange;
                // `pg_range` is keyed by the range; a multirange's subtype
                // is its range's.
                let range_oid = if multirange {
                    snapshot.range_of_multirange(type_oid)
                } else {
                    Some(type_oid)
                };
                let subtype_oid = range_oid
                    .and_then(|r| snapshot.range_subtype(r))
                    .unwrap_or(oid::UNKNOWN);
                let subtype = resolve_type(subtype_oid, None, None, snapshot)?;
                return Ok(Type::Range {
                    schema,
                    name,
                    subtype: Box::new(subtype),
                    extension,
                    typmod,
                    multirange,
                });
            }
            TypType::Composite => {
                // Real composite (named via `CREATE TYPE … AS (…)` or the
                // implicit row type of a table). Surface both the
                // schema-qualified identity and the decomposed field list so
                // the macro layer can synthesize per-field accessors without
                // an extra catalog round-trip, while pg_sanity / Display can
                // render the composite by name (matching PG's wire-protocol
                // Describe OID).
                let attrs = if let Some(relid) = te.typrelid {
                    snapshot.attributes_of(relid).to_vec()
                } else {
                    Vec::new()
                };
                let mut out = Vec::with_capacity(attrs.len());
                for f in &attrs {
                    out.push(crate::types::RecordField {
                        name: f.attname.clone(),
                        ty: resolve_type(f.atttypid, f.atttypmod, f.attcollation, snapshot)?,
                        nullable: !f.attnotnull,
                    });
                }
                return Ok(Type::Composite {
                    schema,
                    name,
                    fields: out,
                    extension,
                });
            }
            TypType::Base | TypType::Pseudo => {
                return Ok(Type::Basic {
                    schema,
                    name,
                    extension,
                    typmod,
                    collation: coll_name,
                });
            }
        }
    }

    // Fallback for the pseudo UNKNOWN OID when not present in the snapshot.
    if type_oid == oid::UNKNOWN {
        return Ok(Type::Basic {
            schema: "pg_catalog".to_owned(),
            name: "unknown".to_owned(),
            extension: None,
            typmod: None,
            collation: None,
        });
    }

    Err(AnalyzeError::UndefinedType(format!(
        "internal: unknown type OID {}",
        type_oid.get()
    )))
}

fn parse_nullability_annotation(name: &str, auto_nullable: bool) -> (String, bool) {
    // PG's placeholder column name `?column?` ends in `?` but isn't a
    // user-supplied nullability annotation — pass it through untouched.
    if name == "?column?" {
        return (name.to_owned(), auto_nullable);
    }
    if let Some(stripped) = name.strip_suffix('!') {
        (stripped.to_owned(), false)
    } else if let Some(stripped) = name.strip_suffix('?') {
        (stripped.to_owned(), true)
    } else {
        (name.to_owned(), auto_nullable)
    }
}
