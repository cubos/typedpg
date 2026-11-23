//! Whether the planner can build a FULL JOIN at all.
//!
//! The executor implements a FULL JOIN only as a merge join or a hash
//! join: a nested loop can't emit the inner side's unmatched rows. So
//! `populate_joinrel_with_paths` (joinrels.c) raises `FULL JOIN is only
//! supported with merge-joinable or hash-joinable join conditions` when
//! `add_paths_to_joinrel` built no path for the join:
//!
//! - a hash join (`hash_inner_and_outer`, joinpath.c) needs one join
//!   clause that is hash-joinable (`check_hashjoinable`: a two-argument
//!   `OpExpr` whose operator `op_hashjoinable` accepts, nothing volatile)
//!   and relates the two sides (`can_join`, `clause_sides_match_join`);
//!   any other clause is evaluated as a join filter;
//! - a merge join needs every join clause to be merge-joinable
//!   (`check_mergejoinable`: `op_mergejoinable` and a btree equality
//!   family, `get_mergejoin_opfamilies`) and to relate the sides — but a
//!   constant clause is fine (`select_mergejoin_clauses`: `FULL JOIN ON
//!   false`), and no clause at all (`ON true`) gives a clauseless merge
//!   join (`match_unsorted_outer`).
//!
//! The clauses are the ON clause's top-level AND arguments once
//! `eval_const_expressions` and `canonicalize_qual` preprocessed it. Each
//! check here errs towards *not* raising: a clause the analyzer can't
//! classify for sure — one that may fold to a constant (a parameter's
//! custom plan included), may be inlined (a SQL function), or a node
//! preprocessing may rewrite — counts as one that may be joinable.

use std::collections::HashSet;

use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast;

use crate::expr::plan_fold::{self, Folded};
use crate::nonnull::{ResolvedOp, StrictLog};
use crate::oid::{PgOperatorOid, PgTypeOid};
use crate::pg_catalog::{PgCatalog, TypType, oid};
use crate::scope::Scope;

/// `ARRAY_EQ_OP`, `RECORD_EQ_OP`, `RANGE_EQ_OP`, `MULTIRANGE_EQ_OP`
/// (pg_operator.dat): the operators `op_mergejoinable` /
/// `op_hashjoinable` ask the type cache about.
const ARRAY_EQ_OP: PgOperatorOid = PgOperatorOid::from_raw(1070);
const RECORD_EQ_OP: PgOperatorOid = PgOperatorOid::from_raw(2988);
const RANGE_EQ_OP: PgOperatorOid = PgOperatorOid::from_raw(3882);
const MULTIRANGE_EQ_OP: PgOperatorOid = PgOperatorOid::from_raw(2860);

/// Which side of the FULL JOIN an operand reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    /// No column of the join (constants, parameters, outer references).
    Neither,
    Left,
    Right,
    Both,
    /// Can't be told.
    Unknown,
}

impl Side {
    fn and(self, other: Side) -> Side {
        match (self, other) {
            (Side::Unknown, _) | (_, Side::Unknown) => Side::Unknown,
            (Side::Neither, s) | (s, Side::Neither) => s,
            (a, b) if a == b => a,
            _ => Side::Both,
        }
    }
}

/// What a join clause allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Clause {
    /// Hash-joinable, relating the sides: a hash join can be built.
    Hash,
    /// Merge-joinable, relating the sides: `left_on_left` when its left
    /// operand reads the join's left side; `commutes` when its operator
    /// has a commutator (needed when the left operand reads the inner
    /// side, `select_mergejoin_clauses`).
    Merge { left_on_left: bool, commutes: bool },
    /// Neither (`have_nonmergeable_joinclause`).
    Neither,
    /// May be either.
    Unknown,
}

