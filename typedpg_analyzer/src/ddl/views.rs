//! CREATE VIEW / CREATE MATERIALIZED VIEW AS handlers.
//!
//! Views resolve columns at creation time (expanding `SELECT *`) and track
//! dependencies on underlying tables/columns. This matches PostgreSQL behavior:
//! - `SELECT *` is expanded to explicit columns at view creation time
//! - ALTER TABLE DROP COLUMN fails if a view depends on the column (without CASCADE)
//! - ALTER TABLE ALTER COLUMN TYPE fails outright if a view depends on the column
//!   (PG SQLSTATE 0A000 — even binary-coercible changes are rejected; the user
//!   must DROP the view first, ALTER, then CREATE the view again)
//! - With CASCADE, dependent views are dropped (transitively)
//!
//! View → relation/column dependencies are stored in `pg_depend` rows with
//! `deptype = Normal`, `classid = refclassid = PG_CLASS_RELID`, `objid =
//! view_oid`, `refobjid = table_oid`, and `refobjsubid = attnum` (or 0 when
//! the view depends on the whole relation).

use typedpg_pg_query::NodeBox;
use typedpg_pg_query::ast::{self, CreateTableAsStmt, ObjectType, ViewStmt};

use crate::oid::{PgClassOid, PgGenericOid, PgNamespaceOid, PgProcOid, PgRewriteOid, PgTypeOid};
use crate::pg_catalog::{
    AstBinding, DepType, EvEnabled, EvType, PG_CLASS_RELID, PG_PROC_RELID, PG_TYPE_RELID,
    PgAttribute, PgClass, PgDepend, PgRewrite, PgType, RelKind, SerializedAst, TypCategory,
    TypStorage, TypType,
};

use super::DdlError;
use super::util::ensure_range_var;
use crate::pg_catalog::PgCatalog;

pub fn create_view(interp: &mut PgCatalog, stmt: &ViewStmt) -> Result<(), DdlError> {
    let rv = stmt
        .view
        .as_ref()
        .ok_or_else(|| DdlError::Parse("CREATE VIEW without name".into()))?;

    // DefineView.
    if rv.relpersistence == "u" {
        return Err(DdlError::Parse(
            "views cannot be unlogged because they do not have storage".into(),
        ));
    }
    let (nsoid, name) = ensure_range_var(interp, rv)?;
    let qn_label = crate::qualified_name::QualifiedName::new(
        interp.namespace_name(nsoid).unwrap_or("?"),
        name.clone(),
    )
    .to_string();

    let aliases = string_list(&stmt.aliases);

    if let Some(query) = stmt.query.as_ref() {
        check_no_parameters(query)?;
    }
    // DefineView only analyzes the query: no error its execution would
    // raise is reported (a query reading the view runs it).
    let resolved = match stmt.query.as_ref() {
        Some(query_node) => crate::resolve::without_execution(|| {
            resolve_view_now(interp, query_node, &aliases, false)
        })
        .map_err(|e| match e {
            DdlError::ViewAnalysis { source, .. } => DdlError::ViewAnalysis {
                view: qn_label.clone(),
                source,
            },
            other => other,
        })?,
        None => ResolvedView::default(),
    };
    // DefineView: parse analysis allows these; a view can't have them.
    if let Some(query) = stmt.query.as_ref() {
        if matches!(query, ast::Node::SelectStmt(sel) if sel.into_clause.is_some()) {
            return Err(DdlError::UnsupportedDdl(
                "views must not contain SELECT INTO".into(),
            ));
        }
        if modifying_cte(query) {
            return Err(DdlError::UnsupportedDdl(
                "views must not contain data-modifying statements in WITH".into(),
            ));
        }
    }
    // DefineView (view.c): the alias list may not outnumber the columns.
    if aliases.len() > resolved.columns.len() {
        return Err(DdlError::Parse(
            "CREATE VIEW specifies more column names than columns".into(),
        ));
    }
    check_duplicate_columns(&resolved.columns)?;
    let updatability = stmt.query.as_ref().map(|q| view_updatability(interp, q));
    // DefineView: WITH CHECK OPTION needs an automatically updatable view
    // (checked before the options are parsed).
    let check_option = stmt.with_check_option != ast::ViewCheckOption::NoCheckOption
        || sets_check_option(&stmt.options);
    let check_option_cascaded = cascaded_check_option(&stmt.options)
        || stmt.with_check_option == ast::ViewCheckOption::CascadedCheckOption;
    if check_option && let Some(query) = stmt.query.as_ref() {
        check_option_allowed(interp, query)?;
    }
    // DefineView takes `toast.` options, which nothing checks.
    super::reloptions::check_reloptions(
        &super::reloptions::without_toast_options(&stmt.options),
        super::reloptions::RelOptKind::View,
        false,
        false,
    )?;

    // DefineVirtualRelation: a collatable column needs a collation.
    if let Some(col) = resolved.columns.iter().find(|c| c.no_collation) {
        return Err(DdlError::Parse(format!(
            "could not determine which collation to use for view column \"{}\"",
            col.name
        )));
    }

    // DefineVirtualRelation: OR REPLACE redefines an existing *view* in
    // place (keeping its OID, so dependents stay attached); any other
    // existing relation is an error.
    if let Some(existing_oid) = interp.class_by_qname.get(&(nsoid, name.clone())).copied() {
        if !stmt.replace {
            return Err(DdlError::DuplicateObject(format!(
                "relation \"{name}\" already exists"
            )));
        }
        if interp.pg_class.get(&existing_oid).map(|c| c.relkind) != Some(RelKind::View) {
            return Err(DdlError::DuplicateObject(format!(
                "\"{name}\" is not a view"
            )));
        }
        check_view_columns(interp, existing_oid, &resolved.columns)?;
        replace_view(interp, existing_oid, resolved)?;
        if let Some(updatability) = updatability {
            interp.view_updatability.insert(existing_oid, updatability);
        }
        // OR REPLACE replaces the options too (AT_ReplaceRelOptions).
        set_security_barrier(interp, existing_oid, &stmt.options, true, false);
        if check_option {
            interp.check_option_views.insert(existing_oid);
            if check_option_cascaded {
                interp.cascaded_check_option_views.insert(existing_oid);
            } else {
                interp.cascaded_check_option_views.remove(&existing_oid);
            }
        }
        return Ok(());
    }

    // DefineView: a view using a temporary object is temporary ("view ...
    // will be a temporary view").
    let reads_temp = temp_dependency(interp, &resolved.deps).is_some();
    let nsoid = if reads_temp && rv.schemaname.is_empty() {
        super::util::temp_namespace(interp)?
    } else if reads_temp && Some(nsoid) != interp.temp_namespace {
        return Err(DdlError::UnsupportedDdl(
            "cannot create temporary relation in non-temporary schema".into(),
        ));
    } else {
        nsoid
    };
    super::util::check_relation_name_free(interp, nsoid, &name)?;
    install_relation(interp, nsoid, name.clone(), RelKind::View, resolved)?;
    if let Some(&oid) = interp.class_by_qname.get(&(nsoid, name)) {
        if Some(nsoid) == interp.temp_namespace {
            interp.relpersistence.insert(oid, 't');
        }
        if let Some(updatability) = updatability {
            interp.view_updatability.insert(oid, updatability);
        }
        set_security_barrier(interp, oid, &stmt.options, true, false);
        if check_option {
            interp.check_option_views.insert(oid);
            if check_option_cascaded {
                interp.cascaded_check_option_views.insert(oid);
            }
        }
    }
    Ok(())
}

/// Record view `relid`'s `security_barrier` and `security_invoker` options
/// as `options` set them:
/// with `replace` (the options replaced wholesale), an absent option is
/// off; otherwise only the options listed change (`ALTER VIEW … SET`),
/// to off when `reset`.
pub(crate) fn set_security_barrier(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    options: &[Option<ast::Node>],
    replace: bool,
    reset: bool,
) {
    // check_option: `local` and `cascaded` both make an INSERT evaluate the
    // view query's WHERE (as the WITH CHECK OPTION qual).
    let check_option = options.iter().find_map(|o| match o.as_ref() {
        Some(ast::Node::DefElem(de)) if de.defname == "check_option" => {
            Some(match de.arg.as_ref() {
                Some(ast::Node::String(s)) => s.sval != "none",
                Some(ast::Node::Boolean(b)) => b.boolval,
                _ => true,
            })
        }
        _ => None,
    });
    let cascaded = cascaded_check_option(options);
    match check_option.or((replace).then_some(false)) {
        Some(true) if !reset => {
            interp.check_option_views.insert(relid);
            if cascaded {
                interp.cascaded_check_option_views.insert(relid);
            } else {
                interp.cascaded_check_option_views.remove(&relid);
            }
        }
        Some(_) => {
            interp.check_option_views.remove(&relid);
            interp.cascaded_check_option_views.remove(&relid);
        }
        None => {}
    }
    for (option, invoker) in [("security_barrier", false), ("security_invoker", true)] {
        let given = options.iter().find_map(|o| match o.as_ref() {
            Some(ast::Node::DefElem(de)) if de.defname == option && de.defnamespace.is_empty() => {
                Some(
                    !reset
                        && match de.arg.as_ref() {
                            None => true,
                            Some(ast::Node::String(s)) => {
                                super::reloptions::parse_bool(&s.sval).unwrap_or(false)
                            }
                            Some(ast::Node::Boolean(b)) => b.boolval,
                            Some(ast::Node::Integer(i)) => i.ival != 0,
                            _ => false,
                        },
                )
            }
            _ => None,
        });
        let set = if invoker {
            &mut interp.security_invoker_views
        } else {
            &mut interp.security_barrier_views
        };
        match given.or((replace).then_some(false)) {
            Some(true) => {
                set.insert(relid);
            }
            Some(false) => {
                set.remove(&relid);
            }
            None => {}
        }
    }
}

pub fn create_table_as(interp: &mut PgCatalog, stmt: &CreateTableAsStmt) -> Result<(), DdlError> {
    create_table_as_planned(interp, stmt, false)
}

/// [`create_table_as`]; with `explained`, under `EXPLAIN ANALYZE`, which
/// plans and runs the query even `WITH NO DATA`.
pub(crate) fn create_table_as_planned(
    interp: &mut PgCatalog,
    stmt: &CreateTableAsStmt,
    explained: bool,
) -> Result<(), DdlError> {
    let into = stmt
        .into
        .as_deref()
        .ok_or_else(|| DdlError::Parse("CREATE TABLE AS without target".into()))?;
    let kind = match stmt.objtype {
        ObjectType::ObjectMatview => RelKind::MaterializedView,
        _ => RelKind::Table,
    };
    // ExecCreateTableAs: WITH NO DATA creates the relation without
    // planning the query (create_ctas_nodata) — but `AS EXECUTE` plans the
    // prepared statement either way (ExecuteQuery).
    let is_execute = matches!(stmt.query.as_ref(), Some(ast::Node::ExecuteStmt(_)));
    create_relation_as(
        interp,
        into,
        stmt.query.as_ref(),
        kind,
        stmt.if_not_exists,
        explained || is_execute || !into.skip_data,
    )
}

/// `SELECT ... INTO [TABLE] name FROM ...`: the same as `CREATE TABLE name
/// AS SELECT ...` (`transformSelectStmt` turns it into a CreateTableAsStmt).
pub fn select_into(interp: &mut PgCatalog, stmt: &ast::SelectStmt) -> Result<(), DdlError> {
    let Some(into) = stmt.into_clause.as_deref() else {
        return Ok(());
    };
    let mut query = stmt.clone();
    query.into_clause = None;
    let query_node = ast::Node::SelectStmt(NodeBox::new(query));
    create_relation_as(interp, into, Some(&query_node), RelKind::Table, false, true)
}

