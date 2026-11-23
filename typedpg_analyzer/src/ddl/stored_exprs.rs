//! Keeping the expressions the analyzer stores as written (CHECK
//! constraints, column defaults and generation expressions, partition
//! keys) in step with renames. PG stores them cooked — a column as its
//! attnum, an enum label as its OID — so `ALTER TABLE ... RENAME COLUMN`
//! and `ALTER TYPE ... RENAME VALUE` leave their meaning unchanged; the
//! analyzer keeps the parse tree, whose names have to follow. Read again
//! over the relation, a stored expression's column references are its
//! columns whatever relation name qualifies them ([`over_own_row`]).

use typedpg_pg_query::ast;

use crate::ddl::tables::check_inherit::StoredExpr;
use crate::oid::{PgClassOid, PgTypeOid};
use crate::pg_catalog::{ConType, PgCatalog, TypType};

/// Run `edit` over every node of `expr` (mutably).
fn edit_nodes(expr: &mut Option<ast::Node>, edit: &mut dyn FnMut(typedpg_pg_query::NodeMut<'_>)) {
    let mut tree = ast::ParseResult {
        version: 0,
        stmts: vec![ast::RawStmt {
            stmt: std::mem::take(expr),
            stmt_location: 0,
            stmt_len: 0,
        }],
    };
    tree.for_each_node_mut(|n, _| edit(n));
    *expr = tree.stmts.pop().and_then(|s| s.stmt);
}

/// Which field of a column reference in an expression over the row of
/// one relation names the column (transformColumnRef): `c`, `t.c` (two
/// names are always a relation and its column), `s.t.c` (or `t.c.f`, a
/// field of composite column `c`), `d.s.t.c`. `is_column` says whether a
/// name is one of the relation's columns; `relname` is the relation's own
/// name — which a copy inherited from a parent, or taken by LIKE, or
/// written before the relation was renamed, may not use.
fn column_field(fields: &[String], relname: &str, is_column: &dyn Fn(&str) -> bool) -> usize {
    match fields {
        [_] => 0,
        [_, _] => 1,
        [_, t, c] => {
            // `schema.table.column` when the middle name isn't one of the
            // columns (or is the relation) and the last one is.
            if (t == relname || !is_column(t)) && is_column(c) {
                2
            } else {
                1
            }
        }
        _ => fields.len() - 1,
    }
}

/// `expr` with the column references to column `old` of the relation it
/// is over (named `relname`, with columns `columns`) renamed `new`.
fn rename_column_refs(
    expr: &mut Option<ast::Node>,
    relname: &str,
    columns: &[String],
    old: &str,
    new: &str,
) {
    let is_column = |n: &str| columns.iter().any(|c| c == n);
    edit_nodes(expr, &mut |n| {
        let typedpg_pg_query::NodeMut::ColumnRef(cr) = n else {
            return;
        };
        let fields = &mut cr.fields;
        if fields
            .iter()
            .any(|f| !matches!(f, Some(ast::Node::String(_))))
        {
            return;
        }
        let names: Vec<String> = crate::expr::extract_string_fields(fields);
        let idx = column_field(&names, relname, &is_column);
        if names.get(idx).map(String::as_str) == Some(old)
            && let Some(ast::Node::String(s)) = fields[idx].as_mut()
        {
            s.sval = new.to_owned();
        }
    });
}

/// `when`, a trigger's WHEN condition over its relation (with columns
/// `columns`, before the rename) as `old` and `new`, with the references
/// to column `old_col` renamed `new_col`: `old.c` / `new.c` (and `new.c.f`,
/// a field of composite column `c`), and the selection `(new).c`. An
/// unqualified column is ambiguous there (CreateTriggerFiringOn refused
/// it), so every column reference is qualified.
fn rename_trigger_when_refs(
    when: &mut Option<ast::Node>,
    columns: &[String],
    old_col: &str,
    new_col: &str,
) {
    // transformColumnRef prefers a column to a whole row of that name.
    let is_row = |n: &str| (n == "old" || n == "new") && !columns.iter().any(|c| c == n);
    let rename = |field: &mut Option<ast::Node>| {
        if let Some(ast::Node::String(s)) = field
            && s.sval == old_col
        {
            s.sval = new_col.to_owned();
        }
    };
    edit_nodes(when, &mut |n| match n {
        typedpg_pg_query::NodeMut::ColumnRef(cr) => {
            if let [Some(ast::Node::String(rel)), field, ..] = cr.fields.as_mut_slice()
                && (rel.sval == "old" || rel.sval == "new")
            {
                rename(field);
            }
        }
        typedpg_pg_query::NodeMut::AIndirection(ind) => {
            let whole_row = matches!(
                ind.arg.as_ref(),
                Some(ast::Node::ColumnRef(cr))
                    if matches!(cr.fields.as_slice(), [Some(ast::Node::String(s))] if is_row(&s.sval))
            );
            if whole_row && let Some(field) = ind.indirection.first_mut() {
                rename(field);
            }
        }
        _ => {}
    });
}

/// `expr`, an expression stored for relation `relid` (a CHECK, a
/// generation or index expression), with its column references as PG
/// keeps them — the relation's columns, whatever the relation was called
/// where the expression was written: `t.a` in a CHECK copied to a child
/// from parent `t`, or written before the table was renamed, is the
/// column `a` (and `t.c.f` the field `f` of column `c`).
pub(crate) fn over_own_row(
    interp: &PgCatalog,
    relid: PgClassOid,
    expr: Option<&ast::Node>,
) -> Option<ast::Node> {
    let relname = interp
        .pg_class
        .get(&relid)
        .map(|c| c.relname.clone())
        .unwrap_or_default();
    let columns: Vec<String> = interp
        .attributes_of(relid)
        .iter()
        .map(|a| a.attname.clone())
        .collect();
    let is_column = |n: &str| columns.iter().any(|c| c == n);
    let mut tree = ast::ParseResult {
        version: 0,
        stmts: vec![ast::RawStmt {
            stmt: expr.cloned(),
            stmt_location: 0,
            stmt_len: 0,
        }],
    };
    tree.for_each_node_mut(|n, _| {
        let typedpg_pg_query::NodeMut::ColumnRef(cr) = n else {
            return;
        };
        let fields = &mut cr.fields;
        if fields
            .iter()
            .any(|f| !matches!(f, Some(ast::Node::String(_))))
        {
            return;
        }
        let names = crate::expr::extract_string_fields(fields);
        let idx = column_field(&names, &relname, &is_column);
        if idx > 0 && is_column(&names[idx]) {
            fields.drain(..idx);
        }
    });
    tree.stmts.pop().and_then(|s| s.stmt)
}

/// ALTER TABLE ... RENAME COLUMN `old` TO `new` of relation `relid`: the
/// relation's CHECK constraints, generation expressions and partition key
/// read the column by its new name. Called before `pg_attribute` is
/// updated.
pub(crate) fn rename_column(interp: &mut PgCatalog, relid: PgClassOid, old: &str, new: &str) {
    let relname = interp
        .pg_class
        .get(&relid)
        .map(|c| c.relname.clone())
        .unwrap_or_default();
    let columns: Vec<String> = interp
        .attributes_of(relid)
        .iter()
        .map(|a| a.attname.clone())
        .collect();
    // Stored policy expressions that read the column follow it (PG keeps
    // them by attnum).
    if let Some(attnum) = interp.attribute_by_name(relid, old).map(|a| a.attnum) {
        crate::ddl::policies::rewrite_column_refs(interp, relid, attnum, new);
    }
    let checks: Vec<_> = interp
        .pg_constraint
        .values()
        .filter(|c| c.conrelid == relid && c.contype == ConType::Check)
        .map(|c| c.oid)
        .collect();
    for oid in checks {
        if let Some(mut def) = interp.check_defs.get(&oid).cloned()
            && let StoredExpr::Written(expr) = &mut def.expr
        {
            rename_column_refs(expr, &relname, &columns, old, new);
            interp.check_defs.insert(oid, def);
        }
    }
    let exprs: Vec<i16> = interp
        .attr_default_exprs
        .keys()
        .filter(|(rel, _)| *rel == relid)
        .map(|(_, attnum)| *attnum)
        .collect();
    for attnum in exprs {
        if let Some(StoredExpr::Written(expr)) = interp.attr_default_exprs.get_mut(&(relid, attnum))
        {
            rename_column_refs(expr, &relname, &columns, old, new);
        }
    }
    if let Some(mut spec) = interp.partition_specs.get(&relid).cloned() {
        spec.rename_key_column(old, new);
        interp.partition_specs.insert(relid, spec);
    }
    // A trigger's UPDATE OF columns (`tgattr`, by attnum: the rename
    // reaches them).
    if let Some(triggers) = interp.triggers.get_mut(&relid) {
        for column in triggers.iter_mut().flat_map(|t| t.columns.iter_mut()) {
            if column == old {
                *column = new.to_owned();
            }
        }
        // And its WHEN condition (`tgqual`, whose Vars are attnums).
        for trigger in triggers.iter_mut() {
            rename_trigger_when_refs(&mut trigger.when, &columns, old, new);
        }
    }
    // The index expressions and predicates over the relation.
    let indexes: Vec<PgClassOid> = interp
        .pg_index
        .values()
        .filter(|i| i.indrelid == relid)
        .map(|i| i.indexrelid)
        .collect();
    for index in indexes {
        let Some(mut row) = interp.pg_index.get(&index).cloned() else {
            continue;
        };
        for ast in row.indexprs.iter_mut().chain(row.indpred.as_mut()) {
            rename_column_refs(&mut ast.ast, &relname, &columns, old, new);
        }
        interp.pg_index.insert(index, row);
    }
}

/// ALTER TYPE ... RENAME VALUE `old` TO `new` of enum `enum_oid`: the
/// label constants of that type in CHECK constraints (by the types their
/// literals were resolved to, see [`crate::nonnull::TrustedNodes`]) and in
/// column defaults read the new label. A CHECK whose label can't be
/// rewritten — inside an array literal — is no longer trusted for its
/// comparisons.
pub(crate) fn rename_enum_label(interp: &mut PgCatalog, enum_oid: PgTypeOid, old: &str, new: &str) {
    let is_enum = |interp: &PgCatalog, t: PgTypeOid| interp.unwrap_domain(t) == enum_oid;
    let is_enum_array = |interp: &PgCatalog, t: PgTypeOid| {
        interp
            .pg_type
            .get(&interp.unwrap_domain(t))
            .and_then(|ty| ty.typelem)
            .is_some_and(|e| interp.unwrap_domain(e) == enum_oid)
    };
    // A literal of a polymorphic type: what it was may be the enum.
    let is_pseudo = |interp: &PgCatalog, t: PgTypeOid| {
        interp
            .pg_type
            .get(&t)
            .is_some_and(|ty| ty.typtype == TypType::Pseudo)
    };
    let oids: Vec<_> = interp.check_defs.keys().copied().collect();
    for oid in oids {
        let Some(mut def) = interp.check_defs.get(&oid).cloned() else {
            continue;
        };
        let Some(trusted) = def.trusted.clone() else {
            continue;
        };
        let StoredExpr::Written(expr) = &mut def.expr else {
            continue;
        };
        let mut changed = false;
        let mut stale = false;
        edit_nodes(expr, &mut |n| {
            let typedpg_pg_query::NodeMut::AConst(c) = n else {
                return;
            };
            let Some(ast::AConstValue::String(s)) = c.val.as_mut() else {
                return;
            };
            let Some(&t) = trusted.literal_types.get(&c.location) else {
                return;
            };
            if is_enum(interp, t) {
                if s.sval == old {
                    s.sval = new.to_owned();
                    changed = true;
                }
            } else if (is_enum_array(interp, t) || is_pseudo(interp, t)) && s.sval.contains(old) {
                stale = true;
            }
        });
        if stale {
            def.trusted = Some(crate::nonnull::TrustedNodes::default());
        }
        if changed || stale {
            interp.check_defs.insert(oid, def);
        }
    }
    // A column default: the label, bare or cast to the column's type.
    let defaults: Vec<(PgClassOid, i16)> = interp
        .attr_default_exprs
        .keys()
        .filter(|(rel, attnum)| {
            interp
                .attributes_of(*rel)
                .iter()
                .any(|a| a.attnum == *attnum && is_enum(interp, a.atttypid))
        })
        .copied()
        .collect();
    for key in defaults {
        let Some(StoredExpr::Written(mut expr)) = interp.attr_default_exprs.get(&key).cloned()
        else {
            continue;
        };
        let constant = match expr.as_mut() {
            Some(ast::Node::AConst(c)) => Some(c),
            Some(ast::Node::TypeCast(tc))
                if tc
                    .type_name
                    .as_ref()
                    .and_then(|t| crate::ddl::util::resolve_type_name(t, interp))
                    .is_some_and(|t| is_enum(interp, t)) =>
            {
                match tc.arg.as_mut() {
                    Some(ast::Node::AConst(c)) => Some(c),
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some(c) = constant
            && let Some(ast::AConstValue::String(s)) = c.val.as_mut()
            && s.sval == old
        {
            s.sval = new.to_owned();
            interp
                .attr_default_exprs
                .insert(key, StoredExpr::Written(expr));
        }
    }
}

// ─── Renamed and moved objects ─────────────────────────────────────────────
//
// PG keeps a stored expression's functions, types, collations and operators
// by OID: `ALTER FUNCTION ... RENAME`, `ALTER TYPE ... SET SCHEMA`,
// `ALTER SCHEMA ... RENAME` leave it meaning what it meant, and a rebuild
// (ALTER COLUMN TYPE deparses it with the current names) still finds them.
// The analyzer's parse trees name them, so before such a change each stored
// expression that may name the object is analyzed (with the policy binding
// recorder on, see [`crate::ddl::policies::PolicyRefs`]), and the name slots
// that resolved to the object are rewritten — the contract the policies'
// own rewrites keep for theirs.

/// An object renamed or moved to another schema.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Renamed<'a> {
    /// A routine: its schema and name after the change.
    Function {
        oid: crate::oid::PgProcOid,
        schema: Option<&'a str>,
        name: &'a str,
    },
    /// A type or domain.
    Type {
        oid: PgTypeOid,
        schema: Option<&'a str>,
        name: &'a str,
    },
    /// A collation.
    Collation {
        oid: crate::oid::PgCollationOid,
        schema: Option<&'a str>,
        name: &'a str,
    },
    /// An operator moved to schema `schema` (operators can't be renamed).
    Operator {
        oid: crate::oid::PgOperatorOid,
        schema: &'a str,
    },
    /// A schema renamed `name`: what lives in it is named in it.
    Schema {
        oid: crate::oid::PgNamespaceOid,
        name: &'a str,
    },
}

/// Where a stored expression lives.
#[derive(Clone, Copy, Debug)]
enum Slot {
    Check(crate::oid::PgConstraintOid),
    Default(PgClassOid, i16),
    IndexExpr(PgClassOid, usize),
    IndexPred(PgClassOid),
    PartitionKey(PgClassOid, usize),
    DomainCheck(PgTypeOid, usize),
    DomainDefault(PgTypeOid),
    TriggerWhen(PgClassOid, usize),
}

/// What a stored expression is analyzed over.
#[derive(Clone, Copy)]
enum Over {
    /// The row of a relation (CHECK, default, generation, index and
    /// partition key expressions).
    Row(PgClassOid),
    /// A trigger's relation as `old` and `new`.
    TriggerRow(PgClassOid),
    /// A domain's `VALUE`, of the domain's base type.
    Value(PgTypeOid),
    /// Nothing (a domain's DEFAULT).
    Nothing,
}

/// Every stored expression, with what it is analyzed over.
fn stored_expressions(interp: &PgCatalog) -> Vec<(Slot, Over, Option<ast::Node>)> {
    let mut out = Vec::new();
    for (&oid, def) in interp.check_defs.iter() {
        if let StoredExpr::Written(expr) = &def.expr
            && let Some(con) = interp.pg_constraint.get(&oid)
        {
            out.push((Slot::Check(oid), Over::Row(con.conrelid), (**expr).clone()));
        }
    }
    for (&(relid, attnum), expr) in &interp.attr_default_exprs {
        if let StoredExpr::Written(expr) = expr {
            out.push((
                Slot::Default(relid, attnum),
                Over::Row(relid),
                (**expr).clone(),
            ));
        }
    }
    for index in interp.pg_index.values() {
        let relid = index.indrelid;
        for (i, e) in index.indexprs.iter().enumerate() {
            out.push((
                Slot::IndexExpr(index.indexrelid, i),
                Over::Row(relid),
                e.ast.clone(),
            ));
        }
        if let Some(pred) = &index.indpred {
            out.push((
                Slot::IndexPred(index.indexrelid),
                Over::Row(relid),
                pred.ast.clone(),
            ));
        }
    }
    for (&relid, exprs) in interp.partition_key_exprs.iter() {
        for (i, e) in exprs.iter().enumerate() {
            out.push((
                Slot::PartitionKey(relid, i),
                Over::Row(relid),
                e.ast.clone(),
            ));
        }
    }
    for (&domain, constraints) in interp.domain_constraints.iter() {
        let base = interp
            .pg_type
            .get(&domain)
            .and_then(|t| t.typbasetype)
            .unwrap_or(domain);
        for (i, c) in constraints.iter().enumerate() {
            if let Some(StoredExpr::Written(expr)) = &c.expr {
                out.push((
                    Slot::DomainCheck(domain, i),
                    Over::Value(base),
                    (**expr).clone(),
                ));
            }
        }
    }
    for (&domain, expr) in interp.domain_default_exprs.iter() {
        if let StoredExpr::Written(expr) = expr {
            out.push((Slot::DomainDefault(domain), Over::Nothing, (**expr).clone()));
        }
    }
    for (&relid, triggers) in interp.triggers.iter() {
        for (i, t) in triggers.iter().enumerate() {
            if t.when.is_some() {
                out.push((
                    Slot::TriggerWhen(relid, i),
                    Over::TriggerRow(relid),
                    t.when.clone(),
                ));
            }
        }
    }
    out
}

/// The name slots of `expr`, analyzed over `over`, and what they resolved
/// to.
fn bindings_of(
    interp: &PgCatalog,
    over: Over,
    expr: Option<&ast::Node>,
) -> crate::ddl::policies::PolicyRefs {
    use crate::qualified_name::QualifiedName;
    let mut scope = crate::scope::Scope::default();
    let mut analyzed = expr.cloned();
    match over {
        Over::Row(relid) | Over::TriggerRow(relid) => {
            let Some(class) = interp.pg_class.get(&relid) else {
                return Default::default();
            };
            let nspname = interp
                .namespace_name(class.relnamespace)
                .unwrap_or("public")
                .to_owned();
            let attrs = interp.attributes_of(relid).to_vec();
            if matches!(over, Over::Row(_)) {
                scope.add_dml_target(
                    interp,
                    &class.relname,
                    QualifiedName::new(nspname, class.relname.clone()),
                    &attrs,
                );
                analyzed = over_own_row(interp, relid, expr);
            } else {
                for alias in ["old", "new"] {
                    scope.add_dml_target(
                        interp,
                        alias,
                        QualifiedName::new(nspname.clone(), class.relname.clone()),
                        &attrs,
                    );
                }
            }
        }
        Over::Value(base) => {
            let value = crate::pg_catalog::PgAttribute {
                attrelid: crate::pg_catalog::PG_CLASS_RELID,
                attname: "value".to_owned(),
                atttypid: base,
                attnum: 1,
                attnotnull: false,
                atthasdef: false,
                attgenerated: None,
                atttypmod: None,
                attidentity: None,
                attcollation: None,
                attislocal: true,
                attinhcount: 0,
            };
            scope.add_dml_target(
                interp,
                "",
                QualifiedName::new("", ""),
                std::slice::from_ref(&value),
            );
        }
        Over::Nothing => {}
    }
    let null_ctx = crate::nullability::NullabilityContext::default();
    let mut params = crate::param_collector::ParamCollector::default();
    // Only the bindings are wanted: what the analysis reports is the
    // expression's own business, and no diagnostic may be left behind.
    let _barrier = crate::error::DiagContextGuard::barrier();
    let (_, refs) = crate::ddl::policies::collect_refs(|| {
        crate::expr::infer_expr(
            analyzed.as_ref(),
            crate::expr::Ctx::new(&scope, &null_ctx, interp),
            &mut params,
            crate::expr::TypeGoal::NONE,
        )
    });
    refs
}

/// Whether `expr` has a name `String` among `names` (a cheap filter: a
/// slot naming the object has its name in it).
fn mentions(expr: Option<&ast::Node>, names: &[String]) -> bool {
    expr.is_some_and(|e| {
        e.nodes().any(
            |(n, ..)| matches!(n, typedpg_pg_query::NodeRef::String(s) if names.contains(&s.sval)),
        )
    })
}

/// The `String` node `sval`.
fn string_node(sval: &str) -> Option<ast::Node> {
    Some(ast::Node::String(typedpg_pg_query::NodeBox::new(
        ast::String {
            sval: sval.to_owned(),
        },
    )))
}

/// Make the qualified name `names` (`[name]` or `[schema, name]`) name
/// `schema` (when given) and `name` (when given).
fn requalify(names: &mut Vec<Option<ast::Node>>, schema: Option<&str>, name: Option<&str>) {
    if let Some(schema) = schema {
        match names.len() {
            0 => names.push(string_node(schema)),
            1 => names.insert(0, string_node(schema)),
            n => {
                if let Some(ast::Node::String(s)) = names[n - 2].as_mut() {
                    s.sval = schema.to_owned();
                }
            }
        }
    }
    if let Some(name) = name
        && let Some(ast::Node::String(s)) = names.last_mut().and_then(|f| f.as_mut())
    {
        s.sval = name.to_owned();
    }
}

/// Rewrite the name slots of `expr` that `refs` says named the object
/// `renamed` changes.
fn rewrite_slots(
    interp: &PgCatalog,
    expr: &mut Option<ast::Node>,
    refs: &crate::ddl::policies::PolicyRefs,
    renamed: Renamed<'_>,
) {
    use typedpg_pg_query::NodeMut;
    let in_schema = |oid: crate::oid::PgNamespaceOid, n: &NodeMut<'_>| -> bool {
        match n {
            NodeMut::FuncCall(fc) => refs
                .functions
                .get(&fc.location)
                .and_then(|p| interp.pg_proc.get(p))
                .is_some_and(|p| p.pronamespace == oid),
            NodeMut::TypeName(tn) => refs
                .types
                .get(&tn.location)
                .and_then(|t| interp.pg_type.get(t))
                .is_some_and(|t| t.typnamespace == oid),
            NodeMut::CollateClause(cc) => refs
                .collations
                .get(&cc.location)
                .and_then(|c| interp.pg_collation.get(c))
                .is_some_and(|c| c.collnamespace == oid),
            NodeMut::AExpr(ae) => refs
                .operators
                .get(&ae.location)
                .and_then(|o| interp.pg_operator.get(o))
                .is_some_and(|o| o.oprnamespace == oid),
            _ => false,
        }
    };
    edit_nodes(expr, &mut |n| {
        if let Renamed::Schema { oid, name } = renamed {
            if !in_schema(oid, &n) {
                return;
            }
            match n {
                NodeMut::FuncCall(fc) => requalify(&mut fc.funcname, Some(name), None),
                NodeMut::TypeName(tn) => requalify(&mut tn.names, Some(name), None),
                NodeMut::CollateClause(cc) => requalify(&mut cc.collname, Some(name), None),
                NodeMut::AExpr(ae) if !ae.name.is_empty() => {
                    requalify(&mut ae.name, Some(name), None);
                }
                _ => {}
            }
            return;
        }
        match (n, renamed) {
            (NodeMut::FuncCall(fc), Renamed::Function { oid, schema, name })
                if refs.functions.get(&fc.location) == Some(&oid) =>
            {
                requalify(&mut fc.funcname, schema, Some(name));
            }
            (NodeMut::TypeName(tn), Renamed::Type { oid, schema, name })
                if refs.types.get(&tn.location) == Some(&oid) =>
            {
                requalify(&mut tn.names, schema, Some(name));
            }
            (NodeMut::CollateClause(cc), Renamed::Collation { oid, schema, name })
                if refs.collations.get(&cc.location) == Some(&oid) =>
            {
                requalify(&mut cc.collname, schema, Some(name));
            }
            (NodeMut::AExpr(ae), Renamed::Operator { oid, schema })
                if refs.operators.get(&ae.location) == Some(&oid) && !ae.name.is_empty() =>
            {
                requalify(&mut ae.name, Some(schema), None);
            }
            _ => {}
        }
    });
}

/// Before `renamed` is applied to the catalog: the stored expressions that
/// name the object go on naming it under its new name and schema.
pub(crate) fn follow_rename(interp: &mut PgCatalog, renamed: Renamed<'_>) {
    // The name a slot naming the object has in it now.
    let names: Option<Vec<String>> = match renamed {
        Renamed::Function { oid, .. } => interp.pg_proc.get(&oid).map(|p| vec![p.proname.clone()]),
        Renamed::Type { oid, .. } => interp.pg_type.get(&oid).map(|t| vec![t.typname.clone()]),
        Renamed::Collation { oid, .. } => interp
            .pg_collation
            .get(&oid)
            .map(|c| vec![c.collname.clone()]),
        Renamed::Operator { oid, .. } => interp
            .pg_operator
            .get(&oid)
            .map(|o| vec![o.oprname.clone()]),
        // Any name may be an object of the schema's.
        Renamed::Schema { .. } => None,
    };
    let rewritten: Vec<(Slot, Option<ast::Node>)> = stored_expressions(interp)
        .into_iter()
        .filter(|(_, _, expr)| names.as_ref().is_none_or(|n| mentions(expr.as_ref(), n)))
        .filter_map(|(slot, over, mut expr)| {
            let refs = bindings_of(interp, over, expr.as_ref());
            let before = expr.clone();
            rewrite_slots(interp, &mut expr, &refs, renamed);
            (expr != before).then_some((slot, expr))
        })
        .collect();
    for (slot, expr) in rewritten {
        match slot {
            Slot::Check(oid) => {
                if let Some(mut def) = interp.check_defs.get(&oid).cloned() {
                    def.expr = StoredExpr::Written(Box::new(expr));
                    interp.check_defs.insert(oid, def);
                }
            }
            Slot::Default(relid, attnum) => {
                interp
                    .attr_default_exprs
                    .insert((relid, attnum), StoredExpr::Written(Box::new(expr)));
            }
            Slot::IndexExpr(index, i) => {
                // The index's columns keep the names the expressions gave
                // them (`inc` for `inc(a)`) until it is rebuilt.
                if !interp.index_attnames.contains_key(&index) {
                    let names = crate::ddl::indexes::index_attnames(interp, index);
                    interp.index_attnames.insert(index, names);
                }
                if let Some(mut row) = interp.pg_index.get(&index).cloned()
                    && let Some(e) = row.indexprs.get_mut(i)
                {
                    e.ast = expr;
                    interp.pg_index.insert(index, row);
                }
            }
            Slot::IndexPred(index) => {
                if let Some(mut row) = interp.pg_index.get(&index).cloned()
                    && let Some(pred) = row.indpred.as_mut()
                {
                    pred.ast = expr;
                    interp.pg_index.insert(index, row);
                }
            }
            Slot::PartitionKey(relid, i) => {
                if let Some(mut exprs) = interp.partition_key_exprs.get(&relid).cloned()
                    && let Some(e) = exprs.get_mut(i)
                {
                    e.ast = expr;
                    interp.partition_key_exprs.insert(relid, exprs);
                }
            }
            Slot::DomainCheck(domain, i) => {
                if let Some(mut constraints) = interp.domain_constraints.get(&domain).cloned()
                    && let Some(c) = constraints.get_mut(i)
                {
                    c.expr = Some(StoredExpr::Written(Box::new(expr)));
                    interp.domain_constraints.insert(domain, constraints);
                }
            }
            Slot::DomainDefault(domain) => {
                interp
                    .domain_default_exprs
                    .insert(domain, StoredExpr::Written(Box::new(expr)));
            }
            Slot::TriggerWhen(relid, i) => {
                if let Some(t) = interp.triggers.get_mut(&relid).and_then(|ts| ts.get_mut(i)) {
                    t.when = expr;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::pg_catalog::PgCatalog;

    /// The names of the trigger `tr`'s WHEN condition on `t`.
    fn when_names(db: &PgCatalog) -> Vec<String> {
        let relid = db
            .pg_class
            .iter()
            .find(|(_, c)| c.relname == "t")
            .map(|(&oid, _)| oid)
            .unwrap();
        let when = db.triggers[&relid][0].when.clone();
        when.iter()
            .flat_map(|w| w.nodes())
            .filter_map(|(n, ..)| match n {
                typedpg_pg_query::NodeRef::String(s) => Some(s.sval.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_trigger_when_condition_names_what_it_reads_as_renamed() {
        // PG keeps tgqual by attnums and OIDs; the stored parse tree follows
        // RENAME COLUMN (`old.a`, `new.a`, `(new).a`) and ALTER FUNCTION
        // ... RENAME.
        let mut db = PgCatalog::new().unwrap();
        db.apply_sql(
            "CREATE TABLE t (a int, b int);
             CREATE FUNCTION tf() RETURNS trigger LANGUAGE plpgsql
                 AS 'BEGIN RETURN NEW; END';
             CREATE FUNCTION pos(int) RETURNS bool LANGUAGE sql IMMUTABLE
                 AS 'SELECT $1 > 0';
             CREATE TRIGGER tr BEFORE UPDATE ON t FOR EACH ROW
                 WHEN (old.a IS DISTINCT FROM new.a AND (new).a > old.b AND pos(new.b))
                 EXECUTE FUNCTION tf();
             ALTER TABLE t RENAME a TO z;
             ALTER FUNCTION pos(int) RENAME TO pos2;",
        )
        .unwrap();
        let names = when_names(&db);
        assert_eq!(names.iter().filter(|n| *n == "z").count(), 3, "{names:?}");
        assert!(!names.iter().any(|n| n == "a" || n == "pos"), "{names:?}");
        assert!(names.iter().any(|n| n == "pos2"), "{names:?}");
    }
}