/// Whether the planner fails every plan of a FULL JOIN with ON clause
/// `quals` between the FROM entries `left` and `right` (their aliases),
/// typed in `ctx` (whose strictness log recorded the operators). `None`
/// quals is `ON true`.
pub(crate) fn on_clause_unplannable(
    quals: Option<&ast::Node>,
    ctx: crate::expr::Ctx<'_>,
    log: &StrictLog,
    left: &HashSet<String>,
    right: &HashSet<String>,
) -> bool {
    let Some(q) = quals else {
        return false;
    };
    // A qual folding to a constant leaves one constant clause, or none.
    // So may one over a parameter: a custom plan replaces it by its
    // value, and a strict expression over a NULL one is the NULL
    // constant — FALSE for the whole qual (`canonicalize_qual`).
    if plan_fold::fold_qual_in(Some(q), ctx, false) != Folded::Var || has_param(q) {
        return false;
    }
    let mut conjuncts = Vec::new();
    flatten(q, false, &mut conjuncts);
    let mut clauses = Vec::new();
    for (c, negated) in conjuncts {
        match plan_fold::fold_qual_in(Some(c), ctx, negated) {
            // `canonicalize_qual`: an AND with a FALSE argument is FALSE.
            Folded::Bool(false) | Folded::Null => return false,
            Folded::Bool(true) => continue,
            Folded::Const => clauses.push(Clause::Unknown),
            Folded::Var => clauses.push(match classify(c, negated, ctx, log, left, right) {
                // PostgreSQL 19 folds what a non-nullable input decides
                // (`expr_is_nonnullable`): `x IS NULL`, `NULL IS DISTINCT
                // FROM x`, a COALESCE's later arguments — which may leave
                // a joinable clause the analyzer's folding doesn't see.
                Clause::Neither if folds_on_nonnull(c) => Clause::Unknown,
                clause => clause,
            }),
        }
    }
    clauses_unplannable(&clauses)
}

/// Whether a FULL JOIN `USING` / `NATURAL` over the columns whose
/// equalities resolved to `ops` (each `l = r`, `l` the left side's
/// column; `None` when it can't be told) can't be planned.
pub(crate) fn using_unplannable(ops: &[Option<ResolvedOp>], snapshot: &PgCatalog) -> bool {
    let clauses: Vec<Clause> = ops
        .iter()
        .map(|op| match op {
            Some(op) => operator_clause(*op, true, snapshot),
            None => Clause::Unknown,
        })
        .collect();
    clauses_unplannable(&clauses)
}

/// No hash join (no hash-joinable clause) and no merge join in either
/// input order (some clause not merge-joinable that way).
fn clauses_unplannable(clauses: &[Clause]) -> bool {
    if clauses
        .iter()
        .any(|c| matches!(c, Clause::Hash | Clause::Unknown))
    {
        return false;
    }
    // `add_paths_to_joinrel` runs with each side as the outer one.
    let merge_ok = |left_outer: bool| {
        clauses.iter().all(|c| match *c {
            Clause::Merge {
                left_on_left,
                commutes,
            } => left_on_left == left_outer || commutes,
            _ => false,
        })
    };
    !merge_ok(true) && !merge_ok(false)
}

/// The top-level AND arguments of `n`, each with whether it is negated:
/// `eval_const_expressions` pushes a NOT down an OR as an AND of NOTs
/// (`negate_clause`) and drops a double NOT, and the nested ANDs are
/// flattened.
fn flatten<'a>(n: &'a ast::Node, negated: bool, out: &mut Vec<(&'a ast::Node, bool)>) {
    if let ast::Node::BoolExpr(b) = n {
        match b.boolop {
            ast::BoolExprType::NotExpr => {
                if let [Some(arg)] = b.args.as_slice() {
                    return flatten(arg, !negated, out);
                }
            }
            op if (op == ast::BoolExprType::AndExpr) != negated => {
                for a in b.args.iter().flatten() {
                    flatten(a, negated, out);
                }
                return;
            }
            _ => {}
        }
    }
    out.push((n, negated));
}

/// Whether `c` holds, below itself, a node PostgreSQL 19 folds when an
/// input is non-nullable.
fn folds_on_nonnull(c: &ast::Node) -> bool {
    c.nodes().skip(1).any(|(n, _)| match n {
        NodeRef::NullTest(_) | NodeRef::BooleanTest(_) | NodeRef::CoalesceExpr(_) => true,
        NodeRef::AExpr(e) => matches!(
            e.kind,
            ast::AExprKind::AexprDistinct | ast::AExprKind::AexprNotDistinct
        ),
        _ => false,
    })
}