/// Shared body of CREATE TABLE AS / CREATE MATERIALIZED VIEW / SELECT INTO
/// (`ExecCreateTableAs` + `intorel_startup`, createas.c). `planned`: the
/// query is planned and run, not only analyzed.
fn create_relation_as(
    interp: &mut PgCatalog,
    into: &ast::IntoClause,
    query: Option<&ast::Node>,
    kind: RelKind,
    if_not_exists: bool,
    planned: bool,
) -> Result<(), DdlError> {
    let rv = into
        .rel
        .as_ref()
        .ok_or_else(|| DdlError::Parse("CREATE TABLE AS without target".into()))?;
    let (nsoid, name) = ensure_range_var(interp, rv)?;

    // CreateTableAsRelExists: checked before the query runs, so IF NOT
    // EXISTS skips the whole statement (NOTICE ... already exists, skipping).
    if interp.class_by_qname.contains_key(&(nsoid, name.clone())) {
        if if_not_exists {
            return Ok(());
        }
        return Err(DdlError::DuplicateObject(format!(
            "relation \"{name}\" already exists"
        )));
    }

    let qn_label = crate::qualified_name::QualifiedName::new(
        interp.namespace_name(nsoid).unwrap_or("?"),
        name.clone(),
    )
    .to_string();
    let aliases = string_list(&into.col_names);
    if let Some(query) = query {
        check_no_parameters(query)?;
    }
    // The query runs to fill the relation, unless WITH NO DATA; a
    // materialized view's under a restricted search_path. WITH NO DATA,
    // a query still `planned` (AS EXECUTE, EXPLAIN ANALYZE) has its
    // executor started but reads no row (ExecuteQuery's count 0,
    // ExplainOnePlan's NoMovementScanDirection).
    let run = |f: &dyn Fn() -> Result<ResolvedView, DdlError>| {
        if into.skip_data && planned {
            crate::resolve::planned_only(f)
        } else if into.skip_data {
            crate::resolve::without_execution(f)
        } else if kind == RelKind::MaterializedView {
            crate::resolve::with_restricted_execution(f)
        } else {
            f()
        }
    };
    let mut resolved = match query {
        // CREATE TABLE ... AS EXECUTE (ExecuteQuery): the prepared SELECT's
        // columns, its arguments checked. The cached plan is revalidated
        // (rewritten again) even WITH NO DATA.
        Some(ast::Node::ExecuteStmt(estmt)) => {
            let prepared = super::prepared::lookup_execute(interp, estmt)?;
            if !prepared.is_select() {
                return Err(DdlError::Parse("prepared statement is not a SELECT".into()));
            }
            let resolve = |interp: &PgCatalog| {
                run(&|| {
                    crate::expr::with_rewriting(true, || {
                        resolve_view_with_params(
                            interp,
                            &prepared.query,
                            &aliases,
                            &prepared.param_types,
                            planned,
                        )
                    })
                })
            };
            if planned {
                super::prepared::planned_for_arguments(interp, estmt, &prepared, resolve)
            } else {
                resolve(interp)
            }
            .map_err(|e| match e {
                DdlError::ViewAnalysis { source, .. } => DdlError::ViewAnalysis {
                    view: qn_label.clone(),
                    source,
                },
                other => other,
            })?
        }
        // ExecCreateTableAs rewrites the query it fills the relation with;
        // WITH NO DATA only creates the relation (create_ctas_nodata).
        Some(query_node) => run(&|| {
            crate::expr::with_rewriting(planned, || {
                resolve_view_now(interp, query_node, &aliases, planned)
            })
        })
        .map_err(|e| match e {
            DdlError::ViewAnalysis { source, .. } => DdlError::ViewAnalysis {
                view: qn_label.clone(),
                source,
            },
            other => other,
        })?,
        None => ResolvedView::default(),
    };
    // transformCreateTableAsStmt: a materialized view outlives the session
    // and is refreshed from its query.
    if kind == RelKind::MaterializedView {
        if query.is_some_and(modifying_cte) {
            return Err(DdlError::UnsupportedDdl(
                "materialized views must not use data-modifying statements in WITH".into(),
            ));
        }
        if let Some(object) = temp_dependency(interp, &resolved.deps) {
            return Err(DdlError::UnsupportedDdl(format!(
                "materialized views must not use temporary objects (This view depends on \
                 temporary {object}.)"
            )));
        }
    }
    if aliases.len() > resolved.columns.len() {
        return Err(DdlError::Parse(
            "too many column names were specified".into(),
        ));
    }
    // ExecCreateTableAs: a table created with data (or from EXECUTE, which
    // always plans) takes its columns from the planned query — whose
    // collations planning may have changed (see `Planned`); a materialized
    // view, or WITH NO DATA, from the parsed one.
    let is_execute = matches!(query, Some(ast::Node::ExecuteStmt(_)));
    if kind == RelKind::Table && (!into.skip_data || is_execute) {
        for col in &mut resolved.columns {
            match col.planned {
                crate::expr::Planned::AsParsed => {}
                // Whatever it may be, it may have one — which the
                // analyzer doesn't know. The default collation stands in
                // for it: it conflicts with no other implicit collation, so
                // a query combining the column with another collated one is
                // accepted whatever PG stored (`CASE WHEN 1 = 1 THEN 'x'
                // ELSE c1.b END` folds to the default-collated 'x', not to
                // the "C" the parsed CASE has).
                crate::expr::Planned::Unknown => {
                    col.no_collation = false;
                    if interp_type_collation(interp, col.type_oid).is_some() {
                        col.collation = Some(crate::expr::DEFAULT_COLLATION);
                        col.uncertain_collation = true;
                    }
                }
            }
        }
    }
    // intorel_startup / create_ctas_nodata: a collatable column needs a
    // collation.
    if let Some(col) = resolved.columns.iter().find(|c| c.no_collation) {
        return Err(DdlError::Parse(format!(
            "no collation was derived for column \"{}\" with collatable type {}",
            col.name,
            super::util::format_type_for_message(interp, col.type_oid)
        )));
    }
    // DefineRelation: ON COMMIT applies to temporary tables only — tested
    // on the statement's TEMP, before the namespace lookup that would make
    // a `pg_temp.name` temporary.
    use ast::OnCommitAction;
    let on_commit = into.on_commit;
    if !matches!(on_commit, OnCommitAction::OncommitNoop) && rv.relpersistence != "t" {
        return Err(DdlError::UnsupportedDdl(
            "ON COMMIT can only be used on temporary tables".into(),
        ));
    }
    check_duplicate_columns(&resolved.columns)?;
    super::util::check_relation_name_free(interp, nsoid, &name)?;
    super::tables::check_tablespace_placement(&into.table_space_name)?;
    if !into.access_method.is_empty() {
        super::opclass::check_table_am(interp, &into.access_method)?;
    }
    super::reloptions::check_reloptions(
        &into.options,
        super::reloptions::RelOptKind::Heap,
        false,
        true,
    )?;

    // A table created from a query is an ordinary table: its columns carry
    // no NOT NULL constraint, whatever the query's nullability. (A matview
    // can only ever hold its query's rows, so it keeps the inferred one.)
    if kind == RelKind::Table {
        for col in &mut resolved.columns {
            col.not_null = false;
        }
    }

    install_relation(interp, nsoid, name.clone(), kind, resolved)?;
    let oid = interp.class_by_qname.get(&(nsoid, name)).copied();
    if let Some(p @ ('u' | 't')) = rv.relpersistence.chars().next()
        && let Some(oid) = oid
    {
        interp.relpersistence.insert(oid, p);
    }
    if on_commit == OnCommitAction::OncommitDrop
        && let Some(oid) = oid
    {
        interp.on_commit_drop.push(oid);
    }
    // WITH NO DATA leaves a materialized view unpopulated.
    if kind == RelKind::MaterializedView
        && into.skip_data
        && let Some(oid) = oid
    {
        interp.unpopulated_matviews.insert(oid);
    }
    Ok(())
}

/// The `String` values of a name list (view aliases, CTAS column names).
fn string_list(nodes: &[Option<ast::Node>]) -> Vec<String> {
    nodes
        .iter()
        .filter_map(|n| match n.as_ref() {
            Some(ast::Node::String(s)) => Some(s.sval.clone()),
            _ => None,
        })
        .collect()
}

/// The relation's columns go through `MergeAttributes`, which rejects a
/// repeated name.
fn check_duplicate_columns(columns: &[ResolvedColumn]) -> Result<(), DdlError> {
    for (i, col) in columns.iter().enumerate() {
        if columns[..i].iter().any(|c| c.name == col.name) {
            return Err(DdlError::DuplicateObject(format!(
                "column \"{}\" specified more than once",
                col.name
            )));
        }
    }
    Ok(())
}

/// `checkViewColumns` (view.c): CREATE OR REPLACE VIEW may only add
/// columns at the end — existing ones keep their name, type and typmod.
fn check_view_columns(
    interp: &PgCatalog,
    view_oid: PgClassOid,
    new_columns: &[ResolvedColumn],
) -> Result<(), DdlError> {
    let old = interp.attributes_of(view_oid);
    if new_columns.len() < old.len() {
        return Err(DdlError::Parse("cannot drop columns from view".into()));
    }
    for (old_col, new_col) in old.iter().zip(new_columns) {
        if old_col.attname != new_col.name {
            return Err(DdlError::Parse(format!(
                "cannot change name of view column \"{}\" to \"{}\"",
                old_col.attname, new_col.name
            )));
        }
        // A domain-typed column may carry the domain's own typmod on one
        // side and none on the other; both mean "the domain's typmod".
        let effective_typmod = |type_oid: PgTypeOid, typmod: Option<i32>| {
            typmod.or_else(|| {
                interp
                    .pg_type
                    .get(&type_oid)
                    .filter(|t| t.typtype == TypType::Domain)
                    .and_then(|t| t.typtypmod)
            })
        };
        if old_col.atttypid != new_col.type_oid
            || effective_typmod(old_col.atttypid, old_col.atttypmod)
                != effective_typmod(new_col.type_oid, new_col.typmod)
        {
            return Err(DdlError::Parse(format!(
                "cannot change data type of view column \"{}\" from {} to {}",
                old_col.attname,
                super::util::format_type_with_typmod(interp, old_col.atttypid, old_col.atttypmod),
                super::util::format_type_with_typmod(interp, new_col.type_oid, new_col.typmod),
            )));
        }
        if old_col.attcollation != new_col.collation
            && !new_col.uncertain_collation
            && !interp
                .uncertain_collations
                .contains(&(view_oid, old_col.attnum))
        {
            let name = |c: Option<crate::oid::PgCollationOid>| {
                c.and_then(|c| interp.pg_collation.get(&c))
                    .map(|c| c.collname.clone())
                    .unwrap_or_default()
            };
            return Err(DdlError::Parse(format!(
                "cannot change collation of view column \"{}\" from \"{}\" to \"{}\"",
                old_col.attname,
                name(old_col.attcollation),
                name(new_col.collation)
            )));
        }
    }
    Ok(())
}

/// The query of a view or CREATE TABLE AS has no parameters: a `$n` is an
/// error (transformParamRef without a parameter hook).
fn check_no_parameters(query: &ast::Node) -> Result<(), DdlError> {
    let number = query.nodes().find_map(|(n, _)| match n {
        typedpg_pg_query::NodeRef::ParamRef(p) => Some(p.number),
        _ => None,
    });
    match number {
        Some(n) => Err(DdlError::Parse(format!("there is no parameter ${n}"))),
        None => Ok(()),
    }
}

/// Whether a CTE of the query's WITH is an INSERT / UPDATE / DELETE /
/// MERGE (`hasModifyingCTE`).
pub(crate) fn has_modifying_cte(query: Option<&ast::Node>) -> bool {
    query.is_some_and(modifying_cte)
}

/// [`has_modifying_cte`] of a present query.
fn modifying_cte(query: &ast::Node) -> bool {
    let ast::Node::SelectStmt(sel) = query else {
        return false;
    };
    sel.with_clause.as_ref().is_some_and(|w| {
        w.ctes.iter().any(|cte| {
            matches!(cte.as_ref(), Some(ast::Node::CommonTableExpr(c))
                if matches!(c.ctequery.as_ref(),
                    Some(ast::Node::InsertStmt(_) | ast::Node::UpdateStmt(_)
                        | ast::Node::DeleteStmt(_) | ast::Node::MergeStmt(_))))
        })
    })
}

