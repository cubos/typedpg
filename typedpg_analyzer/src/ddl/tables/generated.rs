//! Generated columns: the checks PG applies to a generation expression
//! (`cookDefault` with `attgenerated` set, heap.c) and to the type of a
//! virtual generated column (`CheckAttributeType` with
//! `CHKATYPE_IS_VIRTUAL`).

use super::*;

/// `FirstUnpinnedObjectId` (transam.h): objects below it are created by
/// initdb and count as built-in.
const FIRST_UNPINNED_OBJECT_ID: u32 = 12000;

fn is_user_defined(oid: u32) -> bool {
    oid >= FIRST_UNPINNED_OBJECT_ID
}

/// `CheckAttributeType` (heap.c): a column's type may not be (or contain,
/// through a composite's attributes, a range's subtype, a multirange's range
/// or an array's element) a pseudo-type, nor a composite type that contains
/// it — `containing` starts with the row type the column is added to (ALTER
/// TABLE's `list_make1_oid(rel->rd_rel->reltype)`), empty for a new
/// relation. A virtual generated column (`CHKATYPE_IS_VIRTUAL`) may not
/// have a domain nor a user-defined type either. (The "no collation was
/// derived" check is not modeled: the analyzer's derived collations are not
/// precise enough to reject on.)
pub(crate) fn check_attribute_type(
    interp: &PgCatalog,
    attname: &str,
    typid: PgTypeOid,
    containing: Option<PgTypeOid>,
    is_virtual: bool,
) -> Result<(), DdlError> {
    let mut containing: Vec<PgTypeOid> = containing.into_iter().collect();
    check_attribute_type_rec(interp, attname, typid, &mut containing, is_virtual)
}

fn check_attribute_type_rec(
    interp: &PgCatalog,
    attname: &str,
    typid: PgTypeOid,
    containing: &mut Vec<PgTypeOid>,
    is_virtual: bool,
) -> Result<(), DdlError> {
    let Some(typ) = interp.pg_type.get(&typid) else {
        return Ok(());
    };
    match typ.typtype {
        TypType::Pseudo => {
            return Err(DdlError::Parse(format!(
                "column \"{attname}\" has pseudo-type {}",
                super::super::util::format_type_for_message(interp, typid)
            )));
        }
        TypType::Domain => {
            if is_virtual {
                return Err(DdlError::UnsupportedDdl(format!(
                    "virtual generated column \"{attname}\" cannot have a domain type"
                )));
            }
            if let Some(base) = typ.typbasetype {
                check_attribute_type_rec(interp, attname, base, containing, is_virtual)?;
            }
        }
        TypType::Composite => {
            if containing.contains(&typid) {
                return Err(DdlError::Parse(format!(
                    "composite type {} cannot be made a member of itself",
                    super::super::util::format_type_for_message(interp, typid)
                )));
            }
            if let Some(relid) = typ.typrelid {
                containing.push(typid);
                for attr in interp.attributes_of(relid).to_vec() {
                    check_attribute_type_rec(
                        interp,
                        &attr.attname,
                        attr.atttypid,
                        containing,
                        is_virtual,
                    )?;
                }
                containing.pop();
            }
        }
        TypType::Range => {
            if let Some(range) = interp.pg_range.get(&typid) {
                check_attribute_type_rec(
                    interp,
                    attname,
                    range.rngsubtype,
                    containing,
                    is_virtual,
                )?;
            }
        }
        TypType::Multirange => {
            if let Some(range) = interp
                .pg_range
                .values()
                .find(|r| r.rngmultitypid == Some(typid))
            {
                check_attribute_type_rec(interp, attname, range.rngtypid, containing, is_virtual)?;
            }
        }
        _ => {
            if typ.typcategory == TypCategory::Array
                && let Some(elem) = typ.typelem
            {
                check_attribute_type_rec(interp, attname, elem, containing, is_virtual)?;
            }
        }
    }
    // For consistency with check_virtual_generated_security().
    if is_virtual && is_user_defined(typid.get()) {
        return Err(DdlError::UnsupportedDdl(format!(
            "virtual generated column \"{attname}\" cannot have a user-defined type (Virtual \
             generated columns that make use of user-defined types are not yet supported.)"
        )));
    }
    Ok(())
}

/// A generation expression as [`cook_generation_expr`] accepted it.
pub(crate) struct CookedGeneration {
    /// The attnums of the columns it reads.
    pub(crate) refs: Vec<i16>,
    /// Its type before the coercion to the column's type (what ALTER
    /// COLUMN TYPE re-coerces).
    pub(crate) expr_type: PgTypeOid,
    /// The expression as written.
    pub(crate) text: super::check_inherit::StoredExpr,
}

