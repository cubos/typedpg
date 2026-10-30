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

/// What a column read here is: its own refinement, and the values its
/// type, its table's CHECK constraints and the conditions holding here
/// leave it (`WHERE kind IN ('a', 'b')` makes `kind` one of those).
fn column_refinement(col: &crate::scope::ScopeColumn, ctx: Ctx<'_>) -> crate::refine::Refinement {
    let mut refine = col.refine.clone();
    if crate::refine::Exact::of(col.type_oid, col.collation, ctx.snapshot).is_some() {
        let space = crate::nonnull::checks::Space::of(col.type_oid, col.collation, ctx.snapshot)
            .with_values(col.refine.enumerated().as_ref());
        let key = (col.table_alias.clone(), col.name.clone());
        let preds = ctx.null_ctx.column_preds(&key);
        if let Some(cands) = space.candidates_of(&preds) {
            refine.values = cands.iter().map(|l| space.print(l)).collect();
        }
        if let Some(bounds) = space
            .bounds(&preds)
            .filter(|b| *b != crate::refine::IntRange::default())
        {
            refine.range = Some(match refine.range {
                Some(r) => r.meet(&bounds),
                None => bounds,
            });
        }
    } else {
        refine.values = None;
    }
    refine
}

pub(crate) fn infer_column_ref(
    col_ref: &protobuf::ColumnRef,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
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
                // A Var takes its column's collation as is: none for a
                // subquery column that has none (strength NONE).
                no_collation: col.no_collation.then_some(super::NoCollation::Unset),
                planned: col.planned(),
                // Carry the column's record shape forward so downstream
                // `(col).field` indirection and ROW-vs-shape coercion can
                // see through to the field types.
                record_fields: col.record_fields.clone(),
                elem_nullable: col.elem_nullable,
                // (No relation has an `unknown` column: a function in FROM
                // returning it is rejected.)
                unknown_value: false,
                refine: column_refinement(col, ctx),
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
                scope.check_refname_unique(column, None)?;
                return whole_row_ref(src, null_ctx, snapshot);
            }
            // `t.f` naming no column of a FROM entry `t` is the function
            // call `f(t)` on its whole row (transformColumnRef).
            if let Some(table) = table
                && matches!(e, AnalyzeError::UndefinedColumn(_))
                && scope.find_source(table).is_some()
            {
                let mut fields = col_ref.fields[..col_ref.fields.len() - 1].to_vec();
                fields.push(protobuf::Node {
                    node: Some(node::Node::AStar(protobuf::AStar {})),
                });
                let whole_row = protobuf::Node {
                    node: Some(node::Node::ColumnRef(protobuf::ColumnRef {
                        fields,
                        location: col_ref.location,
                    })),
                };
                if let Some(call) = super::func_call::infer_column_notation_call(
                    column,
                    whole_row,
                    col_ref.location,
                    ctx,
                    params,
                ) {
                    return call;
                }
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

/// `rel.*` that names no relation, read as PG's `ExpandColumnRefStar` reads
/// it once the relation lookup failed: through the parser hook, which in a
/// SQL function body (`sql_fn_post_column_ref`) takes `v.*`, `f.v.*` or
/// `v.fld.*` for parameter `v` — whose fields the star then stands for
/// (`ExpandRowReference`). Returns that as the equivalent `(v).*`.
pub(crate) fn sql_function_param_star(
    col_ref: &protobuf::ColumnRef,
    scope: &Scope,
    snapshot: &PgCatalog,
) -> Option<protobuf::AIndirection> {
    let (star, names) = col_ref.fields.split_last()?;
    if !matches!(star.node.as_ref(), Some(node::Node::AStar(_)))
        || !names
            .iter()
            .all(|f| matches!(f.node.as_ref(), Some(node::Node::String(_))))
    {
        return None;
    }
    let names_str = extract_string_fields(names);
    if scope.find_source(names_str.last()?).is_some() {
        return None;
    }
    sql_function_param(&names_str, snapshot)?;
    Some(protobuf::AIndirection {
        arg: Some(Box::new(protobuf::Node {
            node: Some(node::Node::ColumnRef(protobuf::ColumnRef {
                fields: names.to_vec(),
                location: col_ref.location,
            })),
        })),
        indirection: vec![star.clone()],
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

    scope.check_refname_unique(alias, None)?;
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
                    field.ty.nullable = !snapshot.attr_proven_not_null(a);
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
                    no_collation: c.no_collation.then_some(super::NoCollation::Unset),
                    planned: c.planned(),
                    record_fields: c.record_fields.clone(),
                    elem_nullable: c.elem_nullable,
                    refine: c.refine.clone(),
                    unknown_value: false,
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
        no_collation: None,
        planned: super::Planned::AsParsed,
        record_fields: Some(shape_of_columns(&source.columns).into()),
        elem_nullable: None,
        refine: crate::refine::Refinement::NONE,
        unknown_value: false,
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
                no_collation: c.no_collation.then_some(super::NoCollation::Unset),
                planned: c.planned(),
                record_fields: c.record_fields.clone(),
                elem_nullable: c.elem_nullable,
                refine: c.refine.clone(),
                unknown_value: false,
            },
        })
        .collect()
}
