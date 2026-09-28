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

use pg_query::protobuf::{self, CreateTableAsStmt, ObjectType, ViewStmt, node};

use crate::oid::{PgClassOid, PgGenericOid, PgNamespaceOid, PgProcOid, PgRewriteOid, PgTypeOid};
use crate::pg_catalog::{
    AstBinding, DepType, EvEnabled, EvType, PG_CLASS_RELID, PG_PROC_RELID, PG_TYPE_RELID,
    PgAttribute, PgClass, PgDepend, PgRewrite, PgType, RelKind, SerializedAst, TypCategory,
    TypType,
};

use super::DdlError;
use super::util::ensure_range_var;
use crate::pg_catalog::PgCatalog;

pub fn create_view(interp: &mut PgCatalog, stmt: &ViewStmt) -> Result<(), DdlError> {
    let rv = stmt
        .view
        .as_ref()
        .ok_or_else(|| DdlError::Parse("CREATE VIEW without name".into()))?;

    let (nsoid, name) = ensure_range_var(interp, rv)?;
    let qn_label = crate::qualified_name::QualifiedName::new(
        interp.namespace_name(nsoid).unwrap_or("?"),
        name.clone(),
    )
    .to_string();

    let aliases = string_list(&stmt.aliases);

    let resolved = match stmt.query.as_deref() {
        Some(query_node) => {
            resolve_view_now(interp, query_node, &aliases).map_err(|e| match e {
                DdlError::ViewAnalysis { source, .. } => DdlError::ViewAnalysis {
                    view: qn_label.clone(),
                    source,
                },
                other => other,
            })?
        }
        None => ResolvedView::default(),
    };
    // DefineView (view.c): the alias list may not outnumber the columns.
    if aliases.len() > resolved.columns.len() {
        return Err(DdlError::Parse(
            "CREATE VIEW specifies more column names than columns".into(),
        ));
    }
    check_duplicate_columns(&resolved.columns)?;

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
        return Ok(());
    }

    super::util::check_relation_name_free(interp, nsoid, &name)?;
    install_relation(interp, nsoid, name, RelKind::View, resolved)?;
    Ok(())
}

pub fn create_table_as(interp: &mut PgCatalog, stmt: &CreateTableAsStmt) -> Result<(), DdlError> {
    let into = stmt
        .into
        .as_deref()
        .ok_or_else(|| DdlError::Parse("CREATE TABLE AS without target".into()))?;
    let kind = match ObjectType::try_from(stmt.objtype) {
        Ok(ObjectType::ObjectMatview) => RelKind::MaterializedView,
        _ => RelKind::Table,
    };
    create_relation_as(
        interp,
        into,
        stmt.query.as_deref(),
        kind,
        stmt.if_not_exists,
    )
}

/// `SELECT ... INTO [TABLE] name FROM ...`: the same as `CREATE TABLE name
/// AS SELECT ...` (`transformSelectStmt` turns it into a CreateTableAsStmt).
pub fn select_into(interp: &mut PgCatalog, stmt: &protobuf::SelectStmt) -> Result<(), DdlError> {
    let Some(into) = stmt.into_clause.as_deref() else {
        return Ok(());
    };
    let mut query = stmt.clone();
    query.into_clause = None;
    let query_node = protobuf::Node {
        node: Some(node::Node::SelectStmt(Box::new(query))),
    };
    create_relation_as(interp, into, Some(&query_node), RelKind::Table, false)
}

/// Shared body of CREATE TABLE AS / CREATE MATERIALIZED VIEW / SELECT INTO
/// (`ExecCreateTableAs` + `intorel_startup`, createas.c).
fn create_relation_as(
    interp: &mut PgCatalog,
    into: &protobuf::IntoClause,
    query: Option<&protobuf::Node>,
    kind: RelKind,
    if_not_exists: bool,
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
    let mut resolved = match query {
        Some(query_node) => {
            resolve_view_now(interp, query_node, &aliases).map_err(|e| match e {
                DdlError::ViewAnalysis { source, .. } => DdlError::ViewAnalysis {
                    view: qn_label.clone(),
                    source,
                },
                other => other,
            })?
        }
        None => ResolvedView::default(),
    };
    if aliases.len() > resolved.columns.len() {
        return Err(DdlError::Parse(
            "too many column names were specified".into(),
        ));
    }
    check_duplicate_columns(&resolved.columns)?;
    super::util::check_relation_name_free(interp, nsoid, &name)?;

    // A table created from a query is an ordinary table: its columns carry
    // no NOT NULL constraint, whatever the query's nullability. (A matview
    // can only ever hold its query's rows, so it keeps the inferred one.)
    if kind == RelKind::Table {
        for col in &mut resolved.columns {
            col.not_null = false;
        }
    }

    install_relation(interp, nsoid, name, kind, resolved)?;
    Ok(())
}

