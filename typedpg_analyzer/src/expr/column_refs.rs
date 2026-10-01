use super::*;

// ──────────────────────────────────────────────────────────────────────────────
// Column references
// ──────────────────────────────────────────────────────────────────────────────

/// transformColumnRef's field-count rules: four fields are
/// `db.schema.table.column` (or `.*`), valid only when `db` is the current
/// database (see [`crate::pgmsg::cross_database_reference`]); more is
/// never a valid reference. Also applies to select-list `db.s.t.*`.
pub(crate) fn check_column_ref_length(col_ref: &protobuf::ColumnRef) -> Result<(), AnalyzeError> {
    if col_ref.fields.len() < 4 {
        return Ok(());
    }
    let written = col_ref
        .fields
        .iter()
        .map(|f| match f.node.as_ref() {
            Some(node::Node::String(s)) => s.sval.clone(),
            _ => "*".to_owned(),
        })
        .collect::<Vec<_>>()
        .join(".");
    let span = crate::error::SourceSpan::from_node_qname(col_ref.location);
    let err = if col_ref.fields.len() == 4 {
        crate::pgmsg::cross_database_reference(&written, span)
    } else {
        crate::pgmsg::improper_qualified_name(&written, span)
    };
    Err(err.finalize_implicit())
}

pub(crate) fn infer_column_ref(
    col_ref: &protobuf::ColumnRef,
    ctx: Ctx<'_>,
) -> Result<ExprType, AnalyzeError> {
    let Ctx {
        scope,
        null_ctx,
        snapshot,
        ..
    } = ctx;
    // Star expansion in expression context. `alias.*` in PG becomes the
    // composite type of the relation referenced by `alias`. `*` alone
    // (no qualifier) could expand to a ROW of every visible source but
    // the semantic is ambiguous enough that we leave it unsupported.
    check_column_ref_length(col_ref)?;
    let has_star = col_ref
        .fields
        .iter()
        .any(|f| matches!(f.node.as_ref(), Some(node::Node::AStar(_))));
    if has_star {
        return infer_star_ref(col_ref, scope, null_ctx, snapshot);
    }

    let parts = extract_string_fields(&col_ref.fields);

    let (table, column) = match parts.as_slice() {
        [col] => (None, col.as_str()),
        [tbl, col] => (Some(tbl.as_str()), col.as_str()),
        [_schema, tbl, col] => (Some(tbl.as_str()), col.as_str()),
        _ => {
            return Err(AnalyzeError::Internal(format!(
                "column reference with a non-name field: {parts:?}"
            )));
        }
    };

    match scope.resolve_column(
        table,
        column,
        crate::error::SourceSpan::from_node_qname(col_ref.location),
    ) {
        Ok(col) => {
            let nullable = null_ctx.is_nullable(&col.table_alias, &col.name, col.base_not_null);
            Ok(ExprType {
                type_oid: col.type_oid,
                nullable,
                typmod: col.typmod,
                // The column's `attcollation` (if any) flows out as-is. PG
                // never overrides it implicitly — only an explicit
                // `COLLATE "x"` decoration on the surrounding expression
                // does.
                collation: col.collation,
                explicit_collation: false,
                // Carry the column's record shape forward so downstream
                // `(col).field` indirection and ROW-vs-shape coercion can
                // see through to the field types.
                record_fields: col.record_fields.clone(),
                elem_nullable: None,
            })
        }
        Err(e) => {
            // PG row-reference fallback: a single unqualified identifier can
            // name a whole row from the FROM clause (`SELECT u FROM users u`
            // or `(u).name`). Only kick in when the column lookup failed AND
            // the identifier matches a table alias in scope — otherwise we'd
            // shadow legitimate UndefinedColumn errors.
            // PG only tries the whole-row reading when no column matched
            // (an ambiguous or otherwise invalid column is an error).
            if table.is_none()
                && matches!(e, AnalyzeError::UndefinedColumn(_))
                && let Some(src) = scope.find_source(column)
            {
                return whole_row_ref(src, null_ctx, snapshot);
            }
            // Inside a SQL function body, a name no column or relation
            // claims may name one of the function's parameters.
            if let Some(t) = sql_function_param(&parts, snapshot) {
                return Ok(t);
            }
            Err(e)
        }
    }
}

