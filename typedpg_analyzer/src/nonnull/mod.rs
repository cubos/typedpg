//! What a condition proves NOT NULL: PG's `find_nonnullable_vars` /
//! `find_nonnullable_rels` (optimizer/util/clauses.c) over the raw AST.
//!
//! A qual that must be TRUE for a row to go on (WHERE, HAVING, an inner
//! join's ON, an aggregate's FILTER, a CASE branch's WHEN) proves every
//! column it is *strict* in non-NULL: had the column been NULL, the qual
//! would have been NULL — or, at the top level, at least not TRUE. The
//! callers turn these facts into narrower nullability ([`Facts`] on the
//! [`crate::nullability::NullabilityContext`]) and into PG's outer-join
//! reduction (`reduce_outer_joins`: a LEFT JOIN whose nullable side a
//! WHERE qual is strict in is an inner join).
//!
//! Strictness comes from the operators and functions the qual resolved to
//! (`pg_proc.proisstrict`, and the casts applied to their arguments): the
//! expression walk records it per node in a [`StrictLog`] while it types
//! the qual, since the raw AST doesn't say which function an operator is.
//! A node the log has no entry for proves nothing.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};

pub(crate) mod checks;
pub(crate) mod exprs;
pub(crate) mod subst;

use typedpg_pg_query::protobuf::{self, node};

use crate::pg_catalog::PgCatalog;
use crate::scope::Scope;

thread_local! {
    /// Set while a view's columns are read for the rows a DML statement
    /// writes through it (see [`without_narrowing`]).
    static DISABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `f` with no qual proving anything: what a view's columns are over
/// any row of its base relation, not just those its WHERE passes.
pub(crate) fn without_narrowing<R>(f: impl FnOnce() -> R) -> R {
    let before = DISABLED.with(|d| d.replace(true));
    let out = f();
    DISABLED.with(|d| d.set(before));
    out
}

pub(crate) fn disabled() -> bool {
    DISABLED.with(|d| d.get())
}

thread_local! {
    /// The statement being analyzed locks rows (`FOR UPDATE` / `SHARE`).
    static ROW_LOCKING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `f` noting whether the statement locks rows. Under READ COMMITTED a
/// locked row that was updated concurrently is re-fetched and re-joined
/// with the rows already read (EvalPlanQual): its WHERE is checked again,
/// but a foreign key's cross-row promise isn't — its new key may reference
/// a parent the statement's snapshot doesn't see.
pub(crate) fn with_row_locking<R>(locks: bool, f: impl FnOnce() -> R) -> R {
    let before = ROW_LOCKING.with(|d| d.replace(locks));
    let out = f();
    ROW_LOCKING.with(|d| d.set(before));
    out
}

/// Whether the statement being analyzed locks rows.
pub(crate) fn row_locking() -> bool {
    ROW_LOCKING.with(|d| d.get())
}

/// Which kind of node a [`StrictLog`] entry is about (entries are keyed by
/// the node's location; the kind keeps nodes of different kinds apart).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum StrictNode {
    /// An `A_Expr`: every operator it resolved to (BETWEEN and IN resolve
    /// several, all recorded at the A_Expr's location; a simple CASE's
    /// `test = value` is recorded at its WHEN's).
    Op,
    /// A plain function call (not an aggregate, a window call or a
    /// variadic call whose arguments were packed into an array).
    Func,
    /// An explicit cast.
    Cast,
    /// `x op ANY (SELECT …)` with strict comparisons.
    Sublink,
    /// An `A_Expr` comparison (`=`, `<>`, `<`, …) [`subst`] may compute
    /// over constants: a built-in operator over integer, numeric, float or
    /// boolean operands. Recorded as "strict" when every operator at the
    /// location is one.
    StdCompare,
    /// `StdCompare` over text operands (equality only, deterministic
    /// collation).
    TextEquality,
    /// `l IS [NOT] DISTINCT FROM r` whose left / right operand is non-NULL
    /// where the comparison is read.
    DistinctLeftNonNull,
    DistinctRightNonNull,
}

/// Per-node strictness recorded while a qual is typed, plus what the
/// `EXISTS` sublinks in it prove of the enclosing level's columns (keyed
/// by the sublink's location; see [`capture_correlation`]).
#[derive(Debug, Default)]
pub(crate) struct StrictLog(
    RefCell<HashMap<(i32, StrictNode), bool>>,
    RefCell<HashMap<i32, Facts>>,
);

impl StrictLog {
    /// Record that the node of `kind` at `location` runs strict code
    /// (`strict`): a NULL input gives a NULL output. Several records for
    /// one node (BETWEEN's two comparisons) must all be strict.
    pub fn note(&self, location: i32, kind: StrictNode, strict: bool) {
        if location < 0 {
            return;
        }
        self.0
            .borrow_mut()
            .entry((location, kind))
            .and_modify(|s| *s &= strict)
            .or_insert(strict);
    }

    pub fn is_strict(&self, location: i32, kind: StrictNode) -> bool {
        location >= 0 && self.0.borrow().get(&(location, kind)) == Some(&true)
    }

    /// Record what the `EXISTS` sublink at `location` being TRUE proves of
    /// the enclosing level (see [`capture_correlation`]).
    pub fn note_correlated(&self, location: i32, facts: Facts) {
        if location >= 0 {
            self.1.borrow_mut().insert(location, facts);
        }
    }

    fn correlated(&self, location: i32) -> Facts {
        self.1.borrow().get(&location).cloned().unwrap_or_default()
    }
}

thread_local! {
    /// The sublink / LATERAL subquery whose WHERE facts about the
    /// enclosing level are being captured: the `SelectStmt`'s address, and
    /// what it deposited.
    static CORRELATION: RefCell<Option<(usize, Option<Facts>)>> = const { RefCell::new(None) };
}

/// Run `f` (the analysis of subquery `sel`), capturing what `sel`'s WHERE
/// proves of the columns of the levels around it — facts that hold
/// whenever `sel` returns a row: PG's `convert_EXISTS_sublink_to_join`
/// pulls an `EXISTS` up into a semi-join whose quals then count like an
/// inner join's. Only `sel` itself deposits (by address): not the queries
/// nested in it, nor a set operation's arms.
pub(crate) fn capture_correlation<R>(
    sel: &protobuf::SelectStmt,
    f: impl FnOnce() -> R,
) -> (R, Facts) {
    let key = sel as *const protobuf::SelectStmt as usize;
    let before = CORRELATION.with(|c| c.replace(Some((key, None))));
    let out = f();
    let got = CORRELATION.with(|c| c.replace(before));
    let facts = got.and_then(|(_, f)| f).unwrap_or_default();
    (out, facts)
}

/// Deposit what `sel`'s WHERE proves of the enclosing levels, if `sel` is
/// the subquery being captured (see [`capture_correlation`]). Expression
/// facts stay behind: they may speak of `sel`'s own columns.
pub(crate) fn deposit_correlation(sel: &protobuf::SelectStmt, mut facts: Facts) {
    let key = sel as *const protobuf::SelectStmt as usize;
    facts.exprs.clear();
    CORRELATION.with(|c| {
        if let Some((k, slot)) = c.borrow_mut().as_mut()
            && *k == key
        {
            *slot = Some(facts);
        }
    });
}

/// Whether some query is capturing `sel`'s correlation facts.
pub(crate) fn correlation_wanted(sel: &protobuf::SelectStmt) -> bool {
    let key = sel as *const protobuf::SelectStmt as usize;
    CORRELATION.with(|c| c.borrow().as_ref().is_some_and(|(k, _)| *k == key))
}

/// A column of a FROM entry: `(alias, column)`.
pub(crate) type Col = (String, String);

/// How a constant was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum LitKind {
    /// An integer (`1`).
    Integer,
    /// A string (`'1'`), cast or not.
    String,
    /// `true` / `false` (or a bare boolean column, which is `col = true`).
    Boolean,
}

