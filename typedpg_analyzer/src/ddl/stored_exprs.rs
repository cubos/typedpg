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
    /// A role renamed from `old` to `new` (a `regrole` constant names it).
    Role { old: &'a str, new: &'a str },
    /// Another object, only reg* constants name (a relation, a text
    /// search configuration or dictionary).
    Other,
}

/// Where a stored expression lives.
#[derive(Clone, Debug)]
enum Slot {
    Check(crate::oid::PgConstraintOid),
    Default(PgClassOid, i16),
    IndexExpr(PgClassOid, usize),
    IndexPred(PgClassOid),
    PartitionKey(PgClassOid, usize),
    DomainCheck(PgTypeOid, usize),
    DomainDefault(PgTypeOid),
    TriggerWhen(PgClassOid, usize),
    /// A policy's USING (`false`) or WITH CHECK (`true`) expression.
    Policy(PgClassOid, String, bool),
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

/// Every stored expression, with what it is analyzed over — the policies'
/// too when `policies` (their renames are [`crate::ddl::policies`]').
fn stored_expressions(interp: &PgCatalog, policies: bool) -> Vec<(Slot, Over, Option<ast::Node>)> {
    let mut out = Vec::new();
    if policies {
        for ((relid, name), (using, with_check)) in &interp.column_deps.policy_quals {
            for (clause, is_check) in [(using, false), (with_check, true)] {
                if let Some(clause) = clause {
                    out.push((
                        Slot::Policy(*relid, name.clone(), is_check),
                        Over::Row(*relid),
                        clause.expr.clone(),
                    ));
                }
            }
        }
    }
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
fn bindings_of(interp: &PgCatalog, over: Over, expr: Option<&ast::Node>) -> Bindings {
    use crate::qualified_name::QualifiedName;
    let mut scope = crate::scope::Scope::default();
    let mut analyzed = expr.cloned();
    match over {
        Over::Row(relid) | Over::TriggerRow(relid) => {
            let Some(class) = interp.pg_class.get(&relid) else {
                return Bindings::default();
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
    let log = crate::nonnull::StrictLog::default();
    let (_, refs) = crate::ddl::policies::collect_refs(|| {
        crate::expr::infer_expr(
            analyzed.as_ref(),
            crate::expr::Ctx::new(&scope, &null_ctx, interp).logging_strictness(&log),
            &mut params,
            crate::expr::TypeGoal::NONE,
        )
    });
    Bindings {
        refs,
        literals: log.trusted().literal_types,
    }
}

/// What a stored expression's names resolved to ([`bindings_of`]): its
/// name slots, and the types its string literals were coerced to (a reg*
/// constant's names an object).
#[derive(Default)]
struct Bindings {
    refs: crate::ddl::policies::PolicyRefs,
    literals: std::collections::HashMap<i32, PgTypeOid>,
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
        // Only reg* constants name these.
        Renamed::Role { .. } | Renamed::Other => Some(Vec::new()),
    };
    let rewritten: Vec<(Slot, Option<ast::Node>)> = stored_expressions(interp, false)
        .into_iter()
        .filter(|(_, _, expr)| names.as_ref().is_none_or(|n| mentions(expr.as_ref(), n)))
        .filter_map(|(slot, over, mut expr)| {
            let refs = bindings_of(interp, over, expr.as_ref()).refs;
            let before = expr.clone();
            rewrite_slots(interp, &mut expr, &refs, renamed);
            (expr != before).then_some((slot, expr))
        })
        .collect();
    for (slot, expr) in rewritten {
        write_slot(interp, slot, expr);
    }
    note_reg_constants(interp, renamed);
}

/// Store `expr` as the expression of `slot`.
fn write_slot(interp: &mut PgCatalog, slot: Slot, expr: Option<ast::Node>) {
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
        Slot::Policy(relid, name, is_check) => {
            if let Some((using, with_check)) =
                interp.column_deps.policy_quals.get_mut(&(relid, name))
                && let Some(clause) = if is_check { with_check } else { using }.as_mut()
            {
                clause.expr = expr;
            }
        }
    }
}

// ─── Binding to the objects found at creation ──────────────────────────────
//
// PG stores an expression analyzed: its routines, operators, types,
// collations and `regclass` constants are OIDs, found under the search path
// in effect when it was written. When it re-parses one (ALTER COLUMN TYPE
// rebuilds a CHECK or an index from `pg_get_constraintdef` /
// `pg_get_indexdef`), ruleutils prints each of those objects unqualified if
// an unqualified lookup under the *current* search path finds that same
// object, schema-qualified otherwise (generate_function_name,
// generate_operator_name, format_type_be, generate_collation_name,
// regclassout).
//
// The analyzer keeps the parse tree. Right after each statement, every
// name in a stored expression that resolves to an object outside
// `pg_catalog` is bound to it by qualifying it with the object's schema
// ([`bind_stored_expressions`]): a later search path then can't make the
// name miss the object, nor an object created later in an earlier schema
// capture it, and a copy of the expression (a child's CHECK, a LIKE copy,
// a partition's index) carries the binding. [`follow_rename`] keeps the
// names right when the object is renamed or moved. Wherever the analyzer
// reads a stored expression again, [`render`] prints it as ruleutils
// would: a qualification the current search path doesn't need is dropped.
// `pg_catalog` objects are never qualified: that schema is searched first
// along every search path that doesn't name it.

/// `pg_catalog`'s or a temporary schema (whose objects are this session's
/// own, under whatever name it has): names in it are left as written.
fn left_as_written(interp: &PgCatalog, ns: crate::oid::PgNamespaceOid) -> bool {
    interp
        .namespace_name(ns)
        .is_none_or(|n| n == "pg_catalog" || n.starts_with("pg_temp"))
}

/// The names of the routines, types, collations, operators and relations
/// outside `pg_catalog`: an unqualified name among them may need binding.
fn user_object_names(interp: &PgCatalog) -> std::collections::HashSet<String> {
    let catalog = interp.namespace_oid("pg_catalog");
    let mut names = std::collections::HashSet::new();
    let user = |ns: crate::oid::PgNamespaceOid| Some(ns) != catalog;
    names.extend(
        interp
            .pg_proc
            .values()
            .filter(|p| user(p.pronamespace))
            .map(|p| p.proname.clone()),
    );
    names.extend(
        interp
            .pg_type
            .values()
            .filter(|t| user(t.typnamespace))
            .map(|t| t.typname.clone()),
    );
    names.extend(
        interp
            .pg_collation
            .values()
            .filter(|c| user(c.collnamespace))
            .map(|c| c.collname.clone()),
    );
    names.extend(
        interp
            .pg_operator
            .values()
            .filter(|o| user(o.oprnamespace))
            .map(|o| o.oprname.clone()),
    );
    names.extend(
        interp
            .pg_class
            .values()
            .filter(|c| user(c.relnamespace))
            .map(|c| c.relname.clone()),
    );
    names
}

/// Whether `expr` has an unqualified name among `names` (or a string
/// constant equal to one, which may be a `regclass`).
fn has_unbound_name(expr: Option<&ast::Node>, names: &std::collections::HashSet<String>) -> bool {
    use typedpg_pg_query::NodeRef;
    let single = |list: &[Option<ast::Node>]| match list {
        [Some(ast::Node::String(s))] => names.contains(&s.sval),
        _ => false,
    };
    expr.is_some_and(|e| {
        e.nodes().any(|(n, ..)| match n {
            NodeRef::FuncCall(fc) => single(&fc.funcname),
            NodeRef::TypeName(tn) => single(&tn.names),
            NodeRef::CollateClause(cc) => single(&cc.collname),
            NodeRef::AExpr(ae) => single(&ae.name),
            // A reg* constant: `name`, or `name(args)`.
            NodeRef::AConst(c) => matches!(&c.val,
                Some(ast::AConstValue::String(s))
                    if names.contains(s.sval.split('(').next().unwrap_or_default().trim())),
            _ => false,
        })
    })
}

/// Qualify the names of `expr` that `refs` resolved to objects outside
/// `pg_catalog` with their schemas.
fn bind_slots(interp: &PgCatalog, expr: &mut Option<ast::Node>, bindings: &Bindings) {
    let refs = &bindings.refs;
    let literals = &bindings.literals;
    use typedpg_pg_query::NodeMut;
    let schema = |ns: crate::oid::PgNamespaceOid| -> Option<String> {
        (!left_as_written(interp, ns))
            .then(|| interp.namespace_name(ns).map(str::to_owned))
            .flatten()
    };
    edit_nodes(expr, &mut |n| match n {
        NodeMut::FuncCall(fc) => {
            let Some(proc) = refs
                .functions
                .get(&fc.location)
                .and_then(|p| interp.pg_proc.get(p))
            else {
                return;
            };
            if fc.funcname.len() == 1
                && let Some(ns) = schema(proc.pronamespace)
            {
                requalify(&mut fc.funcname, Some(&ns), None);
            }
        }
        NodeMut::TypeName(tn) => {
            if tn.names.len() == 1
                && !tn.pct_type
                && let Some(ns) = refs
                    .types
                    .get(&tn.location)
                    .and_then(|t| interp.pg_type.get(t))
                    .and_then(|t| schema(t.typnamespace))
            {
                requalify(&mut tn.names, Some(&ns), None);
            }
        }
        // A reg* constant names its object qualified, as its OID would.
        NodeMut::AConst(c) => {
            if let Some(typname) = literals
                .get(&c.location)
                .and_then(|&t| reg_type_name(interp, t))
                && let Some(ast::AConstValue::String(s)) = c.val.as_mut()
                && let Some(object) = crate::reg_input::resolve(typname, &s.sval, interp)
                && let Some(text) = reg_text(interp, typname, &object, true)
            {
                s.sval = text;
            }
        }
        NodeMut::CollateClause(cc) => {
            if cc.collname.len() == 1
                && let Some(ns) = refs
                    .collations
                    .get(&cc.location)
                    .and_then(|c| interp.pg_collation.get(c))
                    .and_then(|c| schema(c.collnamespace))
            {
                requalify(&mut cc.collname, Some(&ns), None);
            }
        }
        NodeMut::AExpr(ae) => {
            if ae.kind == ast::AExprKind::AexprOp
                && ae.name.len() == 1
                && let Some(ns) = refs
                    .operators
                    .get(&ae.location)
                    .and_then(|o| interp.pg_operator.get(o))
                    .and_then(|o| schema(o.oprnamespace))
            {
                requalify(&mut ae.name, Some(&ns), None);
            }
        }
        _ => {}
    });
}

/// After a statement: bind the stored expressions it wrote (see above).
/// One already bound has no unqualified name of an object outside
/// `pg_catalog` left, and is skipped.
pub(crate) fn bind_stored_expressions(interp: &mut PgCatalog) -> Result<(), super::DdlError> {
    apply_reg_constants(interp);
    refuse_unstorable_constants(interp)?;
    let names = user_object_names(interp);
    if names.is_empty() {
        return Ok(());
    }
    let bound: Vec<(Slot, Option<ast::Node>)> = stored_expressions(interp, true)
        .into_iter()
        .filter(|(_, _, expr)| has_unbound_name(expr.as_ref(), &names))
        .filter_map(|(slot, over, mut expr)| {
            let bindings = bindings_of(interp, over, expr.as_ref());
            let before = expr.clone();
            bind_slots(interp, &mut expr, &bindings);
            (expr != before).then_some((slot, expr))
        })
        .collect();
    for (slot, expr) in bound {
        write_slot(interp, slot, expr);
    }
    Ok(())
}

/// Whether ruleutils would print routine `name` of schema `ns` unqualified
/// (generate_function_name): an unqualified call with the routine's
/// argument types resolves to it — for each routine of that name there.
fn function_visible(interp: &PgCatalog, ns: crate::oid::PgNamespaceOid, name: &str) -> bool {
    let procs: Vec<&crate::pg_catalog::PgProc> = interp
        .pg_proc
        .values()
        .filter(|p| p.pronamespace == ns && p.proname == name)
        .collect();
    !procs.is_empty()
        && procs.iter().all(|p| {
            crate::functions::resolve_function(
                interp,
                None,
                name,
                &p.proargtypes,
                &crate::functions::CallNotation::default(),
                false,
                None,
            )
            .is_ok_and(|f| f.oid == p.oid)
        })
}

/// Whether ruleutils would print operator `name` of schema `ns` without
/// `OPERATOR(schema.op)` (generate_operator_name): an unqualified operator
/// with its operand types resolves to it — for each operator of that name
/// there.
fn operator_visible(interp: &PgCatalog, ns: crate::oid::PgNamespaceOid, name: &str) -> bool {
    let ops: Vec<&crate::pg_catalog::PgOperator> = interp
        .pg_operator
        .values()
        .filter(|o| o.oprnamespace == ns && o.oprname == name)
        .collect();
    !ops.is_empty()
        && ops.iter().all(|o| {
            matches!(interp.find_operator_detailed(name, o.oprleft, o.oprright),
                crate::lookup::OperatorMatch::Found(r) if r.oid == o.oid)
        })
}

/// The namespace a qualified name's schema part names, unless it is one
/// whose names are left as written.
fn qualifier(
    interp: &PgCatalog,
    names: &[Option<ast::Node>],
) -> Option<(crate::oid::PgNamespaceOid, String)> {
    let [Some(ast::Node::String(s)), Some(ast::Node::String(n))] = names else {
        return None;
    };
    let ns = interp.namespace_oid(&s.sval)?;
    (!left_as_written(interp, ns)).then(|| (ns, n.sval.clone()))
}

/// `expr`, a stored expression, as ruleutils prints it under the current
/// search path: a schema qualification is dropped wherever an unqualified
/// lookup finds the same object.
pub(crate) fn render(interp: &PgCatalog, expr: Option<&ast::Node>) -> Option<ast::Node> {
    use typedpg_pg_query::NodeMut;
    let mut out = expr.cloned();
    // The lookups below are the renderer's, not the statement's.
    let _barrier = crate::error::DiagContextGuard::barrier();
    crate::ddl::depend::collect(|| {
        edit_nodes(&mut out, &mut |n| match n {
            NodeMut::FuncCall(fc) => {
                if let Some((ns, name)) = qualifier(interp, &fc.funcname)
                    && function_visible(interp, ns, &name)
                {
                    fc.funcname.remove(0);
                }
                // `nextval('seq')`: a regclass constant.
                let sequence_function = match fc.funcname.as_slice() {
                    [Some(ast::Node::String(n))] => {
                        matches!(n.sval.as_str(), "nextval" | "currval" | "setval")
                    }
                    [Some(ast::Node::String(s)), Some(ast::Node::String(n))] => {
                        s.sval == "pg_catalog"
                            && matches!(n.sval.as_str(), "nextval" | "currval" | "setval")
                    }
                    _ => false,
                };
                if sequence_function
                    && let Some(Some(ast::Node::AConst(c))) = fc.args.first_mut()
                    && let Some(ast::AConstValue::String(s)) = c.val.as_mut()
                    && let Some(object) = crate::reg_input::resolve("regclass", &s.sval, interp)
                    && let Some(text) = reg_text(interp, "regclass", &object, false)
                {
                    s.sval = text;
                }
            }
            NodeMut::TypeName(tn) => {
                if !tn.pct_type
                    && let Some((ns, name)) = qualifier(interp, &tn.names)
                    && interp
                        .resolve_type_by_name(None, &name)
                        .is_some_and(|t| t.typnamespace == ns)
                {
                    tn.names.remove(0);
                }
            }
            // `'name'::regtype` and the like.
            NodeMut::TypeCast(tc) => {
                let typname = tc.type_name.as_ref().and_then(|tn| match tn.names.last() {
                    Some(Some(ast::Node::String(s))) => crate::reg_input::REG_TYPES
                        .iter()
                        .find(|r| **r == s.sval)
                        .copied(),
                    _ => None,
                });
                if let Some(typname) = typname
                    && let Some(ast::Node::AConst(c)) = tc.arg.as_mut()
                    && let Some(ast::AConstValue::String(s)) = c.val.as_mut()
                    && let Some(object) = crate::reg_input::resolve(typname, &s.sval, interp)
                    && let Some(text) = reg_text(interp, typname, &object, false)
                {
                    s.sval = text;
                }
            }
            NodeMut::CollateClause(cc) => {
                if let Some((ns, name)) = qualifier(interp, &cc.collname)
                    && interp
                        .resolve_collation(None, &name)
                        .is_some_and(|c| c.collnamespace == ns)
                {
                    cc.collname.remove(0);
                }
            }
            NodeMut::AExpr(ae) => {
                if ae.kind == ast::AExprKind::AexprOp
                    && let Some((ns, name)) = qualifier(interp, &ae.name)
                    && operator_visible(interp, ns, &name)
                {
                    ae.name.remove(0);
                }
            }
            _ => {}
        });
    });
    out
}

// ─── reg* constants ────────────────────────────────────────────────────────
//
// PG stores a reg* constant (`'s.f'::regproc`, `nextval('s.sq')`) as the
// OID its input function found, and prints it with its output function:
// qualified only where the current search path wouldn't find that object
// (regclassout, regprocout, format_procedure, format_operator,
// format_type_be, regcollationout, regconfigout, regdictionaryout;
// regnamespaceout and regroleout print the bare name). The analyzer keeps
// the string: bound qualified at creation, rewritten when the object it
// names is renamed or moved ([`note_reg_constants`]), printed as the
// output function would where a stored expression is read back.

/// `t`'s name when it is one of the reg* types naming an object.
fn reg_type_name(interp: &PgCatalog, t: PgTypeOid) -> Option<&'static str> {
    let ty = interp.pg_type.get(&t)?;
    if interp.namespace_name(ty.typnamespace) != Some("pg_catalog") {
        return None;
    }
    crate::reg_input::REG_TYPES
        .iter()
        .find(|r| **r == ty.typname)
        .copied()
}

/// `name` in schema `ns`: qualified when `bind` (bound to an object
/// outside `pg_catalog`) or when not `visible`.
fn qualified_unless(
    interp: &PgCatalog,
    ns: crate::oid::PgNamespaceOid,
    name: &str,
    visible: bool,
    bind: bool,
) -> String {
    let qualify = if bind {
        !left_as_written(interp, ns)
    } else {
        !visible
    };
    match interp.namespace_name(ns) {
        Some(schema) if qualify => {
            crate::qualified_name::QualifiedName::new(schema, name).to_string()
        }
        _ => typedpg_core::quote_identifier(name),
    }
}

/// `format_type_be` of `t` (bound: a type outside `pg_catalog` qualified).
fn reg_type_text(interp: &PgCatalog, t: PgTypeOid, bind: bool) -> String {
    let Some(ty) = interp.pg_type.get(&t) else {
        return String::new();
    };
    if interp.namespace_name(ty.typnamespace) == Some("pg_catalog") {
        return crate::ddl::util::format_type_for_message(interp, t);
    }
    if let Some(elem) = ty.typelem
        && ty.typname.starts_with('_')
    {
        return format!("{}[]", reg_type_text(interp, elem, bind));
    }
    let visible = interp
        .resolve_type_by_name(None, &ty.typname)
        .is_some_and(|v| v.oid == t);
    qualified_unless(interp, ty.typnamespace, &ty.typname, visible, bind)
}

/// The text the output function of reg* type `typname` prints for
/// `object` — or, when `bind`, the text binding it: every object outside
/// `pg_catalog` qualified.
pub(crate) fn reg_text(
    interp: &PgCatalog,
    typname: &str,
    object: &crate::reg_input::RegObject,
    bind: bool,
) -> Option<String> {
    use crate::reg_input::RegObject;
    Some(match object {
        RegObject::Class(c) => {
            let class = interp.pg_class.get(c)?;
            let visible = interp
                .resolve_table(None, &class.relname)
                .is_some_and(|v| v.oid == *c);
            qualified_unless(interp, class.relnamespace, &class.relname, visible, bind)
        }
        RegObject::Proc(p) => {
            let proc = interp.pg_proc.get(p)?;
            if typname == "regproc" {
                // regprocout: unqualified when the name alone finds it,
                // uniquely.
                let found = interp.find_functions(None, &proc.proname);
                let visible = found.len() == 1 && found[0].oid == *p;
                qualified_unless(interp, proc.pronamespace, &proc.proname, visible, bind)
            } else {
                // format_procedure: FunctionIsVisible, then the argument
                // types.
                let visible = interp
                    .schemas_for_function_lookup(None)
                    .into_iter()
                    .find_map(|ns| {
                        interp
                            .find_functions(interp.namespace_name(ns), &proc.proname)
                            .into_iter()
                            .find(|q| q.pronamespace == ns && q.proargtypes == proc.proargtypes)
                            .map(|q| q.oid)
                    })
                    == Some(*p);
                let args: Vec<String> = proc
                    .proargtypes
                    .iter()
                    .map(|&t| reg_type_text(interp, t, bind))
                    .collect();
                format!(
                    "{}({})",
                    qualified_unless(interp, proc.pronamespace, &proc.proname, visible, bind),
                    args.join(",")
                )
            }
        }
        RegObject::Operator(o) => {
            let op = interp.pg_operator.get(o)?;
            let path = interp.schemas_for_function_lookup(None);
            if typname == "regoper" {
                // regoperout: unqualified when the name alone finds it,
                // uniquely.
                let found: Vec<_> = interp
                    .pg_operator
                    .values()
                    .filter(|x| x.oprname == op.oprname && path.contains(&x.oprnamespace))
                    .collect();
                let visible = found.len() == 1 && found[0].oid == *o;
                qualified_unless(interp, op.oprnamespace, &op.oprname, visible, bind)
            } else {
                // format_operator: OperatorIsVisible, then the operand
                // types (`NONE` for a prefix operator's left).
                let visible = path.iter().find_map(|ns| {
                    interp
                        .pg_operator
                        .values()
                        .find(|x| {
                            x.oprnamespace == *ns
                                && x.oprname == op.oprname
                                && x.oprleft == op.oprleft
                                && x.oprright == op.oprright
                        })
                        .map(|x| x.oid)
                }) == Some(*o);
                let left = op
                    .oprleft
                    .map_or_else(|| "NONE".to_owned(), |t| reg_type_text(interp, t, bind));
                format!(
                    "{}({left},{})",
                    qualified_unless(interp, op.oprnamespace, &op.oprname, visible, bind),
                    reg_type_text(interp, op.oprright, bind)
                )
            }
        }
        RegObject::Type(t) => reg_type_text(interp, *t, bind),
        RegObject::Collation(c) => {
            let coll = interp.pg_collation.get(c)?;
            let visible = interp
                .resolve_collation(None, &coll.collname)
                .is_some_and(|v| v.oid == *c);
            qualified_unless(interp, coll.collnamespace, &coll.collname, visible, bind)
        }
        RegObject::Namespace(ns) => typedpg_core::quote_identifier(interp.namespace_name(*ns)?),
        RegObject::Role(name) => typedpg_core::quote_identifier(name),
        RegObject::TsObject(oid) => {
            let object = interp.pg_ts_objects.iter().find(|o| o.oid == *oid)?;
            // TSConfigIsVisible / TSDictionaryIsVisible.
            let first = interp.schemas_for_lookup(None).into_iter().find(|ns| {
                interp
                    .pg_ts_objects
                    .iter()
                    .any(|o| o.kind == object.kind && o.name == object.name && o.namespace == *ns)
            });
            qualified_unless(
                interp,
                object.namespace,
                &object.name,
                first == Some(object.namespace),
                bind,
            )
        }
    })
}

/// What the output function of type `t` prints for the reg* constant
/// written `text`, under the current search path; `None` when `t` isn't
/// a reg* type or the constant names nothing.
pub(crate) fn reg_output(interp: &PgCatalog, t: PgTypeOid, text: &str) -> Option<String> {
    let typname = reg_type_name(interp, t)?;
    let object = crate::reg_input::resolve(typname, text, interp)?;
    reg_text(interp, typname, &object, false)
}

/// A reg* constant to bind again once a rename is applied: the slot, the
/// constant's location, its type, and the object it named.
type PendingReg = (Slot, i32, &'static str, crate::reg_input::RegObject);

thread_local! {
    /// The reg* constants a rename in the current statement may have left
    /// naming their objects by an old name.
    static PENDING_REG: std::cell::RefCell<Vec<PendingReg>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Before `renamed` is applied: note every reg* constant of the stored
/// expressions with the object it names now, to write it again under the
/// object's name once the statement has renamed it.
fn note_reg_constants(interp: &PgCatalog, renamed: Renamed<'_>) {
    use crate::reg_input::RegObject;
    let mut pending = Vec::new();
    for (slot, over, expr) in stored_expressions(interp, true) {
        let has_string = expr.as_ref().is_some_and(|e| {
            e.nodes().any(|(n, ..)| {
                matches!(n, typedpg_pg_query::NodeRef::AConst(c)
                    if matches!(c.val, Some(ast::AConstValue::String(_))))
            })
        });
        if !has_string {
            continue;
        }
        let literals = bindings_of(interp, over, expr.as_ref()).literals;
        for (n, ..) in expr.iter().flat_map(|e| e.nodes()) {
            let typedpg_pg_query::NodeRef::AConst(c) = n else {
                continue;
            };
            let Some(ast::AConstValue::String(s)) = &c.val else {
                continue;
            };
            let Some(typname) = literals
                .get(&c.location)
                .and_then(|&t| reg_type_name(interp, t))
            else {
                continue;
            };
            let Some(mut object) = crate::reg_input::resolve(typname, &s.sval, interp) else {
                continue;
            };
            // A role's name is all a `regrole` constant has.
            if let (Renamed::Role { old, new }, RegObject::Role(name)) = (renamed, &object)
                && name == old
            {
                object = RegObject::Role(new.to_owned());
            }
            pending.push((slot.clone(), c.location, typname, object));
        }
    }
    PENDING_REG.with(|p| p.borrow_mut().extend(pending));
}

/// A statement failed: the renames it noted didn't happen.
pub(crate) fn discard_pending_reg_constants() {
    PENDING_REG.with(|p| p.borrow_mut().clear());
}

/// After a statement: write the reg* constants [`note_reg_constants`]
/// noted again, naming their objects as the catalog now does.
fn apply_reg_constants(interp: &mut PgCatalog) {
    let pending = PENDING_REG.with(|p| std::mem::take(&mut *p.borrow_mut()));
    for (slot, location, typname, object) in pending {
        let Some(text) = reg_text(interp, typname, &object, true) else {
            continue;
        };
        let Some(mut expr) = read_slot(interp, &slot) else {
            continue;
        };
        let mut changed = false;
        edit_nodes(&mut expr, &mut |n| {
            if let typedpg_pg_query::NodeMut::AConst(c) = n
                && c.location == location
                && let Some(ast::AConstValue::String(s)) = c.val.as_mut()
                && s.sval != text
            {
                s.sval = text.clone();
                changed = true;
            }
        });
        if changed {
            write_slot(interp, slot, expr);
        }
    }
}

/// The expression stored in `slot`, if it still exists.
fn read_slot(interp: &PgCatalog, slot: &Slot) -> Option<Option<ast::Node>> {
    stored_expressions(interp, true)
        .into_iter()
        .find(|(s, ..)| slot_eq(s, slot))
        .map(|(_, _, e)| e)
}

fn slot_eq(a: &Slot, b: &Slot) -> bool {
    match (a, b) {
        (Slot::Check(x), Slot::Check(y)) => x == y,
        (Slot::Default(r1, a1), Slot::Default(r2, a2)) => r1 == r2 && a1 == a2,
        (Slot::IndexExpr(i1, n1), Slot::IndexExpr(i2, n2)) => i1 == i2 && n1 == n2,
        (Slot::IndexPred(i1), Slot::IndexPred(i2)) => i1 == i2,
        (Slot::PartitionKey(r1, n1), Slot::PartitionKey(r2, n2)) => r1 == r2 && n1 == n2,
        (Slot::DomainCheck(d1, n1), Slot::DomainCheck(d2, n2)) => d1 == d2 && n1 == n2,
        (Slot::DomainDefault(d1), Slot::DomainDefault(d2)) => d1 == d2,
        (Slot::TriggerWhen(r1, n1), Slot::TriggerWhen(r2, n2)) => r1 == r2 && n1 == n2,
        (Slot::Policy(r1, p1, c1), Slot::Policy(r2, p2, c2)) => r1 == r2 && p1 == p2 && c1 == c2,
        _ => false,
    }
}

/// The reg* types whose constants no stored expression may hold:
/// find_expr_references_walker records no dependency on a role or a
/// database, so it refuses them.
const UNSTORABLE_REG_TYPES: &[&str] = &["regrole", "regdatabase"];

/// Whether `t` (a domain: its base type) is one of
/// [`UNSTORABLE_REG_TYPES`], which it returns.
fn unstorable_type(interp: &PgCatalog, t: PgTypeOid) -> Option<&'static str> {
    let ty = interp.pg_type.get(&interp.unwrap_domain(t))?;
    if interp.namespace_name(ty.typnamespace) != Some("pg_catalog") {
        return None;
    }
    UNSTORABLE_REG_TYPES
        .iter()
        .find(|r| **r == ty.typname)
        .copied()
}

/// recordDependencyOnExpr over a stored expression holding a `regrole` or
/// `regdatabase` constant: `constant of the type regrole cannot be used
/// here`. Only an expression that can hold one is analyzed — casting to
/// one of those types, over a relation with a column of one, or the
/// default of such a column.
fn refuse_unstorable_constants(interp: &PgCatalog) -> Result<(), super::DdlError> {
    let has_cast = |expr: Option<&ast::Node>| {
        expr.is_some_and(|e| {
            e.nodes().any(|(n, ..)| {
                matches!(n, typedpg_pg_query::NodeRef::TypeName(tn)
                    if matches!(tn.names.last(), Some(Some(ast::Node::String(s)))
                        if UNSTORABLE_REG_TYPES.contains(&s.sval.as_str())))
            })
        })
    };
    let relation_holds = |relid: PgClassOid| {
        interp
            .attributes_of(relid)
            .iter()
            .any(|a| unstorable_type(interp, a.atttypid).is_some())
    };
    let refused = |typname: &str| {
        Err(super::DdlError::UnsupportedDdl(format!(
            "constant of the type {typname} cannot be used here"
        )))
    };
    for (slot, over, expr) in stored_expressions(interp, true) {
        // A default of such a column: the constant it is coerced to.
        if let Slot::Default(relid, attnum) = slot
            && let Some(typname) = interp
                .attributes_of(relid)
                .iter()
                .find(|a| a.attnum == attnum)
                .and_then(|a| unstorable_type(interp, a.atttypid))
            && matches!(expr, Some(ast::Node::AConst(ref c)) if !c.isnull)
        {
            return refused(typname);
        }
        let candidate = has_cast(expr.as_ref())
            || match over {
                Over::Row(relid) | Over::TriggerRow(relid) => relation_holds(relid),
                Over::Value(base) => unstorable_type(interp, base).is_some(),
                Over::Nothing => false,
            };
        if !candidate {
            continue;
        }
        let literals = bindings_of(interp, over, expr.as_ref()).literals;
        if let Some(typname) = literals.values().find_map(|&t| unstorable_type(interp, t)) {
            return refused(typname);
        }
    }
    Ok(())
}

// ─── Rebuilt by ALTER COLUMN TYPE ──────────────────────────────────────────

/// ALTER COLUMN TYPE of column `attnum` (named `attname`) of `relid`,
/// before the column changes: the CHECK constraints reading it, and the
/// expressions and predicates of the indexes reading it, become the text
/// `pg_get_constraintdef` / `pg_get_indexdef` print for them now
/// (RememberConstraintForRebuilding, RememberIndexForRebuilding), which the
/// rebuild then parses over the new type ([`crate::ddl::cooked::deparse`]).
pub(crate) fn deparse_for_rebuild(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    attnum: i16,
    attname: &str,
) {
    let checks: Vec<crate::oid::PgConstraintOid> = interp
        .pg_constraint
        .values()
        .filter(|c| {
            c.conrelid == relid && c.contype == ConType::Check && c.conkey.contains(&attnum)
        })
        .map(|c| c.oid)
        .collect();
    for oid in checks {
        let Some(mut def) = interp.check_defs.get(&oid).cloned() else {
            continue;
        };
        let StoredExpr::Written(expr) = &def.expr else {
            continue;
        };
        let Some(text) = crate::ddl::cooked::deparse(interp, relid, (**expr).as_ref()) else {
            continue;
        };
        // What the rebuilt text resolves to before the change, which the
        // recook after it compares with (`recook_checks_reading`).
        def.trusted = crate::ddl::volatile::trusted_over_relation(interp, relid, Some(&text));
        def.expr = StoredExpr::Written(Box::new(Some(text)));
        interp.check_defs.insert(oid, def);
    }
    let reads = |ast: &crate::pg_catalog::SerializedAst| {
        ast.ast.as_ref().is_some_and(|inner| {
            inner.nodes().any(|(n, ..)| {
                matches!(n, typedpg_pg_query::NodeRef::ColumnRef(cr)
                    if cr.fields.last().and_then(|n| crate::ddl::util::node_string(n.as_ref()))
                        == Some(attname))
            })
        })
    };
    let indexes: Vec<PgClassOid> = interp
        .pg_index
        .values()
        .filter(|i| {
            i.indrelid == relid
                && (i.indkey.contains(&attnum)
                    || i.indexprs.iter().any(reads)
                    || i.indpred.as_ref().is_some_and(reads))
        })
        .map(|i| i.indexrelid)
        .collect();
    for index in indexes {
        let Some(mut row) = interp.pg_index.get(&index).cloned() else {
            continue;
        };
        for e in row.indexprs.iter_mut().chain(row.indpred.as_mut()) {
            if let Some(text) = crate::ddl::cooked::deparse(interp, relid, e.ast.as_ref()) {
                e.ast = Some(text);
            }
        }
        interp.pg_index.insert(index, row);
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
    /// The routine name the deparsed CHECK of table `t` calls.
    fn deparsed_call(db: &PgCatalog) -> Vec<String> {
        let relid = db
            .pg_class
            .iter()
            .find(|(_, c)| c.relname == "t")
            .map(|(&oid, _)| oid)
            .unwrap();
        let def = db
            .check_defs
            .values()
            .next()
            .map(|d| d.expr.clone())
            .unwrap();
        let crate::ddl::tables::check_inherit::StoredExpr::Written(expr) = def else {
            panic!("a written CHECK");
        };
        let deparsed = crate::ddl::cooked::deparse(db, relid, (*expr).as_ref()).unwrap();
        let typedpg_pg_query::ast::Node::FuncCall(fc) = deparsed else {
            panic!("a call: {deparsed:?}");
        };
        crate::expr::extract_string_fields(&fc.funcname)
    }

    #[test]
    fn a_deparsed_call_is_named_for_its_actual_argument_types() {
        // generate_function_name looks the name up with the arguments'
        // types (exprType, after their coercions): `s.g(anyelement)` called
        // on an integer is captured by `public.g(integer)` once that exists
        // (an exact match beats a polymorphic one, whatever the schemas'
        // order), so it is printed qualified — while its declared
        // `anyelement` would still find it.
        let mut db = PgCatalog::new().unwrap();
        db.apply_sql(
            "CREATE SCHEMA s;
             CREATE FUNCTION s.g(anyelement) RETURNS bool LANGUAGE sql IMMUTABLE
                 AS 'SELECT true';
             SET search_path = s, public;
             CREATE TABLE public.t (c int CHECK (g(c)));",
        )
        .unwrap();
        let path = |db: &mut PgCatalog, names: &[&str]| {
            db.search_path = names.iter().map(|n| db.namespace_oid(n).unwrap()).collect();
        };
        path(&mut db, &["s", "public"]);
        assert_eq!(deparsed_call(&db), ["g"]);
        db.apply_sql(
            "CREATE FUNCTION public.g(int) RETURNS bool LANGUAGE sql IMMUTABLE
                 AS 'SELECT false';",
        )
        .unwrap();
        path(&mut db, &["s", "public"]);
        assert_eq!(deparsed_call(&db), ["s", "g"]);
    }
}