/// The `String` values of a name list (view aliases, CTAS column names).
fn string_list(nodes: &[protobuf::Node]) -> Vec<String> {
    nodes
        .iter()
        .filter_map(|n| match n.node.as_ref() {
            Some(node::Node::String(s)) => Some(s.sval.clone()),
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
    }
    Ok(())
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
    } = resolved;
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
    interp.remove_pg_rewrites_of(view_oid);
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
    Ok(())
}

/// `REFRESH MATERIALIZED VIEW [CONCURRENTLY] name [WITH [NO] DATA]`: the
/// data changes, the shape does not. PG requires a materialized view
/// (`ExecRefreshMatView`).
pub fn refresh_materialized_view(
    interp: &mut PgCatalog,
    stmt: &protobuf::RefreshMatViewStmt,
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
        Some(_) => Ok(()),
    }
}

/// Bundle of analyzer outputs that [`install_relation`] needs to wire up a
/// view: column shape, the deparse-time binding side-table, the encoded
/// AST, and the deps to record in `pg_depend`.
#[derive(Default)]
struct ResolvedView {
    columns: Vec<ResolvedColumn>,
    bindings: Vec<AstBinding>,
    ast: Vec<u8>,
    deps: ViewDeps,
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
    } = resolved;
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
    });
    interp.insert_pg_type(PgType {
        oid: array_oid,
        typname: format!("_{name}"),
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
    });

    record_view_dependencies(interp, class_oid, &deps);
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
}

/// Resolve view columns at creation time, walk the AST to emit a binding
/// stream (one OID-resolved entry per name slot), and derive the
/// `pg_depend` deps from those bindings.
fn resolve_view_now(
    snapshot: &PgCatalog,
    query_node: &protobuf::Node,
    aliases: &[String],
) -> Result<ResolvedView, DdlError> {
    let inner = query_node
        .node
        .as_ref()
        .ok_or_else(|| DdlError::Parse("CREATE VIEW with empty query node".into()))?;
    let (raw_columns, _) =
        crate::resolve::analyze_raw_node(snapshot, inner, &[]).map_err(|source| {
            DdlError::ViewAnalysis {
                view: String::new(),
                source: Box::new(source),
            }
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
            ResolvedColumn {
                name,
                type_oid: col.type_oid,
                typmod: col.typmod,
                not_null: !col.nullable,
                collation: col
                    .collation
                    .or_else(|| interp_type_collation(snapshot, col.type_oid)),
            }
        })
        .collect();

    let (bindings, deps) = collect_view_bindings_and_deps(query_node, snapshot);
    let ast = encode_ast(query_node);

    Ok(ResolvedView {
        columns,
        bindings,
        ast,
        deps,
    })
}

/// Encode a single AST node as protobuf bytes.
fn encode_ast(node: &protobuf::Node) -> Vec<u8> {
    use prost::Message;
    let mut buf = Vec::with_capacity(256);
    node.encode(&mut buf).ok();
    buf
}

/// Parse `sql` (typically a synthetic `SELECT …`), pluck out a subnode
/// via `pick`, walk it through the [`BindingWalker`] against `snapshot`, and
/// return a [`SerializedAst`] of the picked node + bindings. Powers
/// [`crate::pg_catalog::PgCatalog::serialize_expression`] and friends so
/// the seed exporter can capture index expressions and predicates with
/// fully resolved bindings, without re-implementing the walker.
#[cfg(any(test, feature = "internal"))]
pub(crate) fn serialize_subnode(
    snapshot: &PgCatalog,
    sql: &str,
    pick: impl Fn(&protobuf::Node) -> Option<&protobuf::Node>,
) -> Result<crate::pg_catalog::SerializedAst, super::DdlError> {
    let parsed = pg_query::parse(sql)
        .map_err(|e| super::DdlError::Parse(format!("failed to parse `{sql}`: {e}")))?;
    let proto = parsed.protobuf;
    let stmt = proto
        .stmts
        .first()
        .and_then(|s| s.stmt.as_ref())
        .ok_or_else(|| super::DdlError::Parse(format!("`{sql}` produced no statement")))?;
    let target = pick(stmt)
        .ok_or_else(|| super::DdlError::Parse(format!("`{sql}` has no expression to extract")))?;
    let mut walker = BindingWalker::default();
    walker.walk(target, snapshot);
    Ok(crate::pg_catalog::SerializedAst {
        ast: encode_ast(target),
        bindings: walker.bindings,
    })
}