/// A constant a column is compared with: its text and how it was written.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Literal {
    pub text: String,
    pub kind: LitKind,
}

impl Literal {
    pub fn boolean(value: bool) -> Literal {
        Literal {
            text: value.to_string(),
            kind: LitKind::Boolean,
        }
    }

    pub fn is_integer(&self) -> bool {
        self.kind == LitKind::Integer
    }
}

/// An ordering comparison of a column with a constant (`c < v`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum CmpOp {
    Lt,
    Le,
    Gt,
    Ge,
}

impl CmpOp {
    fn parse(op: &str) -> Option<CmpOp> {
        match op {
            "<" => Some(CmpOp::Lt),
            "<=" => Some(CmpOp::Le),
            ">" => Some(CmpOp::Gt),
            ">=" => Some(CmpOp::Ge),
            _ => None,
        }
    }

    /// `v op c` as `c op' v`.
    fn flipped(self) -> CmpOp {
        match self {
            CmpOp::Lt => CmpOp::Gt,
            CmpOp::Le => CmpOp::Ge,
            CmpOp::Gt => CmpOp::Lt,
            CmpOp::Ge => CmpOp::Le,
        }
    }

    /// `NOT (c op v)` for a non-NULL `c`, under a total order.
    pub fn negated(self) -> CmpOp {
        match self {
            CmpOp::Lt => CmpOp::Ge,
            CmpOp::Le => CmpOp::Gt,
            CmpOp::Gt => CmpOp::Le,
            CmpOp::Ge => CmpOp::Lt,
        }
    }
}

/// What holds of a column's value *when it is non-NULL*: what a qual that
/// is TRUE, or a qual that is not TRUE, says of it (`kind = 'a'` not TRUE:
/// a non-NULL `kind` isn't `'a'`). As a literal of a CHECK constraint, what
/// the constraint not being FALSE says (`kind = 'a'` is NULL for a NULL
/// `kind`). The constants are never NULL.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum ValPred {
    /// One of these (`c = v`, `c IN (…)`, `c = ANY (…)`).
    In(Vec<Literal>),
    /// None of these (`c <> v`, `c NOT IN (…)`, `c <> ALL (…)`).
    NotIn(Vec<Literal>),
    /// `c op v`.
    Cmp(CmpOp, Literal),
}

impl ValPred {
    /// What the comparison not being TRUE says of a non-NULL column.
    pub fn negated(&self) -> ValPred {
        match self {
            ValPred::In(vs) => ValPred::NotIn(vs.clone()),
            ValPred::NotIn(vs) => ValPred::In(vs.clone()),
            ValPred::Cmp(op, v) => ValPred::Cmp(op.negated(), v.clone()),
        }
    }
}

/// What a condition proves:
///
/// - `columns` / `rels`: columns non-NULL and FROM entries not
///   null-extended (a column proven non-NULL also proves its entry);
/// - `nulls`: columns proven NULL;
/// - `disjunctions`: sets of columns at least one of which is non-NULL
///   (`a IS NOT NULL OR b IS NOT NULL`, `num_nonnulls(a, b) > 0`);
/// - `equals`: columns proven equal to a constant (`kind = 'a'`);
/// - `preds`: what holds of a column's value if it is non-NULL
///   ([`ValPred`]: `kind <> 'b'`, `kind IN ('a', 'b')`, `lvl >= 10`, or a
///   CASE branch's `kind = 'a'` not being TRUE).
/// - `exprs`: expressions other than a column proven non-NULL, by
///   [`exprs::key`] (`j ->> 'k' IS NOT NULL`, `max(b) > 0` in HAVING) —
///   only ones that give the same value wherever they are evaluated for
///   a row (see [`exprs::reusable`]).
///
/// The last four feed the reasoning with CHECK constraints and
/// COALESCE / GREATEST / LEAST (see [`checks`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Facts {
    pub columns: HashSet<Col>,
    pub rels: HashSet<String>,
    pub nulls: HashSet<Col>,
    pub disjunctions: Vec<BTreeSet<Col>>,
    pub equals: HashMap<Col, Literal>,
    pub preds: Vec<(Col, ValPred)>,
    pub exprs: HashSet<String>,
}