/// Redefine an existing view in place: same OID and row type, the column
/// list extended / refreshed, the `_RETURN` rule and `pg_depend` edges
/// replaced.
fn replace_view(
    interp: &mut PgCatalog,
    view_oid: PgClassOid,
    resolved: ResolvedView,
) -> Result<(), DdlError> {
    let ResolvedView {
        columns,
        bindings,
        ast,
        deps,
        access,
    } = resolved;
    // DefineVirtualRelation adds the new columns with ATExecAddColumn.
    let system = interp
        .pg_class
        .get(&view_oid)
        .and_then(|c| interp.namespace_name(c.relnamespace))
        == Some("pg_catalog");
    check_relation_columns(interp, RelKind::View, &columns, system)?;
    let attrs: Vec<PgAttribute> = columns
        .iter()
        .enumerate()
        .map(|(i, col)| PgAttribute {
            attrelid: view_oid,
            attname: col.name.clone(),
            atttypid: col.type_oid,
            attnum: (i + 1) as i16,
            attnotnull: col.not_null,
            atthasdef: false,
            attgenerated: None,
            atttypmod: col.typmod,
            attidentity: None,
            attcollation: col.collation,
            attislocal: true,
            attinhcount: 0,
        })
        .collect();
    interp.pg_attribute.insert(view_oid, attrs);
    record_uncertain_collations(interp, view_oid, &columns);
    interp.remove_pg_rewrites_of(view_oid);
    let bound = bound_references(interp, &deps.references);
    interp.view_references.insert(view_oid, bound);
    interp.view_access.insert(view_oid, access);
    let rewrite_oid = PgRewriteOid::from_nonzero(interp.alloc_oid()?);
    interp.insert_pg_rewrite(PgRewrite {
        oid: rewrite_oid,
        rulename: "_RETURN".to_owned(),
        ev_class: view_oid,
        ev_type: EvType::Select,
        ev_enabled: EvEnabled::Origin,
        is_instead: true,
        ev_qual: None,
        ev_action: SerializedAst { ast, bindings },
    });
    interp.remove_dependencies_of(
        PG_CLASS_RELID,
        PgGenericOid::from_nonzero(view_oid.into_nonzero()),
    );
    record_view_dependencies(interp, view_oid, &deps);
    // The view keeps its OID but not its definition: nothing the seed said
    // about its rows (a system view's guaranteed keys included) holds now.
    interp.mark_relation_modified(view_oid);
    Ok(())
}

/// `REFRESH MATERIALIZED VIEW [CONCURRENTLY] name [WITH [NO] DATA]`: the
/// data changes, the shape does not. PG requires a materialized view
/// (`ExecRefreshMatView`).
pub fn refresh_materialized_view(
    interp: &mut PgCatalog,
    stmt: &ast::RefreshMatViewStmt,
) -> Result<(), DdlError> {
    let Some(rv) = stmt.relation.as_ref() else {
        return Ok(());
    };
    let (schema, name) = super::util::range_var_names(rv, interp);
    let class = interp
        .namespace_oid(&schema)
        .and_then(|ns| interp.class_by_qname.get(&(ns, name.clone())))
        .and_then(|oid| interp.pg_class.get(oid));
    match class {
        None => Err(DdlError::TableNotFound(format!(
            "relation \"{}\" does not exist",
            if rv.schemaname.is_empty() {
                name
            } else {
                crate::qualified_name::QualifiedName::new(&schema, &name).to_string()
            }
        ))),
        Some(c) if c.relkind != RelKind::MaterializedView => Err(DdlError::Parse(format!(
            "\"{}\" is not a materialized view",
            c.relname
        ))),
        Some(c) => {
            let oid = c.oid;
            if stmt.concurrent {
                if interp.unpopulated_matviews.contains(&oid) {
                    return Err(DdlError::UnsupportedDdl(
                        "CONCURRENTLY cannot be used when the materialized view is not populated"
                            .into(),
                    ));
                }
                if stmt.skip_data {
                    return Err(DdlError::Parse(
                        "REFRESH options CONCURRENTLY and WITH NO DATA cannot be used together"
                            .into(),
                    ));
                }
                // is_usable_unique_index: unique, immediate, valid, not
                // partial, over plain columns only.
                let usable = interp.pg_index.values().any(|i| {
                    i.indrelid == oid
                        && i.indisunique
                        && i.indpred.is_none()
                        && !i.indkey.is_empty()
                        && i.indkey.iter().all(|&k| k > 0)
                        && !interp.nonimmediate_indexes.contains(&i.indexrelid)
                        && !interp.invalid_indexes.contains(&i.indexrelid)
                });
                if !usable {
                    let schema = interp.namespace_name(c.relnamespace).unwrap_or_default();
                    return Err(DdlError::UnsupportedDdl(format!(
                        "cannot refresh materialized view \"{}\" concurrently (Create a unique \
                         index with no WHERE clause on one or more columns of the materialized \
                         view.)",
                        crate::qualified_name::QualifiedName::new(schema, &c.relname)
                    )));
                }
            }
            // refresh_matview_datafill rewrites the stored query.
            if !stmt.skip_data
                && let Some(relname) = query_rule_recursion(interp, oid)
            {
                return Err(DdlError::UnsupportedDdl(
                    crate::pgmsg::infinite_rule_recursion(&relname)
                        .finalize_implicit()
                        .to_string(),
                ));
            }
            if stmt.skip_data {
                interp.unpopulated_matviews.insert(oid);
            } else {
                check_refill(interp, oid)?;
                interp.unpopulated_matviews.remove(&oid);
            }
            // The rows it held are gone (an unpopulated one is refilled by
            // a later REFRESH, from the query as it stands then): what its
            // query returns now is what it holds.
            rederive_view_nullability(interp, vec![oid], false, Some(oid));
            // Without CONCURRENTLY the data goes in a new heap
            // (finish_heap_swap), its indexes rebuilt with it.
            if !stmt.concurrent {
                super::indexes::table_rewritten(interp, oid);
                super::tables::toast::heap_rewritten(interp, oid);
            }
            Ok(())
        }
    }
}

/// `REFRESH MATERIALIZED VIEW` with data plans and runs the stored query
/// (refresh_matview_datafill, RefreshMatViewByOid), under the restricted
/// `search_path`: what that raises on every execution — a NULL cast to a
/// NOT NULL domain, a plan-time error — fails it; a materialized view
/// created `WITH NO DATA` was never planned. The query is checked only
/// while it resolves as the view bound it ([`reanalyze_view`]).
fn check_refill(interp: &PgCatalog, relid: PgClassOid) -> Result<(), DdlError> {
    if reanalyze_view(interp, relid).is_none() {
        return Ok(());
    }
    match view_query(interp, relid) {
        Some(inner) => {
            crate::resolve::with_restricted_execution(|| super::dml::check_executed(interp, &inner))
        }
        None => Ok(()),
    }
}

/// Bundle of analyzer outputs that [`install_relation`] needs to wire up a
/// view: column shape, the binding side-table, the encoded AST, and the
/// deps to record in `pg_depend`.
#[derive(Default)]
struct ResolvedView {
    columns: Vec<ResolvedColumn>,
    bindings: Vec<AstBinding>,
    ast: Option<ast::Node>,
    deps: ViewDeps,
    /// What the query reads, runs and changes: kept for the statements
    /// that read the view.
    access: crate::privileges::ViewAccess,
}

/// CheckAttributeNamesTypes (heap_create_with_catalog): a table or
/// materialized view's column may not take a system column's name (a view
/// has none), and no column may have a pseudo-type.
///
/// A relation of `pg_catalog` is created by initdb, under
/// `allow_system_table_mods`: its columns may be `anyarray`
/// (`CHKATYPE_ANYARRAY`, as `pg_stats` has). The seed replays those views.
fn check_relation_columns(
    interp: &PgCatalog,
    relkind: RelKind,
    columns: &[ResolvedColumn],
    system: bool,
) -> Result<(), DdlError> {
    const ANYARRAY: PgTypeOid = PgTypeOid::from_raw(2277);
    if relkind != RelKind::View {
        for col in columns {
            super::tables::check_system_column_name(&col.name)?;
        }
    }
    for col in columns {
        if system && col.type_oid == ANYARRAY {
            continue;
        }
        super::tables::check_attribute_type(interp, &col.name, col.type_oid, None, false)?;
    }
    Ok(())
}

/// Build a `pg_class` row + the matching `pg_attribute` rows + composite type
/// + array type, and write the `pg_depend` rows for the view's dependencies.
fn install_relation(
    interp: &mut PgCatalog,
    nsoid: PgNamespaceOid,
    name: String,
    relkind: RelKind,
    resolved: ResolvedView,
) -> Result<(), DdlError> {
    let ResolvedView {
        columns,
        bindings,
        ast,
        deps,
        access,
    } = resolved;
    let system = interp.namespace_name(nsoid) == Some("pg_catalog");
    check_relation_columns(interp, relkind, &columns, system)?;
    let class_oid = PgClassOid::from_nonzero(interp.alloc_oid()?);
    let composite_oid = PgTypeOid::from_nonzero(interp.alloc_oid()?);
    let array_oid = PgTypeOid::from_nonzero(interp.alloc_oid()?);

    interp.insert_pg_class(PgClass {
        oid: class_oid,
        relname: name.clone(),
        relnamespace: nsoid,
        relkind,
        reltype: Some(composite_oid),
    });
    // PG stores the SELECT body as a `_RETURN` rule in pg_rewrite — only
    // for views/matviews; CTAS-as-table doesn't get one.
    if matches!(relkind, RelKind::View | RelKind::MaterializedView) {
        let bound = bound_references(interp, &deps.references);
        interp.view_references.insert(class_oid, bound);
        interp.view_access.insert(class_oid, access);
        let rewrite_oid = PgRewriteOid::from_nonzero(interp.alloc_oid()?);
        interp.insert_pg_rewrite(PgRewrite {
            oid: rewrite_oid,
            rulename: "_RETURN".to_owned(),
            ev_class: class_oid,
            ev_type: EvType::Select,
            ev_enabled: EvEnabled::Origin,
            is_instead: true,
            ev_qual: None,
            ev_action: SerializedAst { ast, bindings },
        });
    }
    for (i, col) in columns.iter().enumerate() {
        interp.insert_pg_attribute(PgAttribute {
            attrelid: class_oid,
            attname: col.name.clone(),
            atttypid: col.type_oid,
            attnum: (i + 1) as i16,
            attnotnull: col.not_null,
            atthasdef: false,
            attgenerated: None,
            atttypmod: col.typmod,
            attidentity: None,
            attcollation: col.collation,
            attislocal: true,
            attinhcount: 0,
        });
    }
    record_uncertain_collations(interp, class_oid, &columns);
    let phys = super::types::TypePhysical::COMPOSITE;
    // An autogenerated array type named like the row type moves aside.
    super::types::claim_type_name(interp, nsoid, &name)?;
    interp.insert_pg_type(PgType {
        oid: composite_oid,
        typname: name.clone(),
        typnamespace: nsoid,
        typtype: TypType::Composite,
        typcategory: TypCategory::Composite,
        typispreferred: false,
        typrelid: Some(class_oid),
        typelem: None,
        typarray: Some(array_oid),
        typbasetype: None,
        typnotnull: false,
        typtypmod: None,
        typcollation: None,
        typstorage: TypStorage::Extended,
        typlen: phys.typlen,
        typbyval: phys.typbyval,
        typalign: phys.typalign,
        typsubscript: phys.typsubscript,
        typisdefined: true,
    });
    let phys = super::types::TypePhysical::array(interp, composite_oid);
    let array_name = super::types::make_array_type_name(interp, nsoid, &name);
    interp.insert_pg_type(PgType {
        oid: array_oid,
        typname: array_name,
        typnamespace: nsoid,
        typtype: TypType::Base,
        typcategory: TypCategory::Array,
        typispreferred: false,
        typrelid: None,
        typelem: Some(composite_oid),
        typarray: None,
        typbasetype: None,
        typnotnull: false,
        typtypmod: None,
        typcollation: None,
        typstorage: TypStorage::Extended,
        typlen: phys.typlen,
        typbyval: phys.typbyval,
        typalign: phys.typalign,
        typsubscript: phys.typsubscript,
        typisdefined: true,
    });

    // CREATE TABLE AS stores the query's rows, not the query: the table
    // depends on nothing it read (intorel_startup creates a plain table).
    if matches!(relkind, RelKind::View | RelKind::MaterializedView) {
        record_view_dependencies(interp, class_oid, &deps);
    }
    // DefineVirtualRelation / intorel_startup: the view or table is the
    // current role's, with the relation default privileges; its row type and
    // array are dependent types, owned alike.
    let owner = super::roles::current_role(interp);
    super::acl::relation_created(
        interp,
        class_oid,
        nsoid,
        owner,
        super::acl::DefaultAclClass::Table,
    );
    super::acl::type_created_dependent(interp, composite_oid, owner);
    super::acl::type_created_dependent(interp, array_oid, owner);
    Ok(())
}