/// `pick` for `serialize_expression`: pulls the value of the first target
/// in `SELECT <expr>`. Returns `None` if the input wasn't a `SelectStmt`
/// or has no targets.
#[cfg(any(test, feature = "internal"))]
pub(crate) fn extract_first_target(stmt: &protobuf::Node) -> Option<&protobuf::Node> {
    let node::Node::SelectStmt(sel) = stmt.node.as_ref()? else {
        return None;
    };
    let target = sel.target_list.first()?;
    let node::Node::ResTarget(rt) = target.node.as_ref()? else {
        return None;
    };
    rt.val.as_deref()
}

/// `pick` for `serialize_predicate`: extracts the WHERE clause of
/// `SELECT 1 WHERE <pred>`. Returns `None` if there is no WHERE.
#[cfg(any(test, feature = "internal"))]
pub(crate) fn extract_where(stmt: &protobuf::Node) -> Option<&protobuf::Node> {
    let node::Node::SelectStmt(sel) = stmt.node.as_ref()? else {
        return None;
    };
    sel.where_clause.as_deref()
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
}

impl BindingWalker {
    #[cfg(any(test, feature = "internal"))]
    fn walk(&mut self, node: &protobuf::Node, snapshot: &PgCatalog) {
        let scope_stack: Vec<Vec<FromSource>> = Vec::new();
        self.walk_node(node, snapshot, &scope_stack);
    }
}

fn collect_view_bindings_and_deps(
    query_node: &protobuf::Node,
    snapshot: &PgCatalog,
) -> (Vec<AstBinding>, ViewDeps) {
    let mut walker = BindingWalker::default();
    let scope_stack: Vec<Vec<FromSource>> = Vec::new();
    walker.walk_node(query_node, snapshot, &scope_stack);
    let deps = derive_deps_from_bindings(&walker.bindings);
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
    }
}