/// The parameters of the SQL function whose body is being validated.
pub(crate) struct SqlFunctionParams {
    /// The function's name, which may qualify a parameter (`f.x`).
    pub name: String,
    /// Named input parameters and their declared types.
    pub params: Vec<(String, PgTypeOid)>,
}

thread_local! {
    static SQL_FUNCTION_PARAMS: std::cell::RefCell<Option<SqlFunctionParams>> =
        const { std::cell::RefCell::new(None) };
}

/// Run `f` with `params` as the parameter namespace of column references —
/// what PG installs for a SQL function body (`sql_fn_parser_setup`).
pub(crate) fn with_sql_function_params<R>(params: SqlFunctionParams, f: impl FnOnce() -> R) -> R {
    let prev = SQL_FUNCTION_PARAMS.with(|p| p.replace(Some(params)));
    let out = f();
    SQL_FUNCTION_PARAMS.with(|p| *p.borrow_mut() = prev);
    out
}

/// PG's `sql_fn_post_column_ref`: consulted only once a column reference
/// matched no column or relation, so a column always wins over a same-named
/// parameter. `x` names a parameter; `f.x` a parameter qualified by the
/// function's name; `x.fld` / `f.x.fld` a field of a composite parameter.
fn sql_function_param(parts: &[String], snapshot: &PgCatalog) -> Option<ExprType> {
    SQL_FUNCTION_PARAMS.with(|cell| {
        let guard = cell.borrow();
        let fp = guard.as_ref()?;
        let param = |name: &str| {
            fp.params
                .iter()
                .find(|(n, _)| !n.is_empty() && n == name)
                .map(|&(_, t)| ExprType::scalar(t, true))
        };
        let (value, field) = match parts {
            [p] => (param(p)?, None),
            [f, p, fld] if *f == fp.name => (param(p)?, Some(fld)),
            [f, p] if *f == fp.name && param(p).is_some() => (param(p)?, None),
            [p, fld] => (param(p)?, Some(fld)),
            _ => return None,
        };
        match field {
            None => Some(value),
            Some(fld) => resolve_composite_field(&value, fld, snapshot, None).ok(),
        }
    })
}

/// Resolve `alias.*` (or `schema.alias.*`) to the composite type of the
/// underlying relation. The composite is the per-table `TypeEntry` that
/// `create_table` registers alongside the table — same OID that a call site
/// like `row_to_json(alias.*)` would see at runtime.
/// A whole-row reference is NULL when its entry is on the nullable side of
/// an outer join, or is a RETURNING OLD / NEW row that may not exist.
fn whole_row_nullable(src: &crate::scope::TableSource, null_ctx: &NullabilityContext) -> bool {
    src.null_row || null_ctx.alias_is_nullable(&src.alias)
}

fn infer_star_ref(
    col_ref: &protobuf::ColumnRef,
    scope: &Scope,
    null_ctx: &NullabilityContext,
    snapshot: &PgCatalog,
) -> Result<ExprType, AnalyzeError> {
    // The alias/relname qualifying the star is the last String field before
    // AStar. For `t.*` it's index 0; for `schema.t.*` it's index 1.
    let alias = col_ref
        .fields
        .iter()
        .rev()
        .skip_while(|f| !matches!(f.node.as_ref(), Some(node::Node::AStar(_))))
        .nth(1)
        .and_then(|f| match f.node.as_ref()? {
            node::Node::String(s) => Some(s.sval.as_str()),
            _ => None,
        })
        .ok_or_else(|| {
            AnalyzeError::Unsupported("unqualified * has no relation — use alias.* instead".into())
        })?;

    let source = scope.find_source(alias).ok_or_else(|| {
        AnalyzeError::UndefinedTable(format!("missing FROM-clause entry for table \"{alias}\""))
    })?;
    whole_row_ref(source, null_ctx, snapshot)
}