fn has_param(n: &ast::Node) -> bool {
    n.nodes().any(|(n, _)| matches!(n, NodeRef::ParamRef(_)))
}

/// What join clause `c` (negated when `negated`, already known not to
/// fold to a constant) allows.
fn classify(
    c: &ast::Node,
    negated: bool,
    ctx: crate::expr::Ctx<'_>,
    log: &StrictLog,
    left: &HashSet<String>,
    right: &HashSet<String>,
) -> Clause {
    let snapshot = ctx.snapshot;
    match c {
        ast::Node::AExpr(e) => match e.kind {
            ast::AExprKind::AexprOp => {
                let (Some(l), Some(r)) = (e.lexpr.as_ref(), e.rexpr.as_ref()) else {
                    // A prefix operator's OpExpr has one argument.
                    return match log.operator_at(e.location) {
                        Some(op) if !inlinable_operator(op.oid, snapshot) => Clause::Neither,
                        _ => Clause::Unknown,
                    };
                };
                // A row comparison expands to several (`make_row_comparison_op`).
                if matches!(l, ast::Node::RowExpr(_)) || matches!(r, ast::Node::RowExpr(_)) {
                    return Clause::Unknown;
                }
                let Some(mut op) = log.operator_at(e.location) else {
                    return Clause::Unknown;
                };
                if negated {
                    // `negate_clause`: `NOT (a op b)` is `a negator b`
                    // when the operator has one.
                    match snapshot.pg_operator.get(&op.oid).and_then(|o| o.oprnegate) {
                        Some(neg) => op.oid = neg,
                        None if inlinable_operator(op.oid, snapshot) => return Clause::Unknown,
                        None => return Clause::Neither,
                    }
                }
                if inlinable_operator(op.oid, snapshot)
                    || plan_fold::may_be_volatile(Some(c), snapshot)
                {
                    return Clause::Unknown;
                }
                let (ls, rs) = (
                    side_of(l, ctx.scope, left, right),
                    side_of(r, ctx.scope, left, right),
                );
                match (ls, rs) {
                    (Side::Unknown, _) | (_, Side::Unknown) => Clause::Unknown,
                    // `can_join` and `clause_sides_match_join`.
                    (Side::Left, Side::Right) => operator_clause(op, true, snapshot),
                    (Side::Right, Side::Left) => operator_clause(op, false, snapshot),
                    _ => Clause::Neither,
                }
            }
            // `ScalarArrayOpExpr`: never an `OpExpr`.
            ast::AExprKind::AexprOpAny | ast::AExprKind::AexprOpAll => Clause::Neither,
            // A `DistinctExpr`, which PostgreSQL 19 turns into `<>` / `=`
            // only when no input may be NULL (`expr_is_nonnullable`): a
            // column without NOT NULL may be.
            ast::AExprKind::AexprDistinct | ast::AExprKind::AexprNotDistinct => {
                let nullable_column = |n: Option<&ast::Node>| match n {
                    Some(ast::Node::ColumnRef(cr)) => {
                        let names = crate::expr::extract_string_fields(&cr.fields);
                        let found = match names.as_slice() {
                            [c] if names.len() == cr.fields.len() => {
                                ctx.scope.resolve_column(None, c, None).ok()
                            }
                            [t, c] if names.len() == cr.fields.len() => {
                                ctx.scope.resolve_column(Some(t), c, None).ok()
                            }
                            _ => None,
                        };
                        found.is_some_and(|c| !c.base_not_null)
                    }
                    _ => false,
                };
                if nullable_column(e.lexpr.as_ref()) || nullable_column(e.rexpr.as_ref()) {
                    Clause::Neither
                } else {
                    Clause::Unknown
                }
            }
            // IN, BETWEEN, LIKE take other paths.
            _ => Clause::Unknown,
        },
        // A NOT the planner keeps (`negate_clause` turns a NullTest or a
        // BooleanTest into the opposite test, keeping its kind).
        ast::Node::NullTest(_)
        | ast::Node::BooleanTest(_)
        | ast::Node::ColumnRef(_)
        | ast::Node::SubLink(_) => Clause::Neither,
        // A CASE stays one when a WHEN left after the constant ones are
        // dropped is not constant (`eval_const_expressions`).
        ast::Node::CaseExpr(case) => {
            let test = case
                .arg
                .as_ref()
                .map(|a| plan_fold::fold(Some(a), snapshot));
            for w in &case.args {
                let Some(ast::Node::CaseWhen(w)) = w.as_ref() else {
                    return Clause::Unknown;
                };
                match plan_fold::case_when(test, w.expr.as_ref(), snapshot) {
                    Folded::Bool(false) | Folded::Null => {}
                    Folded::Var => return Clause::Neither,
                    Folded::Bool(true) | Folded::Const => return Clause::Unknown,
                }
            }
            Clause::Unknown
        }
        // `x OR y`, unless `eval_const_expressions` drops all its arms but
        // one (FALSE ones), or `find_duplicate_ors` pulls a common AND
        // argument out of them (or the arms are one).
        ast::Node::BoolExpr(b) if b.boolop != ast::BoolExprType::NotExpr && !negated => {
            let mut seen = HashSet::new();
            let plain = b.args.iter().all(|a| {
                !matches!(a, Some(ast::Node::BoolExpr(_)))
                    && plan_fold::fold_qual_in(a.as_ref(), ctx, false) == Folded::Var
                    && seen.insert(crate::resolve::node_fingerprint(a.as_ref()))
            });
            if plain {
                Clause::Neither
            } else {
                Clause::Unknown
            }
        }
        // A `FuncExpr`, unless `inline_function` replaces a SQL function
        // by its body.
        ast::Node::FuncCall(fc) => {
            let names = crate::expr::extract_string_fields(&fc.funcname);
            let (schema, name) = match names.as_slice() {
                [n] => (None, n.as_str()),
                [s, n] => (Some(s.as_str()), n.as_str()),
                _ => return Clause::Unknown,
            };
            let candidates = snapshot.find_functions(schema, name);
            if !candidates.is_empty()
                && candidates
                    .iter()
                    .all(|p| p.prolang != crate::pg_catalog::SQL_LANGUAGE)
            {
                Clause::Neither
            } else {
                Clause::Unknown
            }
        }
        _ => Clause::Unknown,
    }
}

