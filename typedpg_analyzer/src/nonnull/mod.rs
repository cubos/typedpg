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

use typedpg_pg_query::NodeBox;
use typedpg_pg_query::ast;

use crate::node_key::NodeKey;
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
    /// An `A_Expr` (or a simple CASE's `test = value`, at its WHEN) whose
    /// every operator is a built-in comparison (`=` `<>` `<` `<=` `>` `>=`
    /// of `pg_catalog`, run by a `pg_catalog` function, returning
    /// boolean): what [`column_pred`] and the CHECK reading take it to be.
    /// A user-defined `=` (an exact-signature one on an enum or a domain,
    /// which beats the built-in) may mean anything.
    BuiltinCompare,
    /// A call that resolved to `pg_catalog.num_nulls` /
    /// `pg_catalog.num_nonnulls` (not a user function of the name).
    NullCount,
    /// A call that resolved to `pg_catalog.array_position(anycompatiblearray,
    /// anycompatible)` (not a user function of the name).
    ArrayPosition,
    /// A call that resolved to `pg_catalog.isfinite` (not a user function
    /// of the name).
    IsFinite,
    /// An operator or function call that resolved to the built-in jsonb
    /// one: `?` (`jsonb_exists`), `?&` (`jsonb_exists_all`), `->` / `->>`
    /// by key (`jsonb_object_field[_text]`), `jsonb_typeof`.
    Jsonb(JsonbBuiltin),
    /// A COALESCE / GREATEST / LEAST whose common type keeps the integer,
    /// text and boolean constants [`subst`] folds exactly (no float, no
    /// typmod): its folded value is the one PG computes.
    ExactFold,
    /// A COALESCE / GREATEST / LEAST of a float type: it keeps an integer
    /// constant only up to the float's precision (`16777217::real` is
    /// 16777216).
    FloatFold,
    /// `StdCompare` with a float operand: integer constants compare as
    /// written only up to the float's precision.
    FloatCompare,
}

/// The built-in jsonb operators and functions a CHECK constraint's JSON
/// reading knows (see [`StrictNode::Jsonb`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum JsonbBuiltin {
    Exists,
    ExistsAll,
    Field,
    FieldText,
    Typeof,
}

impl JsonbBuiltin {
    /// The built-in `proname` resolved to.
    pub(crate) fn of(proname: &str) -> Option<JsonbBuiltin> {
        Some(match proname {
            "jsonb_exists" => JsonbBuiltin::Exists,
            "jsonb_exists_all" => JsonbBuiltin::ExistsAll,
            "jsonb_object_field" => JsonbBuiltin::Field,
            "jsonb_object_field_text" => JsonbBuiltin::FieldText,
            "jsonb_typeof" => JsonbBuiltin::Typeof,
            _ => return None,
        })
    }
}

/// What a node being trusted means: [`StrictLog::is_strict`], or the same
/// question asked of what a CHECK constraint resolved to when it was
/// created ([`TrustedNodes`]).
pub(crate) trait Trust {
    fn trusts(&self, location: i32, kind: StrictNode) -> bool;

    /// The routine the plain function call at `location` resolved to.
    fn call(&self, _location: i32) -> Option<crate::oid::PgProcOid> {
        None
    }
}

impl Trust for StrictLog {
    fn trusts(&self, location: i32, kind: StrictNode) -> bool {
        self.is_strict(location, kind)
    }

    fn call(&self, location: i32) -> Option<crate::oid::PgProcOid> {
        self.3.borrow().get(&location).copied()
    }
}

/// The nodes of a stored expression a [`StrictLog`] recorded as strict
/// (or as a built-in comparison, …) when the expression was resolved — as
/// PG keeps a CHECK constraint's operators and functions by OID — and the
/// type each untyped string literal in it was coerced to.
#[derive(Debug, Clone, Default)]
pub(crate) struct TrustedNodes {
    pub nodes: HashSet<(i32, StrictNode)>,
    pub literal_types: HashMap<i32, crate::oid::PgTypeOid>,
    /// The routine each plain function call resolved to, by location.
    pub calls: HashMap<i32, crate::oid::PgProcOid>,
}

impl TrustedNodes {
    /// The routine the plain call at `location` resolved to.
    pub(crate) fn call(&self, location: i32) -> Option<crate::oid::PgProcOid> {
        self.calls.get(&location).copied()
    }
}

impl Trust for TrustedNodes {
    fn trusts(&self, location: i32, kind: StrictNode) -> bool {
        location >= 0 && self.nodes.contains(&(location, kind))
    }

    fn call(&self, location: i32) -> Option<crate::oid::PgProcOid> {
        TrustedNodes::call(self, location)
    }
}

/// Per-node strictness recorded while a qual is typed, plus what the
/// `EXISTS` sublinks in it prove of the enclosing level's columns (keyed
/// by the sublink's location; see [`capture_correlation`]), the type
/// each untyped string literal was coerced to (by its location), and what
/// is known of the elements of the array an `op ANY / ALL (array)` reads
/// (by the expression's location; see [`StrictLog::note_array_elements`]).
#[derive(Debug, Default)]
pub(crate) struct StrictLog(
    RefCell<HashMap<(i32, StrictNode), bool>>,
    RefCell<HashMap<i32, Facts>>,
    RefCell<HashMap<i32, crate::oid::PgTypeOid>>,
    RefCell<HashMap<i32, crate::oid::PgProcOid>>,
    RefCell<HashMap<i32, ArrayElements>>,
);

/// The elements of the array an `x op ANY / ALL (array)` reads: their type
/// and what every non-NULL one of them is (nothing when the array's
/// collation is nondeterministic: its equality then holds between
/// different strings).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ArrayElements {
    pub elem_type: crate::oid::PgTypeOid,
    pub refine: crate::refine::Refinement,
}

impl StrictLog {
    /// Record what the elements of the array the `op ANY / ALL (array)` at
    /// `location` reads are. A node typed several times keeps only what
    /// holds of every reading.
    pub fn note_array_elements(&self, location: i32, elems: ArrayElements) {
        if location < 0 {
            return;
        }
        let mut map = self.4.borrow_mut();
        let merged = match map.remove(&location) {
            Some(before) if before.elem_type == elems.elem_type => ArrayElements {
                elem_type: elems.elem_type,
                refine: crate::refine::Refinement::either([&before.refine, &elems.refine]),
            },
            Some(before) => ArrayElements {
                elem_type: before.elem_type,
                refine: crate::refine::Refinement::NONE,
            },
            None => elems,
        };
        map.insert(location, merged);
    }

    /// What [`StrictLog::note_array_elements`] recorded at `location`.
    fn array_elements(&self, location: i32) -> Option<ArrayElements> {
        self.4.borrow().get(&location).cloned()
    }

    /// Record that the plain function call at `location` resolved to
    /// `proc`.
    pub fn note_call(&self, location: i32, proc: crate::oid::PgProcOid) {
        if location >= 0 {
            self.3.borrow_mut().insert(location, proc);
        }
    }

    /// Record that the untyped string literal at `location` was coerced
    /// to `type_oid`.
    pub fn note_literal_type(&self, location: i32, type_oid: crate::oid::PgTypeOid) {
        if location >= 0 {
            self.2.borrow_mut().insert(location, type_oid);
        }
    }

    /// Add what `other` (a log kept for part of the expression this one
    /// is about) recorded.
    pub fn absorb(&self, other: &StrictLog) {
        for (&(location, kind), &strict) in other.0.borrow().iter() {
            self.note(location, kind, strict);
        }
        self.2
            .borrow_mut()
            .extend(other.2.borrow().iter().map(|(k, v)| (*k, *v)));
        self.3
            .borrow_mut()
            .extend(other.3.borrow().iter().map(|(k, v)| (*k, *v)));
        for (&location, elems) in other.4.borrow().iter() {
            self.note_array_elements(location, elems.clone());
        }
    }

    /// What this log trusts, kept apart from it.
    pub fn trusted(&self) -> TrustedNodes {
        TrustedNodes {
            nodes: self
                .0
                .borrow()
                .iter()
                .filter(|(_, s)| **s)
                .map(|(k, _)| *k)
                .collect(),
            literal_types: self.2.borrow().clone(),
            calls: self.3.borrow().clone(),
        }
    }

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
    /// enclosing level are being captured: the `SelectStmt`, and what it
    /// deposited.
    static CORRELATION: RefCell<Option<(NodeKey<ast::SelectStmt>, Option<Facts>)>> = const { RefCell::new(None) };
}