impl Facts {
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
            && self.rels.is_empty()
            && self.nulls.is_empty()
            && self.disjunctions.is_empty()
            && self.equals.is_empty()
            && self.preds.is_empty()
            && self.exprs.is_empty()
    }

    fn pred(col: Col, pred: ValPred) -> Facts {
        let mut f = Facts::default();
        f.preds.push((col, pred));
        f
    }

    pub fn column(alias: &str, column: &str) -> Facts {
        let mut f = Facts::default();
        f.columns.insert((alias.to_owned(), column.to_owned()));
        f.rels.insert(alias.to_owned());
        f
    }

    pub fn rel(alias: &str) -> Facts {
        let mut f = Facts::default();
        f.rels.insert(alias.to_owned());
        f
    }

    fn null(col: Col) -> Facts {
        let mut f = Facts::default();
        f.nulls.insert(col);
        f
    }

    fn disjunction(cols: BTreeSet<Col>) -> Facts {
        let mut f = Facts::default();
        match cols.len() {
            0 => {}
            1 => {
                let (a, c) = cols.into_iter().next().expect("one column");
                return Facts::column(&a, &c);
            }
            _ => f.disjunctions.push(cols),
        }
        f
    }

    /// Both hold.
    pub fn union(mut self, other: Facts) -> Facts {
        self.columns.extend(other.columns);
        self.rels.extend(other.rels);
        self.nulls.extend(other.nulls);
        for d in other.disjunctions {
            if !self.disjunctions.contains(&d) {
                self.disjunctions.push(d);
            }
        }
        for (c, v) in other.equals {
            self.equals.entry(c).or_insert(v);
        }
        for p in other.preds {
            if !self.preds.contains(&p) {
                self.preds.push(p);
            }
        }
        self.exprs.extend(other.exprs);
        self
    }

    /// The constants a column is known to be one of (if non-NULL).
    fn one_of(&self, c: &Col) -> Option<Vec<Literal>> {
        if let Some(v) = self.equals.get(c) {
            return Some(vec![v.clone()]);
        }
        self.preds.iter().find_map(|(pc, p)| match p {
            ValPred::In(vs) if pc == c => Some(vs.clone()),
            _ => None,
        })
    }

    /// One of the two holds: what both prove, plus "at least one of" the
    /// columns each proves (a witness per side).
    fn intersect(self, other: &Facts) -> Facts {
        let witness = |f: &Facts| -> Option<BTreeSet<Col>> {
            if !f.columns.is_empty() {
                Some(f.columns.iter().cloned().collect())
            } else {
                f.disjunctions.iter().min_by_key(|d| d.len()).cloned()
            }
        };
        let either = match (witness(&self), witness(other)) {
            (Some(a), Some(b)) => Some(a.into_iter().chain(b).collect::<BTreeSet<Col>>()),
            _ => None,
        };
        let mut out = Facts {
            columns: self
                .columns
                .iter()
                .filter(|c| other.columns.contains(*c))
                .cloned()
                .collect(),
            rels: self
                .rels
                .iter()
                .filter(|r| other.rels.contains(*r))
                .cloned()
                .collect(),
            nulls: self
                .nulls
                .iter()
                .filter(|c| other.nulls.contains(*c))
                .cloned()
                .collect(),
            disjunctions: self
                .disjunctions
                .iter()
                .filter(|d| other.disjunctions.contains(d))
                .cloned()
                .collect(),
            equals: self
                .equals
                .iter()
                .filter(|(c, v)| other.equals.get(*c) == Some(*v))
                .map(|(c, v)| (c.clone(), v.clone()))
                .collect(),
            preds: self
                .preds
                .iter()
                .filter(|p| other.preds.contains(p))
                .cloned()
                .collect(),
            exprs: self
                .exprs
                .iter()
                .filter(|e| other.exprs.contains(*e))
                .cloned()
                .collect(),
        };
        // `kind = 'a' OR kind = 'b'`: one of the constants either side
        // allows.
        let mut cols: Vec<&Col> = self
            .equals
            .keys()
            .chain(self.preds.iter().map(|(c, _)| c))
            .collect();
        cols.sort();
        cols.dedup();
        for c in cols {
            if let (Some(mut a), Some(b)) = (self.one_of(c), other.one_of(c)) {
                for v in b {
                    if !a.contains(&v) {
                        a.push(v);
                    }
                }
                let p = (c.clone(), ValPred::In(a));
                if !out.preds.contains(&p) {
                    out.preds.push(p);
                }
            }
        }
        if let Some(d) = either
            && !d.iter().any(|c| out.columns.contains(c))
            && !out.disjunctions.contains(&d)
            && d.len() > 1
        {
            out.disjunctions.push(d);
        }
        out
    }

    /// Only the facts about the FROM entries named in `aliases`.
    pub fn restricted_to(mut self, aliases: &HashSet<String>) -> Facts {
        self.columns.retain(|(a, _)| aliases.contains(a));
        self.rels.retain(|a| aliases.contains(a));
        self.nulls.retain(|(a, _)| aliases.contains(a));
        self.disjunctions
            .retain(|d| d.iter().all(|(a, _)| aliases.contains(a)));
        self.equals.retain(|(a, _), _| aliases.contains(a));
        self.preds.retain(|((a, _), _)| aliases.contains(a));
        self
    }
}

/// The aliases of `scope`'s own FROM entries (not the LATERAL / outer
/// tiers): the entries a fact of this query level may speak about.
pub(crate) fn own_aliases(scope: &Scope) -> HashSet<String> {
    scope.sources.iter().map(|s| s.alias.clone()).collect()
}

/// Intersection of the facts of every node (`None` when there is none).
fn intersection<'a>(
    nodes: impl IntoIterator<Item = &'a protobuf::Node>,
    mut f: impl FnMut(&'a protobuf::Node) -> Facts,
) -> Facts {
    let mut acc: Option<Facts> = None;
    for n in nodes {
        let facts = f(n);
        acc = Some(match acc {
            None => facts,
            Some(a) => a.intersect(&facts),
        });
    }
    acc.unwrap_or_default()
}

/// PG's `find_nonnullable_vars(clause)` (with `find_nonnullable_rels`'
/// whole-row entries): what `node` being TRUE (`top_level`) — or merely
/// non-NULL (below the top level) — proves non-NULL. Column references
/// resolve against `scope`, as the qual's own resolution did.
pub(crate) fn nonnullable(
    node: &protobuf::Node,
    top_level: bool,
    scope: &Scope,
    log: &StrictLog,
    snapshot: &PgCatalog,
) -> Facts {
    if disabled() {
        return Facts::default();
    }
    let mut facts = nonnullable_node(node, top_level, scope, log, snapshot);
    // The node itself is non-NULL (TRUE, at the top level).
    if exprs::reusable(node, scope, snapshot) {
        facts.exprs.insert(exprs::key(node));
    }
    // A TRUE qual is neither FALSE nor NULL: so is every column it would
    // be FALSE or NULL for if that column were NULL. (AND / OR combine
    // what their arms prove.)
    if top_level && !is_and_or(node) {
        facts = facts.union(substituted(node, scope, log, snapshot, |v| {
            matches!(v, subst::Val::Null | subst::Val::Bool(false))
        }));
    }
    facts
}

fn is_and_or(n: &protobuf::Node) -> bool {
    matches!(n.node.as_ref(), Some(node::Node::BoolExpr(b))
        if b.boolop != protobuf::BoolExprType::NotExpr as i32)
}

/// The columns of `node` that, replaced by NULL, make it evaluate to a
/// value `excluded` says it doesn't have (see [`subst`]).
fn substituted(
    node: &protobuf::Node,
    scope: &Scope,
    log: &StrictLog,
    snapshot: &PgCatalog,
    excluded: impl Fn(&subst::Val) -> bool,
) -> Facts {
    let mut out = Facts::default();
    for col in subst::candidate_columns(node, scope) {
        let s = subst::Subst {
            null: &col,
            scope,
            log,
            snapshot,
        };
        if excluded(&s.eval(node)) {
            out = out.union(Facts::column(&col.0, &col.1));
        }
    }
    out
}

/// What `l IS [NOT] DISTINCT FROM r` being known to say `distinct` (or
/// not) proves; it is never NULL. `x IS DISTINCT FROM NULL` is `x IS NOT
/// NULL` (PG's parser rewrites it so — as a scalar test, even for a
/// composite `x`); not distinct from NULL, `x` is NULL. Not distinct from
/// a non-NULL value, `x` is non-NULL.
fn distinct_facts(
    e: &protobuf::AExpr,
    distinct: bool,
    scope: &Scope,
    log: &StrictLog,
    snapshot: &PgCatalog,
) -> Facts {
    let (Some(l), Some(r)) = (e.lexpr.as_deref(), e.rexpr.as_deref()) else {
        return Facts::default();
    };
    let is_null =
        |n: &protobuf::Node| matches!(n.node.as_ref(), Some(node::Node::AConst(c)) if c.isnull);
    let walk = |n: &protobuf::Node| nonnullable(n, false, scope, log, snapshot);
    let other = if is_null(r) {
        Some(l)
    } else if is_null(l) {
        Some(r)
    } else {
        None
    };
    match other {
        Some(x) if distinct => walk(x),
        Some(x) => match scalar_column(x, scope, snapshot) {
            Some(col) => Facts::null(col),
            None => Facts::default(),
        },
        None if distinct => Facts::default(),
        None => {
            let mut f = Facts::default();
            if log.is_strict(e.location, StrictNode::DistinctRightNonNull) {
                f = f.union(walk(l));
            }
            if log.is_strict(e.location, StrictNode::DistinctLeftNonNull) {
                f = f.union(walk(r));
            }
            f
        }
    }
}