/// What an `OpExpr` of operator `op` relating the two sides allows
/// (`left_on_left`: its left operand reads the join's left side).
fn operator_clause(op: ResolvedOp, left_on_left: bool, snapshot: &PgCatalog) -> Clause {
    if inlinable_operator(op.oid, snapshot) {
        return Clause::Unknown;
    }
    if op_hashjoinable(op, snapshot) {
        return Clause::Hash;
    }
    if op_mergejoinable(op, snapshot) && has_mergejoin_opfamily(op.oid, snapshot) {
        let commutes = snapshot
            .pg_operator
            .get(&op.oid)
            .is_some_and(|o| o.oprcom.is_some());
        return Clause::Merge {
            left_on_left,
            commutes,
        };
    }
    Clause::Neither
}

/// An operator whose function is a SQL function: `simplify_function` may
/// inline it, the clause becoming whatever its body is.
fn inlinable_operator(op: PgOperatorOid, snapshot: &PgCatalog) -> bool {
    snapshot
        .pg_operator
        .get(&op)
        .and_then(|o| o.oprcode)
        .and_then(|f| snapshot.pg_proc.get(&f))
        .is_none_or(|p| p.prolang == crate::pg_catalog::SQL_LANGUAGE)
}

/// `op_hashjoinable`: `oprcanhash`, or for the array, record, range and
/// multirange equalities whether the type cache finds a hash function for
/// the input type (`hash_array` / `hash_record` / `hash_range` /
/// `hash_multirange`, which need their element's).
fn op_hashjoinable(op: ResolvedOp, snapshot: &PgCatalog) -> bool {
    match op.oid {
        ARRAY_EQ_OP | RECORD_EQ_OP | RANGE_EQ_OP | MULTIRANGE_EQ_OP => {
            type_hashable(snapshot, op.input_type, 0)
        }
        _ => snapshot
            .pg_operator
            .get(&op.oid)
            .is_some_and(|o| o.oprcanhash),
    }
}

