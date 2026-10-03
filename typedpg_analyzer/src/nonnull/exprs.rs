//! Facts about whole expressions: a qual proving `j ->> 'k'` (or, in
//! HAVING, `max(b)`) non-NULL proves the same expression non-NULL wherever
//! else the query reads it for that row — PG matches such expressions
//! with `equal()` (setrefs.c, predtest.c; equal aggregates are computed
//! once). That holds only for expressions that give the same value every
//! time they are evaluated for a row within one statement: no volatile
//! function, operator or cast, and a sublink only over a subquery whose
//! result can't change between two evaluations under the statement's
//! snapshot (see [`reusable`]).

use std::collections::HashSet;

use typedpg_pg_query::protobuf::{self, node};
use typedpg_pg_query::{NodeEnum, NodeRef};

use crate::pg_catalog::{PgCatalog, ProVolatile, RelKind};
use crate::scope::Scope;

/// The key an expression fact is stored and matched under: the tree with
/// source locations dropped. Column references are kept as written, so
/// `t.j ->> 'k'` and `u.j ->> 'k'` never meet (an unqualified `j` and a
/// qualified `t.j` simply don't match).
pub(crate) fn key(n: &protobuf::Node) -> String {
    crate::resolve::node_fingerprint(n)
}

/// Whether `n` may carry an expression fact: an expression other than a
/// bare column, constant or parameter, evaluating the same way each time
/// for one row of one statement.
pub(crate) fn reusable(n: &protobuf::Node, scope: &Scope, snapshot: &PgCatalog) -> bool {
    // An implicit cast may run anywhere an argument meets a parameter of
    // another type (`fc(x)` over an `int` `x` for `fc(c)`): with a volatile
    // one in the catalog, no expression is assumed to give the same value
    // twice.
    if has_volatile_cast(snapshot, true) {
        return false;
    }
    match n.node.as_ref() {
        None
        | Some(
            node::Node::ColumnRef(_)
            | node::Node::AConst(_)
            | node::Node::ParamRef(_)
            | node::Node::NamedArgExpr(_)
            | node::Node::CollateClause(_),
        ) => return false,
        Some(_) => {}
    }
    let Some(root) = n.node.as_ref() else {
        return false;
    };
    let nodes = NodeEnum::nodes(root);
    let in_subquery = nodes
        .iter()
        .any(|(n, _)| matches!(n, NodeRef::SelectStmt(_)));
    if in_subquery && super::row_locking() {
        // EvalPlanQual re-evaluates a locked row's quals with the row's
        // latest version; keep sublinks out of it.
        return false;
    }
    let ctes: HashSet<&str> = nodes
        .iter()
        .filter_map(|(n, _)| match n {
            NodeRef::CommonTableExpr(c) => Some(c.ctename.as_str()),
            _ => None,
        })
        .collect();
    nodes.iter().all(|(n, _)| match n {
        // Plain expression nodes.
        NodeRef::ColumnRef(_)
        | NodeRef::AConst(_)
        | NodeRef::ParamRef(_)
        | NodeRef::String(_)
        | NodeRef::Integer(_)
        | NodeRef::Float(_)
        | NodeRef::Boolean(_)
        | NodeRef::BitString(_)
        | NodeRef::AStar(_)
        | NodeRef::List(_)
        | NodeRef::BoolExpr(_)
        | NodeRef::NullTest(_)
        | NodeRef::BooleanTest(_)
        | NodeRef::CoalesceExpr(_)
        | NodeRef::MinMaxExpr(_)
        | NodeRef::CaseExpr(_)
        | NodeRef::CaseWhen(_)
        | NodeRef::RowExpr(_)
        | NodeRef::AArrayExpr(_)
        | NodeRef::AIndirection(_)
        | NodeRef::AIndices(_)
        | NodeRef::NamedArgExpr(_)
        | NodeRef::CollateClause(_)
        | NodeRef::SqlvalueFunction(_)
        | NodeRef::GroupingFunc(_)
        | NodeRef::TypeName(_)
        | NodeRef::SortBy(_) => true,
        NodeRef::WindowDef(_) => !in_subquery,
        NodeRef::TypeCast(_) => !has_volatile_cast(snapshot, false),
        NodeRef::AExpr(e) => operator_not_volatile(e, snapshot),
        NodeRef::FuncCall(f) => function_not_volatile(f, in_subquery, snapshot),
        // A scalar sublink over a subquery that reads the same rows each
        // time (one snapshot per statement).
        NodeRef::SubLink(s) => {
            protobuf::SubLinkType::try_from(s.sub_link_type)
                == Ok(protobuf::SubLinkType::ExprSublink)
        }
        NodeRef::SelectStmt(s) => {
            // LIMIT / OFFSET over an unordered (or tied) input, DISTINCT ON
            // and row locking pick rows that may differ between two runs
            // (synchronized scans start anywhere).
            s.limit_count.is_none()
                && s.limit_offset.is_none()
                && s.locking_clause.is_empty()
                && s.distinct_clause.iter().all(|d| d.node.is_none())
        }
        NodeRef::ResTarget(_)
        | NodeRef::JoinExpr(_)
        | NodeRef::RangeSubselect(_)
        | NodeRef::Alias(_)
        | NodeRef::WithClause(_)
        | NodeRef::CommonTableExpr(_) => true,
        NodeRef::RangeVar(rv) => relation_is_stable(rv, &ctes, scope, snapshot),
        _ => false,
    })
}