/// A row constructor's fields, unless one expands into several (`t.*`,
/// `(x).*`): then they don't pair up with the other side's.
fn plain_row_fields(n: &protobuf::Node) -> Option<&[protobuf::Node]> {
    let Some(node::Node::RowExpr(r)) = n.node.as_ref() else {
        return None;
    };
    let star = |f: &protobuf::Node| match f.node.as_ref() {
        Some(node::Node::ColumnRef(c)) => c
            .fields
            .iter()
            .any(|f| matches!(f.node.as_ref(), Some(node::Node::AStar(_)))),
        Some(node::Node::AIndirection(i)) => i
            .indirection
            .iter()
            .any(|f| matches!(f.node.as_ref(), Some(node::Node::AStar(_)))),
        _ => false,
    };
    (!r.args.iter().any(star)).then_some(r.args.as_slice())
}

fn is_row(n: &Option<Box<protobuf::Node>>) -> bool {
    matches!(
        n.as_deref().and_then(|n| n.node.as_ref()),
        Some(node::Node::RowExpr(_))
    )
}

fn nonnullable_node(
    node: &protobuf::Node,
    top_level: bool,
    scope: &Scope,
    log: &StrictLog,
    snapshot: &PgCatalog,
) -> Facts {
    let walk = |n: &protobuf::Node, top: bool| nonnullable(n, top, scope, log, snapshot);
    let walk_opt = |n: &Option<Box<protobuf::Node>>, top: bool| {
        n.as_deref().map(|n| walk(n, top)).unwrap_or_default()
    };
    let Some(inner) = node.node.as_ref() else {
        return Facts::default();
    };
    match inner {
        node::Node::ColumnRef(c) => {
            let mut f = column_ref_facts(c, scope);
            // A boolean column TRUE at the top level: `done` is `done = true`.
            if top_level && let Some((col, _)) = plain_column(node, scope) {
                f.equals.insert(col, Literal::boolean(true));
            }
            f
        }
        node::Node::BoolExpr(b) => match protobuf::BoolExprType::try_from(b.boolop) {
            // At the top level every conjunct must be TRUE. Below it an
            // AND of a NULL and a FALSE is FALSE, so only what every arm
            // proves holds — as for OR at any level.
            Ok(protobuf::BoolExprType::AndExpr) if top_level => b
                .args
                .iter()
                .fold(Facts::default(), |acc, a| acc.union(walk(a, true))),
            Ok(protobuf::BoolExprType::AndExpr | protobuf::BoolExprType::OrExpr) => {
                intersection(&b.args, |a| walk(a, top_level))
            }
            // NOT is strict. At the top level `NOT e` TRUE also means `e`
            // FALSE, so not TRUE: `NOT (x IS NULL)` proves `x`.
            Ok(protobuf::BoolExprType::NotExpr) => {
                b.args.iter().fold(Facts::default(), |acc, a| {
                    let mut below = walk(a, false);
                    if top_level && let Some((col, _)) = plain_column(a, scope) {
                        below.equals.insert(col, Literal::boolean(false));
                    }
                    if top_level {
                        acc.union(below)
                            .union(nonnullable_unless_true(a, scope, log, snapshot))
                    } else {
                        acc.union(below)
                    }
                })
            }
            _ => Facts::default(),
        },
        node::Node::AExpr(e) => {
            use protobuf::AExprKind as K;
            if top_level && let Some(f) = null_count_facts(e, scope) {
                return f;
            }
            match K::try_from(e.kind) {
                Ok(K::AexprDistinct) if top_level => {
                    return distinct_facts(e, true, scope, log, snapshot);
                }
                Ok(K::AexprNotDistinct) if top_level => {
                    return distinct_facts(e, false, scope, log, snapshot);
                }
                _ => {}
            }
            if !log.is_strict(e.location, StrictNode::Op) {
                return Facts::default();
            }
            let op = crate::expr::extract_string_fields(&e.name).join(".");
            match K::try_from(e.kind) {
                // `(a, b) = (1, 2)` is `a = 1 AND b = 2` and `(a, b) <> (1,
                // 2)` `a <> 1 OR b <> 2` (make_row_comparison_op): at the
                // top level. Below it a FALSE pair hides a NULL one.
                Ok(K::AexprOp) if is_row(&e.lexpr) && is_row(&e.rexpr) => {
                    let (Some(l), Some(r)) = (
                        e.lexpr.as_deref().and_then(plain_row_fields),
                        e.rexpr.as_deref().and_then(plain_row_fields),
                    ) else {
                        return Facts::default();
                    };
                    if !top_level || l.len() != r.len() {
                        return Facts::default();
                    }
                    let pair = |(a, b): (&protobuf::Node, &protobuf::Node)| {
                        walk(a, false).union(walk(b, false))
                    };
                    match op.as_str() {
                        "=" => l
                            .iter()
                            .zip(r)
                            .fold(Facts::default(), |acc, p| acc.union(pair(p))),
                        "<>" => {
                            let mut acc: Option<Facts> = None;
                            for p in l.iter().zip(r) {
                                let f = pair(p);
                                acc = Some(match acc {
                                    None => f,
                                    Some(a) => a.intersect(&f),
                                });
                            }
                            acc.unwrap_or_default()
                        }
                        _ => Facts::default(),
                    }
                }
                // `(a, b) IN ((1, 2), (5, -5))`: an OR of row equalities,
                // each needing every field of the left row non-NULL.
                Ok(K::AexprIn) if is_row(&e.lexpr) => {
                    let items_are_rows = matches!(
                        e.rexpr.as_deref().and_then(|n| n.node.as_ref()),
                        Some(node::Node::List(l))
                            if !l.items.is_empty()
                                && l.items.iter().all(|i| plain_row_fields(i).is_some())
                    );
                    match e.lexpr.as_deref().and_then(plain_row_fields) {
                        Some(fields) if top_level && op == "=" && items_are_rows => fields
                            .iter()
                            .fold(Facts::default(), |acc, a| acc.union(walk(a, false))),
                        _ => Facts::default(),
                    }
                }
                Ok(K::AexprOp) => {
                    let strict = walk_opt(&e.lexpr, false).union(walk_opt(&e.rexpr, false));
                    if top_level {
                        strict.union(comparison_facts(e, scope, snapshot))
                    } else {
                        strict
                    }
                }
                Ok(K::AexprLike | K::AexprIlike | K::AexprSimilar) => {
                    walk_opt(&e.lexpr, false).union(walk_opt(&e.rexpr, false))
                }
                // `x IN (…)` is `x = ANY(ARRAY[…])` or an OR of `x = item`
                // (NOT IN: `<> ALL` over a non-empty array, or an AND of
                // `x <> item`), BETWEEN an AND / OR of comparisons on `x`:
                // all strict in `x` at any level.
                Ok(K::AexprIn) if top_level => {
                    walk_opt(&e.lexpr, false).union(comparison_facts(e, scope, snapshot))
                }
                Ok(K::AexprBetween) if top_level => {
                    walk_opt(&e.lexpr, false).union(between_facts(e, scope, snapshot))
                }
                Ok(
                    K::AexprIn
                    | K::AexprBetween
                    | K::AexprNotBetween
                    | K::AexprBetweenSym
                    | K::AexprNotBetweenSym,
                ) => walk_opt(&e.lexpr, false),
                // `is_strict_saop(expr, falseOK)`: `x op ANY(array)` is NULL
                // or FALSE when an input is NULL, enough at the top level.
                // `ALL` over an empty array is TRUE whatever `x` is.
                Ok(K::AexprOpAny) if top_level => walk_opt(&e.lexpr, false)
                    .union(walk_opt(&e.rexpr, false))
                    .union(comparison_facts(e, scope, snapshot)),
                Ok(K::AexprOpAll) if top_level => comparison_facts(e, scope, snapshot),
                _ => Facts::default(),
            }
        }
        node::Node::FuncCall(f) => {
            if !log.is_strict(f.location, StrictNode::Func) {
                return Facts::default();
            }
            f.args
                .iter()
                .fold(Facts::default(), |acc, a| acc.union(walk(a, false)))
        }
        node::Node::NamedArgExpr(na) => walk_opt(&na.arg, top_level),
        node::Node::TypeCast(tc) => {
            if !log.is_strict(tc.location, StrictNode::Cast) {
                return Facts::default();
            }
            walk_opt(&tc.arg, false)
        }
        node::Node::CollateClause(c) => walk_opt(&c.arg, top_level),
        // A field or an element of a NULL value is NULL.
        node::Node::AIndirection(i) => walk_opt(&i.arg, false),
        node::Node::NullTest(t)
            if top_level
                && protobuf::NullTestType::try_from(t.nulltesttype)
                    == Ok(protobuf::NullTestType::IsNotNull) =>
        {
            let Some(arg) = t.arg.as_deref() else {
                return Facts::default();
            };
            match arg.node.as_ref() {
                // `ROW(a, b) IS NOT NULL`: every field is non-NULL.
                Some(node::Node::RowExpr(r)) => r
                    .args
                    .iter()
                    .fold(Facts::default(), |acc, a| acc.union(walk(a, false))),
                // `t IS NOT NULL` over a whole-row reference: so is every
                // column of `t`.
                _ => match whole_row_source(arg, scope) {
                    Some(src) => src
                        .visible_columns()
                        .fold(Facts::rel(&src.alias), |acc, c| {
                            acc.union(Facts::column(&src.alias, &c.name))
                        }),
                    None => {
                        let mut f = walk(arg, false);
                        f.exprs.extend(composite_field_keys(arg, scope, snapshot));
                        f
                    }
                },
            }
        }
        // `x IS NULL` TRUE: `x` is NULL (a composite can be a row of NULLs).
        node::Node::NullTest(t)
            if top_level
                && protobuf::NullTestType::try_from(t.nulltesttype)
                    == Ok(protobuf::NullTestType::IsNull) =>
        {
            match t
                .arg
                .as_deref()
                .and_then(|a| scalar_column(a, scope, snapshot))
            {
                Some(col) => Facts::null(col),
                None => Facts::default(),
            }
        }
        // `EXISTS (SELECT …)`: what the subquery's WHERE proves of this
        // level's columns, for the rows it returns (a semi-join's quals).
        node::Node::SubLink(sub)
            if top_level
                && protobuf::SubLinkType::try_from(sub.sub_link_type)
                    == Ok(protobuf::SubLinkType::ExistsSublink) =>
        {
            log.correlated(sub.location)
        }
        // `x IN (SELECT …)` / `x op ANY (SELECT …)`: like `op ANY(array)`.
        node::Node::SubLink(sub)
            if top_level && log.is_strict(sub.location, StrictNode::Sublink) =>
        {
            match sub.testexpr.as_deref() {
                Some(t) => match t.node.as_ref() {
                    Some(node::Node::RowExpr(r)) => r
                        .args
                        .iter()
                        .fold(Facts::default(), |acc, a| acc.union(walk(a, false))),
                    _ => walk(t, false),
                },
                None => Facts::default(),
            }
        }
        // Boolean tests that are not TRUE for a NULL input.
        node::Node::BooleanTest(t)
            if top_level
                && matches!(
                    protobuf::BoolTestType::try_from(t.booltesttype),
                    Ok(protobuf::BoolTestType::IsTrue
                        | protobuf::BoolTestType::IsFalse
                        | protobuf::BoolTestType::IsNotUnknown)
                ) =>
        {
            walk_opt(&t.arg, false)
        }
        _ => Facts::default(),
    }
}

