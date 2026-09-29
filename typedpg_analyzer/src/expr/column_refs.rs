use super::*;

// ──────────────────────────────────────────────────────────────────────────────
// Column references
// ──────────────────────────────────────────────────────────────────────────────

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
    let has_star = col_ref
        .fields
        .iter()
        .any(|f| matches!(f.node.as_ref(), Some(node::Node::AStar(_))));
    if has_star {
        return infer_star_ref(col_ref, scope, snapshot);
    }

    let parts = extract_string_fields(&col_ref.fields);

    let (table, column) = match parts.as_slice() {
        [col] => (None, col.as_str()),
        [tbl, col] => (Some(tbl.as_str()), col.as_str()),
        [_schema, tbl, col] => (Some(tbl.as_str()), col.as_str()),
        _ => {
            return Err(AnalyzeError::UndefinedColumn(format!(
                "invalid column ref: {:?}",
                parts
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
            })
        }
        Err(e) => {
            // PG row-reference fallback: a single unqualified identifier can
            // name a whole row from the FROM clause (`SELECT u FROM users u`
            // or `(u).name`). Only kick in when the column lookup failed AND
            // the identifier matches a table alias in scope — otherwise we'd
            // shadow legitimate UndefinedColumn errors.
            if table.is_none()
                && let Some(src) = scope.find_source(column)
                && let Some(qn) = src.source_qn.as_ref()
                && let Some(nsoid) = snapshot.namespace_oid(&qn.schema)
                && let Some(&composite_oid) = snapshot.type_by_qname.get(&(nsoid, qn.name.clone()))
            {
                return Ok(ExprType::scalar(composite_oid, false));
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
fn infer_star_ref(
    col_ref: &protobuf::ColumnRef,
    scope: &Scope,
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
        return Ok(ExprType::scalar(composite_oid, false));
    }

    let fields: Vec<RecordField> = source
        .columns
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
            },
        })
        .collect();
    Ok(ExprType {
        type_oid: oid::RECORD,
        nullable: false,
        typmod: None,
        collation: None,
        explicit_collation: false,
        record_fields: Some(fields),
    })
}