/// Whether some cast of the catalog runs a volatile function (none built
/// in does) — an implicit one only, when `implicit_only`: then no cast
/// (that kind of cast) is assumed to give the same value twice.
fn has_volatile_cast(snapshot: &PgCatalog, implicit_only: bool) -> bool {
    snapshot.pg_cast.values().any(|c| {
        (!implicit_only || c.castcontext == crate::pg_catalog::CastContext::Implicit)
            && c.castfunc
                .and_then(|f| snapshot.pg_proc.get(&f))
                .is_some_and(|p| p.provolatile == ProVolatile::Volatile)
    })
}

/// No operator the expression may have resolved to runs a volatile
/// function: none of the name it is written with — or, for BETWEEN, of
/// the comparisons PG rewrites it into (`transformAExprBetween`: `a >= b
/// AND a <= c`, `a < b OR a > c`).
fn operator_not_volatile(e: &protobuf::AExpr, snapshot: &PgCatalog) -> bool {
    use protobuf::AExprKind as K;
    let name = crate::expr::extract_string_fields(&e.name);
    let names: Vec<&str> = match K::try_from(e.kind) {
        Ok(K::AexprBetween | K::AexprBetweenSym) => vec![">=", "<="],
        Ok(K::AexprNotBetween | K::AexprNotBetweenSym) => vec!["<", ">"],
        _ => match name.last() {
            Some(op) => vec![op.as_str()],
            None => return true,
        },
    };
    !snapshot.pg_operator.values().any(|o| {
        names.contains(&o.oprname.as_str())
            && o.oprcode
                .and_then(|f| snapshot.pg_proc.get(&f))
                .is_none_or(|p| p.provolatile == ProVolatile::Volatile)
    })
}

/// No function of this name is volatile. Inside a subquery, also no
/// window call (ties make its value depend on the scan order) and no
/// user-defined aggregate (its transition may depend on the input order).
fn function_not_volatile(f: &protobuf::FuncCall, in_subquery: bool, snapshot: &PgCatalog) -> bool {
    if in_subquery && f.over.is_some() {
        return false;
    }
    let parts = crate::expr::extract_string_fields(&f.funcname);
    let (schema, name) = match parts.as_slice() {
        [n] => (None, n.as_str()),
        [s, n] => (Some(s.as_str()), n.as_str()),
        _ => return false,
    };
    let candidates = snapshot.find_functions(schema, name);
    // A call leaving parameters out runs their DEFAULT expressions too: a
    // built-in function's are constants; a migration's are known by the
    // functions they run.
    let relies_on_defaults = |p: &crate::pg_catalog::PgProc| {
        p.pronargdefaults > 0
            && f.args.len() < p.proargtypes.len()
            && snapshot.namespace_name(p.pronamespace) != Some("pg_catalog")
            && snapshot.proc_default_procs.get(&p.oid).is_none_or(|procs| {
                procs.iter().any(|d| {
                    snapshot
                        .pg_proc
                        .get(d)
                        .is_none_or(|d| d.provolatile == ProVolatile::Volatile)
                })
            })
    };
    !candidates.is_empty()
        && candidates.iter().all(|p| {
            p.provolatile != ProVolatile::Volatile
                && !relies_on_defaults(p)
                && !(in_subquery
                    && matches!(p.prokind, crate::pg_catalog::ProKind::Aggregate)
                    && snapshot.namespace_name(p.pronamespace) != Some("pg_catalog"))
        })
}

/// A relation a subquery reads that returns the same rows to every scan
/// within the statement: a table (or materialized view), with no row
/// security, no view or foreign table behind it and no descendant that is
/// one. A CTE of the subquery's own is fine (its body is checked with the
/// rest); one of the enclosing query is not looked at, so is refused.
fn relation_is_stable(
    rv: &protobuf::RangeVar,
    own_ctes: &HashSet<&str>,
    scope: &Scope,
    snapshot: &PgCatalog,
) -> bool {
    if rv.schemaname.is_empty() {
        if scope.ctes.contains_key(&rv.relname) {
            return false;
        }
        // A CTE of the subquery's own may be out of scope where the name
        // is read (`WITH v AS …` in a nested query doesn't hide view `v`
        // from the query around it): then it is the relation, which must
        // be stable too — if there is one.
        if own_ctes.contains(rv.relname.as_str()) {
            return match crate::ddl::util::lookup_relation(snapshot, rv) {
                Ok(_) => relation_is_stable(rv, &HashSet::new(), scope, snapshot),
                Err(_) => true,
            };
        }
    }
    let Ok((_, relid)) = crate::ddl::util::lookup_relation(snapshot, rv) else {
        return false;
    };
    let mut todo = vec![relid];
    let mut seen = HashSet::new();
    while let Some(rel) = todo.pop() {
        if !seen.insert(rel) {
            continue;
        }
        let ok = snapshot.pg_class.get(&rel).is_some_and(|c| {
            matches!(
                c.relkind,
                RelKind::Table | RelKind::Partitioned | RelKind::MaterializedView
            )
        }) && !snapshot.row_security.contains(&rel);
        if !ok {
            return false;
        }
        todo.extend(
            snapshot
                .pg_inherits
                .iter()
                .filter(|i| i.inhparent == rel)
                .map(|i| i.inhrelid),
        );
    }
    true
}