impl BindingWalker {
    fn walk_node(
        &mut self,
        node: &protobuf::Node,
        snapshot: &PgCatalog,
        stack: &[Vec<FromSource>],
    ) {
        let Some(inner) = node.node.as_ref() else {
            return;
        };
        match inner {
            node::Node::SelectStmt(sel) => self.walk_select(sel, snapshot, stack),
            node::Node::ColumnRef(cr) => self.record_column_ref(cr, snapshot, stack),
            node::Node::AExpr(expr) => {
                if let Some(lexpr) = expr.lexpr.as_deref() {
                    self.walk_node(lexpr, snapshot, stack);
                }
                if let Some(rexpr) = expr.rexpr.as_deref() {
                    self.walk_node(rexpr, snapshot, stack);
                }
            }
            node::Node::BoolExpr(b) => {
                for arg in &b.args {
                    self.walk_node(arg, snapshot, stack);
                }
            }
            node::Node::FuncCall(fc) => {
                self.bind_func_name(&fc.funcname, snapshot);
                for arg in &fc.args {
                    self.walk_node(arg, snapshot, stack);
                }
                if let Some(over) = fc.over.as_deref() {
                    for arg in &over.partition_clause {
                        self.walk_node(arg, snapshot, stack);
                    }
                    for arg in &over.order_clause {
                        self.walk_node(arg, snapshot, stack);
                    }
                }
                if let Some(filter) = fc.agg_filter.as_deref() {
                    self.walk_node(filter, snapshot, stack);
                }
            }
            node::Node::TypeCast(tc) => {
                if let Some(arg) = tc.arg.as_deref() {
                    self.walk_node(arg, snapshot, stack);
                }
                if let Some(tn) = tc.type_name.as_ref() {
                    self.bind_type_name(tn, snapshot);
                }
            }
            node::Node::CollateClause(cc) => {
                if let Some(arg) = cc.arg.as_deref() {
                    self.walk_node(arg, snapshot, stack);
                }
            }
            node::Node::NamedArgExpr(na) => {
                if let Some(arg) = na.arg.as_deref() {
                    self.walk_node(arg, snapshot, stack);
                }
            }
            node::Node::CoalesceExpr(c) => {
                for arg in &c.args {
                    self.walk_node(arg, snapshot, stack);
                }
            }
            node::Node::MinMaxExpr(m) => {
                for arg in &m.args {
                    self.walk_node(arg, snapshot, stack);
                }
            }
            node::Node::NullIfExpr(n) => {
                for arg in &n.args {
                    self.walk_node(arg, snapshot, stack);
                }
            }
            node::Node::CaseExpr(c) => {
                if let Some(arg) = c.arg.as_deref() {
                    self.walk_node(arg, snapshot, stack);
                }
                for branch in &c.args {
                    self.walk_node(branch, snapshot, stack);
                }
                if let Some(def) = c.defresult.as_deref() {
                    self.walk_node(def, snapshot, stack);
                }
            }
            node::Node::CaseWhen(w) => {
                if let Some(expr) = w.expr.as_deref() {
                    self.walk_node(expr, snapshot, stack);
                }
                if let Some(result) = w.result.as_deref() {
                    self.walk_node(result, snapshot, stack);
                }
            }
            node::Node::SubLink(sl) => {
                if let Some(testexpr) = sl.testexpr.as_deref() {
                    self.walk_node(testexpr, snapshot, stack);
                }
                if let Some(subselect) = sl.subselect.as_deref() {
                    self.walk_node(subselect, snapshot, stack);
                }
            }
            node::Node::NullTest(t) => {
                if let Some(arg) = t.arg.as_deref() {
                    self.walk_node(arg, snapshot, stack);
                }
            }
            node::Node::BooleanTest(t) => {
                if let Some(arg) = t.arg.as_deref() {
                    self.walk_node(arg, snapshot, stack);
                }
            }
            node::Node::List(l) => {
                for item in &l.items {
                    self.walk_node(item, snapshot, stack);
                }
            }
            node::Node::ArrayExpr(a) => {
                for elem in &a.elements {
                    self.walk_node(elem, snapshot, stack);
                }
            }
            node::Node::RowExpr(r) => {
                for arg in &r.args {
                    self.walk_node(arg, snapshot, stack);
                }
            }
            node::Node::ResTarget(rt) => {
                if let Some(val) = rt.val.as_deref() {
                    self.walk_node(val, snapshot, stack);
                }
            }
            node::Node::SortBy(sb) => {
                if let Some(n) = sb.node.as_deref() {
                    self.walk_node(n, snapshot, stack);
                }
            }
            _ => {}
        }
    }