/// What `node` *not* being TRUE (FALSE or NULL) proves non-NULL — a CASE
/// branch after `WHEN node`. Only tests that never yield NULL say
/// anything: `x IS NULL` not TRUE is `x` non-NULL, while `x > 0` not TRUE
/// may well be `x` NULL.
pub(crate) fn nonnullable_unless_true(
    node: &protobuf::Node,
    scope: &Scope,
    log: &StrictLog,
    snapshot: &PgCatalog,
) -> Facts {
    if disabled() {
        return Facts::default();
    }
    let mut facts = unless_true_node(node, scope, log, snapshot);
    // Not TRUE: so is every column it would be TRUE for, were it NULL.
    if !is_and_or(node) {
        facts = facts.union(substituted(node, scope, log, snapshot, |v| {
            *v == subst::Val::Bool(true)
        }));
    }
    facts
}

/// What `node` *not* being FALSE (TRUE or NULL) proves non-NULL — an AND
/// operand after it: a test that is never NULL is then TRUE.
pub(crate) fn nonnullable_unless_false(
    node: &protobuf::Node,
    scope: &Scope,
    log: &StrictLog,
    snapshot: &PgCatalog,
) -> Facts {
    if disabled() {
        return Facts::default();
    }
    let mut facts = match node.node.as_ref() {
        Some(node::Node::NullTest(_) | node::Node::BooleanTest(_)) => {
            nonnullable(node, true, scope, log, snapshot)
        }
        Some(node::Node::AExpr(e))
            if matches!(
                protobuf::AExprKind::try_from(e.kind),
                Ok(protobuf::AExprKind::AexprDistinct | protobuf::AExprKind::AexprNotDistinct)
            ) =>
        {
            nonnullable(node, true, scope, log, snapshot)
        }
        Some(node::Node::BoolExpr(b)) => match protobuf::BoolExprType::try_from(b.boolop) {
            // Not FALSE: no arm is FALSE.
            Ok(protobuf::BoolExprType::AndExpr) => {
                b.args.iter().fold(Facts::default(), |acc, a| {
                    acc.union(nonnullable_unless_false(a, scope, log, snapshot))
                })
            }
            // Not FALSE: some arm isn't.
            Ok(protobuf::BoolExprType::OrExpr) => intersection(&b.args, |a| {
                nonnullable_unless_false(a, scope, log, snapshot)
            }),
            // `NOT e` not FALSE: `e` not TRUE.
            Ok(protobuf::BoolExprType::NotExpr) => match b.args.as_slice() {
                [e] => nonnullable_unless_true(e, scope, log, snapshot),
                _ => Facts::default(),
            },
            _ => Facts::default(),
        },
        _ => Facts::default(),
    };
    if !is_and_or(node) {
        facts = facts.union(substituted(node, scope, log, snapshot, |v| {
            *v == subst::Val::Bool(false)
        }));
    }
    facts
}