/// Record the view's `pg_depend` edges: one row per column it reads, per whole
/// relation it reads, per function it calls, and per named type it references.
/// DROP of any referenced object without CASCADE must reject while the view is
/// reachable through these edges.
fn record_view_dependencies(interp: &mut PgCatalog, class_oid: PgClassOid, deps: &ViewDeps) {
    let class_obj = PgGenericOid::from_nonzero(class_oid.into_nonzero());
    let dep = |refclassid: PgClassOid, refobjid: PgGenericOid, refobjsubid: i16| PgDepend {
        classid: PG_CLASS_RELID,
        objid: class_obj,
        objsubid: 0,
        refclassid,
        refobjid,
        refobjsubid,
        deptype: DepType::Normal,
    };
    let mut whole_recorded = std::collections::HashSet::new();
    for (refrelid, refattnum) in &deps.column_refs {
        interp.add_dependency(dep(
            PG_CLASS_RELID,
            PgGenericOid::from_nonzero(refrelid.into_nonzero()),
            *refattnum,
        ));
    }
    for refrelid in &deps.relation_refs {
        if whole_recorded.insert(*refrelid) {
            interp.add_dependency(dep(
                PG_CLASS_RELID,
                PgGenericOid::from_nonzero(refrelid.into_nonzero()),
                0,
            ));
        }
    }
    for proc_oid in &deps.function_refs {
        interp.add_dependency(dep(
            PG_PROC_RELID,
            PgGenericOid::from_nonzero(proc_oid.into_nonzero()),
            0,
        ));
    }
    for type_oid in &deps.type_refs {
        interp.add_dependency(dep(
            PG_TYPE_RELID,
            PgGenericOid::from_nonzero(type_oid.into_nonzero()),
            0,
        ));
    }
    super::depend::record_references(
        interp,
        super::depend::ObjectAddress::relation(class_oid),
        &deps.references,
        DepType::Normal,
    );
}

#[derive(Clone)]
struct ResolvedColumn {
    name: String,
    type_oid: PgTypeOid,
    typmod: Option<i32>,
    not_null: bool,
    /// The expression's collation (`exprCollation`), which PG stores as the
    /// view / CTAS column's `attcollation`.
    collation: Option<crate::oid::PgCollationOid>,
    /// Of a collatable type, the expression has no collation (see
    /// [`crate::resolve::RawColumn::no_collation`]).
    no_collation: bool,
    /// What planning makes of the collation (see [`crate::expr::Planned`]).
    planned: crate::expr::Planned,
    /// The analyzer doesn't know the collation PG stores (see
    /// [`PgCatalog::uncertain_collations`]).
    uncertain_collation: bool,
}

/// query_uses_temp_object (dependency.c, PostgreSQL 19): the first object
/// a view's query depends on that lives in a temporary schema — a
/// relation, a function, a type or an operator — described as
/// getObjectDescription does.
fn temp_dependency(interp: &PgCatalog, deps: &ViewDeps) -> Option<String> {
    use super::depend::{ObjectAddress, Reference};
    let temp = interp.temp_namespace?;
    let relations = deps
        .relation_refs
        .iter()
        .chain(deps.column_refs.iter().map(|(r, _)| r))
        .copied()
        .map(ObjectAddress::relation);
    let functions = deps.function_refs.iter().copied().map(ObjectAddress::proc);
    let types = deps.type_refs.iter().copied().map(ObjectAddress::type_);
    let others = deps.references.iter().filter_map(|r| match r {
        Reference::Object(addr) => Some(*addr),
        Reference::Column(relid, _) => Some(ObjectAddress::relation(*relid)),
        Reference::Named(_) => None,
    });
    let namespace = |addr: &ObjectAddress| {
        let oid = addr.objid.get();
        if addr.classid == crate::pg_catalog::PG_CLASS_RELID {
            crate::oid::PgClassOid::new(oid)
                .and_then(|o| interp.pg_class.get(&o))
                .map(|c| c.relnamespace)
        } else if addr.classid == crate::pg_catalog::PG_PROC_RELID {
            crate::oid::PgProcOid::new(oid)
                .and_then(|o| interp.pg_proc.get(&o))
                .map(|p| p.pronamespace)
        } else if addr.classid == crate::pg_catalog::PG_TYPE_RELID {
            crate::oid::PgTypeOid::new(oid)
                .and_then(|o| interp.pg_type.get(&o))
                .map(|t| t.typnamespace)
        } else if addr.classid == crate::pg_catalog::PG_OPERATOR_RELID {
            crate::oid::PgOperatorOid::new(oid)
                .and_then(|o| interp.pg_operator.get(&o))
                .map(|o| o.oprnamespace)
        } else {
            None
        }
    };
    relations
        .chain(functions)
        .chain(types)
        .chain(others)
        .find(|addr| namespace(addr) == Some(temp))
        .map(|addr| super::depend::describe(interp, addr))
}

#[derive(Default, Clone)]
struct ViewDeps {
    /// `(refrelid, attnum)` pairs — one per distinct column dependency.
    column_refs: Vec<(PgClassOid, i16)>,
    /// `refrelid` values — relations the view reads from (whole-row deps).
    relation_refs: Vec<PgClassOid>,
    /// `pg_proc.oid` values — functions/operators called from the view.
    function_refs: Vec<PgProcOid>,
    /// `pg_type.oid` values — types named explicitly (e.g. as CAST targets).
    type_refs: Vec<PgTypeOid>,
    /// What the analyzed query refers to: the operators it resolved, the
    /// sequences its regclass literals name, the composite fields it
    /// selects, ... (`recordDependencyOnExpr` over the view's query).
    references: Vec<super::depend::Reference>,
}

/// Resolve view columns at creation time, walk the AST to emit a binding
/// stream (one OID-resolved entry per name slot), and derive the
/// `pg_depend` deps from those bindings.
fn resolve_view_now(
    snapshot: &PgCatalog,
    query_node: &ast::Node,
    aliases: &[String],
    planned: bool,
) -> Result<ResolvedView, DdlError> {
    resolve_view_with_params(snapshot, query_node, aliases, &[], planned)
}

/// [`resolve_view_now`] for a query whose `$n` have types (a prepared
/// statement's). `planned`: the query also runs (CREATE TABLE AS with
/// data), with the checks of a query that does
/// ([`crate::resolve::analyze_executed`]).
fn resolve_view_with_params(
    snapshot: &PgCatalog,
    query_node: &ast::Node,
    aliases: &[String],
    param_types: &[PgTypeOid],
    planned: bool,
) -> Result<ResolvedView, DdlError> {
    let inner = query_node;
    let analyze =
        || crate::resolve::analyze_raw_node_with_param_types(snapshot, inner, param_types);
    // The query's dependencies (what a DROP must respect) and what reading
    // the view later requires of the relations, routines and sequences it
    // names.
    let ((analyzed, references), access) = crate::privileges::collect_view(|| {
        super::depend::collect_defined(|| {
            if planned {
                crate::resolve::analyze_executed(snapshot, inner, analyze)
            } else {
                analyze()
            }
        })
    });
    let (raw_columns, _) = analyzed.map_err(|source| DdlError::ViewAnalysis {
        view: String::new(),
        source: Box::new(source),
    })?;

    let columns: Vec<ResolvedColumn> = raw_columns
        .iter()
        .enumerate()
        .map(|(i, col)| {
            let name = if i < aliases.len() {
                aliases[i].clone()
            } else {
                col.name.clone()
            };
            // transformSelectStmt's resolveTargetListUnknowns: an output
            // column still of type unknown is stored as text.
            let type_oid = if col.type_oid == crate::pg_catalog::oid::UNKNOWN {
                crate::pg_catalog::oid::TEXT
            } else {
                col.type_oid
            };
            // exprTypmod of an expression of a domain type is -1 (a column
            // of one has no typmod of its own; CoerceToDomain's is -1): the
            // domain's typmod is its own.
            let domain = snapshot
                .pg_type
                .get(&type_oid)
                .is_some_and(|t| t.typtype == crate::pg_catalog::TypType::Domain);
            ResolvedColumn {
                name,
                type_oid,
                typmod: col.typmod.filter(|_| !domain),
                not_null: !col.nullable,
                collation: col
                    .collation
                    .or_else(|| interp_type_collation(snapshot, type_oid)),
                no_collation: col.no_collation.is_some(),
                // The query is the statement's: its top level's plan.
                planned: col.planned_top.unwrap_or(col.planned),
                // A column read as is from one whose collation is
                // uncertain: so is its own.
                uncertain_collation: col.origin.as_ref().is_some_and(|o| {
                    snapshot
                        .attributes_of(o.relid)
                        .iter()
                        .find(|a| a.attname == o.column && a.attnum > 0)
                        .is_some_and(|a| {
                            snapshot.uncertain_collations.contains(&(o.relid, a.attnum))
                        })
                }),
            }
        })
        .collect();

    let (bindings, mut deps) = collect_view_bindings_and_deps(query_node, snapshot);
    deps.references = references;

    Ok(ResolvedView {
        columns,
        bindings,
        ast: Some(query_node.clone()),
        deps,
        access,
    })
}

/// Parse `sql` (typically a synthetic `SELECT …`), pluck out a subnode
/// via `pick`, walk it through the [`BindingWalker`] against `snapshot`, and
/// return a [`SerializedAst`] of the picked node + bindings. Powers
/// [`crate::pg_catalog::PgCatalog::serialize_expression`] and friends so
/// the seed exporter can capture index expressions and predicates with
/// fully resolved bindings, without re-implementing the walker.
#[cfg(any(test, feature = "internal", feature = "catalog_export"))]
pub(crate) fn serialize_subnode(
    snapshot: &PgCatalog,
    sql: &str,
    pick: impl Fn(Option<&ast::Node>) -> Option<&ast::Node>,
) -> Result<crate::pg_catalog::SerializedAst, super::DdlError> {
    crate::stack::with_stack(|| serialize_subnode_here(snapshot, sql, pick))
}

#[cfg(any(test, feature = "internal", feature = "catalog_export"))]
fn serialize_subnode_here(
    snapshot: &PgCatalog,
    sql: &str,
    pick: impl Fn(Option<&ast::Node>) -> Option<&ast::Node>,
) -> Result<crate::pg_catalog::SerializedAst, super::DdlError> {
    let parsed = typedpg_pg_query::parse(sql)
        .map_err(|e| super::DdlError::Parse(format!("failed to parse `{sql}`: {e}")))?;
    let proto = parsed.tree;
    let stmt = proto
        .stmts
        .first()
        .map(|s| &s.stmt)
        .ok_or_else(|| super::DdlError::Parse(format!("`{sql}` produced no statement")))?;
    let target = pick(stmt.as_ref())
        .ok_or_else(|| super::DdlError::Parse(format!("`{sql}` has no expression to extract")))?;
    let mut walker = BindingWalker::default();
    walker.walk(target, snapshot);
    Ok(crate::pg_catalog::SerializedAst {
        ast: Some(target.clone()),
        bindings: walker.bindings,
    })
}

