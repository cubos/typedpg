use super::*;

// ──────────────────────────────────────────────────────────────────────────────
// Target list (SELECT columns)
// ──────────────────────────────────────────────────────────────────────────────

/// One output column of the *expanded* target list.
#[derive(Clone, Copy)]
pub(crate) enum ExpandedTarget<'a> {
    /// A plain select-list entry.
    Expr(&'a protobuf::ResTarget),
    /// A column contributed by `*` / `t.*` (`column: Some((alias, name))`)
    /// or by `(row).*` (`None` — a field, not a range-table column);
    /// `location` is the star expression's.
    StarColumn {
        column: Option<(&'a str, &'a str)>,
        location: i32,
    },
}

/// The shape of the *expanded* target list, one entry per output column.
/// PG expands stars in `transformTargetList` before any clause runs, so
/// ordinals in ORDER BY / GROUP BY / DISTINCT ON (`findTargetlistEntrySQL92`)
/// count every column a star contributes. Types are not needed here;
/// `(row).*` is inferred on a scratch collector so the real parameter
/// numbering is left to [`resolve_target_list`].
pub(crate) fn expanded_target_entries<'a>(
    target_list: &'a [protobuf::Node],
    scope: &'a Scope,
    ctx: Ctx<'_>,
    params: &ParamCollector,
) -> Vec<ExpandedTarget<'a>> {
    let mut out = Vec::new();
    for target in target_list {
        let Some(node::Node::ResTarget(rt)) = target.node.as_ref() else {
            continue;
        };
        let Some(val) = &rt.val else {
            continue;
        };
        match val.node.as_ref() {
            Some(node::Node::ColumnRef(cr))
                if cr
                    .fields
                    .iter()
                    .any(|f| matches!(f.node.as_ref(), Some(node::Node::AStar(_)))) =>
            {
                let table_filter = cr.fields.iter().rev().find_map(|f| match f.node.as_ref()? {
                    node::Node::String(s) => Some(s.sval.as_str()),
                    _ => None,
                });
                let cols: Vec<&ScopeColumn> = match table_filter {
                    Some(tbl) => scope
                        .find_source(tbl)
                        .map(|s| s.columns.iter().collect())
                        .unwrap_or_default(),
                    None => scope.star_columns(),
                };
                out.extend(cols.into_iter().map(|c| ExpandedTarget::StarColumn {
                    column: Some((c.table_alias.as_str(), c.name.as_str())),
                    location: cr.location,
                }));
                continue;
            }
            Some(node::Node::AIndirection(ind)) => {
                let mut scratch = params.clone();
                if let Some(fields) = expr::expand_indirection_star(ind, ctx, &mut scratch)
                    .ok()
                    .flatten()
                {
                    let location = crate::error::node_location(val).unwrap_or(-1);
                    out.extend(fields.iter().map(|_| ExpandedTarget::StarColumn {
                        column: None,
                        location,
                    }));
                    continue;
                }
            }
            _ => {}
        }
        out.push(ExpandedTarget::Expr(rt));
    }
    out
}