fn unless_true_node(
    node: &protobuf::Node,
    scope: &Scope,
    log: &StrictLog,
    snapshot: &PgCatalog,
) -> Facts {
    let Some(inner) = node.node.as_ref() else {
        return Facts::default();
    };
    match inner {
        // Never NULL: not TRUE is FALSE.
        node::Node::AExpr(e)
            if protobuf::AExprKind::try_from(e.kind) == Ok(protobuf::AExprKind::AexprDistinct) =>
        {
            distinct_facts(e, false, scope, log, snapshot)
        }
        node::Node::AExpr(e)
            if protobuf::AExprKind::try_from(e.kind)
                == Ok(protobuf::AExprKind::AexprNotDistinct) =>
        {
            distinct_facts(e, true, scope, log, snapshot)
        }
        // `x IS NOT NULL` not TRUE: `x` is NULL.
        node::Node::NullTest(t)
            if protobuf::NullTestType::try_from(t.nulltesttype)
                == Ok(protobuf::NullTestType::IsNotNull) =>
        {
            match t
                .arg
                .as_deref()
                .and_then(|a| scalar_column(a, scope, snapshot))
            {
                Some(col) => Facts::null(col),
                None => Facts::default(),
            }
        }
        node::Node::NullTest(t)
            if protobuf::NullTestType::try_from(t.nulltesttype)
                == Ok(protobuf::NullTestType::IsNull) =>
        {
            match t.arg.as_deref() {
                // Some field of the row is non-NULL — which one is unknown.
                Some(a) if matches!(a.node.as_ref(), Some(node::Node::RowExpr(_))) => {
                    Facts::default()
                }
                // A whole row that isn't NULL may still hold NULL columns:
                // only the entry itself is present.
                Some(a) => match whole_row_source(a, scope) {
                    Some(src) => Facts::rel(&src.alias),
                    None => nonnullable(a, false, scope, log, snapshot),
                },
                None => Facts::default(),
            }
        }
        node::Node::BooleanTest(t) => {
            let Some(arg) = t.arg.as_deref() else {
                return Facts::default();
            };
            match protobuf::BoolTestType::try_from(t.booltesttype) {
                // Not (NOT TRUE): TRUE.
                Ok(protobuf::BoolTestType::IsNotTrue) => {
                    nonnullable(arg, true, scope, log, snapshot)
                }
                // Not UNKNOWN / not (NOT FALSE), i.e. FALSE: non-NULL.
                Ok(protobuf::BoolTestType::IsUnknown | protobuf::BoolTestType::IsNotFalse) => {
                    nonnullable(arg, false, scope, log, snapshot)
                }
                _ => Facts::default(),
            }
        }
        node::Node::BoolExpr(b) => match protobuf::BoolExprType::try_from(b.boolop) {
            // Not TRUE: no arm is TRUE.
            Ok(protobuf::BoolExprType::OrExpr) => b.args.iter().fold(Facts::default(), |acc, a| {
                acc.union(nonnullable_unless_true(a, scope, log, snapshot))
            }),
            // Not TRUE: some arm isn't.
            Ok(protobuf::BoolExprType::AndExpr) => intersection(&b.args, |a| {
                nonnullable_unless_true(a, scope, log, snapshot)
            }),
            // `NOT e` not TRUE: `e` not FALSE.
            Ok(protobuf::BoolExprType::NotExpr) => match b.args.as_slice() {
                [e] => {
                    let f = nonnullable_unless_false(e, scope, log, snapshot);
                    // `NOT e` not TRUE: `e` not FALSE, so what `e` says
                    // holds of its column unless that is NULL.
                    match value_pred(e, scope, log, snapshot) {
                        Some((col, p)) => f.union(Facts::pred(col, p)),
                        None => f,
                    }
                }
                _ => Facts::default(),
            },
            _ => Facts::default(),
        },
        // A comparison with constants not TRUE: a non-NULL column fails it
        // (`kind = 'a'` not TRUE, `kind` is NULL or isn't `'a'`).
        node::Node::ColumnRef(_) | node::Node::AExpr(_) => {
            match value_pred(node, scope, log, snapshot) {
                Some((col, p)) => Facts::pred(col, p.negated()),
                None => Facts::default(),
            }
        }
        _ => Facts::default(),
    }
}

/// What a comparison of a column with constants ([`column_pred`]), or a
/// boolean column used as a condition (`done`: `done = true`), says of the
/// column's value when it is TRUE — and, for a non-NULL column, its
/// negation when it isn't.
fn value_pred(
    n: &protobuf::Node,
    scope: &Scope,
    log: &StrictLog,
    snapshot: &PgCatalog,
) -> Option<(Col, ValPred)> {
    match n.node.as_ref()? {
        node::Node::ColumnRef(_) => {
            let (col, t) = plain_column(n, scope)?;
            (snapshot.unwrap_domain(t) == crate::pg_catalog::oid::BOOL)
                .then(|| (col, ValPred::In(vec![Literal::boolean(true)])))
        }
        // The operator resolved (and, being strict, is one of the
        // comparison operators `column_pred` reads).
        node::Node::AExpr(e) if log.is_strict(e.location, StrictNode::Op) => {
            column_pred(e, scope, snapshot)
        }
        _ => None,
    }
}

/// `(x).f` for every field `f` of composite column `x`: what `x IS NOT
/// NULL` proves (a row-wise test, TRUE only when every field is non-NULL:
/// `ExecEvalRowNullInt`).
fn composite_field_keys(arg: &protobuf::Node, scope: &Scope, snapshot: &PgCatalog) -> Vec<String> {
    let Some((_, ty)) = plain_column(arg, scope) else {
        return Vec::new();
    };
    let Some(relid) = snapshot
        .get_type(snapshot.unwrap_domain(ty))
        .filter(|t| t.typtype == crate::pg_catalog::TypType::Composite)
        .and_then(|t| t.typrelid)
    else {
        return Vec::new();
    };
    snapshot
        .attributes_of(relid)
        .iter()
        .map(|a| {
            exprs::key(&protobuf::Node {
                node: Some(node::Node::AIndirection(Box::new(protobuf::AIndirection {
                    arg: Some(Box::new(arg.clone())),
                    indirection: vec![protobuf::Node {
                        node: Some(node::Node::String(protobuf::String {
                            sval: a.attname.clone(),
                        })),
                    }],
                }))),
            })
        })
        .collect()
}

/// The FROM entry a bare `t` / `t.*` reference names as a whole row.
fn whole_row_source<'s>(
    n: &protobuf::Node,
    scope: &'s Scope,
) -> Option<&'s crate::scope::TableSource> {
    let Some(node::Node::ColumnRef(c)) = n.node.as_ref() else {
        return None;
    };
    let parts = crate::expr::extract_string_fields(&c.fields);
    let star = c
        .fields
        .iter()
        .any(|f| matches!(f.node.as_ref(), Some(node::Node::AStar(_))));
    match (parts.as_slice(), star) {
        ([.., t], true) => scope.find_source(t),
        ([name], false)
            if matches!(
                scope.resolve_column(None, name, None),
                Err(crate::error::AnalyzeError::UndefinedColumn(_))
            ) =>
        {
            scope.find_source(name)
        }
        _ => None,
    }
}