/// `pick` for `serialize_expression`: pulls the value of the first target
/// in `SELECT <expr>`. Returns `None` if the input wasn't a `SelectStmt`
/// or has no targets.
#[cfg(any(test, feature = "internal", feature = "catalog_export"))]
pub(crate) fn extract_first_target(stmt: Option<&ast::Node>) -> Option<&ast::Node> {
    let ast::Node::SelectStmt(sel) = stmt? else {
        return None;
    };
    let target = sel.target_list.first()?;
    let ast::Node::ResTarget(rt) = target.as_ref()? else {
        return None;
    };
    rt.val.as_ref()
}

/// `pick` for `serialize_predicate`: extracts the WHERE clause of
/// `SELECT 1 WHERE <pred>`. Returns `None` if there is no WHERE.
#[cfg(any(test, feature = "internal", feature = "catalog_export"))]
pub(crate) fn extract_where(stmt: Option<&ast::Node>) -> Option<&ast::Node> {
    let ast::Node::SelectStmt(sel) = stmt? else {
        return None;
    };
    sel.where_clause.as_ref()
}

// ─── Structured walker: emits bindings + collects deps ──────────────────────

/// A table source visible in the current FROM scope.
#[derive(Debug, Clone)]
struct FromSource {
    alias: String,
    /// `Some` for real relations (tracked as deps); `None` for CTEs and
    /// subquery sources that are local to the query.
    relid: Option<PgClassOid>,
    /// Column names visible through this source.
    columns: Vec<String>,
}

/// Walks a view's parsed AST and emits a deterministic stream of
/// [`AstBinding`]s — one per name slot, in pre-order. Deps for `pg_depend`
/// are derived from the bindings as a side-effect.
#[derive(Default)]
pub(crate) struct BindingWalker {
    bindings: Vec<AstBinding>,
    /// The columns a `*` / `alias.*` expands to. Not name slots (the star
    /// stays one `Unresolved` binding), but the view depends on each of
    /// them, as PG records for the expanded target list.
    star_columns: Vec<(PgClassOid, i16)>,
}

impl BindingWalker {
    #[cfg(any(test, feature = "internal", feature = "catalog_export"))]
    fn walk(&mut self, node: &ast::Node, snapshot: &PgCatalog) {
        let scope_stack: Vec<Vec<FromSource>> = Vec::new();
        self.walk_node(node, snapshot, &scope_stack);
    }
}

/// The relations and `(relation, attnum)` columns a SELECT reads, as the
/// binding walker resolves them (what a view would record in pg_depend).
pub(crate) fn statement_references(
    snapshot: &PgCatalog,
    query_node: &ast::Node,
) -> (Vec<PgClassOid>, Vec<(PgClassOid, i16)>) {
    let (_, deps) = collect_view_bindings_and_deps(query_node, snapshot);
    (deps.relation_refs, deps.column_refs)
}

/// The functions a statement calls (the view machinery's binding walker).
pub(crate) fn statement_functions(snapshot: &PgCatalog, query_node: &ast::Node) -> Vec<PgProcOid> {
    let (_, deps) = collect_view_bindings_and_deps(query_node, snapshot);
    deps.function_refs
}

fn collect_view_bindings_and_deps(
    query_node: &ast::Node,
    snapshot: &PgCatalog,
) -> (Vec<AstBinding>, ViewDeps) {
    let mut walker = BindingWalker::default();
    let scope_stack: Vec<Vec<FromSource>> = Vec::new();
    walker.walk_node(query_node, snapshot, &scope_stack);
    let mut deps = derive_deps_from_bindings(&walker.bindings);
    deps.column_refs.extend(walker.star_columns);
    deps.column_refs.sort();
    deps.column_refs.dedup();
    (walker.bindings, deps)
}

/// Derive distinct dep lists from emitted bindings — the pieces
/// `install_relation` writes into `pg_depend`. Mirrors PG: a view depends on
/// every relation/column it reads, every function it calls, and every named
/// type (cast target / typed literal). DROP of any of these without CASCADE
/// must reject when a view is reachable through these edges.
fn derive_deps_from_bindings(bindings: &[AstBinding]) -> ViewDeps {
    let mut relation_refs: Vec<PgClassOid> = Vec::new();
    let mut column_refs: Vec<(PgClassOid, i16)> = Vec::new();
    let mut function_refs: Vec<PgProcOid> = Vec::new();
    let mut type_refs: Vec<PgTypeOid> = Vec::new();
    for b in bindings {
        match b {
            AstBinding::Relation(oid) => relation_refs.push(*oid),
            AstBinding::Column(rel, attnum) => column_refs.push((*rel, *attnum)),
            AstBinding::Function(oid) => function_refs.push(*oid),
            AstBinding::Type(oid) => type_refs.push(*oid),
            AstBinding::Unresolved => {}
        }
    }
    relation_refs.sort();
    relation_refs.dedup();
    column_refs.sort();
    column_refs.dedup();
    function_refs.sort();
    function_refs.dedup();
    type_refs.sort();
    type_refs.dedup();
    ViewDeps {
        column_refs,
        relation_refs,
        function_refs,
        type_refs,
        references: Vec::new(),
    }
}

impl BindingWalker {
    fn walk_node(&mut self, node: &ast::Node, snapshot: &PgCatalog, stack: &[Vec<FromSource>]) {
        match node {
            ast::Node::SelectStmt(sel) => self.walk_select(sel, snapshot, stack),
            ast::Node::ColumnRef(cr) => self.record_column_ref(cr, snapshot, stack),
            ast::Node::AExpr(expr) => {
                if let Some(lexpr) = expr.lexpr.as_ref() {
                    self.walk_node(lexpr, snapshot, stack);
                }
                if let Some(rexpr) = expr.rexpr.as_ref() {
                    self.walk_node(rexpr, snapshot, stack);
                }
            }
            // `(expr).field` / `(expr)[i]`: the argument and every subscript
            // are expressions of their own (a function under an indirection
            // is recorded like any other).
            ast::Node::AIndirection(ind) => {
                if let Some(arg) = ind.arg.as_ref() {
                    self.walk_node(arg, snapshot, stack);
                }
                for step in ind.indirection.iter().flatten() {
                    self.walk_node(step, snapshot, stack);
                }
            }
            ast::Node::AIndices(ind) => {
                if let Some(lidx) = ind.lidx.as_ref() {
                    self.walk_node(lidx, snapshot, stack);
                }
                if let Some(uidx) = ind.uidx.as_ref() {
                    self.walk_node(uidx, snapshot, stack);
                }
            }
            ast::Node::BoolExpr(b) => {
                for arg in b.args.iter().flatten() {
                    self.walk_node(arg, snapshot, stack);
                }
            }
            ast::Node::FuncCall(fc) => {
                self.bind_func_name(&fc.funcname, snapshot);
                for arg in fc.args.iter().flatten() {
                    self.walk_node(arg, snapshot, stack);
                }
                if let Some(over) = fc.over.as_deref() {
                    for arg in over.partition_clause.iter().flatten() {
                        self.walk_node(arg, snapshot, stack);
                    }
                    for arg in over.order_clause.iter().flatten() {
                        self.walk_node(arg, snapshot, stack);
                    }
                }
                if let Some(filter) = fc.agg_filter.as_ref() {
                    self.walk_node(filter, snapshot, stack);
                }
            }
            ast::Node::TypeCast(tc) => {
                if let Some(arg) = tc.arg.as_ref() {
                    self.walk_node(arg, snapshot, stack);
                }
                if let Some(tn) = tc.type_name.as_ref() {
                    self.bind_type_name(tn, snapshot);
                }
            }
            ast::Node::CollateClause(cc) => {
                if let Some(arg) = cc.arg.as_ref() {
                    self.walk_node(arg, snapshot, stack);
                }
            }
            ast::Node::NamedArgExpr(na) => {
                if let Some(arg) = na.arg.as_ref() {
                    self.walk_node(arg, snapshot, stack);
                }
            }
            ast::Node::CoalesceExpr(c) => {
                for arg in c.args.iter().flatten() {
                    self.walk_node(arg, snapshot, stack);
                }
            }
            ast::Node::MinMaxExpr(m) => {
                for arg in m.args.iter().flatten() {
                    self.walk_node(arg, snapshot, stack);
                }
            }
            ast::Node::NullIfExpr(n) => {
                for arg in n.args.iter().flatten() {
                    self.walk_node(arg, snapshot, stack);
                }
            }
            ast::Node::CaseExpr(c) => {
                if let Some(arg) = c.arg.as_ref() {
                    self.walk_node(arg, snapshot, stack);
                }
                for branch in c.args.iter().flatten() {
                    self.walk_node(branch, snapshot, stack);
                }
                if let Some(def) = c.defresult.as_ref() {
                    self.walk_node(def, snapshot, stack);
                }
            }
            ast::Node::CaseWhen(w) => {
                if let Some(expr) = w.expr.as_ref() {
                    self.walk_node(expr, snapshot, stack);
                }
                if let Some(result) = w.result.as_ref() {
                    self.walk_node(result, snapshot, stack);
                }
            }
            ast::Node::SubLink(sl) => {
                if let Some(testexpr) = sl.testexpr.as_ref() {
                    self.walk_node(testexpr, snapshot, stack);
                }
                if let Some(subselect) = sl.subselect.as_ref() {
                    self.walk_node(subselect, snapshot, stack);
                }
            }
            ast::Node::NullTest(t) => {
                if let Some(arg) = t.arg.as_ref() {
                    self.walk_node(arg, snapshot, stack);
                }
            }
            ast::Node::BooleanTest(t) => {
                if let Some(arg) = t.arg.as_ref() {
                    self.walk_node(arg, snapshot, stack);
                }
            }
            ast::Node::List(l) => {
                for item in l.items.iter().flatten() {
                    self.walk_node(item, snapshot, stack);
                }
            }
            ast::Node::ArrayExpr(a) => {
                for elem in a.elements.iter().flatten() {
                    self.walk_node(elem, snapshot, stack);
                }
            }
            ast::Node::RowExpr(r) => {
                for arg in r.args.iter().flatten() {
                    self.walk_node(arg, snapshot, stack);
                }
            }
            ast::Node::ResTarget(rt) => {
                if let Some(val) = rt.val.as_ref() {
                    self.walk_node(val, snapshot, stack);
                }
            }
            ast::Node::SortBy(sb) => {
                if let Some(n) = sb.node.as_ref() {
                    self.walk_node(n, snapshot, stack);
                }
            }
            _ => {}
        }
    }

    fn walk_select(
        &mut self,
        sel: &ast::SelectStmt,
        snapshot: &PgCatalog,
        parent_stack: &[Vec<FromSource>],
    ) {
        if let Some(larg) = sel.larg.as_deref() {
            self.walk_select(larg, snapshot, parent_stack);
        }
        if let Some(rarg) = sel.rarg.as_deref() {
            self.walk_select(rarg, snapshot, parent_stack);
        }

        let mut frame: Vec<FromSource> = Vec::new();

        if let Some(with) = sel.with_clause.as_ref() {
            for cte_node in &with.ctes {
                if let Some(ast::Node::CommonTableExpr(cte)) = cte_node.as_ref() {
                    if let Some(query) = cte.ctequery.as_ref() {
                        let mut stack_with_partial = parent_stack.to_vec();
                        stack_with_partial.push(frame.clone());
                        self.walk_node(query, snapshot, &stack_with_partial);
                    }
                    frame.push(FromSource {
                        alias: cte.ctename.clone(),
                        relid: None,
                        columns: cte
                            .aliascolnames
                            .iter()
                            .filter_map(|n| match n.as_ref()? {
                                ast::Node::String(s) => Some(s.sval.clone()),
                                _ => None,
                            })
                            .collect(),
                    });
                }
            }
        }

        for from_item in sel.from_clause.iter().flatten() {
            self.process_from_item(from_item, snapshot, parent_stack, &mut frame);
        }

        let mut stack = parent_stack.to_vec();
        stack.push(frame);

        for t in sel.target_list.iter().flatten() {
            self.walk_node(t, snapshot, &stack);
        }
        if let Some(w) = sel.where_clause.as_ref() {
            self.walk_node(w, snapshot, &stack);
        }
        for g in sel.group_clause.iter().flatten() {
            self.walk_node(g, snapshot, &stack);
        }
        if let Some(h) = sel.having_clause.as_ref() {
            self.walk_node(h, snapshot, &stack);
        }
        for s in sel.sort_clause.iter().flatten() {
            self.walk_node(s, snapshot, &stack);
        }
        for d in sel.distinct_clause.iter().flatten() {
            self.walk_node(d, snapshot, &stack);
        }
    }