pub(crate) fn resolve_target_list(
    target_list: &[protobuf::Node],
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<Vec<RawColumn>, AnalyzeError> {
    let Ctx {
        scope,
        null_ctx,
        snapshot,
        ..
    } = ctx;
    let mut columns = Vec::new();

    for (i, target) in target_list.iter().enumerate() {
        let rt = match target.node.as_ref() {
            Some(node::Node::ResTarget(rt)) => rt,
            _ => continue,
        };

        let val = match &rt.val {
            Some(v) => v,
            None => continue,
        };

        // Check for SELECT * or t.*.
        if let Some(node::Node::ColumnRef(cr)) = val.node.as_ref()
            && cr
                .fields
                .iter()
                .any(|f| matches!(f.node.as_ref(), Some(node::Node::AStar(_))))
        {
            expr::check_column_ref_length(cr)?;
            // Star expansion.
            // `t.*` / `schema.t.*`: the entry is named by the last name part.
            let table_filter = cr.fields.iter().rev().find_map(|f| match f.node.as_ref()? {
                node::Node::String(s) => Some(s.sval.as_str()),
                _ => None,
            });

            let star_cols: Vec<&ScopeColumn> = if let Some(tbl) = table_filter {
                // Any entry the reference can see — this level's, a LATERAL
                // one, or an outer query's (PG's `ExpandColumnRefStar`
                // resolves the name like any qualifier).
                match scope.find_source(tbl) {
                    Some(src) => src.columns.iter().collect(),
                    None => {
                        // No such entry: the qualified lookup reports PG's
                        // `missing FROM-clause entry` / `invalid reference`.
                        scope.resolve_column(
                            Some(tbl),
                            "*",
                            crate::error::SourceSpan::from_node_qname(cr.location),
                        )?;
                        Vec::new()
                    }
                }
            } else {
                scope.star_columns()
            };

            for col in star_cols {
                let nullable = null_ctx.is_nullable(&col.table_alias, &col.name, col.base_not_null);
                columns.push(RawColumn {
                    name: col.name.clone(),
                    type_oid: col.type_oid,
                    nullable,
                    typmod: col.typmod,
                    collation: col.collation,
                    record_fields: col.record_fields.clone(),
                });
            }
            continue;
        }

        // `(expr).*` expands to one column per field (ExpandIndirectionStar).
        if let Some(node::Node::AIndirection(ind)) = val.node.as_ref()
            && let Some(fields) = expr::expand_indirection_star(
                ind,
                expr::Ctx::new(scope, null_ctx, snapshot),
                params,
            )?
        {
            for (name, t) in fields {
                unknown_field_output_check(val, t.type_oid)?;
                columns.push(RawColumn {
                    name,
                    type_oid: t.type_oid,
                    nullable: t.nullable,
                    typmod: t.typmod,
                    collation: t.collation,
                    record_fields: t.record_fields,
                });
            }
            continue;
        }

        // No type expectation for SELECT expressions.
        let expr_type = expr::infer_expr(
            val,
            expr::Ctx::new(scope, null_ctx, snapshot),
            params,
            TypeGoal::NONE,
        )?;

        // Determine column name: explicit alias, or inferred from expression.
        let name = if !rt.name.is_empty() {
            rt.name.clone()
        } else {
            // Match PG's default for unnamed expressions: when the target
            // is something we can't infer a name from (operator, literal,
            // arithmetic, …), PG labels the column `?column?` regardless
            // of position. Using the position would diverge from the
            // wire-protocol RowDescription that `pglite_sanity` checks.
            let _ = i;
            infer_column_name(val).unwrap_or_else(|| "?column?".to_string())
        };

        // Inferred shape from the expression (ROW(...), nested indirection,
        // column propagation) takes priority. As a fallback, if the target
        // expression is a direct FuncCall with TABLE/OUT args, lift those so
        // downstream `(alias.col).field` can look them up — this covers the
        // SRF-as-target-list case where the expression itself is the call.
        let record_fields = expr_type.record_fields.or_else(|| {
            if let Some(node::Node::FuncCall(fc)) = val.node.as_ref() {
                resolve_funccall_record_fields(fc, snapshot, params)
            } else {
                None
            }
        });

        // Bare string literals are carried as `text` at the target-list
        // boundary — this matches PG's `select_common_type` behavior at
        // the SELECT output level, and (more importantly) gives UNION /
        // subquery reconciliation a concrete type to compare against so
        // `SELECT 1 UNION SELECT 'x'` fails instead of silently coercing.
        let type_oid = expr::unknown_literal_as_text(Some(val), expr_type.type_oid);
        if matches!(val.node.as_ref(), Some(node::Node::AIndirection(_))) {
            unknown_field_output_check(val, type_oid)?;
        }

        columns.push(RawColumn {
            name,
            type_oid,
            nullable: expr_type.nullable,
            typmod: expr_type.typmod,
            collation: expr_type.collation,
            record_fields,
        });
    }

    Ok(columns)
}

/// PG resolves an `unknown` output column to `text` (`resolveTargetListUnknowns`,
/// or the set operation / INSERT target it feeds), which its `coerce_type`
/// can't do for a field selected from an anonymous record over an untyped
/// literal: `SELECT (ROW(1, 'x')).f2` fails to prepare.
fn unknown_field_output_check(
    val: &protobuf::Node,
    type_oid: PgTypeOid,
) -> Result<(), AnalyzeError> {
    if type_oid != oid::UNKNOWN {
        return Ok(());
    }
    Err(crate::pgmsg::unknown_field_not_coercible(
        "text",
        crate::error::node_location(val).and_then(crate::error::SourceSpan::from_node_qname),
    )
    .finalize_implicit())
}

/// Analyze a `VALUES (…), (…)` list. Each row must have the same arity;
/// column types are unified across rows via `coerce::find_common_type`.
/// Nullability is `true` if any row's element at that position is nullable.
pub(crate) fn analyze_values_lists(
    values_lists: &[protobuf::Node],
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    scope: &Scope,
) -> Result<Vec<RawColumn>, AnalyzeError> {
    let null_ctx = NullabilityContext::default();
    let ctx = || expr::Ctx::new(scope, &null_ctx, snapshot);
    // transformValuesClause: each row goes through transformExpressionList,
    // which expands `rel.*` / `(expr).*` items into one item per column
    // (`VALUES (v.*)` in a LATERAL subquery).
    let rows: Vec<Vec<protobuf::Node>> = values_lists
        .iter()
        .filter_map(|n| match n.node.as_ref()? {
            node::Node::List(l) => {
                Some(expr::expand_row_args(&l.items, ctx(), params).into_owned())
            }
            _ => None,
        })
        .collect();
    // An empty VALUES list would be a grammar error in PG, but we guard
    // anyway for robustness.
    let first = rows
        .first()
        .ok_or_else(|| AnalyzeError::Unsupported("empty VALUES list".into()))?;

    let arity = first.len();

    let mut column_types: Vec<Vec<PgTypeOid>> = vec![Vec::new(); arity];
    let mut column_typmods: Vec<Vec<Option<i32>>> = vec![Vec::new(); arity];
    let mut column_nullable: Vec<bool> = vec![false; arity];

    for row in &rows {
        // PG (SQLSTATE 42601): every row must have the first row's arity.
        if row.len() != arity {
            return Err(crate::pgmsg::values_lists_length(arity, row.len()).finalize_implicit());
        }
        for (i, item) in row.iter().enumerate() {
            let t = expr::infer_expr(item, ctx(), params, TypeGoal::NONE)?;
            check_no_srf_in_clause(item, snapshot, "VALUES")?;
            column_types[i].push(t.type_oid);
            column_typmods[i].push(t.typmod);
            column_nullable[i] |= t.nullable;
        }
    }

    let common: Vec<PgTypeOid> = (0..arity)
        .map(|i| {
            crate::coerce::find_common_type(&column_types[i], snapshot).unwrap_or(oid::UNKNOWN)
        })
        .collect();

    // Back-fill: re-walk cells whose type stayed UNKNOWN with the column's
    // resolved common type as the goal, so `(VALUES (42), ($1))` pins the
    // param to int4 (matching PG's Describe) and string-literal contents
    // get validated. Speculative-walk rules apply: only literal-content
    // rejections propagate.
    for (row_idx, row) in rows.iter().enumerate() {
        for (i, item) in row.iter().enumerate() {
            if common[i] == oid::UNKNOWN {
                continue;
            }
            if column_types[i].get(row_idx) == Some(&oid::UNKNOWN) {
                expr::coerce_unknown_to(item, ctx(), params, common[i])?;
            }
        }
    }

    let columns = (0..arity)
        .map(|i| RawColumn {
            name: format!("column{}", i + 1),
            type_oid: common[i],
            nullable: column_nullable[i],
            // `select_common_typmod`: rows agreeing on type *and* typmod
            // keep it (`VALUES ('x'::varchar(3)), ('yy'::varchar(3))` is
            // varchar(3)).
            typmod: column_types[i]
                .iter()
                .all(|&t| t == common[i])
                .then(|| column_typmods[i].first().copied().flatten())
                .flatten()
                .filter(|m| column_typmods[i].iter().all(|x| *x == Some(*m))),
            // VALUES literals don't carry a column-level collation — PG
            // leaves it indeterminate and lets a surrounding `INSERT INTO
            // table (cols)` re-attach the target column's attcollation.
            collation: None,
            record_fields: None,
        })
        .collect();

    Ok(columns)
}

/// Look up the named output columns (TABLE/OUT args) of `fc`, if any.
/// Used by the target-list walker so a `SELECT _pg_expandarray(…) AS x`
/// records the field list on the produced column.
fn resolve_funccall_record_fields(
    fc: &protobuf::FuncCall,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> Option<Vec<crate::expr::RecordField>> {
    let parts = expr::extract_string_fields(&fc.funcname);
    let (schema, name) = match parts.as_slice() {
        [n] => (None, n.as_str()),
        [s, n] => (Some(s.as_str()), n.as_str()),
        _ => return None,
    };
    // Args inferred in an empty scope — we only need their types to drive
    // overload resolution, and FuncCall args don't see the enclosing FROM.
    let empty_scope = Scope::default();
    let empty_null = NullabilityContext::default();
    let mut arg_types = Vec::with_capacity(fc.args.len());
    for a in &fc.args {
        let t = expr::infer_expr(
            a,
            expr::Ctx::new(&empty_scope, &empty_null, snapshot),
            params,
            TypeGoal::NONE,
        )
        .map(|e| e.type_oid)
        .unwrap_or(oid::UNKNOWN);
        arg_types.push(t);
    }
    let notation = functions::CallNotation::of(fc).ok()?;
    let resolved =
        functions::resolve_function(snapshot, schema, name, &arg_types, &notation, false, None)
            .ok()?;
    if resolved.out_args.is_empty() {
        None
    } else {
        Some(crate::expr::RecordField::from_out_args(&resolved.out_args))
    }
}

/// Try to infer a default column name from an expression (for unaliased columns).
fn infer_column_name(node: &protobuf::Node) -> Option<String> {
    figure_colname(node).1
}

/// Mirror PostgreSQL's `FigureColnameInternal` (`parse_target.c`): derive the
/// implicit output-column name for an unaliased target, returning the chosen
/// name together with PG's *strength*.
///
/// Strength encodes how confident the name is: `2` = a strong name (a column
/// reference, function call, …), `1` = a weak fallback (a cast's target type
/// name, the literal `case`), `0` = no name at all (the caller substitutes
/// `?column?`). The strength is what makes `col::type` resolve to `col` while
/// `1::int` resolves to `int4`: a cast keeps its argument's name only when the
/// argument produced a strong one, otherwise it falls back to the type name.
/// The same rule lets `CASE … ELSE col END` be named after the `ELSE` branch
/// and `arr[1]` after the subscripted array.
fn figure_colname(node: &protobuf::Node) -> (i32, Option<String>) {
    let Some(inner) = node.node.as_ref() else {
        return (0, None);
    };
    match inner {
        node::Node::ColumnRef(cr) => match expr::extract_string_fields(&cr.fields).pop() {
            // Last string field is the column name (ignoring a trailing `.*`).
            Some(name) => (2, Some(name)),
            None => (0, None),
        },
        node::Node::AIndirection(ind) => {
            // A trailing field access (`(x).field`) names the column after the
            // field; a pure subscript (`arr[1]`, `data['k']`) inherits the
            // argument's name.
            if let Some(field) = expr::extract_string_fields(&ind.indirection).pop() {
                (2, Some(field))
            } else if let Some(arg) = ind.arg.as_deref() {
                figure_colname(arg)
            } else {
                (0, None)
            }
        }
        node::Node::FuncCall(fc) => match expr::extract_string_fields(&fc.funcname).pop() {
            // Function name.
            Some(name) => (2, Some(name)),
            None => (0, None),
        },
        // A `::T` / `CAST(… AS T)` keeps its argument's name when the argument
        // yielded a strong one; otherwise it falls back to the target type's
        // (last) name with the weak strength `1`.
        node::Node::TypeCast(tc) => {
            let arg = tc.arg.as_deref().map_or((0, None), figure_colname);
            if arg.0 <= 1
                && let Some(ty) = tc
                    .type_name
                    .as_ref()
                    .and_then(|tn| expr::extract_string_fields(&tn.names).pop())
            {
                return (1, Some(ty));
            }
            arg
        }
        node::Node::CollateClause(cc) => cc.arg.as_deref().map_or((0, None), figure_colname),
        // PG names a CASE after its `ELSE` branch when that branch has a strong
        // name, else after the construct itself (lowercased `case`).
        node::Node::CaseExpr(case) => {
            let els = case.defresult.as_deref().map_or((0, None), figure_colname);
            if els.0 <= 1 {
                (1, Some("case".to_string()))
            } else {
                els
            }
        }
        node::Node::CoalesceExpr(_) => (2, Some("coalesce".to_string())),
        node::Node::SubLink(sl) => match protobuf::SubLinkType::try_from(sl.sub_link_type) {
            // `EXISTS (…)` / `ARRAY(…)` are named after the construct.
            Ok(protobuf::SubLinkType::ExistsSublink) => (2, Some("exists".to_string())),
            Ok(protobuf::SubLinkType::ArraySublink) => (2, Some("array".to_string())),
            // A scalar subquery `(SELECT expr …)` is named after its single
            // output column, e.g. `(SELECT count(*) …)` → `count`.
            Ok(protobuf::SubLinkType::ExprSublink) => sublink_target_colname(sl),
            _ => (0, None),
        },
        node::Node::MinMaxExpr(m) => match protobuf::MinMaxOp::try_from(m.op) {
            Ok(protobuf::MinMaxOp::IsLeast) => (2, Some("least".to_string())),
            _ => (2, Some("greatest".to_string())),
        },
        node::Node::NullIfExpr(_) => (2, Some("nullif".to_string())),
        // The *raw* parse tree spells NULLIF as an `AExpr` with the Nullif
        // kind (the `NullIfExpr` node above only exists post-analysis) — PG
        // names the output column `nullif` either way.
        node::Node::AExpr(e)
            if matches!(
                protobuf::AExprKind::try_from(e.kind),
                Ok(protobuf::AExprKind::AexprNullif)
            ) =>
        {
            (2, Some("nullif".to_string()))
        }
        // `ARRAY[…]` constructors are named `array` (FigureColnameInternal's
        // T_A_ArrayExpr arm).
        node::Node::AArrayExpr(_) => (2, Some("array".to_string())),
        node::Node::RowExpr(_) => (2, Some("row".to_string())),
        node::Node::MergeSupportFunc(_) => (2, Some("merge_action".to_string())),
        node::Node::GroupingFunc(_) => (2, Some("grouping".to_string())),
        // SQL/XML constructs act like the functions they're named after
        // (IS DOCUMENT stays unnamed).
        node::Node::XmlExpr(x) => {
            use protobuf::XmlExprOp as Op;
            let name = match protobuf::XmlExprOp::try_from(x.op) {
                Ok(Op::IsXmlconcat) => "xmlconcat",
                Ok(Op::IsXmlelement) => "xmlelement",
                Ok(Op::IsXmlforest) => "xmlforest",
                Ok(Op::IsXmlparse) => "xmlparse",
                Ok(Op::IsXmlpi) => "xmlpi",
                Ok(Op::IsXmlroot) => "xmlroot",
                Ok(Op::IsXmlserialize) => "xmlserialize",
                _ => return (0, None),
            };
            (2, Some(name.to_string()))
        }
        node::Node::XmlSerialize(_) => (2, Some("xmlserialize".to_string())),
        // SQL/JSON constructs act like the functions they're named after.
        node::Node::JsonParseExpr(_) => (2, Some("json".to_string())),
        node::Node::JsonScalarExpr(_) => (2, Some("json_scalar".to_string())),
        node::Node::JsonSerializeExpr(_) => (2, Some("json_serialize".to_string())),
        node::Node::JsonObjectConstructor(_) => (2, Some("json_object".to_string())),
        node::Node::JsonArrayConstructor(_) | node::Node::JsonArrayQueryConstructor(_) => {
            (2, Some("json_array".to_string()))
        }
        node::Node::JsonObjectAgg(_) => (2, Some("json_objectagg".to_string())),
        node::Node::JsonArrayAgg(_) => (2, Some("json_arrayagg".to_string())),
        node::Node::JsonFuncExpr(f) => match protobuf::JsonExprOp::try_from(f.op) {
            Ok(protobuf::JsonExprOp::JsonExistsOp) => (2, Some("json_exists".to_string())),
            Ok(protobuf::JsonExprOp::JsonQueryOp) => (2, Some("json_query".to_string())),
            Ok(protobuf::JsonExprOp::JsonValueOp) => (2, Some("json_value".to_string())),
            _ => (0, None),
        },
        // SQL value functions are named after the keyword spelling (PG's
        // FigureColname): `SELECT current_date` → column `current_date`.
        node::Node::SqlvalueFunction(svf) => {
            use protobuf::SqlValueFunctionOp as Op;
            let name = match protobuf::SqlValueFunctionOp::try_from(svf.op) {
                Ok(Op::SvfopCurrentDate) => "current_date",
                Ok(Op::SvfopCurrentTime | Op::SvfopCurrentTimeN) => "current_time",
                Ok(Op::SvfopCurrentTimestamp | Op::SvfopCurrentTimestampN) => "current_timestamp",
                Ok(Op::SvfopLocaltime | Op::SvfopLocaltimeN) => "localtime",
                Ok(Op::SvfopLocaltimestamp | Op::SvfopLocaltimestampN) => "localtimestamp",
                Ok(Op::SvfopCurrentRole) => "current_role",
                Ok(Op::SvfopCurrentUser) => "current_user",
                Ok(Op::SvfopUser) => "user",
                Ok(Op::SvfopSessionUser) => "session_user",
                Ok(Op::SvfopCurrentCatalog) => "current_catalog",
                Ok(Op::SvfopCurrentSchema) => "current_schema",
                _ => return (0, None),
            };
            (2, Some(name.to_string()))
        }
        _ => (0, None),
    }
}

/// Name an `EXPR_SUBLINK` scalar subquery after its single output column —
/// PG's `FigureColname` recurses into the subquery's first target (honouring
/// an explicit alias, else the target expression's own name). Returns strength
/// `2` when the target has a name, `(0, None)` otherwise (the subquery is then
/// `?column?`, exactly as a bare expression would be).
fn sublink_target_colname(sl: &protobuf::SubLink) -> (i32, Option<String>) {
    let Some(node::Node::SelectStmt(top)) = sl.subselect.as_deref().and_then(|n| n.node.as_ref())
    else {
        return (0, None);
    };
    let mut sel: &protobuf::SelectStmt = top;
    // A set operation's columns are named by its leftmost arm; a VALUES
    // list's first column is `column1`.
    while let Some(larg) = sel.larg.as_deref() {
        sel = larg;
    }
    if !sel.values_lists.is_empty() {
        return (2, Some("column1".to_string()));
    }
    let Some(node::Node::ResTarget(rt)) = sel.target_list.first().and_then(|t| t.node.as_ref())
    else {
        return (0, None);
    };
    if !rt.name.is_empty() {
        return (2, Some(rt.name.clone()));
    }
    match rt.val.as_deref() {
        Some(val) if figure_colname(val).0 > 0 => (2, figure_colname(val).1),
        _ => (0, None),
    }
}