/// Run `f` (the analysis of subquery `sel`), capturing what `sel`'s WHERE
/// proves of the columns of the levels around it — facts that hold
/// whenever `sel` returns a row: PG's `convert_EXISTS_sublink_to_join`
/// pulls an `EXISTS` up into a semi-join whose quals then count like an
/// inner join's. Only `sel` itself deposits (by address): not the queries
/// nested in it, nor a set operation's arms.
pub(crate) fn capture_correlation<R>(sel: &ast::SelectStmt, f: impl FnOnce() -> R) -> (R, Facts) {
    let key = NodeKey::of(sel);
    let before = CORRELATION.with(|c| c.replace(Some((key, None))));
    let out = f();
    let got = CORRELATION.with(|c| c.replace(before));
    let facts = got.and_then(|(_, f)| f).unwrap_or_default();
    (out, facts)
}

/// Deposit what `sel`'s WHERE proves of the enclosing levels, if `sel` is
/// the subquery being captured (see [`capture_correlation`]). Expression
/// facts stay behind: they may speak of `sel`'s own columns.
pub(crate) fn deposit_correlation(sel: &ast::SelectStmt, mut facts: Facts) {
    let key = NodeKey::of(sel);
    facts.exprs.clear();
    CORRELATION.with(|c| {
        if let Some((k, slot)) = c.borrow_mut().as_mut()
            && *k == key
        {
            *slot = Some(facts);
        }
    });
}

/// What a subquery's level proves of its own rows (see [`capture_own`]):
/// its WHERE's facts about its own FROM entries, those entries (alias and
/// relation), and the column its single target is, when it is one.
#[derive(Debug, Clone, Default)]
pub(crate) struct OwnRows {
    pub facts: Facts,
    pub sources: Vec<(String, Option<crate::oid::PgClassOid>)>,
    pub target: Option<Col>,
}

thread_local! {
    /// The subquery whose own rows' facts are being captured: the
    /// `SelectStmt`, and what it deposited.
    static OWN_ROWS: RefCell<Option<(NodeKey<ast::SelectStmt>, Option<OwnRows>)>> = const { RefCell::new(None) };
}

/// Run `f` (the analysis of subquery `sel`), capturing what `sel`'s level
/// proves of its own rows (see [`OwnRows`]), when every row it returns
/// passed its WHERE. Only `sel` itself deposits, by address.
pub(crate) fn capture_own<R>(sel: &ast::SelectStmt, f: impl FnOnce() -> R) -> (R, Option<OwnRows>) {
    let key = NodeKey::of(sel);
    let before = OWN_ROWS.with(|c| c.replace(Some((key, None))));
    let out = f();
    let got = OWN_ROWS.with(|c| c.replace(before));
    (out, got.and_then(|(_, own)| own))
}

/// Deposit what `sel`'s level proves of its own rows, if `sel` is the
/// subquery being captured (see [`capture_own`]).
pub(crate) fn deposit_own(sel: &ast::SelectStmt, mut own: OwnRows) {
    let key = NodeKey::of(sel);
    own.facts.exprs.clear();
    OWN_ROWS.with(|c| {
        if let Some((k, slot)) = c.borrow_mut().as_mut()
            && *k == key
        {
            *slot = Some(own);
        }
    });
}

/// Whether some query is capturing `sel`'s own rows' facts.
pub(crate) fn own_wanted(sel: &ast::SelectStmt) -> bool {
    let key = NodeKey::of(sel);
    OWN_ROWS.with(|c| c.borrow().as_ref().is_some_and(|(k, _)| *k == key))
}