/// `op_mergejoinable`: `oprcanmerge`, or for the array and record
/// equalities whether the type cache finds a comparison function for the
/// input type (`btarraycmp` / `btrecordcmp`, which need their element's).
fn op_mergejoinable(op: ResolvedOp, snapshot: &PgCatalog) -> bool {
    match op.oid {
        ARRAY_EQ_OP | RECORD_EQ_OP => crate::clause::has_ordering_operator(snapshot, op.input_type),
        _ => snapshot
            .pg_operator
            .get(&op.oid)
            .is_some_and(|o| o.oprcanmerge),
    }
}

/// `get_mergejoin_opfamilies` finds some: the operator is the equality
/// of an ordering index's operator family.
fn has_mergejoin_opfamily(op: PgOperatorOid, snapshot: &PgCatalog) -> bool {
    snapshot
        .pg_amop
        .iter()
        .any(|a| a.amopopr == op && a.amopmethod == "btree" && a.amopstrategy == 3)
}

/// Whether the type cache finds a hash function for `t`
/// (`TYPECACHE_HASH_PROC`): its default hash operator class's — for an
/// array, a composite, a range or a multirange only when its element, every
/// field or its subtype has one (`cache_array_element_properties`,
/// `cache_record_field_properties`, `cache_range_element_properties`).
/// `record` itself is assumed not hashable.
fn type_hashable(snapshot: &PgCatalog, t: PgTypeOid, depth: u32) -> bool {
    let t = snapshot.unwrap_domain(t);
    if depth > 32 || t == oid::RECORD {
        return false;
    }
    if let Some(elem) = crate::coerce::element_type(t, snapshot) {
        return type_hashable(snapshot, elem, depth + 1);
    }
    let Some(ty) = snapshot.get_type(t) else {
        return false;
    };
    match ty.typtype {
        TypType::Composite => ty.typrelid.is_some_and(|relid| {
            snapshot
                .attributes_of(relid)
                .iter()
                .filter(|a| a.attnum > 0)
                .all(|a| type_hashable(snapshot, a.atttypid, depth + 1))
        }),
        TypType::Range => snapshot
            .range_subtype(t)
            .is_some_and(|s| type_hashable(snapshot, s, depth + 1)),
        TypType::Multirange => snapshot
            .range_of_multirange(t)
            .is_some_and(|r| type_hashable(snapshot, r, depth + 1)),
        _ => crate::ddl::opclass::default_opclass_intype(snapshot, t, "hash").is_some(),
    }
}

/// Which side of the join `n` reads (`pull_varnos`): every column
/// reference resolved against the join's two sides — a name neither side
/// answers to is an outer reference, a parameter to this level.
fn side_of(n: &ast::Node, scope: &Scope, left: &HashSet<String>, right: &HashSet<String>) -> Side {
    let mut side = Side::Neither;
    for (node, _) in n.nodes() {
        match node {
            // A sublink's vars are its SubPlan's arguments.
            NodeRef::SubLink(_) | NodeRef::SelectStmt(_) => return Side::Unknown,
            NodeRef::ColumnRef(cr) => side = side.and(column_side(cr, scope, left, right)),
            _ => {}
        }
        if side == Side::Unknown {
            break;
        }
    }
    side
}

fn column_side(
    cr: &ast::ColumnRef,
    scope: &Scope,
    left: &HashSet<String>,
    right: &HashSet<String>,
) -> Side {
    let names = crate::expr::extract_string_fields(&cr.fields);
    let star = cr
        .fields
        .last()
        .is_some_and(|f| matches!(f, Some(ast::Node::AStar(_))));
    let side_of_alias = |a: &str| {
        if left.contains(a) {
            Side::Left
        } else if right.contains(a) {
            Side::Right
        } else {
            Side::Unknown
        }
    };
    let entries = || scope.sources.iter();
    let qualified = |t: &str| {
        let mut found = entries().filter(|s| s.answers_to(t)).peekable();
        if found.peek().is_none() {
            // An outer level's entry.
            return Side::Neither;
        }
        found.fold(Side::Neither, |acc, s| acc.and(side_of_alias(&s.alias)))
    };
    match (names.as_slice(), star) {
        ([t], true) => qualified(t),
        ([c], false) => {
            let mut found = entries()
                .filter(|s| s.visible_columns().any(|col| col.name == *c))
                .peekable();
            if found.peek().is_some() {
                found.fold(Side::Neither, |acc, s| acc.and(side_of_alias(&s.alias)))
            } else if entries().any(|s| s.answers_to(c)) {
                // A whole-row reference.
                qualified(c)
            } else {
                // An outer level's column, or a system column.
                Side::Unknown
            }
        }
        ([t, _], false) => qualified(t),
        _ => Side::Unknown,
    }
}