impl CookedGeneration {
    /// Remember the expression for the generated column `relid.attnum`.
    pub(crate) fn record(self, interp: &mut PgCatalog, relid: PgClassOid, attnum: i16) {
        interp
            .attr_default_types
            .insert((relid, attnum), self.expr_type);
        interp.attr_default_exprs.insert((relid, attnum), self.text);
        interp.generated_refs.insert((relid, attnum), self.refs);
        crate::ddl::depend::record_column_expression(interp, relid, attnum);
    }
}

/// Transform and check a column's generation expression the way
/// `cookDefault` does for a generated column (heap.c): the expression is
/// transformed as `EXPR_KIND_GENERATED_COLUMN` over the table's row (no
/// sub-selects, aggregates, window or set-returning functions, and no
/// system column but `tableoid`), may not read another generated column or
/// the whole row (`check_nested_generated`), must be immutable, and — for a
/// virtual column — may use only built-in functions and types
/// (`check_virtual_generated_security`). Its result must be
/// assignment-coercible to the column's type.
pub(crate) fn cook_generation_expr(
    interp: &PgCatalog,
    relid: PgClassOid,
    attname: &str,
    atttypid: PgTypeOid,
    kind: AttGenerated,
    expr: &typedpg_pg_query::protobuf::Node,
) -> Result<CookedGeneration, DdlError> {
    use crate::ddl::expr_kind::{ExprKind, check_expr_kind};
    use crate::ddl::volatile::{ExprLocation, check_mutability, infer_over_relation};

    let unsupported = |e: crate::error::AnalyzeError| DdlError::UnsupportedDdl(e.to_string());
    check_expr_kind(interp, expr, ExprKind::GeneratedColumn)?;
    crate::resolve::check_no_srf_in_clause(expr, interp, "column generation expressions")
        .map_err(unsupported)?;

    let relname = relname_of(interp, relid);
    let column_refs: Vec<&typedpg_pg_query::protobuf::ColumnRef> = expr
        .node
        .as_ref()
        .map(|inner| {
            inner
                .nodes()
                .into_iter()
                .filter_map(|(n, ..)| match n {
                    typedpg_pg_query::NodeRef::ColumnRef(cr) => Some(cr),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    // scanNSItemForColumn: system columns other than tableoid.
    for cr in &column_refs {
        let Some(name) = cr.fields.last().and_then(crate::ddl::util::node_string) else {
            continue;
        };
        if name != "tableoid"
            && interp.attribute_by_name(relid, name).is_none()
            && crate::pg_catalog::SYSTEM_COLUMNS
                .iter()
                .any(|(n, ..)| *n == name)
        {
            return Err(DdlError::Parse(format!(
                "cannot use system column \"{name}\" in column generation expression"
            )));
        }
    }

    let used = std::cell::RefCell::new(Vec::new());
    let result = match infer_over_relation(interp, relid, expr, Some(&used)) {
        Some(r) => r.map_err(|e| {
            DdlError::UnsupportedDdl(format!(
                "{e} (in GENERATED expression on {})",
                QualifiedName::new(&relname, attname),
            ))
        })?,
        None => {
            return Ok(CookedGeneration {
                refs: Vec::new(),
                expr_type: atttypid,
                text: super::check_inherit::StoredExpr::written(expr),
            });
        }
    };

    // A whole-row reference that a field selection (`(t).a`) turns into a
    // plain column reference (transformIndirection).
    let selected: Vec<*const typedpg_pg_query::protobuf::ColumnRef> = expr
        .node
        .as_ref()
        .map(|inner| {
            inner
                .nodes()
                .into_iter()
                .filter_map(|(n, ..)| match n {
                    typedpg_pg_query::NodeRef::AIndirection(ind)
                        if matches!(
                            ind.indirection.first().and_then(|i| i.node.as_ref()),
                            Some(node::Node::String(_))
                        ) =>
                    {
                        match ind.arg.as_deref().and_then(|a| a.node.as_ref()) {
                            Some(node::Node::ColumnRef(cr)) => {
                                Some(cr as *const typedpg_pg_query::protobuf::ColumnRef)
                            }
                            _ => None,
                        }
                    }
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();

    // check_nested_generated.
    let mut refs: Vec<i16> = Vec::new();
    for cr in &column_refs {
        let whole_row = !selected.contains(&(*cr as *const _))
            && match cr.fields.as_slice() {
                [.., last] if matches!(last.node.as_ref(), Some(node::Node::AStar(_))) => true,
                [only] => crate::ddl::util::node_string(only)
                    .is_some_and(|n| n == relname && interp.attribute_by_name(relid, n).is_none()),
                _ => false,
            };
        if whole_row {
            return Err(DdlError::Parse(
                "cannot use whole-row variable in column generation expression (This would \
                 cause the generated column to depend on its own value.)"
                    .into(),
            ));
        }
        let Some(name) = cr.fields.last().and_then(crate::ddl::util::node_string) else {
            continue;
        };
        let Some(attr) = interp.attribute_by_name(relid, name) else {
            continue;
        };
        if !refs.contains(&attr.attnum) {
            refs.push(attr.attnum);
        }
        if attr.attgenerated.is_some() {
            return Err(DdlError::Parse(format!(
                "cannot use generated column \"{name}\" in column generation expression (A \
                 generated column cannot reference another generated column.)"
            )));
        }
    }

    check_mutability(interp, relid, expr, ExprLocation::Generated)?;

    if kind == AttGenerated::Virtual {
        check_virtual_generated_security(interp, relid, expr, &used.into_inner(), &result)?;
    }

    // Coerce to the column's type (COERCION_ASSIGNMENT).
    if result.type_oid == crate::pg_catalog::oid::UNKNOWN {
        // An untyped literal goes through the column type's input.
        infer_with_goal(
            interp,
            relid,
            expr,
            crate::expr::TypeGoal::assignment(atttypid),
        )
        .map_err(unsupported)?;
        return Ok(CookedGeneration {
            refs,
            expr_type: atttypid,
            text: super::check_inherit::StoredExpr::written(expr),
        });
    }
    if !crate::coerce::can_coerce(
        result.type_oid,
        atttypid,
        crate::coerce::CoercionContext::Assignment,
        interp,
    ) {
        return Err(DdlError::UnsupportedDdl(format!(
            "column \"{attname}\" is of type {} but default expression is of type {} (You will \
             need to rewrite or cast the expression.)",
            format_type_for_message(interp, atttypid),
            format_type_for_message(interp, result.type_oid)
        )));
    }
    Ok(CookedGeneration {
        refs,
        expr_type: result.type_oid,
        text: super::check_inherit::StoredExpr::written(expr),
    })
}

/// Analyze `expr` over the row of `relid` toward `goal`.
fn infer_with_goal(
    interp: &PgCatalog,
    relid: PgClassOid,
    expr: &typedpg_pg_query::protobuf::Node,
    goal: crate::expr::TypeGoal,
) -> Result<crate::expr::ExprType, crate::error::AnalyzeError> {
    use crate::nullability::NullabilityContext;
    use crate::param_collector::ParamCollector;
    use crate::scope::Scope;

    let relname = relname_of(interp, relid);
    let nspname = interp
        .pg_class
        .get(&relid)
        .and_then(|c| interp.namespace_name(c.relnamespace))
        .unwrap_or("public")
        .to_owned();
    let attrs = interp.attributes_of(relid).to_vec();
    let mut scope = Scope::default();
    scope.add_dml_target(
        interp,
        &relname,
        QualifiedName::new(nspname, relname.clone()),
        &attrs,
    );
    let null_ctx = NullabilityContext::default();
    let mut params = ParamCollector::default();
    crate::expr::infer_expr(
        expr,
        crate::expr::Ctx::new(&scope, &null_ctx, interp),
        &mut params,
        goal,
    )
}

/// `check_virtual_generated_security` (heap.c): no function and no
/// expression type of a virtual column's generation expression may be
/// user-defined. The expression's node types are those of the columns it
/// reads, the results of the functions it runs (operators and casts
/// included), the types it casts to, and its own result type.
fn check_virtual_generated_security(
    interp: &PgCatalog,
    relid: PgClassOid,
    expr: &typedpg_pg_query::protobuf::Node,
    used: &[crate::oid::PgProcOid],
    result: &crate::expr::ExprType,
) -> Result<(), DdlError> {
    if used.iter().any(|f| is_user_defined(f.get())) {
        return Err(DdlError::UnsupportedDdl(
            "generation expression uses user-defined function (Virtual generated columns that \
             make use of user-defined functions are not yet supported.)"
                .into(),
        ));
    }
    let mut types: Vec<PgTypeOid> = vec![result.type_oid];
    types.extend(
        used.iter()
            .filter_map(|f| interp.pg_proc.get(f).map(|p| p.prorettype)),
    );
    if let Some(inner) = expr.node.as_ref() {
        for (n, ..) in inner.nodes() {
            match n {
                typedpg_pg_query::NodeRef::ColumnRef(cr) => {
                    if let Some(attr) = cr
                        .fields
                        .last()
                        .and_then(crate::ddl::util::node_string)
                        .and_then(|name| interp.attribute_by_name(relid, name))
                    {
                        types.push(attr.atttypid);
                    }
                }
                typedpg_pg_query::NodeRef::TypeCast(tc) => {
                    if let Some(t) = tc
                        .type_name
                        .as_ref()
                        .and_then(|tn| lookup_type_name(tn, interp).ok())
                    {
                        types.push(t);
                    }
                }
                _ => {}
            }
        }
    }
    if types.iter().any(|t| is_user_defined(t.get())) {
        return Err(DdlError::UnsupportedDdl(
            "generation expression uses user-defined type (Virtual generated columns that make \
             use of user-defined types are not yet supported.)"
                .into(),
        ));
    }
    Ok(())
}

/// Whether generated column `attr` of relation `relid` is non-NULL in
/// every row whose other columns `input_not_null` says are non-NULL: its
/// generation expression can't be NULL over them. A STORED column is
/// recomputed from the row on every write, after any BEFORE trigger
/// (`ExecComputeStoredGenerated`), and SET EXPRESSION rewrites the table; a
/// VIRTUAL one is computed when read (`expand_generated_columns_in_expr`).
/// So its value is always the expression over the row's own columns.
pub(crate) fn generation_not_null(
    interp: &PgCatalog,
    relid: PgClassOid,
    attr: &crate::pg_catalog::PgAttribute,
    input_not_null: &dyn Fn(&str) -> bool,
) -> bool {
    use crate::nullability::NullabilityContext;
    use crate::param_collector::ParamCollector;
    use crate::scope::{Scope, ScopeColumn, TableSource};

    if attr.attgenerated.is_none() {
        return false;
    }
    let Some(super::check_inherit::StoredExpr::Written(expr)) =
        interp.attr_default_exprs.get(&(relid, attr.attnum))
    else {
        return false;
    };
    let relname = relname_of(interp, relid);
    let columns = interp
        .attributes_of(relid)
        .iter()
        .map(|a| ScopeColumn {
            name: a.attname.clone(),
            type_oid: a.atttypid,
            // A generation expression reads no generated column.
            base_not_null: a.attgenerated.is_none() && input_not_null(&a.attname),
            typmod: interp.effective_typmod(a.atttypid, a.atttypmod),
            collation: a.attcollation,
            table_alias: relname.clone(),
            record_fields: None,
            elem_nullable: None,
            origin: None,
        })
        .collect();
    let mut scope = Scope::default();
    scope.sources.push(TableSource::derived(&relname, columns));
    let null_ctx = NullabilityContext::default();
    let mut params = ParamCollector::default();
    let expr = crate::ddl::stored_exprs::over_own_row(interp, relid, expr);
    // Analyzed on a level of its own, and without noting what it refers
    // to as a dependency of the statement being analyzed.
    let (inferred, _) = crate::ddl::depend::collect(|| {
        let _level = crate::resolve::QueryLevel::enter();
        crate::expr::infer_expr(
            &expr,
            crate::expr::Ctx::new(&scope, &null_ctx, interp),
            &mut params,
            crate::expr::TypeGoal::assignment(attr.atttypid)
                .with_typmod(interp.effective_typmod(attr.atttypid, attr.atttypmod)),
        )
    });
    // Stored with the column's type, through an assignment coercion that
    // may map a value to NULL.
    inferred.is_ok_and(|t| !crate::expr::assignment_nullable(&t, attr.atttypid, interp))
}

impl PgCatalog {
    /// Whether column `attr` is non-NULL in every row of its relation: its
    /// own NOT NULL ([`PgCatalog::attr_proven_not_null`]), or a generation
    /// expression that can't be NULL over the row ([`generation_not_null`]).
    pub(crate) fn attr_never_null(&self, attr: &crate::pg_catalog::PgAttribute) -> bool {
        if self.attr_proven_not_null(attr) {
            return true;
        }
        if attr.attgenerated.is_none() {
            return false;
        }
        let relid = attr.attrelid;
        let attrs = self.attributes_of(relid);
        generation_not_null(self, relid, attr, &|name| {
            attrs
                .iter()
                .find(|a| a.attname == name)
                .is_some_and(|a| self.attr_proven_not_null(a))
        })
    }
}