/// Whether some query is capturing `sel`'s correlation facts.
pub(crate) fn correlation_wanted(sel: &ast::SelectStmt) -> bool {
    let key = NodeKey::of(sel);
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
    /// A number with a fraction or an exponent, or too large for an
    /// `int4` (`1.5`, `1e3`, `3000000000`: a `numeric` or `int8`
    /// constant, as the lexer leaves it).
    Numeric,
    /// Not written in the query: a point of an ordered type (see
    /// [`crate::refine_bounds`]), from what is known of other values (the
    /// bound of an array's elements `x < ANY (arr)` compares with). Its
    /// text is [`Literal::point`]'s, read only in that type's order.
    Point(crate::refine_bounds::OrdKind),
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

    /// Point `p` of an ordered type of kind `kind`.
    pub fn point(
        kind: crate::refine_bounds::OrdKind,
        p: crate::refine_bounds::OrdPoint,
    ) -> Literal {
        use crate::refine_bounds::OrdPoint;
        Literal {
            text: match p {
                OrdPoint::NegInfinity => "-inf".to_owned(),
                OrdPoint::Infinity => "inf".to_owned(),
                OrdPoint::NaN => "nan".to_owned(),
                OrdPoint::Finite(n) => n.to_string(),
            },
            kind: LitKind::Point(kind),
        }
    }

    /// The point a [`Literal::point`] of kind `kind` is.
    pub fn as_point(
        &self,
        kind: crate::refine_bounds::OrdKind,
    ) -> Option<crate::refine_bounds::OrdPoint> {
        use crate::refine_bounds::{Num, OrdPoint};
        if self.kind != LitKind::Point(kind) {
            return None;
        }
        Some(match self.text.as_str() {
            "-inf" => OrdPoint::NegInfinity,
            "inf" => OrdPoint::Infinity,
            "nan" => OrdPoint::NaN,
            n => OrdPoint::Finite(Num::parse(n)?),
        })
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
    /// Every predicate of one of the arms (an OR of conditions on the
    /// column: `n < 0 OR n BETWEEN 5 AND 7 OR n IN (20, 30)`). A
    /// [`checks::Space`] reads it exactly where it tests a value, and as
    /// the hull of its arms where it computes bounds (a refinement's range
    /// is a single interval). One arm is a conjunction.
    AnyOf(Vec<Vec<ValPred>>),
}

impl ValPred {
    /// What the comparison not being TRUE says of a non-NULL column. For
    /// [`ValPred::AnyOf`], De Morgan's laws: a non-NULL column compared
    /// with non-NULL constants makes every comparison TRUE or FALSE.
    pub fn negated(&self) -> ValPred {
        match self {
            ValPred::In(vs) => ValPred::NotIn(vs.clone()),
            ValPred::NotIn(vs) => ValPred::In(vs.clone()),
            ValPred::Cmp(op, v) => ValPred::Cmp(op.negated(), v.clone()),
            // NOT (A1 OR A2): NOT A1 AND NOT A2, one arm; NOT (p AND q):
            // NOT p OR NOT q.
            ValPred::AnyOf(arms) => ValPred::AnyOf(vec![
                arms.iter()
                    .map(|arm| match arm.as_slice() {
                        [p] => p.negated(),
                        _ => ValPred::AnyOf(arm.iter().map(|p| vec![p.negated()]).collect()),
                    })
                    .collect(),
            ]),
        }
    }
}

/// The most arms a [`ValPred::AnyOf`] an OR merges into keeps (beyond, the
/// OR says nothing of the column but the value set its arms' `IN`s make).
const MAX_ARMS: usize = 32;

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
///   a row (see [`exprs::reusable`]);
/// - `same`: `(target, source)` pairs of columns, where `target`
///   non-NULL means `source` is non-NULL and equal to it under the
///   built-in equality (`a.x = b.y`: both ways; an outer join's ON, only
///   from the nullable side's column to the preserved one's). What holds
///   of `source`'s value holds of `target`'s ([`equivalence_facts`]).
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
    pub same: Vec<(Col, Col)>,
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
            && self.same.is_empty()
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
        for e in other.same {
            if !self.same.contains(&e) {
                self.same.push(e);
            }
        }
        self
    }

    /// Everything these facts say of column `c`'s value if it is
    /// non-NULL: its predicates, and its constant.
    pub(crate) fn preds_of(&self, c: &Col) -> Vec<ValPred> {
        let mut out: Vec<ValPred> = self
            .preds
            .iter()
            .filter(|(pc, _)| pc == c)
            .map(|(_, p)| p.clone())
            .collect();
        if let Some(v) = self.equals.get(c) {
            let p = ValPred::In(vec![v.clone()]);
            if !out.contains(&p) {
                out.push(p);
            }
        }
        out
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
            same: self
                .same
                .iter()
                .filter(|e| other.same.contains(e))
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
            // `n < 0 OR n BETWEEN 5 AND 7`: what one side or the other says
            // of the column (beyond a plain union of value sets, above).
            let (a, b) = (self.preds_of(c), other.preds_of(c));
            let plain = |ps: &[ValPred]| matches!(ps, [ValPred::In(_)]);
            if a.is_empty() || b.is_empty() || (plain(&a) && plain(&b)) {
                continue;
            }
            // An arm that is itself an OR of arms is spliced in.
            let arms_of = |ps: Vec<ValPred>| match <[ValPred; 1]>::try_from(ps) {
                Ok([ValPred::AnyOf(arms)]) => arms,
                Ok([p]) => vec![vec![p]],
                Err(ps) => vec![ps],
            };
            let mut arms = arms_of(a);
            for arm in arms_of(b) {
                if !arms.contains(&arm) {
                    arms.push(arm);
                }
            }
            if arms.len() > MAX_ARMS {
                continue;
            }
            let p = (c.clone(), ValPred::AnyOf(arms));
            if !out.preds.contains(&p) {
                out.preds.push(p);
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
    /// These facts with every column of `from` read as the same column of
    /// `to`, and nothing about any other entry.
    pub fn moved(&self, from: &str, to: &str) -> Facts {
        let one: HashSet<String> = std::iter::once(from.to_owned()).collect();
        let mut f = self.clone().restricted_to(&one);
        let mv = |(_, c): &Col| (to.to_owned(), c.clone());
        f.columns = f.columns.iter().map(mv).collect();
        f.rels = f.rels.iter().map(|_| to.to_owned()).collect();
        f.nulls = f.nulls.iter().map(mv).collect();
        f.disjunctions = f
            .disjunctions
            .iter()
            .map(|d| d.iter().map(mv).collect())
            .collect();
        f.equals = f.equals.iter().map(|(c, l)| (mv(c), l.clone())).collect();
        f.preds = f.preds.iter().map(|(c, p)| (mv(c), p.clone())).collect();
        f.same = f.same.iter().map(|(a, b)| (mv(a), mv(b))).collect();
        f.exprs.clear();
        f
    }

    /// These facts with entry `from` named `to`. Expression facts name
    /// entries by the query's text: kept only with `keep_exprs` (`from`
    /// keeps answering to that text).
    pub fn renamed(mut self, from: &str, to: &str, keep_exprs: bool) -> Facts {
        let mv = |c: Col| if c.0 == from { (to.to_owned(), c.1) } else { c };
        self.columns = self.columns.into_iter().map(mv).collect();
        self.rels = self
            .rels
            .into_iter()
            .map(|a| if a == from { to.to_owned() } else { a })
            .collect();
        self.nulls = self.nulls.into_iter().map(mv).collect();
        self.disjunctions = self
            .disjunctions
            .into_iter()
            .map(|d| d.into_iter().map(mv).collect())
            .collect();
        self.equals = self.equals.into_iter().map(|(c, l)| (mv(c), l)).collect();
        self.preds = self.preds.into_iter().map(|(c, p)| (mv(c), p)).collect();
        self.same = self.same.into_iter().map(|(a, b)| (mv(a), mv(b))).collect();
        if !keep_exprs {
            self.exprs.clear();
        }
        self
    }

    pub fn restricted_to(mut self, aliases: &HashSet<String>) -> Facts {
        self.columns.retain(|(a, _)| aliases.contains(a));
        self.rels.retain(|a| aliases.contains(a));
        self.nulls.retain(|(a, _)| aliases.contains(a));
        self.disjunctions
            .retain(|d| d.iter().all(|(a, _)| aliases.contains(a)));
        self.equals.retain(|(a, _), _| aliases.contains(a));
        self.preds.retain(|((a, _), _)| aliases.contains(a));
        self.same
            .retain(|((a, _), (b, _))| aliases.contains(a) && aliases.contains(b));
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
    nodes: impl IntoIterator<Item = Option<&'a ast::Node>>,
    mut f: impl FnMut(Option<&'a ast::Node>) -> Facts,
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
    node: Option<&ast::Node>,
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

/// For the lint's `redundant_null_check`: record, for each `IS [NOT] NULL`
/// test that is a top-level `AND` argument of `qual` — a `WHERE`, `JOIN …
/// ON` or `HAVING`, where a NULL condition drops the row as FALSE does —
/// whether the other arguments being TRUE prove its argument non-NULL. Then
/// a row whose argument is NULL is dropped by them whatever the test says:
/// an `IS NOT NULL` test changes nothing, and an `IS NULL` one makes the
/// condition never TRUE. The other `IS [NOT] NULL` tests are no proof
/// (`x IS NOT NULL AND x IS NOT NULL`: each would settle the other, and
/// removing both changes the rows).
pub(crate) fn note_settled_null_tests(
    qual: &ast::Node,
    scope: &Scope,
    log: &StrictLog,
    snapshot: &PgCatalog,
) {
    fn conjuncts<'n>(n: &'n ast::Node, out: &mut Vec<&'n ast::Node>) {
        match n {
            ast::Node::BoolExpr(b) if b.boolop == ast::BoolExprType::AndExpr => {
                for arg in b.args.iter().flatten() {
                    conjuncts(arg, out);
                }
            }
            n => out.push(n),
        }
    }
    if !crate::pg_catalog::collecting_column_ref_bindings() {
        return;
    }
    let mut all = Vec::new();
    conjuncts(qual, &mut all);
    for (i, c) in all.iter().enumerate() {
        let ast::Node::NullTest(t) = c else {
            continue;
        };
        let Some(arg) = t.arg.as_ref() else {
            continue;
        };
        let proof = all
            .iter()
            .enumerate()
            .filter(|(j, o)| *j != i && !matches!(o, ast::Node::NullTest(_)))
            .fold(Facts::default(), |acc, (_, o)| {
                acc.union(nonnullable(Some(o), true, scope, log, snapshot))
            });
        let settled = plain_column(arg, scope).is_some_and(|(col, _)| proof.columns.contains(&col))
            || (exprs::reusable(Some(arg), scope, snapshot)
                && proof.exprs.contains(&exprs::key(Some(arg))));
        crate::pg_catalog::note_null_test_settled(t.location, settled);
    }
}

fn is_and_or(n: Option<&ast::Node>) -> bool {
    matches!(n, Some(ast::Node::BoolExpr(b))
        if b.boolop != ast::BoolExprType::NotExpr)
}

/// The columns of `node` that, replaced by NULL, make it evaluate to a
/// value `excluded` says it doesn't have (see [`subst`]).
fn substituted(
    node: Option<&ast::Node>,
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
    e: &ast::AExpr,
    distinct: bool,
    scope: &Scope,
    log: &StrictLog,
    snapshot: &PgCatalog,
) -> Facts {
    if e.lexpr.is_none() || e.rexpr.is_none() {
        return Facts::default();
    }
    let (l, r) = (&e.lexpr, &e.rexpr);
    let is_null = |n: Option<&ast::Node>| matches!(n, Some(ast::Node::AConst(c)) if c.isnull);
    let walk = |n: Option<&ast::Node>| nonnullable(n, false, scope, log, snapshot);
    let other = if is_null(r.as_ref()) {
        Some(l)
    } else if is_null(l.as_ref()) {
        Some(r)
    } else {
        None
    };
    match other {
        Some(x) if distinct => walk(x.as_ref()),
        Some(x) => match scalar_column(x.as_ref(), scope, snapshot) {
            Some(col) => Facts::null(col),
            None => Facts::default(),
        },
        None if distinct => Facts::default(),
        None => {
            let mut f = Facts::default();
            if log.is_strict(e.location, StrictNode::DistinctRightNonNull) {
                f = f.union(walk(l.as_ref()));
            }
            if log.is_strict(e.location, StrictNode::DistinctLeftNonNull) {
                f = f.union(walk(r.as_ref()));
            }
            f
        }
    }
}

/// A row constructor's fields, unless one expands into several (`t.*`,
/// `(x).*`): then they don't pair up with the other side's.
fn plain_row_fields(n: Option<&ast::Node>) -> Option<&[Option<ast::Node>]> {
    let Some(ast::Node::RowExpr(r)) = n else {
        return None;
    };
    let star = |f: Option<&ast::Node>| match f {
        Some(ast::Node::ColumnRef(c)) => c
            .fields
            .iter()
            .any(|f| matches!(f.as_ref(), Some(ast::Node::AStar(_)))),
        Some(ast::Node::AIndirection(i)) => i
            .indirection
            .iter()
            .any(|f| matches!(f.as_ref(), Some(ast::Node::AStar(_)))),
        _ => false,
    };
    (!r.args.iter().any(|n| star(n.as_ref()))).then_some(r.args.as_slice())
}

/// `e` of `NOT (NOT e)`.
fn double_negated(b: &ast::BoolExpr) -> Option<Option<&ast::Node>> {
    match b.args.as_slice() {
        [Some(ast::Node::BoolExpr(inner))]
            if b.boolop == ast::BoolExprType::NotExpr
                && inner.boolop == ast::BoolExprType::NotExpr =>
        {
            match inner.args.as_slice() {
                [e] => Some(e.as_ref()),
                _ => None,
            }
        }
        _ => None,
    }
}

fn is_row(n: Option<&ast::Node>) -> bool {
    matches!(n, Some(ast::Node::RowExpr(_)))
}

fn nonnullable_node(
    node: Option<&ast::Node>,
    top_level: bool,
    scope: &Scope,
    log: &StrictLog,
    snapshot: &PgCatalog,
) -> Facts {
    let walk = |n: Option<&ast::Node>, top: bool| nonnullable(n, top, scope, log, snapshot);
    let walk_opt = |n: Option<&ast::Node>, top: bool| n.map(|_| walk(n, top)).unwrap_or_default();
    let Some(inner) = node else {
        return Facts::default();
    };
    match inner {
        ast::Node::ColumnRef(c) => {
            let mut f = column_ref_facts(c, scope);
            // A boolean column TRUE at the top level: `done` is `done = true`.
            if top_level && let Some((col, _)) = plain_column(inner, scope) {
                f.equals.insert(col, Literal::boolean(true));
            }
            f
        }
        ast::Node::BoolExpr(b) => match b.boolop {
            // At the top level every conjunct must be TRUE. Below it an
            // AND of a NULL and a FALSE is FALSE, so only what every arm
            // proves holds — as for OR at any level.
            ast::BoolExprType::AndExpr if top_level => b
                .args
                .iter()
                .fold(Facts::default(), |acc, a| acc.union(walk(a.as_ref(), true))),
            ast::BoolExprType::AndExpr | ast::BoolExprType::OrExpr => {
                intersection(b.args.iter().map(Option::as_ref), |a| walk(a, top_level))
            }
            // `NOT (NOT e)` is `e` (`negate_clause`, as the planner folds the
            // expressions, before `reduce_outer_joins`): `NOT (x NOT IN
            // (SELECT …))` is `x IN (SELECT …)`, strict in `x` at the top.
            ast::BoolExprType::NotExpr if double_negated(b).is_some() => {
                walk(double_negated(b).flatten(), top_level)
            }
            // NOT is strict. At the top level `NOT e` TRUE also means `e`
            // FALSE, so not TRUE: `NOT (x IS NULL)` proves `x`.
            ast::BoolExprType::NotExpr => b.args.iter().fold(Facts::default(), |acc, a| {
                let mut below = walk(a.as_ref(), false);
                if top_level && let Some((col, _)) = a.as_ref().and_then(|a| plain_column(a, scope))
                {
                    below.equals.insert(col, Literal::boolean(false));
                }
                if top_level {
                    acc.union(below).union(nonnullable_unless_true(
                        a.as_ref(),
                        scope,
                        log,
                        snapshot,
                    ))
                } else {
                    acc.union(below)
                }
            }),
        },
        ast::Node::AExpr(e) => {
            use ast::AExprKind as K;
            if top_level && let Some(f) = null_count_facts(e, scope, log) {
                return f;
            }
            match e.kind {
                K::AexprDistinct if top_level => {
                    return distinct_facts(e, true, scope, log, snapshot);
                }
                K::AexprNotDistinct if top_level => {
                    return distinct_facts(e, false, scope, log, snapshot);
                }
                _ => {}
            }
            if !log.is_strict(e.location, StrictNode::Op) {
                return Facts::default();
            }
            let op = crate::expr::extract_string_fields(&e.name).join(".");
            match e.kind {
                // `(a, b) = (1, 2)` is `a = 1 AND b = 2` and `(a, b) <> (1,
                // 2)` `a <> 1 OR b <> 2` (make_row_comparison_op): at the
                // top level. Below it a FALSE pair hides a NULL one. An
                // ordering comparison is NULL when its first pair is (the
                // row comparison stops at the first NULL pair).
                K::AexprOp if is_row(e.lexpr.as_ref()) && is_row(e.rexpr.as_ref()) => {
                    let (Some(l), Some(r)) = (
                        plain_row_fields(e.lexpr.as_ref()),
                        plain_row_fields(e.rexpr.as_ref()),
                    ) else {
                        return Facts::default();
                    };
                    if !top_level || l.len() != r.len() {
                        return Facts::default();
                    }
                    let pair = |(a, b): (Option<&ast::Node>, Option<&ast::Node>)| {
                        walk(a, false).union(walk(b, false))
                    };
                    match op.as_str() {
                        "=" => l
                            .iter()
                            .map(Option::as_ref)
                            .zip(r.iter().map(Option::as_ref))
                            .fold(Facts::default(), |acc, p| acc.union(pair(p))),
                        "<>" => {
                            let mut acc: Option<Facts> = None;
                            for p in l
                                .iter()
                                .map(Option::as_ref)
                                .zip(r.iter().map(Option::as_ref))
                            {
                                let f = pair(p);
                                acc = Some(match acc {
                                    None => f,
                                    Some(a) => a.intersect(&f),
                                });
                            }
                            acc.unwrap_or_default()
                        }
                        "<" | "<=" | ">" | ">=" => l
                            .iter()
                            .map(Option::as_ref)
                            .zip(r.iter().map(Option::as_ref))
                            .next()
                            .map(pair)
                            .unwrap_or_default(),
                        _ => Facts::default(),
                    }
                }
                // `(a, b) IN ((1, 2), (5, -5))`: an OR of row equalities,
                // each needing every field of the left row non-NULL.
                K::AexprIn if is_row(e.lexpr.as_ref()) => {
                    let items_are_rows = matches!(
                        e.rexpr.as_ref(),
                        Some(ast::Node::List(l))
                            if !l.items.is_empty()
                                && l.items.iter().all(|i| plain_row_fields(i.as_ref()).is_some())
                    );
                    match plain_row_fields(e.lexpr.as_ref()) {
                        Some(fields) if top_level && op == "=" && items_are_rows => {
                            fields.iter().fold(Facts::default(), |acc, a| {
                                acc.union(walk(a.as_ref(), false))
                            })
                        }
                        _ => Facts::default(),
                    }
                }
                K::AexprOp => {
                    let strict =
                        walk_opt(e.lexpr.as_ref(), false).union(walk_opt(e.rexpr.as_ref(), false));
                    if top_level {
                        strict
                            .union(comparison_facts(e, scope, log, snapshot))
                            .union(equivalence_facts(e, scope, log, snapshot))
                    } else {
                        strict
                    }
                }
                K::AexprLike | K::AexprIlike | K::AexprSimilar => {
                    walk_opt(e.lexpr.as_ref(), false).union(walk_opt(e.rexpr.as_ref(), false))
                }
                // `x IN (…)` is `x = ANY(ARRAY[…])` or an OR of `x = item`
                // (NOT IN: `<> ALL` over a non-empty array, or an AND of
                // `x <> item`), BETWEEN an AND / OR of comparisons on `x`:
                // all strict in `x` at any level.
                K::AexprIn if top_level => walk_opt(e.lexpr.as_ref(), false)
                    .union(comparison_facts(e, scope, log, snapshot)),
                K::AexprBetween if top_level => {
                    walk_opt(e.lexpr.as_ref(), false).union(between_facts(e, scope, log, snapshot))
                }
                K::AexprIn
                | K::AexprBetween
                | K::AexprNotBetween
                | K::AexprBetweenSym
                | K::AexprNotBetweenSym => walk_opt(e.lexpr.as_ref(), false),
                // `is_strict_saop(expr, falseOK)`: `x op ANY(array)` is NULL
                // or FALSE when an input is NULL, enough at the top level.
                // `ALL` over an empty array is TRUE whatever `x` is (over
                // one known to have an element, see `nonempty_array_facts`).
                K::AexprOpAny if top_level => walk_opt(e.lexpr.as_ref(), false)
                    .union(walk_opt(e.rexpr.as_ref(), false))
                    .union(comparison_facts(e, scope, log, snapshot)),
                K::AexprOpAll if top_level => comparison_facts(e, scope, log, snapshot)
                    .union(nonempty_array_facts(e, scope, snapshot)),
                K::AexprOpAny | K::AexprOpAll => nonempty_array_facts(e, scope, snapshot),
                _ => Facts::default(),
            }
        }
        ast::Node::FuncCall(f) => {
            if !log.is_strict(f.location, StrictNode::Func) {
                return Facts::default();
            }
            f.args.iter().fold(Facts::default(), |acc, a| {
                acc.union(walk(a.as_ref(), false))
            })
        }
        ast::Node::NamedArgExpr(na) => walk_opt(na.arg.as_ref(), top_level),
        ast::Node::TypeCast(tc) => {
            if !log.is_strict(tc.location, StrictNode::Cast) {
                return Facts::default();
            }
            walk_opt(tc.arg.as_ref(), false)
        }
        ast::Node::CollateClause(c) => walk_opt(c.arg.as_ref(), top_level),
        ast::Node::AIndirection(i) => match crate::expr::whole_row_field_column(i, scope) {
            // A field of a FROM entry's whole row is its column.
            Some(column) => walk(Some(&column), top_level),
            // A field or an element of a NULL value is NULL.
            None => walk_opt(i.arg.as_ref(), false),
        },
        ast::Node::NullTest(t) if top_level && t.nulltesttype == ast::NullTestType::IsNotNull => {
            let Some(arg) = t.arg.as_ref() else {
                return Facts::default();
            };
            match arg {
                // `ROW(a, b) IS NOT NULL`: every field is non-NULL.
                ast::Node::RowExpr(r) => r.args.iter().fold(Facts::default(), |acc, a| {
                    acc.union(walk(a.as_ref(), false))
                }),
                // `t IS NOT NULL` over a whole-row reference: so is every
                // column of `t`.
                _ => match whole_row_source(arg, scope) {
                    Some(src) => src
                        .visible_columns()
                        .fold(Facts::rel(&src.alias), |acc, c| {
                            acc.union(Facts::column(&src.alias, &c.name))
                        }),
                    None => {
                        let mut f = walk(t.arg.as_ref(), false);
                        f.exprs.extend(composite_field_keys(arg, scope, snapshot));
                        f
                    }
                },
            }
        }
        // `x IS NULL` TRUE: `x` is NULL (a composite can be a row of NULLs).
        ast::Node::NullTest(t) if top_level && t.nulltesttype == ast::NullTestType::IsNull => {
            match scalar_column(t.arg.as_ref(), scope, snapshot) {
                Some(col) => Facts::null(col),
                None => Facts::default(),
            }
        }
        // `EXISTS (SELECT …)`: what the subquery's WHERE proves of this
        // level's columns, for the rows it returns (a semi-join's quals).
        ast::Node::SubLink(sub)
            if top_level && sub.sub_link_type == ast::SubLinkType::ExistsSublink =>
        {
            log.correlated(sub.location)
        }
        // `x IN (SELECT …)` / `x op ANY (SELECT …)`: like `op ANY(array)`;
        // and, for `k IN (SELECT k FROM t … FOR UPDATE)` over a key of the
        // same table, what the subquery's WHERE proves of the row (noted
        // by `expr::sublink`).
        ast::Node::SubLink(sub) if top_level => {
            let tested = if log.is_strict(sub.location, StrictNode::Sublink) {
                match sub.testexpr.as_ref() {
                    Some(t) => match t {
                        ast::Node::RowExpr(r) => r.args.iter().fold(Facts::default(), |acc, a| {
                            acc.union(walk(a.as_ref(), false))
                        }),
                        _ => walk(sub.testexpr.as_ref(), false),
                    },
                    None => Facts::default(),
                }
            } else {
                Facts::default()
            };
            tested.union(log.correlated(sub.location))
        }
        // Boolean tests that are not TRUE for a NULL input.
        ast::Node::BooleanTest(t)
            if top_level
                && matches!(
                    t.booltesttype,
                    ast::BoolTestType::IsTrue
                        | ast::BoolTestType::IsFalse
                        | ast::BoolTestType::IsNotUnknown
                ) =>
        {
            walk_opt(t.arg.as_ref(), false)
        }
        _ => Facts::default(),
    }
}

/// What `node` *not* being TRUE (FALSE or NULL) proves non-NULL — a CASE
/// branch after `WHEN node`. Only tests that never yield NULL say
/// anything: `x IS NULL` not TRUE is `x` non-NULL, while `x > 0` not TRUE
/// may well be `x` NULL.
pub(crate) fn nonnullable_unless_true(
    node: Option<&ast::Node>,
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
    node: Option<&ast::Node>,
    scope: &Scope,
    log: &StrictLog,
    snapshot: &PgCatalog,
) -> Facts {
    if disabled() {
        return Facts::default();
    }
    let mut facts = match node {
        Some(ast::Node::NullTest(_) | ast::Node::BooleanTest(_)) => {
            nonnullable(node, true, scope, log, snapshot)
        }
        Some(ast::Node::AExpr(e))
            if matches!(
                e.kind,
                ast::AExprKind::AexprDistinct | ast::AExprKind::AexprNotDistinct
            ) =>
        {
            nonnullable(node, true, scope, log, snapshot)
        }
        Some(ast::Node::BoolExpr(b)) => match b.boolop {
            // Not FALSE: no arm is FALSE.
            ast::BoolExprType::AndExpr => b.args.iter().fold(Facts::default(), |acc, a| {
                acc.union(nonnullable_unless_false(a.as_ref(), scope, log, snapshot))
            }),
            // Not FALSE: some arm isn't.
            ast::BoolExprType::OrExpr => intersection(b.args.iter().map(Option::as_ref), |a| {
                nonnullable_unless_false(a, scope, log, snapshot)
            }),
            // `NOT e` not FALSE: `e` not TRUE.
            ast::BoolExprType::NotExpr => match b.args.as_slice() {
                [e] => nonnullable_unless_true(e.as_ref(), scope, log, snapshot),
                _ => Facts::default(),
            },
        },
        // A comparison with constants not FALSE: what it says holds of its
        // column unless that is NULL (`kind = 'a'` not FALSE: `kind` is
        // NULL or `'a'`).
        // An `IN` list or `= ANY` array holding a NULL is never FALSE (NULL
        // where no other item matches): not FALSE, it says nothing.
        Some(ast::Node::AExpr(e)) if null_item(e, scope, snapshot) => Facts::default(),
        Some(ast::Node::ColumnRef(_) | ast::Node::AExpr(_)) => {
            match value_pred(node, scope, log, snapshot) {
                Some((col, p)) => Facts::pred(col, p),
                None => Facts::default(),
            }
        }
        _ => Facts::default(),
    };
    if !is_and_or(node) {
        facts = facts.union(substituted(node, scope, log, snapshot, |v| {
            *v == subst::Val::Bool(false)
        }));
    }
    facts
}

/// Whether `IN` list or `op ANY` / `op ALL` array `e` holds a NULL item
/// (which [`column_pred`] leaves out of an `IN` / `ANY`: it never makes the
/// test TRUE, only keeps it from being FALSE).
fn null_item(e: &ast::AExpr, scope: &Scope, snapshot: &PgCatalog) -> bool {
    use ast::AExprKind as K;
    match e.kind {
        K::AexprIn => matches!(e.rexpr.as_ref(), Some(ast::Node::List(l))
            if l.items.iter().any(|i| matches!(i, Some(ast::Node::AConst(c)) if c.isnull))),
        K::AexprOpAny | K::AexprOpAll => {
            let Some((_, t)) = e
                .lexpr
                .as_ref()
                .and_then(|l| compared_column(l, scope, snapshot))
            else {
                return false;
            };
            array_constants(e.rexpr.as_ref(), t, false, snapshot).is_none()
                && array_constants(e.rexpr.as_ref(), t, true, snapshot).is_some()
        }
        _ => false,
    }
}

fn unless_true_node(
    node: Option<&ast::Node>,
    scope: &Scope,
    log: &StrictLog,
    snapshot: &PgCatalog,
) -> Facts {
    let Some(inner) = node else {
        return Facts::default();
    };
    match inner {
        // Never NULL: not TRUE is FALSE.
        ast::Node::AExpr(e) if e.kind == ast::AExprKind::AexprDistinct => {
            distinct_facts(e, false, scope, log, snapshot)
        }
        ast::Node::AExpr(e) if e.kind == ast::AExprKind::AexprNotDistinct => {
            distinct_facts(e, true, scope, log, snapshot)
        }
        // `x IS NOT NULL` not TRUE: `x` is NULL.
        ast::Node::NullTest(t) if t.nulltesttype == ast::NullTestType::IsNotNull => {
            match scalar_column(t.arg.as_ref(), scope, snapshot) {
                Some(col) => Facts::null(col),
                None => Facts::default(),
            }
        }
        ast::Node::NullTest(t) if t.nulltesttype == ast::NullTestType::IsNull => {
            match t.arg.as_ref() {
                // Some field of the row is non-NULL — which one is unknown.
                Some(ast::Node::RowExpr(_)) => Facts::default(),
                // A whole row that isn't NULL may still hold NULL columns:
                // only the entry itself is present.
                Some(a) => match whole_row_source(a, scope) {
                    Some(src) => Facts::rel(&src.alias),
                    None => nonnullable(t.arg.as_ref(), false, scope, log, snapshot),
                },
                None => Facts::default(),
            }
        }
        ast::Node::BooleanTest(t) => {
            if t.arg.is_none() {
                return Facts::default();
            }
            let arg = &t.arg;
            match t.booltesttype {
                // Not (NOT TRUE): TRUE.
                ast::BoolTestType::IsNotTrue => {
                    nonnullable(arg.as_ref(), true, scope, log, snapshot)
                }
                // Not UNKNOWN / not (NOT FALSE), i.e. FALSE: non-NULL.
                ast::BoolTestType::IsUnknown | ast::BoolTestType::IsNotFalse => {
                    nonnullable(arg.as_ref(), false, scope, log, snapshot)
                }
                _ => Facts::default(),
            }
        }
        ast::Node::BoolExpr(b) => match b.boolop {
            // Not TRUE: no arm is TRUE.
            ast::BoolExprType::OrExpr => b.args.iter().fold(Facts::default(), |acc, a| {
                acc.union(nonnullable_unless_true(a.as_ref(), scope, log, snapshot))
            }),
            // Not TRUE: some arm isn't.
            ast::BoolExprType::AndExpr => intersection(b.args.iter().map(Option::as_ref), |a| {
                nonnullable_unless_true(a, scope, log, snapshot)
            }),
            // `NOT e` not TRUE: `e` not FALSE.
            ast::BoolExprType::NotExpr => match b.args.as_slice() {
                [e] => {
                    let f = nonnullable_unless_false(e.as_ref(), scope, log, snapshot);
                    if matches!(e, Some(ast::Node::AExpr(x)) if null_item(x, scope, snapshot)) {
                        return f;
                    }
                    // `NOT e` not TRUE: `e` not FALSE, so what `e` says
                    // holds of its column unless that is NULL.
                    match value_pred(e.as_ref(), scope, log, snapshot) {
                        Some((col, p)) => f.union(Facts::pred(col, p)),
                        None => f,
                    }
                }
                _ => Facts::default(),
            },
        },
        // A comparison with constants not TRUE: a non-NULL column fails it
        // (`kind = 'a'` not TRUE, `kind` is NULL or isn't `'a'`).
        ast::Node::ColumnRef(_) | ast::Node::AExpr(_) => {
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
    n: Option<&ast::Node>,
    scope: &Scope,
    log: &StrictLog,
    snapshot: &PgCatalog,
) -> Option<(Col, ValPred)> {
    match n? {
        n @ ast::Node::ColumnRef(_) => {
            let (col, t) = plain_column(n, scope)?;
            (snapshot.unwrap_domain(t) == crate::pg_catalog::oid::BOOL)
                .then(|| (col, ValPred::In(vec![Literal::boolean(true)])))
        }
        // The operator resolved (and, being strict, is one of the
        // comparison operators `column_pred` reads).
        ast::Node::AExpr(e) if log.is_strict(e.location, StrictNode::Op) => {
            column_pred(e, scope, log, snapshot)
        }
        _ => None,
    }
}

/// `(x).f` for every field `f` of composite column `x`: what `x IS NOT
/// NULL` proves (a row-wise test, TRUE only when every field is non-NULL:
/// `ExecEvalRowNullInt`).
fn composite_field_keys(arg: &ast::Node, scope: &Scope, snapshot: &PgCatalog) -> Vec<String> {
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
            exprs::key(
                Some(ast::Node::AIndirection(NodeBox::new(ast::AIndirection {
                    arg: Some(arg.clone()),
                    indirection: vec![Some(ast::Node::String(NodeBox::new(ast::String {
                        sval: a.attname.clone(),
                    })))],
                })))
                .as_ref(),
            )
        })
        .collect()
}

/// The FROM entry a bare `t` / `t.*` reference names as a whole row.
fn whole_row_source<'s>(n: &ast::Node, scope: &'s Scope) -> Option<&'s crate::scope::TableSource> {
    let ast::Node::ColumnRef(c) = n else {
        return None;
    };
    let parts = crate::expr::extract_string_fields(&c.fields);
    let star = c
        .fields
        .iter()
        .any(|f| matches!(f.as_ref(), Some(ast::Node::AStar(_))));
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
fn column_ref_facts(c: &ast::ColumnRef, scope: &Scope) -> Facts {
    let star = c
        .fields
        .iter()
        .any(|f| matches!(f.as_ref(), Some(ast::Node::AStar(_))));
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
fn scalar_column(n: Option<&ast::Node>, scope: &Scope, snapshot: &PgCatalog) -> Option<Col> {
    let col = plain_column(n?, scope)?;
    (!crate::coerce::is_complex(col.1, snapshot) && col.1 != crate::pg_catalog::oid::RECORD)
        .then_some(col.0)
}

/// The column a plain column reference names, with its type.
pub(crate) fn plain_column(n: &ast::Node, scope: &Scope) -> Option<(Col, crate::oid::PgTypeOid)> {
    // A field of a FROM entry's whole row is its column.
    if let ast::Node::AIndirection(i) = n {
        return plain_column(&crate::expr::whole_row_field_column(i, scope)?, scope);
    }
    let ast::Node::ColumnRef(c) = n else {
        return None;
    };
    if c.fields
        .iter()
        .any(|f| matches!(f.as_ref(), Some(ast::Node::AStar(_))))
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
pub(crate) fn literal(n: Option<&ast::Node>) -> Option<Literal> {
    use typedpg_pg_query::ast::AConstValue;
    match n? {
        ast::Node::AConst(c) if !c.isnull => match c.val.as_ref()? {
            AConstValue::Integer(i) => Some(Literal {
                text: i.ival.to_string(),
                kind: LitKind::Integer,
            }),
            AConstValue::String(s) => Some(Literal {
                text: s.sval.clone(),
                kind: LitKind::String,
            }),
            AConstValue::Boolean(b) => Some(Literal::boolean(b.boolval)),
            AConstValue::Float(f) => Some(Literal {
                text: f.fval.clone(),
                kind: LitKind::Numeric,
            }),
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
/// CHECK on the column can use. Nor one cast with a type modifier
/// ([`cast_target`]): `'abc'::varchar(2)` is `'ab'`. A cast of a numeric
/// or boolean literal to the type it already has is the bare literal:
/// `coerce_type` hands the constant back, so `5::int` is `5`.
pub(crate) fn literal_for(
    n: Option<&ast::Node>,
    column_type: crate::oid::PgTypeOid,
    snapshot: &PgCatalog,
) -> Option<Literal> {
    match n? {
        ast::Node::TypeCast(tc) => {
            let target = cast_target(tc, snapshot)?;
            let no_op = match tc.arg.as_ref() {
                Some(ast::Node::AConst(c)) => {
                    crate::expr::literals::infer_a_const(c).is_ok_and(|t| {
                        t.type_oid == target && t.type_oid != crate::pg_catalog::oid::UNKNOWN
                    })
                }
                _ => false,
            };
            (target == column_type || no_op)
                .then(|| literal(tc.arg.as_ref()))
                .flatten()
        }
        _ => literal(n),
    }
}

/// The type an explicit cast converts to, when the conversion keeps a
/// constant's value as written: `None` when it applies a type modifier —
/// written (`varchar(2)`, `numeric(1,-1)`, `char` — which is `char(1)`) or
/// a domain's (`CREATE DOMAIN d AS varchar(2)`) — which may truncate,
/// round or pad the value.
pub(crate) fn cast_target(
    tc: &ast::TypeCast,
    snapshot: &PgCatalog,
) -> Option<crate::oid::PgTypeOid> {
    let type_name = tc.type_name.as_ref()?;
    if !type_name.typmods.is_empty() {
        return None;
    }
    let target = crate::ddl::util::resolve_type_name(type_name, snapshot)?;
    // An array of a domain with a modifier applies it to each element.
    let elem = snapshot
        .pg_type
        .get(&target)
        .and_then(|t| t.typelem)
        .unwrap_or(target);
    (snapshot.effective_typmod(target, None).is_none()
        && snapshot.effective_typmod(elem, None).is_none())
    .then_some(target)
}

/// `col op constant` (either way round), `col IN (…)`, `col op ANY (…)`
/// and `col op ALL (…)` over constants, TRUE: what it says of the column
/// (equal to a constant, one or none of several, ordered against one) —
/// or, over an array of other elements, what [`element_preds`] says.
fn comparison_facts(e: &ast::AExpr, scope: &Scope, log: &StrictLog, snapshot: &PgCatalog) -> Facts {
    let (c, preds) = match column_pred(e, scope, log, snapshot) {
        Some((c, p)) => (c, vec![p]),
        None => match element_preds(e, scope, log, snapshot) {
            Some(found) => found,
            None => return Facts::default(),
        },
    };
    let mut f = Facts::default();
    for p in preds {
        if let ValPred::In(vs) = &p
            && let [v] = vs.as_slice()
        {
            f.equals.insert(c.clone(), v.clone());
        }
        f.preds.push((c.clone(), p));
    }
    f
}

/// `c op ANY (arr)` TRUE over an array whose elements are known
/// ([`StrictLog::note_array_elements`]: `ARRAY[kind]`, a column of an
/// array of a domain, …) but are not constants: `c` is equal, or ordered
/// against, one of them. `= ANY` leaves `c` what every element is (its
/// values and bounds), `< ANY` / `<= ANY` below the elements' upper bound,
/// `> ANY` / `>= ANY` above their lower bound. Only for a column whose
/// values compare with the elements as they are: of one type (domains
/// read as their base type), both integers, or both `text` / `varchar`.
/// Nothing of `op ALL` (TRUE over an empty array whatever `c` is), nor
/// read under a negation: `c` equal to no element says nothing of what
/// the elements may be.
fn element_preds(
    e: &ast::AExpr,
    scope: &Scope,
    log: &StrictLog,
    snapshot: &PgCatalog,
) -> Option<(Col, Vec<ValPred>)> {
    use crate::pg_catalog::oid;
    if e.kind != ast::AExprKind::AexprOpAny || !log.trusts(e.location, StrictNode::BuiltinCompare) {
        return None;
    }
    let (c, t) = plain_column(e.lexpr.as_ref()?, scope)?;
    let elems = log.array_elements(e.location)?;
    let (ct, et) = (
        snapshot.unwrap_domain(t),
        snapshot.unwrap_domain(elems.elem_type),
    );
    let int = |t| [oid::INT2, oid::INT4, oid::INT8].contains(&t);
    let string = |t| [oid::TEXT, oid::VARCHAR].contains(&t);
    if !(ct == et || (int(ct) && int(et)) || (string(ct) && string(et))) {
        return None;
    }
    let r = &elems.refine;
    // The elements' bounds: `(bound, inclusive)`.
    let (lo, hi) = if int(et) {
        let b = r.int_bounds().unwrap_or_default();
        let lit = |v: i128| {
            (
                Literal {
                    text: v.to_string(),
                    kind: LitKind::Integer,
                },
                true,
            )
        };
        (b.lo.map(lit), b.hi.map(lit))
    } else if let Some(kind) = crate::refine_bounds::OrdKind::of(et, snapshot)
        && let Some(b) = r.bounds.filter(|b| b.kind == kind)
    {
        let lit = |b: crate::refine_bounds::OrdBound| (Literal::point(kind, b.point), b.inclusive);
        (b.lo.map(lit), b.hi.map(lit))
    } else {
        (None, None)
    };
    let below = |(v, inclusive): (Literal, bool)| {
        ValPred::Cmp(if inclusive { CmpOp::Le } else { CmpOp::Lt }, v)
    };
    let above = |(v, inclusive): (Literal, bool)| {
        ValPred::Cmp(if inclusive { CmpOp::Ge } else { CmpOp::Gt }, v)
    };
    let op = crate::expr::extract_string_fields(&e.name).join(".");
    let mut preds = Vec::new();
    match op.as_str() {
        "=" => {
            // (The collation was deterministic: see `ArrayElements`.)
            let kind = match crate::refine::Exact::of(elems.elem_type, None, snapshot) {
                Some(crate::refine::Exact::Int) => Some(LitKind::Integer),
                Some(crate::refine::Exact::Bool) => Some(LitKind::Boolean),
                Some(crate::refine::Exact::Text) => Some(LitKind::String),
                None => None,
            };
            if let (Some(vs), Some(kind)) = (&r.values, kind) {
                preds.push(ValPred::In(
                    vs.iter()
                        .map(|v| Literal {
                            text: v.clone(),
                            kind,
                        })
                        .collect(),
                ));
            }
            preds.extend(lo.map(above));
            preds.extend(hi.map(below));
        }
        // Below some element: below their upper bound (strictly, for `<`).
        "<" => preds.extend(hi.map(|(v, _)| ValPred::Cmp(CmpOp::Lt, v))),
        "<=" => preds.extend(hi.map(below)),
        ">" => preds.extend(lo.map(|(v, _)| ValPred::Cmp(CmpOp::Gt, v))),
        ">=" => preds.extend(lo.map(above)),
        _ => {}
    }
    (!preds.is_empty()).then_some((c, preds))
}

/// `x op ANY / ALL (arr)` non-NULL over an array that surely has an
/// element (of a non-array `x`): `x op e` is strict, so `x` is non-NULL —
/// `is_strict_saop` over a non-empty array constant. (Over an empty array
/// ANY is FALSE and ALL TRUE even for a NULL `x`.)
fn nonempty_array_facts(e: &ast::AExpr, scope: &Scope, snapshot: &PgCatalog) -> Facts {
    let Some((c, t)) = e.lexpr.as_ref().and_then(|l| plain_column(l, scope)) else {
        return Facts::default();
    };
    if crate::expr::array_element_type(snapshot, t).is_some() || !array_nonempty(e.rexpr.as_ref()) {
        return Facts::default();
    }
    Facts::column(&c.0, &c.1)
}

/// Whether the array of scalars `n` (an `ARRAY[…]` or an array literal,
/// cast or not) surely has an element. `ARRAY[ARRAY[]::int[]]` has none,
/// nor may `ARRAY[a]` over an array `a`: only constants that are no
/// arrays count as elements of a constructor.
fn array_nonempty(n: Option<&ast::Node>) -> bool {
    use typedpg_pg_query::ast::AConstValue;
    match n {
        Some(ast::Node::AArrayExpr(a)) => {
            let scalar = |i: &Option<ast::Node>| match i {
                Some(ast::Node::AConst(c)) if !c.isnull => match c.val.as_ref() {
                    Some(
                        AConstValue::Integer(_) | AConstValue::Float(_) | AConstValue::Boolean(_),
                    ) => true,
                    // A string beside only constants is an element, not an
                    // array literal of a sub-array.
                    Some(AConstValue::String(_)) => a
                        .elements
                        .iter()
                        .all(|i| matches!(i, Some(ast::Node::AConst(_)))),
                    _ => false,
                },
                Some(n @ ast::Node::AArrayExpr(_)) => array_nonempty(Some(n)),
                _ => false,
            };
            a.elements.iter().any(scalar)
        }
        Some(ast::Node::TypeCast(tc)) => array_nonempty(tc.arg.as_ref()),
        Some(ast::Node::AConst(c)) if !c.isnull => match c.val.as_ref() {
            Some(AConstValue::String(s)) => {
                crate::array_input::array_dimensions(&s.sval, b',').is_some_and(|d| !d.is_empty())
            }
            _ => false,
        },
        _ => false,
    }
}

/// `a.x = b.y` over two plain columns, TRUE: each is the other's value
/// (both ways, see [`Facts::same`]) — when the `=` is the built-in one
/// (`trust`), the columns are of one type (domains read as their base
/// type) or both integers (`int2`, `int4`, `int8`: compared exactly) or
/// both `text` / `varchar` (`texteq`; not `char(n)`, whose `bpchar = text`
/// compares the value with its padding trimmed), and
/// equal values of the type are the same value wherever a column's
/// predicates read it: not under a nondeterministic collation (`'a'` equal
/// to `'A'`) on either column. Values of other types are compared by the
/// [`checks::Space`] of the column read, which knows its type's equality
/// (`numeric` `1` and `1.0`: only bounds, never a value set). Not a
/// column compared with a parameter or an expression.
fn equivalence_facts(
    e: &ast::AExpr,
    scope: &Scope,
    trust: &dyn Trust,
    snapshot: &PgCatalog,
) -> Facts {
    use crate::pg_catalog::oid;
    let mut f = Facts::default();
    if e.kind != ast::AExprKind::AexprOp
        || !trust.trusts(e.location, StrictNode::BuiltinCompare)
        || crate::expr::extract_string_fields(&e.name).join(".") != "="
    {
        return f;
    }
    let column = |n: Option<&ast::Node>| {
        let (col, t) = plain_column(n?, scope)?;
        let collation = scope
            .resolve_column(Some(&col.0), &col.1, None)
            .ok()
            .and_then(|c| c.collation);
        Some((col, snapshot.unwrap_domain(t), collation))
    };
    let (Some((a, ta, ca)), Some((b, tb, cb))) =
        (column(e.lexpr.as_ref()), column(e.rexpr.as_ref()))
    else {
        return f;
    };
    let int = |t| [oid::INT2, oid::INT4, oid::INT8].contains(&t);
    let string = |t| [oid::TEXT, oid::VARCHAR].contains(&t);
    if a == b
        || !(ta == tb || (int(ta) && int(tb)) || (string(ta) && string(tb)))
        || !crate::refine::deterministic(ca, snapshot)
        || !crate::refine::deterministic(cb, snapshot)
    {
        return f;
    }
    f.same.push((a.clone(), b.clone()));
    f.same.push((b, a));
    f
}

/// `col BETWEEN a AND b` over constants, TRUE: `col >= a AND col <= b`
/// (when both comparisons are the built-in ones).
fn between_facts(e: &ast::AExpr, scope: &Scope, log: &StrictLog, snapshot: &PgCatalog) -> Facts {
    let (Some(l), Some(r)) = (e.lexpr.as_ref(), e.rexpr.as_ref()) else {
        return Facts::default();
    };
    if !log.trusts(e.location, StrictNode::BuiltinCompare) {
        return Facts::default();
    }
    let (Some((c, t)), ast::Node::List(list)) = (compared_column(l, scope, snapshot), r) else {
        return Facts::default();
    };
    let bound = |i: usize| {
        list.items
            .get(i)
            .and_then(|n| literal_for(n.as_ref(), t, snapshot))
    };
    let (Some(a), Some(b)) = (bound(0), bound(1)) else {
        return Facts::default();
    };
    Facts::pred(c.clone(), ValPred::Cmp(CmpOp::Ge, a))
        .union(Facts::pred(c, ValPred::Cmp(CmpOp::Le, b)))
}

/// A plain column ([`plain_column`]), or one cast to the type and typmod
/// it already has: coerce_type adds no node for that cast, so
/// `name::text = 'x'` compares `name` itself.
fn compared_column(
    n: &ast::Node,
    scope: &Scope,
    snapshot: &PgCatalog,
) -> Option<(Col, crate::oid::PgTypeOid)> {
    let ast::Node::TypeCast(tc) = n else {
        return plain_column(n, scope);
    };
    let (col, t) = plain_column(tc.arg.as_ref()?, scope)?;
    let type_name = tc.type_name.as_ref()?;
    let target = crate::ddl::util::resolve_type_name(type_name, snapshot)?;
    let written = if type_name.typmods.is_empty() {
        None
    } else {
        crate::typmod::encode(snapshot, target, type_name).ok()?
    };
    let typmod = scope
        .resolve_column(Some(&col.0), &col.1, None)
        .ok()?
        .typmod;
    (target == t && written.filter(|m| *m >= 0) == typmod.filter(|m| *m >= 0)).then_some((col, t))
}

/// What a comparison of a plain column with constants, TRUE, says of the
/// column's value ([`literal_for`] says which constants qualify): `c = v`,
/// `c <> v`, `c < v` (and `v > c`), `c IN (…)`, `c NOT IN (…)`,
/// `c op ANY (…)` and `c op ALL (…)` over an `ARRAY[…]` of constants or
/// an array literal (`'{a,b}'`). A NULL in an IN list or an `ANY` array
/// never makes it TRUE and is dropped; one in a NOT IN list or an `ALL`
/// array makes it never TRUE, and nothing is said. Only for the built-in
/// comparisons (`trust` says which the expression resolved to): a
/// user-defined `=` may hold for values that differ.
pub(crate) fn column_pred(
    e: &ast::AExpr,
    scope: &Scope,
    trust: &dyn Trust,
    snapshot: &PgCatalog,
) -> Option<(Col, ValPred)> {
    use ast::AExprKind as K;
    if !trust.trusts(e.location, StrictNode::BuiltinCompare) {
        return None;
    }
    let op = crate::expr::extract_string_fields(&e.name).join(".");
    let (l, r) = (e.lexpr.as_ref()?, e.rexpr.as_ref()?);
    match e.kind {
        K::AexprOp => {
            let (c, v, flipped) = match (
                compared_column(l, scope, snapshot),
                compared_column(r, scope, snapshot),
            ) {
                (Some((c, t)), None) => (c, literal_for(e.rexpr.as_ref(), t, snapshot)?, false),
                (None, Some((c, t))) => (c, literal_for(e.lexpr.as_ref(), t, snapshot)?, true),
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
            let (c, t) = compared_column(l, scope, snapshot)?;
            let ast::Node::List(list) = r else {
                return None;
            };
            let positive = match op.as_str() {
                "=" => true,
                "<>" => false,
                _ => return None,
            };
            let vs = constants(list.items.iter().map(Option::as_ref), t, positive, snapshot)?;
            Some((
                c,
                if positive {
                    ValPred::In(vs)
                } else {
                    ValPred::NotIn(vs)
                },
            ))
        }
        // `c op ANY (arr)` is an OR of `c op v` over the array's non-NULL
        // elements (a NULL one is never TRUE: TRUE exactly when some other
        // element makes it so); `c op ALL (arr)` the AND of them, when no
        // element is NULL. An empty array makes ANY FALSE and ALL TRUE, as
        // an empty OR and an empty AND are.
        kind @ (K::AexprOpAny | K::AexprOpAll) => {
            let (c, t) = compared_column(l, scope, snapshot)?;
            let any = kind == K::AexprOpAny;
            let cmp = match op.as_str() {
                "=" | "<>" => None,
                other => Some(CmpOp::parse(other)?),
            };
            let one = |v: Literal| match cmp {
                Some(cmp) => ValPred::Cmp(cmp, v),
                None if op == "=" => ValPred::In(vec![v]),
                None => ValPred::NotIn(vec![v]),
            };
            let vs = array_constants(e.rexpr.as_ref(), t, any, snapshot)?;
            let p = match (any, op.as_str()) {
                (true, "=") => ValPred::In(vs),
                (false, "<>") => ValPred::NotIn(vs),
                _ => {
                    let mut ps: Vec<ValPred> = vs.into_iter().map(one).collect();
                    match ps.len() {
                        1 => ps.pop()?,
                        n if any && n > MAX_ARMS => return None,
                        _ if any => ValPred::AnyOf(ps.into_iter().map(|p| vec![p]).collect()),
                        _ => ValPred::AnyOf(vec![ps]),
                    }
                }
            };
            Some((c, p))
        }
        _ => None,
    }
}

/// The constants of an IN list or an array, for a column of type `t`
/// (NULLs dropped when `drop_nulls`, else refused).
pub(crate) fn constants<'n>(
    items: impl Iterator<Item = Option<&'n ast::Node>>,
    t: crate::oid::PgTypeOid,
    drop_nulls: bool,
    snapshot: &PgCatalog,
) -> Option<Vec<Literal>> {
    let mut out = Vec::new();
    for i in items {
        if let Some(ast::Node::AConst(c)) = i
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
/// compared with a column of type `t`. An array literal is read as
/// `array_in` reads it (quotes, escapes, nesting, explicit bounds, the
/// type's delimiter), each element as the string constant its input
/// function gets; `None` when it doesn't parse.
pub(crate) fn array_constants(
    n: Option<&ast::Node>,
    t: crate::oid::PgTypeOid,
    drop_nulls: bool,
    snapshot: &PgCatalog,
) -> Option<Vec<Literal>> {
    use typedpg_pg_query::ast::AConstValue;
    match n? {
        ast::Node::AArrayExpr(a) => constants(
            a.elements.iter().map(Option::as_ref),
            t,
            drop_nulls,
            snapshot,
        ),
        ast::Node::TypeCast(tc) => {
            let target = cast_target(tc, snapshot)?;
            if snapshot.pg_type.get(&t).and_then(|ty| ty.typarray) != Some(target) {
                return None;
            }
            match tc.arg.as_ref()? {
                ast::Node::AConst(_) => array_constants(tc.arg.as_ref(), t, drop_nulls, snapshot),
                _ => None,
            }
        }
        ast::Node::AConst(c) if !c.isnull => {
            let Some(AConstValue::String(s)) = c.val.as_ref() else {
                return None;
            };
            // `array_in`'s walk: each element de-quoted and de-escaped, as
            // the element type's input function gets it — what the same
            // text written as a string constant of that type is.
            let delim = crate::literal_input::array_delimiter(t, snapshot)?;
            let mut out = Vec::new();
            let mut null = false;
            crate::array_input::parse_array(&s.sval, delim, &mut |e| {
                match e {
                    None => null = true,
                    Some(text) => {
                        let v = Literal {
                            text: text.to_owned(),
                            kind: LitKind::String,
                        };
                        if !out.contains(&v) {
                            out.push(v);
                        }
                    }
                }
                Ok(())
            })
            .ok()?;
            (drop_nulls || !null).then_some(out)
        }
        _ => None,
    }
}

/// How many of `num_nonnulls(args)` / `num_nulls(args)` a comparison with
/// an integer constant allows non-NULL: `(at least, at most)` — when the
/// call is `pg_catalog`'s (`trust` says what it resolved to) and so is
/// the comparison.
pub(crate) fn null_count_bounds<'e>(
    e: &'e ast::AExpr,
    trust: &dyn Trust,
) -> Option<(&'e [Option<ast::Node>], i64, i64)> {
    if !trust.trusts(e.location, StrictNode::BuiltinCompare) {
        return None;
    }
    let op = crate::expr::extract_string_fields(&e.name).join(".");
    let (l, r) = (e.lexpr.as_ref()?, e.rexpr.as_ref()?);
    // `f(args) op n` or `n op f(args)` (flipped).
    let (call, n, op) = match (l, literal(e.rexpr.as_ref())) {
        (ast::Node::FuncCall(fc), Some(v)) if v.is_integer() => {
            (fc, v.text.parse::<i64>().ok()?, op)
        }
        _ => match (r, literal(e.lexpr.as_ref())) {
            (ast::Node::FuncCall(fc), Some(v)) if v.is_integer() => {
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
    if call.func_variadic
        || call.agg_filter.is_some()
        || call.over.is_some()
        || !trust.trusts(call.location, StrictNode::NullCount)
    {
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
fn null_count_facts(e: &ast::AExpr, scope: &Scope, log: &StrictLog) -> Option<Facts> {
    let (args, at_least, _) = null_count_bounds(e, log)?;
    let cols: Option<BTreeSet<Col>> = args
        .iter()
        .map(|a| {
            a.as_ref()
                .and_then(|a| plain_column(a, scope))
                .map(|(c, _)| c)
        })
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