/// A column reference proves its own column non-NULL (a whole-row one,
/// its entry).
fn column_ref_facts(c: &protobuf::ColumnRef, scope: &Scope) -> Facts {
    let star = c
        .fields
        .iter()
        .any(|f| matches!(f.node.as_ref(), Some(node::Node::AStar(_))));
    let parts = crate::expr::extract_string_fields(&c.fields);
    if star {
        return match parts.last() {
            Some(t) => scope
                .find_source(t)
                .map(|s| Facts::rel(&s.alias))
                .unwrap_or_default(),
            None => Facts::default(),
        };
    }
    let (table, column) = match parts.as_slice() {
        [col] => (None, col.as_str()),
        [tbl, col] => (Some(tbl.as_str()), col.as_str()),
        [_schema, tbl, col] => (Some(tbl.as_str()), col.as_str()),
        _ => return Facts::default(),
    };
    match scope.resolve_column(table, column, None) {
        Ok(col) => Facts::column(&col.table_alias, &col.name),
        Err(crate::error::AnalyzeError::UndefinedColumn(_)) if table.is_none() => scope
            .find_source(column)
            .map(|s| Facts::rel(&s.alias))
            .unwrap_or_default(),
        Err(_) => Facts::default(),
    }
}

/// The column a plain, non-composite column reference names (for a
/// composite `x`, `x IS NULL` also holds for a row of NULLs).
fn scalar_column(n: &protobuf::Node, scope: &Scope, snapshot: &PgCatalog) -> Option<Col> {
    let col = plain_column(n, scope)?;
    (!crate::coerce::is_complex(col.1, snapshot) && col.1 != crate::pg_catalog::oid::RECORD)
        .then_some(col.0)
}

/// The column a plain column reference names, with its type.
pub(crate) fn plain_column(
    n: &protobuf::Node,
    scope: &Scope,
) -> Option<(Col, crate::oid::PgTypeOid)> {
    let Some(node::Node::ColumnRef(c)) = n.node.as_ref() else {
        return None;
    };
    if c.fields
        .iter()
        .any(|f| matches!(f.node.as_ref(), Some(node::Node::AStar(_))))
    {
        return None;
    }
    let parts = crate::expr::extract_string_fields(&c.fields);
    let (table, column) = match parts.as_slice() {
        [col] => (None, col.as_str()),
        [tbl, col] => (Some(tbl.as_str()), col.as_str()),
        [_schema, tbl, col] => (Some(tbl.as_str()), col.as_str()),
        _ => return None,
    };
    let col = scope.resolve_column(table, column, None).ok()?;
    Some(((col.table_alias.clone(), col.name.clone()), col.type_oid))
}

/// A bare constant operand.
pub(crate) fn literal(n: &protobuf::Node) -> Option<Literal> {
    use typedpg_pg_query::protobuf::a_const::Val;
    match n.node.as_ref()? {
        node::Node::AConst(c) if !c.isnull => match c.val.as_ref()? {
            Val::Ival(i) => Some(Literal {
                text: i.ival.to_string(),
                kind: LitKind::Integer,
            }),
            Val::Sval(s) => Some(Literal {
                text: s.sval.clone(),
                kind: LitKind::String,
            }),
            Val::Boolval(b) => Some(Literal::boolean(b.boolval)),
            _ => None,
        },
        _ => None,
    }
}

/// A constant compared with a column of type `column_type`: a bare
/// literal, or one cast to exactly the column's type. Not one cast to
/// another type, nor under a COLLATE: the comparison would then be
/// another type's or collation's (`'A'::citext`, a nondeterministic
/// collation make `'A'` equal `'a'`), and its equality says nothing a
/// CHECK on the column can use.
pub(crate) fn literal_for(
    n: &protobuf::Node,
    column_type: crate::oid::PgTypeOid,
    snapshot: &PgCatalog,
) -> Option<Literal> {
    match n.node.as_ref()? {
        node::Node::TypeCast(tc) => {
            let target = crate::ddl::util::resolve_type_name(tc.type_name.as_ref()?, snapshot)?;
            (target == column_type)
                .then(|| literal(tc.arg.as_deref()?))
                .flatten()
        }
        _ => literal(n),
    }
}

/// `col op constant` (either way round), `col IN (…)`, `col = ANY (…)`
/// and `col <> ALL (…)` over constants, TRUE: what it says of the column
/// (equal to a constant, one or none of several, ordered against one).
fn comparison_facts(e: &protobuf::AExpr, scope: &Scope, snapshot: &PgCatalog) -> Facts {
    let Some((c, p)) = column_pred(e, scope, snapshot) else {
        return Facts::default();
    };
    let mut f = Facts::default();
    if let ValPred::In(vs) = &p
        && let [v] = vs.as_slice()
    {
        f.equals.insert(c.clone(), v.clone());
    }
    f.preds.push((c, p));
    f
}

/// `col BETWEEN a AND b` over constants, TRUE: `col >= a AND col <= b`.
fn between_facts(e: &protobuf::AExpr, scope: &Scope, snapshot: &PgCatalog) -> Facts {
    let (Some(l), Some(r)) = (e.lexpr.as_deref(), e.rexpr.as_deref()) else {
        return Facts::default();
    };
    let (Some((c, t)), Some(node::Node::List(list))) = (plain_column(l, scope), r.node.as_ref())
    else {
        return Facts::default();
    };
    let bound = |i: usize| list.items.get(i).and_then(|n| literal_for(n, t, snapshot));
    let (Some(a), Some(b)) = (bound(0), bound(1)) else {
        return Facts::default();
    };
    Facts::pred(c.clone(), ValPred::Cmp(CmpOp::Ge, a))
        .union(Facts::pred(c, ValPred::Cmp(CmpOp::Le, b)))
}

/// What a comparison of a plain column with constants, TRUE, says of the
/// column's value ([`literal_for`] says which constants qualify): `c = v`,
/// `c <> v`, `c < v` (and `v > c`), `c IN (…)`, `c NOT IN (…)`,
/// `c = ANY (…)` and `c <> ALL (…)` over an `ARRAY[…]` of constants or an
/// array literal (`'{a,b}'`). A NULL in an IN list or an `ANY` array
/// never makes it TRUE and is dropped; one in a NOT IN list or an `ALL`
/// array makes it never TRUE, and nothing is said.
pub(crate) fn column_pred(
    e: &protobuf::AExpr,
    scope: &Scope,
    snapshot: &PgCatalog,
) -> Option<(Col, ValPred)> {
    use protobuf::AExprKind as K;
    let op = crate::expr::extract_string_fields(&e.name).join(".");
    let (l, r) = (e.lexpr.as_deref()?, e.rexpr.as_deref()?);
    match K::try_from(e.kind).ok()? {
        K::AexprOp => {
            let (c, v, flipped) = match (plain_column(l, scope), plain_column(r, scope)) {
                (Some((c, t)), None) => (c, literal_for(r, t, snapshot)?, false),
                (None, Some((c, t))) => (c, literal_for(l, t, snapshot)?, true),
                _ => return None,
            };
            let p = match op.as_str() {
                "=" => ValPred::In(vec![v]),
                "<>" => ValPred::NotIn(vec![v]),
                other => {
                    let cmp = CmpOp::parse(other)?;
                    ValPred::Cmp(if flipped { cmp.flipped() } else { cmp }, v)
                }
            };
            Some((c, p))
        }
        K::AexprIn => {
            let (c, t) = plain_column(l, scope)?;
            let Some(node::Node::List(list)) = r.node.as_ref() else {
                return None;
            };
            let positive = match op.as_str() {
                "=" => true,
                "<>" => false,
                _ => return None,
            };
            let vs = constants(list.items.iter(), t, positive, snapshot)?;
            Some((
                c,
                if positive {
                    ValPred::In(vs)
                } else {
                    ValPred::NotIn(vs)
                },
            ))
        }
        kind @ (K::AexprOpAny | K::AexprOpAll) => {
            let (c, t) = plain_column(l, scope)?;
            let positive = match (kind, op.as_str()) {
                (K::AexprOpAny, "=") => true,
                (K::AexprOpAll, "<>") => false,
                _ => return None,
            };
            let vs = array_constants(r, t, positive, snapshot)?;
            Some((
                c,
                if positive {
                    ValPred::In(vs)
                } else {
                    ValPred::NotIn(vs)
                },
            ))
        }
        _ => None,
    }
}