    fn walk_select(
        &mut self,
        sel: &protobuf::SelectStmt,
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
                if let Some(node::Node::CommonTableExpr(cte)) = cte_node.node.as_ref() {
                    if let Some(query) = cte.ctequery.as_deref() {
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
                            .filter_map(|n| match n.node.as_ref()? {
                                node::Node::String(s) => Some(s.sval.clone()),
                                _ => None,
                            })
                            .collect(),
                    });
                }
            }
        }

        for from_item in &sel.from_clause {
            self.process_from_item(from_item, snapshot, parent_stack, &mut frame);
        }

        let mut stack = parent_stack.to_vec();
        stack.push(frame);

        for t in &sel.target_list {
            self.walk_node(t, snapshot, &stack);
        }
        if let Some(w) = sel.where_clause.as_deref() {
            self.walk_node(w, snapshot, &stack);
        }
        for g in &sel.group_clause {
            self.walk_node(g, snapshot, &stack);
        }
        if let Some(h) = sel.having_clause.as_deref() {
            self.walk_node(h, snapshot, &stack);
        }
        for s in &sel.sort_clause {
            self.walk_node(s, snapshot, &stack);
        }
        for d in &sel.distinct_clause {
            self.walk_node(d, snapshot, &stack);
        }
    }

    fn process_from_item(
        &mut self,
        node: &protobuf::Node,
        snapshot: &PgCatalog,
        parent_stack: &[Vec<FromSource>],
        frame: &mut Vec<FromSource>,
    ) {
        let Some(inner) = node.node.as_ref() else {
            return;
        };
        match inner {
            node::Node::RangeVar(rv) => {
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
            node::Node::JoinExpr(join) => {
                if let Some(larg) = join.larg.as_deref() {
                    self.process_from_item(larg, snapshot, parent_stack, frame);
                }
                if let Some(rarg) = join.rarg.as_deref() {
                    self.process_from_item(rarg, snapshot, parent_stack, frame);
                }
                let mut stack = parent_stack.to_vec();
                stack.push(frame.clone());
                if let Some(q) = join.quals.as_deref() {
                    self.walk_node(q, snapshot, &stack);
                }
            }
            node::Node::RangeSubselect(sub) => {
                let alias = sub
                    .alias
                    .as_ref()
                    .map(|a| a.aliasname.clone())
                    .unwrap_or_else(|| "_subquery".into());
                if let Some(query) = sub.subquery.as_deref() {
                    self.walk_node(query, snapshot, parent_stack);
                }
                frame.push(FromSource {
                    alias,
                    relid: None,
                    columns: Vec::new(),
                });
            }
            node::Node::RangeFunction(rf) => {
                for arg in &rf.functions {
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
        cr: &protobuf::ColumnRef,
        snapshot: &PgCatalog,
        stack: &[Vec<FromSource>],
    ) {
        let parts: Vec<String> = cr
            .fields
            .iter()
            .filter_map(|f| match f.node.as_ref()? {
                node::Node::String(s) => Some(s.sval.clone()),
                _ => None,
            })
            .collect();

        let resolved: Option<(PgClassOid, i16)> = match parts.as_slice() {
            [col] => resolve_column_unqualified(col, snapshot, stack),
            [alias, col] => resolve_column_aliased(alias, col, snapshot, stack),
            [schema, relation, col] => {
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
    /// function (any overload) and emit a Function binding with its OID;
    /// the applier uses the proc's namespace + proname to rewrite the
    /// AST literals on deparse.
    fn bind_func_name(&mut self, funcname: &[protobuf::Node], snapshot: &PgCatalog) {
        let parts: Vec<&str> = funcname
            .iter()
            .filter_map(|n| match n.node.as_ref()? {
                node::Node::String(s) => Some(s.sval.as_str()),
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
    fn bind_type_name(&mut self, tn: &protobuf::TypeName, snapshot: &PgCatalog) {
        let parts: Vec<&str> = tn
            .names
            .iter()
            .filter_map(|n| match n.node.as_ref()? {
                node::Node::String(s) => Some(s.sval.as_str()),
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
/// possible (the body no longer resolves, e.g. after a rename, or its shape
/// changed) and the change can only have *added* NULLs (`relaxing`), every
/// column of the view is conservatively marked nullable.
///
/// Relation-level dependencies are used (not column-level ones) because a
/// `SELECT *` body records no column references.
pub(crate) fn refresh_dependent_view_nullability(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    relaxing: bool,
) {
    use prost::Message;

    let mut queue: std::collections::VecDeque<PgClassOid> =
        find_dependent_views(interp, relid).into();
    let mut seen = std::collections::HashSet::new();
    while let Some(view) = queue.pop_front() {
        if !seen.insert(view) {
            continue;
        }
        let reanalyzed = interp
            .view_body(view)
            .and_then(|body| protobuf::Node::decode(body.ast.as_slice()).ok())
            .and_then(|node| {
                let inner = node.node.as_ref()?;
                crate::resolve::analyze_raw_node(interp, inner, &[])
                    .ok()
                    .map(|(cols, _)| cols)
            });
        let attrs = interp.attributes_of(view).to_vec();
        let new_not_null: Option<Vec<bool>> = reanalyzed.and_then(|cols| {
            (cols.len() >= attrs.len()
                && attrs
                    .iter()
                    .zip(&cols)
                    .all(|(a, c)| a.atttypid == c.type_oid))
            .then(|| attrs.iter().zip(&cols).map(|(_, c)| !c.nullable).collect())
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
// cached deparse).

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
fn interp_type_collation(
    snapshot: &PgCatalog,
    type_oid: PgTypeOid,
) -> Option<crate::oid::PgCollationOid> {
    snapshot.pg_type.get(&type_oid).and_then(|t| t.typcollation)
}