    fn process_from_item(
        &mut self,
        node: &ast::Node,
        snapshot: &PgCatalog,
        parent_stack: &[Vec<FromSource>],
        frame: &mut Vec<FromSource>,
    ) {
        match node {
            ast::Node::RangeVar(rv) => {
                let alias = rv
                    .alias
                    .as_ref()
                    .map(|a| a.aliasname.clone())
                    .unwrap_or_else(|| rv.relname.clone());

                if rv.schemaname.is_empty()
                    && parent_stack
                        .iter()
                        .chain(std::iter::once(&*frame))
                        .any(|f| f.iter().any(|s| s.relid.is_none() && s.alias == rv.relname))
                {
                    // CTE / subquery alias shadowing: keep literal AST text.
                    self.bindings.push(AstBinding::Unresolved);
                    frame.push(FromSource {
                        alias,
                        relid: None,
                        columns: Vec::new(),
                    });
                    return;
                }

                let schema = if rv.schemaname.is_empty() {
                    None
                } else {
                    Some(rv.schemaname.as_str())
                };

                if let Some(table) = snapshot.resolve_table(schema, &rv.relname) {
                    let relid = table.oid;
                    self.bindings.push(AstBinding::Relation(relid));
                    let columns = snapshot
                        .attributes_of(relid)
                        .iter()
                        .map(|a| a.attname.clone())
                        .collect();
                    frame.push(FromSource {
                        alias,
                        relid: Some(relid),
                        columns,
                    });
                } else {
                    self.bindings.push(AstBinding::Unresolved);
                    frame.push(FromSource {
                        alias,
                        relid: None,
                        columns: Vec::new(),
                    });
                }
            }
            ast::Node::JoinExpr(join) => {
                if let Some(larg) = join.larg.as_ref() {
                    self.process_from_item(larg, snapshot, parent_stack, frame);
                }
                if let Some(rarg) = join.rarg.as_ref() {
                    self.process_from_item(rarg, snapshot, parent_stack, frame);
                }
                let mut stack = parent_stack.to_vec();
                stack.push(frame.clone());
                if let Some(q) = join.quals.as_ref() {
                    self.walk_node(q, snapshot, &stack);
                }
            }
            ast::Node::RangeSubselect(sub) => {
                let alias = sub
                    .alias
                    .as_ref()
                    .map(|a| a.aliasname.clone())
                    .unwrap_or_else(|| "_subquery".into());
                if let Some(query) = sub.subquery.as_ref() {
                    self.walk_node(query, snapshot, parent_stack);
                }
                frame.push(FromSource {
                    alias,
                    relid: None,
                    columns: Vec::new(),
                });
            }
            ast::Node::RangeFunction(rf) => {
                for arg in rf.functions.iter().flatten() {
                    self.walk_node(arg, snapshot, parent_stack);
                }
                let alias = rf
                    .alias
                    .as_ref()
                    .map(|a| a.aliasname.clone())
                    .unwrap_or_else(|| "_srf".into());
                frame.push(FromSource {
                    alias,
                    relid: None,
                    columns: Vec::new(),
                });
            }
            _ => {}
        }
    }

    fn record_column_ref(
        &mut self,
        cr: &ast::ColumnRef,
        snapshot: &PgCatalog,
        stack: &[Vec<FromSource>],
    ) {
        let parts: Vec<String> = cr
            .fields
            .iter()
            .filter_map(|f| match f.as_ref()? {
                ast::Node::String(s) => Some(s.sval.clone()),
                _ => None,
            })
            .collect();

        let is_star = matches!(
            cr.fields.last().and_then(|f| f.as_ref()),
            Some(ast::Node::AStar(_))
        );
        if is_star {
            // `*` expands to every column of the query level's FROM items,
            // `alias.*` to that item's.
            let sources: Vec<&FromSource> = match parts.as_slice() {
                [] => stack.last().map(|f| f.iter().collect()).unwrap_or_default(),
                [.., alias] => stack
                    .iter()
                    .rev()
                    .find_map(|f| f.iter().find(|s| &s.alias == alias))
                    .into_iter()
                    .collect(),
            };
            for source in sources {
                if let Some(relid) = source.relid {
                    self.star_columns.extend(
                        snapshot
                            .attributes_of(relid)
                            .iter()
                            .map(|a| (relid, a.attnum)),
                    );
                }
            }
        }

        let resolved: Option<(PgClassOid, i16)> = match parts.as_slice() {
            [col] if !is_star => resolve_column_unqualified(col, snapshot, stack),
            [alias, col] if !is_star => resolve_column_aliased(alias, col, snapshot, stack),
            [schema, relation, col] if !is_star => {
                resolve_column_qualified(schema, relation, col, snapshot, stack)
            }
            _ => None,
        };
        match resolved {
            Some((relid, attnum)) => self.bindings.push(AstBinding::Column(relid, attnum)),
            None => self.bindings.push(AstBinding::Unresolved),
        }
    }

    /// FuncCall.funcname is `[name]` or `[schema, name]`. We look up the
    /// function (any overload) and emit a Function binding with its OID,
    /// which the view's `pg_depend` entries are derived from.
    fn bind_func_name(&mut self, funcname: &[Option<ast::Node>], snapshot: &PgCatalog) {
        let parts: Vec<&str> = funcname
            .iter()
            .filter_map(|n| match n.as_ref()? {
                ast::Node::String(s) => Some(s.sval.as_str()),
                _ => None,
            })
            .collect();
        let (schema, name) = match parts.as_slice() {
            [n] => (None, *n),
            [s, n] => (Some(*s), *n),
            _ => {
                self.bindings.push(AstBinding::Unresolved);
                return;
            }
        };
        let resolved = snapshot
            .find_functions(schema, name)
            .into_iter()
            .next()
            .map(|p| p.oid);
        match resolved {
            Some(oid) => self.bindings.push(AstBinding::Function(oid)),
            None => self.bindings.push(AstBinding::Unresolved),
        }
    }

    /// TypeName.names is `[name]` or `[schema, name]`. Emit a Type binding
    /// when the type resolves; applier rewrites the schema portion.
    fn bind_type_name(&mut self, tn: &ast::TypeName, snapshot: &PgCatalog) {
        let parts: Vec<&str> = tn
            .names
            .iter()
            .filter_map(|n| match n.as_ref()? {
                ast::Node::String(s) => Some(s.sval.as_str()),
                _ => None,
            })
            .collect();
        let (schema, name) = match parts.as_slice() {
            [n] => (None, *n),
            [s, n] => (Some(*s), *n),
            _ => {
                self.bindings.push(AstBinding::Unresolved);
                return;
            }
        };
        let resolved = snapshot.resolve_type_by_name(schema, name).map(|t| t.oid);
        match resolved {
            Some(oid) => self.bindings.push(AstBinding::Type(oid)),
            None => self.bindings.push(AstBinding::Unresolved),
        }
    }
}

fn resolve_column_unqualified(
    col: &str,
    snapshot: &PgCatalog,
    stack: &[Vec<FromSource>],
) -> Option<(PgClassOid, i16)> {
    let mut found: Option<&FromSource> = None;
    for frame in stack.iter().rev() {
        for src in frame {
            if src.columns.iter().any(|c| c == col) {
                if found.is_some() {
                    return None;
                }
                found = Some(src);
            }
        }
        if found.is_some() {
            break;
        }
    }
    let src = found?;
    let relid = src.relid?;
    let attr = snapshot.attribute_by_name(relid, col)?;
    Some((relid, attr.attnum))
}

fn resolve_column_aliased(
    alias: &str,
    col: &str,
    snapshot: &PgCatalog,
    stack: &[Vec<FromSource>],
) -> Option<(PgClassOid, i16)> {
    for frame in stack.iter().rev() {
        for src in frame {
            let qn_name_match = src
                .relid
                .and_then(|r| snapshot.pg_class.get(&r))
                .is_some_and(|c| c.relname.as_str() == alias);
            if src.alias == alias || qn_name_match {
                let relid = src.relid?;
                let attr = snapshot.attribute_by_name(relid, col)?;
                return Some((relid, attr.attnum));
            }
        }
    }
    None
}

fn resolve_column_qualified(
    schema: &str,
    relation: &str,
    col: &str,
    snapshot: &PgCatalog,
    stack: &[Vec<FromSource>],
) -> Option<(PgClassOid, i16)> {
    for frame in stack.iter().rev() {
        for src in frame {
            if let Some(relid) = src.relid
                && let Some(class) = snapshot.pg_class.get(&relid)
                && snapshot
                    .namespace_name(class.relnamespace)
                    .is_some_and(|ns| ns == schema)
                && class.relname.as_str() == relation
                && let Some(attr) = snapshot.attribute_by_name(relid, col)
            {
                return Some((relid, attr.attnum));
            }
        }
    }
    None
}

// ─── Dependency checking ────────────────────────────────────────────────────