/// PG's `transformWholeRowRef` / `makeWholeRowVar`: a whole-row reference
/// to a FROM entry (`t` or `t.*` in an expression).
fn whole_row_ref(
    source: &crate::scope::TableSource,
    null_ctx: &NullabilityContext,
    snapshot: &PgCatalog,
) -> Result<ExprType, AnalyzeError> {
    // Real tables / views resolve to their backing composite type so calls
    // like `row_to_json(t.*)` see the registered row OID. CTE and subquery
    // sources have no `source_qn` — PG composes an anonymous row type at
    // planning time, so we surface `pg_catalog.record` with the source's
    // columns threaded as the record shape. The shape lets downstream
    // `(t.*).field` indirection still resolve, and the `record` OID lines
    // up with what PG's wire-protocol Describe reports for these queries.
    if let Some(qn) = source.source_qn.as_ref() {
        let composite_oid = snapshot
            .namespace_oid(&qn.schema)
            .and_then(|nsoid| {
                snapshot
                    .type_by_qname
                    .get(&(nsoid, qn.name.clone()))
                    .copied()
            })
            .ok_or_else(|| {
                AnalyzeError::UndefinedType(format!(
                    "internal: no composite type registered for relation {qn}"
                ))
            })?;
        // A row read from the relation keeps its columns' NOT NULL, which
        // a value of the row type in general doesn't (`ROW(NULL)::t`, a
        // column of type `t`): carry the columns as the value's shape, the
        // only place field nullability is taken from for a composite.
        let mut shape = shape_of_columns(&source.columns);
        // A RETURNING OLD / NEW row may be missing (`null_row`), which its
        // columns fold into their own nullability; a row that is there
        // has the target's NOT NULL columns.
        if source.null_row
            && let Some(relid) = source.relid
        {
            let attrs = snapshot.attributes_of(relid);
            for field in &mut shape {
                if let Some(a) = attrs.iter().find(|a| a.attname == field.name) {
                    field.ty.nullable = !(snapshot.attr_proven_not_null(a)
                        || snapshot.type_is_not_null(a.atttypid));
                }
            }
        }
        return Ok(ExprType {
            record_fields: Some(shape.into()),
            ..ExprType::scalar(composite_oid, whole_row_nullable(source, null_ctx))
        });
    }

    match source.whole_row {
        // A single function returning a named composite keeps its type.
        crate::scope::WholeRow::Composite(t) => {
            return Ok(ExprType::scalar(t, whole_row_nullable(source, null_ctx)));
        }
        // A single scalar function: the reference is the function's value.
        crate::scope::WholeRow::Scalar => {
            if let Some(c) = source.columns.first() {
                return Ok(ExprType {
                    type_oid: c.type_oid,
                    nullable: null_ctx.is_nullable(&c.table_alias, &c.name, c.base_not_null),
                    typmod: c.typmod,
                    collation: c.collation,
                    explicit_collation: false,
                    record_fields: c.record_fields.clone(),
                    elem_nullable: None,
                });
            }
        }
        crate::scope::WholeRow::Record => {}
    }

    Ok(ExprType {
        type_oid: oid::RECORD,
        nullable: whole_row_nullable(source, null_ctx),
        typmod: None,
        collation: None,
        explicit_collation: false,
        record_fields: Some(shape_of_columns(&source.columns).into()),
        elem_nullable: None,
    })
}

/// The row shape of a FROM entry's columns, each field NULL only where the
/// column itself may be (outer-join nullability is the whole row's).
fn shape_of_columns(columns: &[crate::scope::ScopeColumn]) -> Vec<RecordField> {
    columns
        .iter()
        .map(|c| RecordField {
            name: c.name.clone(),
            ty: ExprType {
                type_oid: c.type_oid,
                nullable: !c.base_not_null,
                typmod: c.typmod,
                collation: c.collation,
                explicit_collation: false,
                record_fields: c.record_fields.clone(),
                elem_nullable: None,
            },
        })
        .collect()
}