/// The constants of an IN list or an array, for a column of type `t`
/// (NULLs dropped when `drop_nulls`, else refused).
pub(crate) fn constants<'n>(
    items: impl Iterator<Item = &'n protobuf::Node>,
    t: crate::oid::PgTypeOid,
    drop_nulls: bool,
    snapshot: &PgCatalog,
) -> Option<Vec<Literal>> {
    let mut out = Vec::new();
    for i in items {
        if let Some(node::Node::AConst(c)) = i.node.as_ref()
            && c.isnull
        {
            if drop_nulls {
                continue;
            }
            return None;
        }
        let v = literal_for(i, t, snapshot)?;
        if !out.contains(&v) {
            out.push(v);
        }
    }
    Some(out)
}

/// The elements of `ARRAY[…]` over constants, or of an array literal
/// (`'{a,b}'`, bare or cast to the array type of the column's type),
/// compared with a column of type `t`. An array literal is read only in
/// its plain form — comma-separated unquoted elements, no nesting or
/// escapes — and for a column whose type reads it that way (text,
/// varchar, enums, integers: `array_in` with `,` as the delimiter,
/// trimming the blanks around an element).
pub(crate) fn array_constants(
    n: &protobuf::Node,
    t: crate::oid::PgTypeOid,
    drop_nulls: bool,
    snapshot: &PgCatalog,
) -> Option<Vec<Literal>> {
    use typedpg_pg_query::protobuf::a_const::Val;
    match n.node.as_ref()? {
        node::Node::AArrayExpr(a) => constants(a.elements.iter(), t, drop_nulls, snapshot),
        node::Node::TypeCast(tc) => {
            let target = crate::ddl::util::resolve_type_name(tc.type_name.as_ref()?, snapshot)?;
            if snapshot.pg_type.get(&t).and_then(|ty| ty.typarray) != Some(target) {
                return None;
            }
            match tc.arg.as_deref()?.node.as_ref()? {
                node::Node::AConst(_) => {
                    array_constants(tc.arg.as_deref()?, t, drop_nulls, snapshot)
                }
                _ => None,
            }
        }
        node::Node::AConst(c) if !c.isnull => {
            let Some(Val::Sval(s)) = c.val.as_ref() else {
                return None;
            };
            let base = snapshot.unwrap_domain(t);
            let plain = matches!(
                base,
                crate::pg_catalog::oid::TEXT
                    | crate::pg_catalog::oid::VARCHAR
                    | crate::pg_catalog::oid::INT2
                    | crate::pg_catalog::oid::INT4
                    | crate::pg_catalog::oid::INT8
            ) || snapshot
                .pg_type
                .get(&base)
                .is_some_and(|ty| ty.typtype == crate::pg_catalog::TypType::Enum);
            if !plain {
                return None;
            }
            let blank = |c: char| matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c');
            let inner = s
                .sval
                .trim_matches(blank)
                .strip_prefix('{')?
                .strip_suffix('}')?;
            if inner.contains(['{', '}', '"', '\\']) {
                return None;
            }
            let mut out = Vec::new();
            if inner.trim_matches(blank).is_empty() {
                return Some(out);
            }
            for item in inner.split(',') {
                let item = item.trim_matches(blank);
                if item.is_empty() {
                    return None;
                }
                if item.eq_ignore_ascii_case("null") {
                    if drop_nulls {
                        continue;
                    }
                    return None;
                }
                let v = Literal {
                    text: item.to_owned(),
                    kind: LitKind::String,
                };
                if !out.contains(&v) {
                    out.push(v);
                }
            }
            Some(out)
        }
        _ => None,
    }
}

/// How many of `num_nonnulls(args)` / `num_nulls(args)` a comparison with
/// an integer constant allows non-NULL: `(at least, at most)`.
pub(crate) fn null_count_bounds(e: &protobuf::AExpr) -> Option<(&[protobuf::Node], i64, i64)> {
    let op = crate::expr::extract_string_fields(&e.name).join(".");
    let (l, r) = (e.lexpr.as_deref()?, e.rexpr.as_deref()?);
    // `f(args) op n` or `n op f(args)` (flipped).
    let (call, n, op) = match (l.node.as_ref()?, literal(r)) {
        (node::Node::FuncCall(fc), Some(v)) if v.is_integer() => {
            (fc, v.text.parse::<i64>().ok()?, op)
        }
        _ => match (r.node.as_ref()?, literal(l)) {
            (node::Node::FuncCall(fc), Some(v)) if v.is_integer() => {
                let flipped = match op.as_str() {
                    "<" => ">",
                    ">" => "<",
                    "<=" => ">=",
                    ">=" => "<=",
                    other => other,
                };
                (fc, v.text.parse::<i64>().ok()?, flipped.to_owned())
            }
            _ => return None,
        },
    };
    let name = crate::expr::extract_string_fields(&call.funcname);
    let counts_nonnulls = match name
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["num_nonnulls"] | ["pg_catalog", "num_nonnulls"] => true,
        ["num_nulls"] | ["pg_catalog", "num_nulls"] => false,
        _ => return None,
    };
    if call.func_variadic || call.agg_filter.is_some() || call.over.is_some() {
        return None;
    }
    let m = call.args.len() as i64;
    // Bounds on the function's value.
    let (lo, hi) = match op.as_str() {
        "=" => (n, n),
        ">=" => (n, m),
        ">" => (n + 1, m),
        "<=" => (0, n),
        "<" => (0, n - 1),
        _ => return None,
    };
    let (lo, hi) = if counts_nonnulls {
        (lo, hi)
    } else {
        (m - hi, m - lo)
    };
    Some((call.args.as_slice(), lo.max(0), hi.min(m)))
}

/// `num_nonnulls(a, b) > 0` and kin over plain columns: at least one of
/// them non-NULL — or every one, when all must be.
fn null_count_facts(e: &protobuf::AExpr, scope: &Scope) -> Option<Facts> {
    let (args, at_least, _) = null_count_bounds(e)?;
    let cols: Option<BTreeSet<Col>> = args
        .iter()
        .map(|a| plain_column(a, scope).map(|(c, _)| c))
        .collect();
    let cols = cols?;
    if at_least <= 0 {
        return Some(Facts::default());
    }
    if at_least as usize >= args.len() {
        return Some(cols.iter().fold(Facts::default(), |acc, (a, c)| {
            acc.union(Facts::column(a, c))
        }));
    }
    Some(Facts::disjunction(cols))
}