/// Find all view OIDs whose `pg_depend` row points at `(refclassid, refobjid)`.
/// Specialized callers below funnel into this — `find_dependent_views` for
/// table/view OIDs, plus the proc/type variants used by DROP FUNCTION /
/// DROP TYPE.
fn find_views_depending_on(
    snapshot: &PgCatalog,
    refclassid: PgClassOid,
    refobjid: u32,
) -> Vec<PgClassOid> {
    let mut out: Vec<PgClassOid> = snapshot
        .iter_pg_depend()
        .filter(|d| {
            matches!(d.deptype, DepType::Normal)
                && d.classid == PG_CLASS_RELID
                && d.refclassid == refclassid
                && d.refobjid.get() == refobjid
        })
        .filter_map(|d| {
            let obj = PgClassOid::new(d.objid.get())?;
            let class = snapshot.pg_class.get(&obj)?;
            matches!(class.relkind, RelKind::View | RelKind::MaterializedView).then_some(obj)
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Find all view OIDs that depend on the given table or view OID.
pub fn find_dependent_views(snapshot: &PgCatalog, relid: PgClassOid) -> Vec<PgClassOid> {
    find_views_depending_on(snapshot, PG_CLASS_RELID, relid.get())
}

/// Find all view OIDs that depend on the given function/aggregate/window OID.
pub fn find_views_depending_on_function(
    snapshot: &PgCatalog,
    proc_oid: PgProcOid,
) -> Vec<PgClassOid> {
    find_views_depending_on(snapshot, PG_PROC_RELID, proc_oid.get())
}

/// Find all view OIDs that depend on the given type OID.
pub fn find_views_depending_on_type(snapshot: &PgCatalog, type_oid: PgTypeOid) -> Vec<PgClassOid> {
    find_views_depending_on(snapshot, PG_TYPE_RELID, type_oid.get())
}

/// Find all view OIDs that depend on a specific column of a relation.
pub fn find_views_depending_on_column(
    snapshot: &PgCatalog,
    relid: PgClassOid,
    column_name: &str,
) -> Vec<PgClassOid> {
    let Some(attr) = snapshot.attribute_by_name(relid, column_name) else {
        return Vec::new();
    };
    let attnum = attr.attnum;
    let mut out: Vec<PgClassOid> = snapshot
        .iter_pg_depend()
        .filter(|d| {
            matches!(d.deptype, DepType::Normal)
                && d.classid == PG_CLASS_RELID
                && d.refclassid == PG_CLASS_RELID
                && d.refobjid.get() == relid.get()
                && d.refobjsubid == attnum
        })
        .filter_map(|d| {
            let obj = PgClassOid::new(d.objid.get())?;
            let class = snapshot.pg_class.get(&obj)?;
            matches!(class.relkind, RelKind::View | RelKind::MaterializedView).then_some(obj)
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Drop views by OID, transitively dropping any views that depend on them.
pub fn drop_views(snapshot: &mut PgCatalog, view_oids: &[PgClassOid]) {
    let mut to_drop: Vec<PgClassOid> = view_oids.to_vec();
    let mut dropped: Vec<PgClassOid> = Vec::new();

    while let Some(oid) = to_drop.pop() {
        if dropped.contains(&oid) {
            continue;
        }
        let dependents = find_dependent_views(snapshot, oid);
        for dep in dependents {
            if !dropped.contains(&dep) {
                to_drop.push(dep);
            }
        }
        super::drop::drop_relation_by_oid(snapshot, oid);
        dropped.push(oid);
    }
}

// ─── Nullability refresh ────────────────────────────────────────────────────

/// Re-derive the nullability of the views (and matviews) that read relation
/// `relid`, and of the views reading those, after the NOT NULL-ness of one of
/// its columns changed.
///
/// PG never freezes a view's column nullability: the rewriter re-expands the
/// view over its base tables on every use, so `ALTER TABLE t ALTER b DROP NOT
/// NULL` is immediately visible through `v`. The analyzer resolves views once
/// at CREATE time, so it re-analyzes the stored body here. When that is not
/// possible (the body no longer resolves, or not to what it read at CREATE
/// time — e.g. after a rename —, or its shape changed; see
/// [`reanalyze_view`]) and the change can only have *added* NULLs
/// (`relaxing`), every column of the view is conservatively marked
/// nullable.
///
/// A materialized view is no re-expansion but the rows its query returned
/// when it was last populated (CREATE, REFRESH): a change adding NULLs
/// reaches it (the next REFRESH, which the application may run at any
/// time, stores them), but one ruling NULLs out says nothing of the rows
/// it already holds until a REFRESH re-derives it
/// ([`refresh_materialized_view`]).
///
/// Relation-level dependencies are used (not column-level ones) because a
/// `SELECT *` body records no column references.
pub(crate) fn refresh_dependent_view_nullability(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    relaxing: bool,
) {
    let views = find_dependent_views(interp, relid);
    rederive_view_nullability(interp, views, relaxing, None);
}

/// Re-derive the nullability of `views` and of the views reading those (see
/// [`refresh_dependent_view_nullability`]). A materialized view keeps the
/// NOT NULL-ness of the rows it holds, only ever losing it — unless it is
/// `refreshed`, whose rows were just replaced by its query's.
fn rederive_view_nullability(
    interp: &mut PgCatalog,
    views: Vec<PgClassOid>,
    relaxing: bool,
    refreshed: Option<PgClassOid>,
) {
    let mut queue: std::collections::VecDeque<PgClassOid> = views.into();
    let mut seen = std::collections::HashSet::new();
    while let Some(view) = queue.pop_front() {
        if !seen.insert(view) {
            continue;
        }
        let keeps_rows = refreshed != Some(view)
            && interp
                .pg_class
                .get(&view)
                .is_some_and(|c| c.relkind == RelKind::MaterializedView);
        let reanalyzed = reanalyze_view(interp, view);
        let attrs = interp.attributes_of(view).to_vec();
        let new_not_null: Option<Vec<bool>> = reanalyzed.and_then(|cols| {
            (cols.len() >= attrs.len()
                && attrs
                    .iter()
                    .zip(&cols)
                    .all(|(a, c)| a.atttypid == c.type_oid))
            .then(|| {
                attrs
                    .iter()
                    .zip(&cols)
                    .map(|(a, c)| !c.nullable && (a.attnotnull || !keeps_rows))
                    .collect()
            })
        });
        let changed = match (new_not_null, relaxing) {
            (Some(flags), _) => {
                let changed = attrs.iter().zip(&flags).any(|(a, &nn)| a.attnotnull != nn);
                if let Some(view_attrs) = interp.pg_attribute.get_mut(&view) {
                    for (a, nn) in view_attrs.iter_mut().zip(flags) {
                        a.attnotnull = nn;
                    }
                }
                changed
            }
            (None, true) => {
                let changed = attrs.iter().any(|a| a.attnotnull);
                if let Some(view_attrs) = interp.pg_attribute.get_mut(&view) {
                    for a in view_attrs.iter_mut() {
                        a.attnotnull = false;
                    }
                }
                changed
            }
            (None, false) => false,
        };
        if changed {
            queue.extend(find_dependent_views(interp, view));
        }
    }
}

// ─── AST rewriting entry points (called from ALTER handlers) ────────────────
//
// With the OID-resolved binding side-table, these are now no-ops: a rename
// or schema move doesn't change any view's stored AST, because the bindings
// keep pointing at the same OIDs. The functions are preserved as named
// callsites in case we reintroduce a side-effect later (e.g. invalidating a
// cached analysis).

pub fn rewrite_views_on_table_rename(
    _snapshot: &mut PgCatalog,
    _old_schema: &str,
    _old_name: &str,
    _new_schema: &str,
    _new_name: &str,
) {
}

pub fn rewrite_views_on_column_rename(
    _snapshot: &mut PgCatalog,
    _relid: PgClassOid,
    _old_col: &str,
    _new_col: &str,
) {
}

pub fn rewrite_views_on_schema_rename(
    _snapshot: &mut PgCatalog,
    _old_schema: &str,
    _new_schema: &str,
) {
}

/// The default collation of `type_oid` (`typcollation`).
/// Record which of a view's or CREATE TABLE AS table's columns have an
/// uncertain collation (see [`PgCatalog::uncertain_collations`]).
fn record_uncertain_collations(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    columns: &[ResolvedColumn],
) {
    interp.uncertain_collations.retain(|(r, _)| *r != relid);
    for (i, col) in columns.iter().enumerate() {
        if col.uncertain_collation {
            interp.uncertain_collations.insert((relid, (i + 1) as i16));
        }
    }
}

fn interp_type_collation(
    snapshot: &PgCatalog,
    type_oid: PgTypeOid,
) -> Option<crate::oid::PgCollationOid> {
    snapshot.pg_type.get(&type_oid).and_then(|t| t.typcollation)
}

/// What the rewriter (rewriteTargetView) needs from a view's stored query
/// to update it automatically. Computed when the view is (re)defined: the
/// stored query never changes afterwards.
#[derive(Clone, Debug)]
pub(crate) struct ViewUpdatability {
    /// `view_query_is_auto_updatable(query, check_cols = false)`: why the
    /// view can't be updated automatically, if it can't.
    pub(crate) not_updatable: Option<&'static str>,
    /// The single base relation (when auto-updatable).
    pub(crate) base: Option<PgClassOid>,
    /// Per view column, in attnum order: the base column's attnum, or why
    /// the column isn't updatable (`view_col_is_auto_updatable`).
    pub(crate) columns: Vec<Result<i16, &'static str>>,
}

impl ViewUpdatability {
    /// `view_query_is_auto_updatable` with `check_cols`: at least one
    /// column must be updatable (INSERT / UPDATE need one).
    pub(crate) fn reason(&self, check_cols: bool) -> Option<&'static str> {
        self.not_updatable.or_else(|| {
            (check_cols && !self.columns.iter().any(Result::is_ok))
                .then_some("Views that have no updatable columns are not automatically updatable.")
        })
    }
}

/// view_query_is_auto_updatable / view_col_is_auto_updatable
/// (rewriteHandler.c) over a view's defining SELECT.
pub(crate) fn view_updatability(interp: &PgCatalog, query: &ast::Node) -> ViewUpdatability {
    let not = |reason: &'static str| ViewUpdatability {
        not_updatable: Some(reason),
        base: None,
        columns: Vec::new(),
    };
    let ast::Node::SelectStmt(sel) = query else {
        return not(
            "Views that do not select from a single table or view are not automatically updatable.",
        );
    };
    if sel.op != ast::SetOperation::SetopNone {
        return not(
            "Views containing UNION, INTERSECT, or EXCEPT are not automatically updatable.",
        );
    }
    if !sel.distinct_clause.is_empty() {
        return not("Views containing DISTINCT are not automatically updatable.");
    }
    if !sel.group_clause.is_empty() {
        return not("Views containing GROUP BY are not automatically updatable.");
    }
    if sel.having_clause.is_some() {
        return not("Views containing HAVING are not automatically updatable.");
    }
    if sel.with_clause.is_some() {
        return not("Views containing WITH are not automatically updatable.");
    }
    if sel.limit_count.is_some() || sel.limit_offset.is_some() {
        return not("Views containing LIMIT or OFFSET are not automatically updatable.");
    }
    // hasAggs / hasWindowFuncs cover the whole query level (an ORDER BY
    // aggregate makes it one), hasTargetSRFs only the target list.
    let mut kinds = crate::expr::FuncKindPresence::default();
    let mut srf = false;
    for target in &sel.target_list {
        let Some(ast::Node::ResTarget(rt)) = target.as_ref() else {
            continue;
        };
        let Some(val) = rt.val.as_ref() else {
            continue;
        };
        let found = crate::expr::detect_func_kinds(rt.val.as_ref(), interp);
        kinds.has_aggregate |= found.has_aggregate;
        kinds.has_window |= found.has_window;
        srf |= returns_set(interp, val);
    }
    for sort in &sel.sort_clause {
        let found = crate::expr::detect_func_kinds(sort.as_ref(), interp);
        kinds.has_aggregate |= found.has_aggregate;
        kinds.has_window |= found.has_window;
    }
    if kinds.has_aggregate {
        return not("Views that return aggregate functions are not automatically updatable.");
    }
    if kinds.has_window {
        return not("Views that return window functions are not automatically updatable.");
    }
    if srf {
        return not("Views that return set-returning functions are not automatically updatable.");
    }
    let single_table =
        "Views that do not select from a single table or view are not automatically updatable.";
    let [from] = sel.from_clause.as_slice() else {
        return not(single_table);
    };
    let (rv, tablesample) = match from.as_ref() {
        Some(ast::Node::RangeVar(rv)) => (rv, false),
        Some(ast::Node::RangeTableSample(ts)) => match ts.relation.as_ref() {
            Some(ast::Node::RangeVar(rv)) => (rv, true),
            _ => return not(single_table),
        },
        _ => return not(single_table),
    };
    let Ok((_, base)) = super::util::lookup_relation(interp, rv) else {
        return not(single_table);
    };
    if !matches!(
        interp.pg_class.get(&base).map(|c| c.relkind),
        Some(RelKind::Table | RelKind::Partitioned | RelKind::View | RelKind::ForeignTable)
    ) {
        return not(single_table);
    }
    if tablesample {
        return not("Views containing TABLESAMPLE are not automatically updatable.");
    }
    let alias = rv
        .alias
        .as_ref()
        .map_or(rv.relname.as_str(), |a| a.aliasname.as_str());
    let base_attrs = interp.attributes_of(base);
    let not_a_column =
        "View columns that are not columns of their base relation are not updatable.";
    // The column a name denotes: a base column, a system column, or the
    // whole row (a bare relation name).
    let column = |name: &str, qualified: bool| -> Result<i16, &'static str> {
        if let Some(a) = base_attrs.iter().find(|a| a.attname == name) {
            return Ok(a.attnum);
        }
        if crate::pg_catalog::SYSTEM_COLUMNS
            .iter()
            .any(|(n, ..)| *n == name)
        {
            return Err("View columns that refer to system columns are not updatable.");
        }
        if !qualified && name == alias {
            return Err("View columns that return whole-row references are not updatable.");
        }
        Err(not_a_column)
    };
    let mut columns = Vec::new();
    for target in &sel.target_list {
        let Some(ast::Node::ResTarget(rt)) = target.as_ref() else {
            continue;
        };
        let Some(ast::Node::ColumnRef(cr)) = rt.val.as_ref() else {
            columns.push(Err(not_a_column));
            continue;
        };
        let parts: Vec<&str> = cr
            .fields
            .iter()
            .filter_map(|n| super::util::node_string(n.as_ref()))
            .collect();
        let star = matches!(
            cr.fields.last().and_then(|f| f.as_ref()),
            Some(ast::Node::AStar(_))
        );
        if star {
            // `*` / `alias.*`: every column of the base relation.
            columns.extend(base_attrs.iter().map(|a| Ok(a.attnum)));
            continue;
        }
        columns.push(match parts.as_slice() {
            [name] => column(name, false),
            [_, name] | [_, _, name] => column(name, true),
            _ => Err(not_a_column),
        });
    }
    ViewUpdatability {
        not_updatable: None,
        base: Some(base),
        columns,
    }
}

/// Whether `node` calls a set-returning function outside a sub-select.
fn returns_set(interp: &PgCatalog, node: &ast::Node) -> bool {
    match node {
        ast::Node::SubLink(_) => false,
        ast::Node::FuncCall(fc) => call_returns_set(interp, fc),
        other => other.nodes().skip(1).any(|(n, ..)| match n {
            typedpg_pg_query::NodeRef::FuncCall(fc) => call_returns_set(interp, fc),
            _ => false,
        }),
    }
}

/// [`returns_set`] of function call `fc`.
fn call_returns_set(interp: &PgCatalog, fc: &ast::FuncCall) -> bool {
    let parts: Vec<&str> = fc
        .funcname
        .iter()
        .filter_map(|n| super::util::node_string(n.as_ref()))
        .collect();
    let (schema, name) = match parts.as_slice() {
        [name] => (None, *name),
        [schema, name] => (Some(*schema), *name),
        _ => return false,
    };
    let candidates = interp.find_functions(schema, name);
    (!candidates.is_empty() && candidates.iter().all(|p| p.proretset))
        || fc.args.iter().flatten().any(|a| returns_set(interp, a))
}

/// DefineView / ATExecSetRelOptions: a view with a check option must be
/// automatically updatable.
pub(crate) fn check_option_allowed(interp: &PgCatalog, query: &ast::Node) -> Result<(), DdlError> {
    match view_updatability(interp, query).reason(true) {
        Some(reason) => Err(DdlError::UnsupportedDdl(format!(
            "WITH CHECK OPTION is supported only on automatically updatable views ({reason})"
        ))),
        None => Ok(()),
    }
}

/// Whether a `WITH (...)` option list asks for `CASCADED` (the default of
/// `WITH CHECK OPTION`) rather than `LOCAL`.
fn cascaded_check_option(options: &[Option<ast::Node>]) -> bool {
    options.iter().any(|o| {
        matches!(o.as_ref(), Some(ast::Node::DefElem(de)) if de.defname == "check_option"
        && match de.arg.as_ref() {
            Some(ast::Node::String(s)) => !s.sval.eq_ignore_ascii_case("local")
                && s.sval != "none",
            Some(ast::Node::Boolean(b)) => b.boolval,
            _ => true,
        })
    })
}

/// Whether a `WITH (...)` / `SET (...)` option list sets `check_option`.
pub(crate) fn sets_check_option(options: &[Option<ast::Node>]) -> bool {
    options
        .iter()
        .any(|o| matches!(o.as_ref(), Some(ast::Node::DefElem(de)) if de.defname == "check_option"))
}

/// What an analysis resolved its names to ([`super::depend::collect`]),
/// in the order it first did: each column by its attnum (a rename keeps
/// it), each object once (the analysis looks some up again while
/// narrowing).
fn bound_references(
    interp: &PgCatalog,
    references: &[super::depend::Reference],
) -> Vec<super::depend::Reference> {
    use super::depend::{ObjectAddress, Reference};
    let mut out: Vec<Reference> = Vec::new();
    for r in references {
        let bound = match r {
            Reference::Column(relid, name) => interp.attribute_by_name(*relid, name).map_or_else(
                || r.clone(),
                |a| Reference::Object(ObjectAddress::column(*relid, a.attnum)),
            ),
            other => other.clone(),
        };
        if !out.contains(&bound) {
            out.push(bound);
        }
    }
    out
}

/// The output columns of view (or materialized view) `relid`'s stored
/// query, analyzed again against the current catalog — provided its names
/// still resolve to the relations, columns, functions, operators and types
/// they resolved to when the view was defined. PG binds a view's query to
/// those objects then (a renamed table, a table created ahead of it on the
/// search path, a renamed or newly added column of the same name, a new
/// overload: none changes what the view reads), while the analyzer keeps
/// the query's text: re-resolved by name, it could read another relation
/// or column than PG does. `None` when it no longer resolves alike, or not
/// at all.
pub(crate) fn reanalyze_view(
    snapshot: &PgCatalog,
    relid: PgClassOid,
) -> Option<Vec<crate::resolve::RawColumn>> {
    let stored = snapshot.view_references.get(&relid)?;
    let query = view_query(snapshot, relid)?;
    let inner = &query;
    // The query's shape only: whether it would run, and what the planner
    // folds of it, is the reading statement's to report.
    let (analyzed, references) = expanding_view(relid, || {
        super::depend::collect(|| {
            crate::resolve::without_execution(|| {
                crate::expr::may_skip(|| crate::resolve::analyze_raw_node(snapshot, inner, &[]))
            })
        })
    })?;
    let (columns, _) = analyzed.ok()?;
    (bound_references(snapshot, &references) == *stored).then_some(columns)
}

thread_local! {
    /// The views whose stored query is being looked into, outermost first.
    static EXPANDING_VIEWS: std::cell::RefCell<Vec<PgClassOid>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Whether some view's stored query is being looked into
/// ([`expanding_view`]): its locations are in the view's text.
pub(crate) fn expanding_any_view() -> bool {
    EXPANDING_VIEWS.with(|v| !v.borrow().is_empty())
}

/// Run `f` looking into view `relid`'s stored query — `None` when that
/// query is already being looked into: views made to read each other
/// (`CREATE OR REPLACE VIEW` can close the loop) would recurse forever.
/// A statement reading such a view fails ([`view_rule_recursion`]); a
/// definition merely naming one is stored as PG stores it.
pub(crate) fn expanding_view<R>(relid: PgClassOid, f: impl FnOnce() -> R) -> Option<R> {
    /// Pops the view pushed below when dropped — also when `f` unwinds, so
    /// a panic caught by the caller leaves no stale entry behind.
    struct Expanding;
    impl Drop for Expanding {
        fn drop(&mut self) {
            EXPANDING_VIEWS.with(|v| v.borrow_mut().pop());
        }
    }
    if EXPANDING_VIEWS.with(|v| v.borrow().contains(&relid)) {
        return None;
    }
    EXPANDING_VIEWS.with(|v| v.borrow_mut().push(relid));
    let _expanding = Expanding;
    Some(f())
}

/// The rewriter's `fireRIRrules`: expanding view `relid` in a statement,
/// and the views its query reads in turn, depth first in the order the
/// query names them, reaches a view already being expanded — the
/// relation PG names in `infinite recursion detected in rules for
/// relation "…"`.
pub(crate) fn view_rule_recursion(snapshot: &PgCatalog, relid: PgClassOid) -> Option<String> {
    /// `done`: views whose expansion reached no loop (none reachable from
    /// them can then close one), each walked once.
    fn walk(
        snapshot: &PgCatalog,
        relid: PgClassOid,
        active: &mut Vec<PgClassOid>,
        done: &mut std::collections::HashSet<PgClassOid>,
    ) -> Option<String> {
        let class = snapshot.pg_class.get(&relid)?;
        if class.relkind != RelKind::View || done.contains(&relid) {
            return None;
        }
        if active.contains(&relid) {
            return Some(class.relname.clone());
        }
        active.push(relid);
        let found = snapshot
            .view_references
            .get(&relid)
            .into_iter()
            .flatten()
            .find_map(|r| match r {
                super::depend::Reference::Object(a) if a.classid == PG_CLASS_RELID => walk(
                    snapshot,
                    PgClassOid::from_nonzero(a.objid.into_nonzero()),
                    active,
                    done,
                ),
                _ => None,
            });
        active.pop();
        done.insert(relid);
        found
    }
    walk(snapshot, relid, &mut Vec::new(), &mut Default::default())
}

/// [`view_rule_recursion`] for the stored query of view or materialized
/// view `relid`, as it bound its relations: expanding the views it reads,
/// in the order it first named them, reaches a loop.
pub(crate) fn query_rule_recursion(snapshot: &PgCatalog, relid: PgClassOid) -> Option<String> {
    relations_rule_recursion(
        snapshot,
        snapshot
            .view_references
            .get(&relid)
            .into_iter()
            .flatten()
            .filter_map(|r| match r {
                super::depend::Reference::Object(a) if a.classid == PG_CLASS_RELID => {
                    Some(PgClassOid::from_nonzero(a.objid.into_nonzero()))
                }
                _ => None,
            }),
    )
}

/// `fireRIRrules` over a query reading `relations`, in range-table order:
/// the relation PG names for the first view loop their expansion meets.
pub(crate) fn relations_rule_recursion(
    snapshot: &PgCatalog,
    relations: impl IntoIterator<Item = PgClassOid>,
) -> Option<String> {
    relations
        .into_iter()
        .find_map(|r| view_rule_recursion(snapshot, r))
}

/// What the planner raises planning view `relid`'s query, once the
/// rewriter put it in the statement reading the view and the planner
/// pulled it up, like a FROM subquery's (see `resolve::deferred`): `None`
/// when the planner may not pull it up (a `security_barrier` view, a
/// query `deferred::plan_of` can't tell about, a system view), or the
/// query no longer resolves as it did (see [`reanalyze_view`]).
pub(crate) fn planned_view_errors(
    snapshot: &PgCatalog,
    relid: PgClassOid,
    kept: bool,
    removable_side: bool,
) -> Option<crate::resolve::Split> {
    /// FirstNormalObjectId (transam.h): below, what initdb made.
    const FIRST_NORMAL_OBJECT_ID: u32 = 16384;
    if relid.get() < FIRST_NORMAL_OBJECT_ID {
        return None;
    }
    let barrier = snapshot.security_barrier_views.contains(&relid);
    let stored = snapshot.view_references.get(&relid)?;
    let query = view_query(snapshot, relid)?;
    let inner = &query;
    let ast::Node::SelectStmt(sel) = inner else {
        return None;
    };
    // The query is another text: its errors keep their plain messages.
    let _barrier = crate::error::DiagContextGuard::barrier();
    let ((analyzed, references), split) = expanding_view(relid, || {
        // The stored query's names were fixed when the view was defined:
        // planning it again resolves no schema (PG looks none up), so no
        // schema's USAGE is the statement's to require.
        crate::privileges::suspended(|| {
            crate::resolve::planned_errors(sel, snapshot, barrier, kept, removable_side, || {
                super::depend::collect(|| crate::resolve::analyze_raw_node(snapshot, inner, &[]))
            })
        })
    })??;
    analyzed.ok()?;
    (bound_references(snapshot, &references) == *stored).then_some(split)
}

/// Whether reading view `relid` locks rows: its query, or that of a view
/// it reads (as it bound them, see [`reanalyze_view`]), has a `FOR UPDATE`
/// / `FOR SHARE` clause somewhere. The rewriter expands the view into the
/// statement, locking clause included, so the statement then re-checks
/// concurrently updated rows like one locking them itself.
pub(crate) fn view_locks_rows(snapshot: &PgCatalog, relid: PgClassOid) -> bool {
    let mut todo = vec![relid];
    let mut seen = std::collections::HashSet::new();
    while let Some(view) = todo.pop() {
        if !seen.insert(view) {
            continue;
        }
        if view_query(snapshot, view).is_some_and(|q| crate::resolve::locks_rows(&q)) {
            return true;
        }
        for r in snapshot.view_references.get(&view).into_iter().flatten() {
            if let super::depend::Reference::Object(a) = r
                && a.classid == PG_CLASS_RELID
            {
                let rel = PgClassOid::from_nonzero(a.objid.into_nonzero());
                if snapshot
                    .pg_class
                    .get(&rel)
                    .is_some_and(|c| c.relkind == RelKind::View)
                {
                    todo.push(rel);
                }
            }
        }
    }
    false
}

/// The stored SELECT of view `relid`.
pub(crate) fn view_query(interp: &PgCatalog, relid: crate::oid::PgClassOid) -> Option<ast::Node> {
    stored_view_query(interp, relid).cloned()
}

/// [`view_query`], borrowed from the catalog.
pub(crate) fn stored_view_query(
    interp: &PgCatalog,
    relid: crate::oid::PgClassOid,
) -> Option<&ast::Node> {
    interp.view_body(relid)?.ast.as_ref()
}