/// A location in join `j` (its ON clause's, or a FROM item's): where a
/// planner error about it is placed among the statement's (see
/// `deferred::split`).
pub(crate) fn join_location(j: &ast::JoinExpr) -> Option<i32> {
    [j.quals.as_ref(), j.larg.as_ref(), j.rarg.as_ref()]
        .into_iter()
        .flatten()
        .find_map(|n| match n {
            ast::Node::JoinExpr(inner) => join_location(inner),
            n => n.nodes().find_map(|(r, _)| match r {
                NodeRef::RangeVar(rv) if rv.location >= 0 => Some(rv.location),
                NodeRef::ColumnRef(c) if c.location >= 0 => Some(c.location),
                NodeRef::AExpr(e) if e.location >= 0 => Some(e.location),
                _ => None,
            }),
        })
}

/// Whether the planner may prove FROM item `n` empty (`is_dummy_rel`),
/// beside what the level's own quals prove (`plan_fold::level_may_exclude`):
/// a partitioned table (with no partition left), a subquery or view whose
/// own qual may fold to a constant or that has a LIMIT, or a join one of
/// whose sides may be (errs towards yes).
pub(crate) fn item_may_be_dummy(n: Option<&ast::Node>, snapshot: &PgCatalog) -> bool {
    fn query_may_be_empty(sel: &ast::SelectStmt, snapshot: &PgCatalog) -> bool {
        if sel.op != ast::SetOperation::SetopNone {
            return true;
        }
        sel.limit_count.is_some()
            || sel.where_clause.as_ref().is_some_and(|w| {
                plan_fold::fold_qual(Some(w), snapshot) != Folded::Var || has_param(w)
            })
            || sel
                .from_clause
                .iter()
                .any(|f| item_may_be_dummy(f.as_ref(), snapshot))
    }
    match n {
        Some(ast::Node::RangeVar(rv)) => {
            let schema = (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str());
            match snapshot.resolve_table(schema, &rv.relname) {
                Some(class) => match class.relkind {
                    crate::pg_catalog::RelKind::Partitioned => true,
                    crate::pg_catalog::RelKind::View => {
                        match crate::ddl::views::view_query(snapshot, class.oid) {
                            Some(ast::Node::SelectStmt(s)) => {
                                crate::ddl::views::expanding_view(class.oid, || {
                                    query_may_be_empty(&s, snapshot)
                                })
                                .unwrap_or(true)
                            }
                            _ => true,
                        }
                    }
                    _ => false,
                },
                // A CTE.
                None => true,
            }
        }
        Some(ast::Node::RangeSubselect(rs)) => match rs.subquery.as_ref() {
            Some(ast::Node::SelectStmt(s)) => query_may_be_empty(s, snapshot),
            _ => true,
        },
        Some(ast::Node::RangeTableSample(ts)) => item_may_be_dummy(ts.relation.as_ref(), snapshot),
        Some(ast::Node::JoinExpr(j)) => {
            item_may_be_dummy(j.larg.as_ref(), snapshot)
                || item_may_be_dummy(j.rarg.as_ref(), snapshot)
                || j.quals.as_ref().is_some_and(|q| {
                    plan_fold::fold_qual(Some(q), snapshot) != Folded::Var || has_param(q)
                })
        }
        // Functions, VALUES, JSON_TABLE, XMLTABLE.
        Some(_) => false,
        None => true,
    }
}
