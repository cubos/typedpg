//! Comparing stored expressions the way PostgreSQL does: on their
//! *transformed* trees, not on what was written.
//!
//! PG keeps a CHECK constraint, a column default, an index expression or
//! predicate and a statistics expression cooked (`transformExpr` and the
//! coercions around it), and compares two of them with `equal()` on those
//! trees (MergeWithExistingConstraint, MergeCheckConstraint,
//! MergeAttributes' defaults, CompareIndexInfo, CreateStatistics) — or by
//! deparsing them (constraints_equivalent), which shows implicit and
//! explicit casts alike. So `a > '0'` is `a > 0` for an integer column (the
//! untyped literal is coerced to an integer constant while parsing),
//! `'x'::text` is `'x'`, `a::int` is `a`, `a IN (1, 2)` is
//! `a = ANY (ARRAY[1, 2])` and `OPERATOR(pg_catalog.>)` is `>` — while
//! `a > 1 + 1` is not `a > 2` (the planner folds constants, parse analysis
//! doesn't), nor `a::bigint > 0` `a > 0`.
//!
//! The analyzer keeps the parse tree; [`cook`] analyzes it over its
//! relation, recording what each node resolved to ([`CookLog`]), and
//! builds a [`Cooked`] tree of the parts that matter to `equal()`:
//! operators and routines by OID, literals as typed constants, casts that
//! change nothing dropped, the coercions PG inserts made explicit. Whatever
//! it doesn't model stays a parse tree compared as written ([`Kind::Raw`]),
//! so two expressions compare equal only when PG's trees certainly are.
//! An index's expressions and predicate reach CompareIndexInfo from the
//! relcache, which folds their constants (RelationGetIndexExpressions /
//! RelationGetIndexPredicate run `eval_const_expressions`), so there
//! `(a + (1 + 1))` is `(a + 2)`: [`same_index_expr`] folds the cooked trees
//! first, as far as [`Folder`] models it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::Hash;

use typedpg_pg_query::ast;

use crate::node_key::NodeKey;
use crate::oid::{PgClassOid, PgOperatorOid, PgProcOid, PgTypeOid};
use crate::pg_catalog::{PgCatalog, TypType, oid};

/// What the expression inference resolved each node of one tree to,
/// keyed by node identity. A node resolved twice to different things (an
/// inference pass under another goal) is recorded as unknown (`None`).
#[derive(Debug, Default)]
pub(crate) struct CookLog {
    types: RefCell<HashMap<NodeKey<ast::Node>, Option<Ty>>>,
    operators: RefCell<HashMap<NodeKey<ast::AExpr>, Option<PgOperatorOid>>>,
    in_lists: RefCell<HashMap<NodeKey<ast::AExpr>, Option<InList>>>,
    between: RefCell<HashMap<NodeKey<ast::AExpr>, Option<Vec<PgOperatorOid>>>>,
    calls: RefCell<HashMap<NodeKey<ast::FuncCall>, Option<PgProcOid>>>,
    literals: RefCell<HashMap<NodeKey<ast::AConst>, Option<PgTypeOid>>>,
}

fn record<K: Eq + Hash, V: PartialEq>(map: &RefCell<HashMap<K, Option<V>>>, key: K, value: V) {
    map.borrow_mut()
        .entry(key)
        .and_modify(|v| {
            if v.as_ref() != Some(&value) {
                *v = None;
            }
        })
        .or_insert(Some(value));
}

fn recorded<K: Eq + Hash, V: Clone>(map: &RefCell<HashMap<K, Option<V>>>, key: &K) -> Option<V> {
    map.borrow().get(key).cloned().flatten()
}

/// What an IN list became (transformAExprIn): `left op ANY (ARRAY[…])`
/// over the items without columns, coerced to a common type, when there
/// were several; then a comparison by each remaining item (its index in
/// the list, the operator).
type InList = (
    Option<(PgOperatorOid, PgTypeOid)>,
    Vec<(usize, PgOperatorOid)>,
);

impl CookLog {
    pub(crate) fn note_type(&self, node: &ast::Node, type_oid: PgTypeOid, typmod: Option<i32>) {
        record(
            &self.types,
            NodeKey::of(node),
            Ty {
                oid: type_oid,
                typmod,
            },
        );
    }

    pub(crate) fn note_operator(&self, expr: &ast::AExpr, op: PgOperatorOid) {
        record(&self.operators, NodeKey::of(expr), op);
    }

    /// The operator recorded for `expr`, removed: `expr` is a synthetic
    /// node about to be dropped, whose address another may take.
    pub(crate) fn take_operator(&self, expr: &ast::AExpr) -> Option<PgOperatorOid> {
        self.operators
            .borrow_mut()
            .remove(&NodeKey::of(expr))
            .flatten()
    }

    pub(crate) fn note_in_list(
        &self,
        expr: &ast::AExpr,
        fold: Option<(PgOperatorOid, PgTypeOid)>,
        comparisons: Vec<(usize, PgOperatorOid)>,
    ) {
        record(&self.in_lists, NodeKey::of(expr), (fold, comparisons));
    }

    pub(crate) fn note_between(&self, expr: &ast::AExpr, ops: Vec<PgOperatorOid>) {
        record(&self.between, NodeKey::of(expr), ops);
    }

    pub(crate) fn note_call(&self, func: &ast::FuncCall, proc: PgProcOid) {
        record(&self.calls, NodeKey::of(func), proc);
    }

    pub(crate) fn note_literal(&self, literal: &ast::AConst, type_oid: PgTypeOid) {
        record(&self.literals, NodeKey::of(literal), type_oid);
    }
}

/// A node's type and type modifier (`None`: PG's -1).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Ty {
    oid: PgTypeOid,
    typmod: Option<i32>,
}

/// A transformed expression, as far as `equal()` tells two apart. Two
/// compare equal only when PG's trees are (source locations and coercion
/// forms aside, which `equal()` ignores).
#[derive(Clone, Debug)]
pub(crate) struct Cooked {
    kind: Kind,
    /// The node's type (`exprType` / `exprTypmod`), where known. Not
    /// compared: the kind determines it.
    ty: Option<Ty>,
}

#[derive(Clone, Debug)]
enum Kind {
    /// A column of the relation (a `Var`, by name: the relations compared
    /// share their columns' names and types, not their attnums).
    Var(String),
    /// A constant of a type, its value in a canonical text form (`None`:
    /// NULL).
    Const(PgTypeOid, Option<String>),
    /// An `OpExpr`.
    Op(PgOperatorOid, Vec<Cooked>),
    /// A `ScalarArrayOpExpr` (`useOr`).
    ScalarArrayOp(PgOperatorOid, bool, Vec<Cooked>),
    /// A plain `FuncExpr`.
    Func(PgProcOid, Vec<Cooked>),
    /// A coercion to a type: a cast function call, a `RelabelType`, a
    /// `CoerceViaIO`, an `ArrayCoerceExpr`, a `CoerceToDomain` — which one
    /// follows from the two types. Explicit (written as a cast) or implicit
    /// (inserted by the parser): `equal()` doesn't tell them apart, ruleutils
    /// hides an implicit one where it isn't asked to show it.
    Coerce(PgTypeOid, Box<Cooked>, bool),
    /// An `ArrayExpr` of an element type.
    Array(PgTypeOid, Vec<Cooked>),
    /// A `BoolExpr` (AND / OR / NOT).
    Bool(u8, Vec<Cooked>),
    /// A `NullTest` (IS NULL when true).
    NullTest(bool, Box<Cooked>),
    /// A `CaseExpr`: the argument of a simple CASE, each WHEN's condition
    /// (for a simple CASE, the comparison's right side) and result, and the
    /// ELSE result (a NULL constant when there is none).
    Case(Option<Box<Cooked>>, Vec<(Cooked, Cooked)>, Box<Cooked>),
    /// A `CoalesceExpr`.
    Coalesce(Vec<Cooked>),
    /// A `MinMaxExpr`: GREATEST when true, LEAST otherwise.
    MinMax(bool, Vec<Cooked>),
    /// A `NullIfExpr`, by its `=` operator.
    NullIf(PgOperatorOid, Vec<Cooked>),
    /// A `DistinctExpr` (IS DISTINCT FROM), by its `=` operator.
    Distinct(PgOperatorOid, Vec<Cooked>),
    /// A `RowExpr`, cast to a composite type when given.
    Row(Vec<Cooked>, Option<PgTypeOid>),
    /// A `BooleanTest` (`BoolTestType` as an integer).
    BoolTest(u8, Box<Cooked>),
    /// A `CollateExpr`.
    Collate(crate::oid::PgCollationOid, Box<Cooked>),
    /// A `SubscriptingRef`: each `[upper]`, or `[lower:upper]` when a
    /// slice (an omitted bound is `None`).
    Subscript(Box<Cooked>, Vec<(Option<Cooked>, Option<Cooked>, bool)>),
    /// A `FieldSelect`.
    Field(Box<Cooked>, String),
    /// A `FuncExpr` with named arguments, a VARIADIC array (its last
    /// argument) or written in a SQL syntax of its own.
    Call(PgProcOid, Vec<(Option<String>, Cooked)>, CallForm),
    /// Anything else, as written.
    Raw(Option<ast::Node>),
}

/// How a [`Kind::Call`] was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CallForm {
    /// `funcvariadic`: its last argument is the variadic array.
    variadic: bool,
    /// `COERCE_SQL_SYNTAX` (`EXTRACT(… FROM …)`, `SUBSTRING(…)`): not a
    /// difference `equal()` sees.
    sql_syntax: bool,
}

#[cfg(feature = "pg_sanity")]
impl Cooked {
    /// Whether every part of it is transformed: nothing is left as written
    /// (`Kind::Raw`), so equality is `equal()`'s.
    /// This tree as a deparse of it parses back: an array coercion of an
    /// `ArrayExpr` — which ruleutils prints as a cast of the `ARRAY[…]`,
    /// and transformTypeCast pushes into its elements — as the `ArrayExpr`
    /// of the elements coerced. The same values either way.
    pub(crate) fn round_tripped(&self, interp: &PgCatalog) -> Cooked {
        let map = |args: &[Cooked]| args.iter().map(|a| a.round_tripped(interp)).collect();
        let boxed = |a: &Cooked| Box::new(a.round_tripped(interp));
        let kind = match &self.kind {
            Kind::Coerce(t, arg, _) => {
                let elem = crate::coerce::element_type(*t, interp);
                match (&arg.kind, elem) {
                    (Kind::Array(_, items), Some(elem)) => Kind::Array(
                        elem,
                        items
                            .iter()
                            .map(|i| Cooked {
                                kind: Kind::Coerce(elem, Box::new(i.round_tripped(interp)), true),
                                ty: Some(Ty {
                                    oid: elem,
                                    typmod: None,
                                }),
                            })
                            .collect(),
                    ),
                    _ => Kind::Coerce(*t, boxed(arg), true),
                }
            }
            Kind::Op(o, a) => Kind::Op(*o, map(a)),
            Kind::ScalarArrayOp(o, u, a) => Kind::ScalarArrayOp(*o, *u, map(a)),
            Kind::Func(p, a) => Kind::Func(*p, map(a)),
            Kind::Array(t, a) => Kind::Array(*t, map(a)),
            // makeAndExpr / makeOrExpr: an AND (OR) whose left operand is
            // an AND (OR) — `BETWEEN`'s, printed parenthesized — parses
            // back as one, its operands first.
            Kind::Bool(k, a) => {
                let mut args: Vec<Cooked> = map(a);
                let and_or =
                    *k == ast::BoolExprType::AndExpr as u8 || *k == ast::BoolExprType::OrExpr as u8;
                while and_or
                    && let Some(Cooked {
                        kind: Kind::Bool(inner, _),
                        ..
                    }) = args.first()
                    && inner == k
                {
                    let Kind::Bool(_, first) = args.remove(0).kind else {
                        unreachable!()
                    };
                    args.splice(0..0, first);
                }
                Kind::Bool(*k, args)
            }
            Kind::NullTest(n, a) => Kind::NullTest(*n, boxed(a)),
            Kind::Coalesce(a) => Kind::Coalesce(map(a)),
            Kind::MinMax(g, a) => Kind::MinMax(*g, map(a)),
            Kind::NullIf(o, a) => Kind::NullIf(*o, map(a)),
            Kind::Distinct(o, a) => Kind::Distinct(*o, map(a)),
            Kind::Row(a, t) => Kind::Row(map(a), *t),
            Kind::BoolTest(k, a) => Kind::BoolTest(*k, boxed(a)),
            Kind::Collate(c, a) => Kind::Collate(*c, boxed(a)),
            Kind::Field(a, f) => Kind::Field(boxed(a), f.clone()),
            Kind::Case(test, arms, default) => Kind::Case(
                test.as_deref().map(boxed),
                arms.iter()
                    .map(|(w, t)| (w.round_tripped(interp), t.round_tripped(interp)))
                    .collect(),
                boxed(default),
            ),
            other => other.clone(),
        };
        Cooked { kind, ty: self.ty }
    }

    pub(crate) fn fully_cooked(&self) -> bool {
        let all = |args: &[Cooked]| args.iter().all(Cooked::fully_cooked);
        match &self.kind {
            Kind::Raw(_) => false,
            Kind::Var(_) => true,
            // A constant is canonical only where `literal` knows the
            // input function's canonical form: NULL, an integer, a
            // boolean, a string.
            Kind::Const(_, None) => true,
            Kind::Const(t, Some(_)) => [
                oid::INT2,
                oid::INT4,
                oid::INT8,
                oid::BOOL,
                oid::TEXT,
                oid::VARCHAR,
                oid::NAME,
            ]
            .contains(t),
            Kind::Op(_, args)
            | Kind::ScalarArrayOp(_, _, args)
            | Kind::Func(_, args)
            | Kind::Array(_, args)
            | Kind::Bool(_, args)
            | Kind::Coalesce(args)
            | Kind::MinMax(_, args)
            | Kind::NullIf(_, args)
            | Kind::Distinct(_, args)
            | Kind::Row(args, _) => all(args),
            Kind::Coerce(_, arg, _)
            | Kind::NullTest(_, arg)
            | Kind::BoolTest(_, arg)
            | Kind::Collate(_, arg)
            | Kind::Field(arg, _) => arg.fully_cooked(),
            Kind::Case(test, arms, default) => {
                test.as_ref().is_none_or(|t| t.fully_cooked())
                    && arms
                        .iter()
                        .all(|(w, t)| w.fully_cooked() && t.fully_cooked())
                    && default.fully_cooked()
            }
            Kind::Subscript(base, subs) => {
                base.fully_cooked()
                    && subs.iter().all(|(a, b, _)| {
                        a.as_ref().is_none_or(Cooked::fully_cooked)
                            && b.as_ref().is_none_or(Cooked::fully_cooked)
                    })
            }
            Kind::Call(_, args, _) => args.iter().all(|(_, a)| a.fully_cooked()),
        }
    }
}

impl PartialEq for Cooked {
    fn eq(&self, other: &Self) -> bool {
        use typedpg_pg_query::Equal;
        match (&self.kind, &other.kind) {
            (Kind::Var(a), Kind::Var(b)) => a == b,
            (Kind::Const(t1, v1), Kind::Const(t2, v2)) => t1 == t2 && v1 == v2,
            (Kind::Op(o1, a1), Kind::Op(o2, a2)) => o1 == o2 && a1 == a2,
            (Kind::ScalarArrayOp(o1, u1, a1), Kind::ScalarArrayOp(o2, u2, a2)) => {
                o1 == o2 && u1 == u2 && a1 == a2
            }
            (Kind::Func(p1, a1), Kind::Func(p2, a2)) => p1 == p2 && a1 == a2,
            (Kind::Coerce(t1, a1, _), Kind::Coerce(t2, a2, _)) => t1 == t2 && a1 == a2,
            (Kind::Array(t1, e1), Kind::Array(t2, e2)) => t1 == t2 && e1 == e2,
            (Kind::Bool(k1, a1), Kind::Bool(k2, a2)) => k1 == k2 && a1 == a2,
            (Kind::NullTest(n1, a1), Kind::NullTest(n2, a2)) => n1 == n2 && a1 == a2,
            (Kind::Case(a1, w1, d1), Kind::Case(a2, w2, d2)) => a1 == a2 && w1 == w2 && d1 == d2,
            (Kind::Coalesce(a1), Kind::Coalesce(a2)) => a1 == a2,
            (Kind::MinMax(g1, a1), Kind::MinMax(g2, a2)) => g1 == g2 && a1 == a2,
            (Kind::NullIf(o1, a1), Kind::NullIf(o2, a2))
            | (Kind::Distinct(o1, a1), Kind::Distinct(o2, a2)) => o1 == o2 && a1 == a2,
            (Kind::Row(a1, t1), Kind::Row(a2, t2)) => a1 == a2 && t1 == t2,
            (Kind::BoolTest(k1, a1), Kind::BoolTest(k2, a2)) => k1 == k2 && a1 == a2,
            (Kind::Collate(c1, a1), Kind::Collate(c2, a2)) => c1 == c2 && a1 == a2,
            (Kind::Subscript(a1, i1), Kind::Subscript(a2, i2)) => a1 == a2 && i1 == i2,
            (Kind::Field(a1, f1), Kind::Field(a2, f2)) => a1 == a2 && f1 == f2,
            (Kind::Call(p1, a1, f1), Kind::Call(p2, a2, f2)) => {
                p1 == p2 && a1 == a2 && f1.variadic == f2.variadic
            }
            (Kind::Raw(a), Kind::Raw(b)) => a.equal(b),
            _ => false,
        }
    }
}

/// `expr`, an expression over the row of `relid`, as PG transforms it —
/// coerced to `target` (a column default's type) when given. `None` when
/// it doesn't analyze.
pub(crate) fn cook(
    interp: &PgCatalog,
    relid: PgClassOid,
    expr: Option<&ast::Node>,
    target: Option<PgTypeOid>,
) -> Option<Cooked> {
    // Its unqualified names are `pg_catalog`'s (see
    // [`crate::lookup::catalog_first`]).
    crate::lookup::catalog_first(|| cook_under_path(interp, relid, expr, target))
}

/// [`cook`] for a text ruleutils printed under the current search path (the
/// catalog oracle's server deparse): an unqualified name in it is what the
/// path finds, not `pg_catalog`'s.
#[cfg(feature = "pg_sanity")]
pub(crate) fn cook_deparsed(
    interp: &PgCatalog,
    relid: PgClassOid,
    expr: Option<&ast::Node>,
    target: Option<PgTypeOid>,
) -> Option<Cooked> {
    cook_under_path(interp, relid, expr, target)
}

fn cook_under_path(
    interp: &PgCatalog,
    relid: PgClassOid,
    expr: Option<&ast::Node>,
    target: Option<PgTypeOid>,
) -> Option<Cooked> {
    use crate::expr::{TypeGoal, infer_expr};
    let class = interp.pg_class.get(&relid)?;
    let nspname = interp
        .namespace_name(class.relnamespace)
        .unwrap_or("public")
        .to_owned();
    let attrs = interp.attributes_of(relid).to_vec();
    let mut scope = crate::scope::Scope::default();
    scope.add_dml_target(
        interp,
        &class.relname,
        crate::qualified_name::QualifiedName::new(nspname, class.relname.clone()),
        &attrs,
    );
    let null_ctx = crate::nullability::NullabilityContext::default();
    let mut params = crate::param_collector::ParamCollector::default();
    let log = CookLog::default();
    let ctx = crate::expr::Ctx::new(&scope, &null_ctx, interp).cooking(&log);
    // A column reference reads the column whatever relation name
    // qualifies it.
    let expr = crate::ddl::stored_exprs::over_own_row(interp, relid, expr);
    let goal = target.map_or(TypeGoal::NONE, TypeGoal::assignment);
    let (inferred, _) = crate::ddl::depend::collect(|| {
        let _level = crate::resolve::QueryLevel::enter();
        infer_expr(expr.as_ref(), ctx, &mut params, goal)
    });
    inferred.ok()?;
    let cooker = Cooker {
        interp,
        relid,
        log: &log,
    };
    let node = expr.as_ref()?;
    Some(match target {
        // coerce_to_target_type, as the default is stored.
        Some(t) => cooker.arg(node, Some(t)),
        None => cooker.cook(node),
    })
}

struct Cooker<'a> {
    interp: &'a PgCatalog,
    relid: PgClassOid,
    log: &'a CookLog,
}

impl Cooker<'_> {
    fn ty(&self, node: &ast::Node) -> Option<Ty> {
        recorded(&self.log.types, &NodeKey::of(node))
    }

    fn raw(&self, node: &ast::Node) -> Cooked {
        Cooked {
            kind: Kind::Raw(Some(node.clone())),
            ty: self.ty(node),
        }
    }

    fn cook(&self, node: &ast::Node) -> Cooked {
        self.try_cook(node).unwrap_or_else(|| self.raw(node))
    }

    fn cook_opt(&self, node: Option<&ast::Node>) -> Cooked {
        match node {
            Some(n) => self.cook(n),
            None => Cooked {
                kind: Kind::Raw(None),
                ty: None,
            },
        }
    }

    /// Whether a type is one an argument isn't coerced to (a polymorphic
    /// or other pseudo-type: the argument keeps its own type, or one the
    /// call's other arguments decide).
    fn is_pseudo(&self, t: PgTypeOid) -> bool {
        crate::polymorphic::is_polymorphic(t)
            || self
                .interp
                .get_type(t)
                .is_none_or(|ty| ty.typtype == TypType::Pseudo)
    }

    /// A constant of type `t` from the text of an untyped literal
    /// (stringTypeDatum): the value in a canonical form where the analyzer
    /// knows one, else as written (an input function maps one text to one
    /// value). A domain's constant is its base type's, coerced to the
    /// domain (coerce_type → coerce_to_domain).
    fn literal(&self, t: PgTypeOid, text: Option<&str>) -> Option<Cooked> {
        let base = self.interp.unwrap_domain(t);
        let value = match text {
            None => None,
            Some(s) if [oid::INT2, oid::INT4, oid::INT8].contains(&base) => {
                Some(crate::literal_input::parse_pg_integer(s)?.to_string())
            }
            Some(s) if base == oid::BOOL => Some(
                if crate::ddl::reloptions::parse_bool(s)? {
                    "t"
                } else {
                    "f"
                }
                .to_owned(),
            ),
            // A reg* input function: the object it names, however it is
            // named (`'r'` and `'public.r'` are one `regclass` constant),
            // as its bound text — valid input still, which a deparse prints.
            Some(s) if reg_type_name(self.interp, base).is_some() => {
                let typname = reg_type_name(self.interp, base).unwrap_or_default();
                Some(
                    crate::reg_input::resolve(typname, s, self.interp)
                        .and_then(|o| {
                            crate::ddl::stored_exprs::reg_text(self.interp, typname, &o, true)
                        })
                        .unwrap_or_else(|| s.to_owned()),
                )
            }
            Some(s) => Some(s.to_owned()),
        };
        let constant = Cooked {
            kind: Kind::Const(base, value),
            ty: Some(Ty {
                oid: base,
                typmod: None,
            }),
        };
        Some(if base == t {
            constant
        } else {
            coerced(t, constant)
        })
    }

    /// An untyped literal (a string or NULL constant): its text.
    fn untyped(node: &ast::Node) -> Option<(&ast::AConst, Option<&str>)> {
        match node {
            ast::Node::AConst(ac) if ac.isnull => Some((ac, None)),
            ast::Node::AConst(ac) => match &ac.val {
                Some(ast::AConstValue::String(s)) => Some((ac, Some(s.sval.as_str()))),
                _ => None,
            },
            _ => None,
        }
    }

    /// `node` as an argument of declared type `decl` (make_fn_arguments /
    /// coerce_type): an untyped literal becomes a constant of the type,
    /// another type is coerced to it. `None`: a pseudo-type or not known.
    fn arg(&self, node: &ast::Node, decl: Option<PgTypeOid>) -> Cooked {
        // Of a type the analysis doesn't know: whether PG coerces it isn't
        // known either.
        if decl.is_some_and(|d| !self.is_pseudo(d))
            && Self::untyped(node).is_none()
            && self.operand_type(node).is_none()
        {
            return self.raw(node);
        }
        if let Some((ac, text)) = Self::untyped(node) {
            // The declared type when there is one, else the type the
            // inference coerced the literal to.
            let t = decl.filter(|d| !self.is_pseudo(*d)).or_else(|| {
                recorded(&self.log.literals, &NodeKey::of(ac)).filter(|t| *t != oid::UNKNOWN)
            });
            return t
                .and_then(|t| self.literal(t, text))
                .unwrap_or_else(|| self.raw(node));
        }
        self.coerce(self.cook(node), decl)
    }

    /// `c` coerced to `decl` (coerce_type, no type modifier): unchanged
    /// when it is of that type already — or when either type isn't known,
    /// which the rest of the tree then determines.
    fn coerce(&self, c: Cooked, decl: Option<PgTypeOid>) -> Cooked {
        // coerce_type: a COLLATE's argument is coerced, the COLLATE put
        // back on top.
        if let Kind::Collate(collation, inner) = c.kind {
            let inner = self.coerce(*inner, decl);
            return Cooked {
                ty: inner.ty,
                kind: Kind::Collate(collation, Box::new(inner)),
            };
        }
        match (decl, c.ty) {
            (Some(d), Some(t)) if t.oid != d && t.oid != oid::UNKNOWN && !self.is_pseudo(d) => {
                coerced(d, c)
            }
            _ => c,
        }
    }

    fn try_cook(&self, node: &ast::Node) -> Option<Cooked> {
        let ty = self.ty(node);
        let kind = match node {
            // A column, of its own type (BETWEEN and IN infer copies of
            // their left operand, never the node itself).
            ast::Node::ColumnRef(cr) => match cr.fields.as_slice() {
                [Some(ast::Node::String(s))] => {
                    let attr = self.interp.attribute_by_name(self.relid, &s.sval)?;
                    return Some(Cooked {
                        kind: Kind::Var(s.sval.clone()),
                        ty: Some(Ty {
                            oid: attr.atttypid,
                            typmod: attr.atttypmod,
                        }),
                    });
                }
                _ => return None,
            },
            ast::Node::AConst(ac) => return self.constant(ac),
            ast::Node::TypeCast(tc) => return self.cast(tc),
            ast::Node::AExpr(e) => return self.a_expr(node, e),
            ast::Node::FuncCall(fc) => return self.call(node, fc),
            ast::Node::BoolExpr(b) => Kind::Bool(
                b.boolop as u8,
                b.args
                    .iter()
                    .map(|a| match a {
                        // coerce_to_boolean.
                        Some(a) => self.arg(a, Some(oid::BOOL)),
                        None => self.cook_opt(None),
                    })
                    .collect(),
            ),
            ast::Node::NullTest(n) => Kind::NullTest(
                n.nulltesttype == ast::NullTestType::IsNull,
                Box::new(self.cook_opt(n.arg.as_ref())),
            ),
            ast::Node::AArrayExpr(a) => {
                let elem = crate::expr::array_element_type(self.interp, ty?.oid)?;
                return self.array(a, ty?.oid, elem);
            }
            // transformCaseExpr: each result (and the ELSE, a NULL when
            // omitted) coerced to the common type; a WHEN of a simple CASE
            // is `arg = value`, of a searched one a boolean.
            ast::Node::CaseExpr(ce) => {
                let result = ty?.oid;
                let arg = ce.arg.as_ref().map(|a| Box::new(self.cook(a)));
                let mut whens = Vec::new();
                for w in &ce.args {
                    let Some(ast::Node::CaseWhen(w)) = w else {
                        return None;
                    };
                    let cond = w.expr.as_ref()?;
                    let cond = match &arg {
                        Some(a) => {
                            let right = self.operand_type(cond)?;
                            let op = self.equality(a.ty?.oid, right)?;
                            let row = self.interp.pg_operator.get(&op)?;
                            self.arg(cond, Some(row.oprright))
                        }
                        None => self.arg(cond, Some(oid::BOOL)),
                    };
                    whens.push((cond, self.arg(w.result.as_ref()?, Some(result))));
                }
                let default = match ce.defresult.as_ref() {
                    Some(d) => self.arg(d, Some(result)),
                    None => constant(result, None),
                };
                Kind::Case(arg, whens, Box::new(default))
            }
            // transformCoalesceExpr / transformMinMaxExpr: each argument
            // coerced to the common type.
            ast::Node::CoalesceExpr(c) => {
                let result = ty?.oid;
                Kind::Coalesce(
                    c.args
                        .iter()
                        .map(|a| a.as_ref().map(|a| self.arg(a, Some(result))))
                        .collect::<Option<Vec<_>>>()?,
                )
            }
            ast::Node::MinMaxExpr(m) => {
                let result = ty?.oid;
                Kind::MinMax(
                    m.op == ast::MinMaxOp::IsGreatest,
                    m.args
                        .iter()
                        .map(|a| a.as_ref().map(|a| self.arg(a, Some(result))))
                        .collect::<Option<Vec<_>>>()?,
                )
            }
            ast::Node::RowExpr(r) => Kind::Row(
                r.args
                    .iter()
                    .map(|a| a.as_ref().map(|a| self.cook(a)))
                    .collect::<Option<Vec<_>>>()?,
                None,
            ),
            // transformBooleanTest: coerce_to_boolean.
            ast::Node::BooleanTest(b) => Kind::BoolTest(
                b.booltesttype as u8,
                Box::new(self.arg(b.arg.as_ref()?, Some(oid::BOOL))),
            ),
            ast::Node::CollateClause(cc) => {
                let names = crate::expr::extract_string_fields(&cc.collname);
                let (schema, name) = match names.as_slice() {
                    [n] => (None, n.as_str()),
                    [s, n] => (Some(s.as_str()), n.as_str()),
                    _ => return None,
                };
                let collation = self.interp.resolve_collation(schema, name)?.oid;
                let arg = self.cook(cc.arg.as_ref()?);
                return Some(Cooked {
                    ty: arg.ty,
                    kind: Kind::Collate(collation, Box::new(arg)),
                });
            }
            ast::Node::AIndirection(ind) => {
                let arg = self.cook(ind.arg.as_ref()?);
                let items: Vec<&ast::Node> = ind.indirection.iter().flatten().collect();
                if items.iter().all(|i| matches!(i, ast::Node::AIndices(_))) {
                    let mut subscripts = Vec::new();
                    for i in items {
                        let ast::Node::AIndices(i) = i else {
                            return None;
                        };
                        // transformContainerSubscripts: each subscript an
                        // integer.
                        let bound = |b: Option<&ast::Node>| b.map(|b| self.arg(b, Some(oid::INT4)));
                        subscripts.push((
                            bound(i.lidx.as_ref()),
                            bound(i.uidx.as_ref()),
                            i.is_slice,
                        ));
                    }
                    Kind::Subscript(Box::new(arg), subscripts)
                } else if let [ast::Node::String(field)] = items.as_slice() {
                    Kind::Field(Box::new(arg), field.sval.clone())
                } else {
                    return None;
                }
            }
            _ => return None,
        };
        Some(Cooked { kind, ty })
    }

    /// The type of an operand (the inference's; an untyped literal's is
    /// `unknown`).
    fn operand_type(&self, node: &ast::Node) -> Option<PgTypeOid> {
        if Self::untyped(node).is_some() {
            return Some(oid::UNKNOWN);
        }
        // BETWEEN, IN and a simple CASE's WHEN infer copies of an operand,
        // not the node itself: then it is typed as cooked.
        self.ty(node)
            .map(|t| t.oid)
            .or_else(|| self.cook(node).ty.map(|t| t.oid))
    }

    /// The `=` operator over `left` and `right` (make_op).
    fn equality(&self, left: PgTypeOid, right: PgTypeOid) -> Option<PgOperatorOid> {
        match self.interp.find_operator_detailed("=", Some(left), right) {
            crate::lookup::OperatorMatch::Found(op) => Some(op.oid),
            _ => None,
        }
    }

    /// make_const: an integer is an `int4` constant, a numeric literal an
    /// `int8` one when it fits, else `numeric`; TRUE / FALSE are `bool`.
    /// An untyped string literal is the constant of the type it was
    /// coerced to.
    fn constant(&self, ac: &ast::AConst) -> Option<Cooked> {
        let (t, value) = match ac.val.as_ref()? {
            _ if ac.isnull => return None,
            ast::AConstValue::Integer(i) => (oid::INT4, i.ival.to_string()),
            ast::AConstValue::Float(f) => match f.fval.parse::<i64>() {
                Ok(i) if !f.fval.contains(['.', 'e', 'E']) => (oid::INT8, i.to_string()),
                _ => (oid::NUMERIC, f.fval.clone()),
            },
            ast::AConstValue::Boolean(b) => (oid::BOOL, if b.boolval { "t" } else { "f" }.into()),
            // `B'101'` / `X'1F'`: a `bit` constant of that many bits.
            ast::AConstValue::BitString(b) => (BIT, b.bsval.clone()),
            ast::AConstValue::String(s) => {
                let t = recorded(&self.log.literals, &NodeKey::of(ac))
                    .filter(|t| *t != oid::UNKNOWN)?;
                return self.literal(t, Some(&s.sval));
            }
        };
        Some(Cooked {
            kind: Kind::Const(t, Some(value)),
            ty: Some(Ty {
                oid: t,
                typmod: None,
            }),
        })
    }

    /// transformTypeCast (no type modifier: [`crate::nonnull::cast_target`]
    /// refuses those). An untyped literal becomes a constant of the type;
    /// `ARRAY[…]` is built of that element type; a value of the type
    /// already is left alone, unless its type modifier is to be dropped
    /// (coerce_type_typmod's `RelabelType`).
    fn cast(&self, tc: &ast::TypeCast) -> Option<Cooked> {
        if let Some(tn) = tc.type_name.as_ref()
            && !tn.typmods.is_empty()
        {
            return self.typmod_cast(tc, tn);
        }
        let target = crate::nonnull::cast_target(tc, self.interp)?;
        let arg = tc.arg.as_ref()?;
        if let Some((_, text)) = Self::untyped(arg) {
            return self.literal(target, text);
        }
        if let ast::Node::AArrayExpr(a) = arg {
            let elem = crate::expr::array_element_type(self.interp, target)?;
            return self.array(a, target, elem);
        }
        // coerce_record_to_complex: a ROW cast to a composite type is that
        // type's row (`row_format` EXPLICIT_CAST), each field coerced to
        // its column's type.
        if let ast::Node::RowExpr(r) = arg
            && let Some(relid) = self.interp.pg_type.get(&target).and_then(|t| t.typrelid)
        {
            let columns: Vec<PgTypeOid> = self
                .interp
                .attributes_of(relid)
                .iter()
                .filter(|a| a.attnum > 0)
                .map(|a| a.atttypid)
                .collect();
            if columns.len() != r.args.len() {
                return None;
            }
            let fields = r
                .args
                .iter()
                .zip(columns)
                .map(|(a, t)| a.as_ref().map(|a| self.arg(a, Some(t))))
                .collect::<Option<Vec<_>>>()?;
            return Some(Cooked {
                kind: Kind::Row(fields, Some(target)),
                ty: Some(Ty {
                    oid: target,
                    typmod: None,
                }),
            });
        }
        let c = self.cook(arg);
        let t = c.ty?;
        if t.oid == oid::UNKNOWN {
            return None;
        }
        if t.oid != target {
            // coerce_type: below a COLLATE.
            if let Kind::Collate(collation, inner) = c.kind {
                let inner = cast_to(target, *inner);
                return Some(Cooked {
                    ty: inner.ty,
                    kind: Kind::Collate(collation, Box::new(inner)),
                });
            }
            return Some(cast_to(target, c));
        }
        // The same type: no node when the value's type modifier is -1
        // already, as it is for a constant or an operator's or function's
        // result.
        let default_typmod = match &c.kind {
            Kind::Var(_) | Kind::Subscript(..) | Kind::Field(..) => t.typmod.is_none(),
            Kind::Const(..)
            | Kind::Op(..)
            | Kind::ScalarArrayOp(..)
            | Kind::Func(..)
            | Kind::Coerce(..)
            | Kind::Bool(..)
            | Kind::NullTest(..)
            | Kind::Case(..)
            | Kind::Coalesce(..)
            | Kind::MinMax(..)
            | Kind::NullIf(..)
            | Kind::Distinct(..)
            | Kind::Row(..)
            | Kind::BoolTest(..)
            | Kind::Collate(..)
            | Kind::Call(..) => true,
            Kind::Array(..) | Kind::Raw(_) => false,
        };
        Some(if default_typmod {
            c
        } else {
            cast_to(target, c)
        })
    }

    /// A cast to a type with a modifier (`c::varchar(5)`): the value cast
    /// to the type (an untyped literal: a constant of it), then its length
    /// coercion — none when it has that modifier already
    /// (coerce_type_typmod).
    fn typmod_cast(&self, tc: &ast::TypeCast, tn: &ast::TypeName) -> Option<Cooked> {
        if !tn.array_bounds.is_empty() {
            return None;
        }
        let target = crate::ddl::util::resolve_type_name(tn, self.interp)?;
        let typmod = crate::typmod::encode(self.interp, target, tn).ok()??;
        let arg = tc.arg.as_ref()?;
        let base = match Self::untyped(arg) {
            Some((_, text)) => self.literal(target, text)?,
            None => {
                let c = self.cook(arg);
                let t = c.ty?;
                if t.oid == oid::UNKNOWN {
                    return None;
                }
                if t.oid == target {
                    if t.typmod == Some(typmod) {
                        return Some(c);
                    }
                    c
                } else {
                    // coerce_to_target_type: with a length coercion added
                    // too, the inner coercion is shown as implicit
                    // (hideInputCoercion).
                    coerced(target, c)
                }
            }
        };
        Some(Cooked {
            kind: Kind::Coerce(target, Box::new(base), true),
            ty: Some(Ty {
                oid: target,
                typmod: Some(typmod),
            }),
        })
    }

    /// transformArrayExpr: each element coerced to the element type.
    fn array(&self, a: &ast::AArrayExpr, array: PgTypeOid, elem: PgTypeOid) -> Option<Cooked> {
        let mut elems = Vec::new();
        for e in &a.elements {
            let e = e.as_ref()?;
            // A multidimensional constructor merges its sub-arrays.
            if matches!(e, ast::Node::AArrayExpr(_)) {
                return None;
            }
            elems.push(self.arg(e, Some(elem)));
        }
        Some(Cooked {
            kind: Kind::Array(elem, elems),
            ty: Some(Ty {
                oid: array,
                typmod: None,
            }),
        })
    }

    fn a_expr(&self, node: &ast::Node, e: &ast::AExpr) -> Option<Cooked> {
        let ty = self.ty(node);
        let key = NodeKey::of(e);
        let kind = match e.kind {
            ast::AExprKind::AexprOp => {
                let op = recorded(&self.log.operators, &key)?;
                let mut c = self.op(op, e.lexpr.as_ref(), e.rexpr.as_ref()?)?;
                // A polymorphic operator's result is the inference's.
                c.ty = c.ty.or(ty);
                return Some(c);
            }
            // LIKE / ILIKE: their operators (`~~`, `!~~`, `~~*`, `!~~*`),
            // named in the expression.
            ast::AExprKind::AexprLike | ast::AExprKind::AexprIlike => {
                let name = crate::expr::extract_string_fields(&e.name).join(".");
                let (l, r) = (e.lexpr.as_ref()?, e.rexpr.as_ref()?);
                let op = match self.interp.find_operator_detailed(
                    &name,
                    Some(self.operand_type(l)?),
                    self.operand_type(r)?,
                ) {
                    crate::lookup::OperatorMatch::Found(op) => op.oid,
                    _ => return None,
                };
                let mut c = self.op(op, Some(l), r)?;
                c.ty = c.ty.or(ty);
                return Some(c);
            }
            // transformAExprSimilar... (makeSimpleA_Expr over
            // `similar_to_escape(pattern [, escape])`): `~` (`!~`).
            ast::AExprKind::AexprSimilar => {
                let name = crate::expr::extract_string_fields(&e.name).join(".");
                let l = e.lexpr.as_ref()?;
                let ast::Node::FuncCall(escape) = e.rexpr.as_ref()? else {
                    return None;
                };
                let right = self.general_call(e.rexpr.as_ref()?, escape, false)?;
                let op = match self.interp.find_operator_detailed(
                    &name,
                    Some(self.operand_type(l)?),
                    right.ty?.oid,
                ) {
                    crate::lookup::OperatorMatch::Found(op) => op.oid,
                    _ => return None,
                };
                let row = self.interp.pg_operator.get(&op)?;
                let right = self.coerce(right, Some(row.oprright));
                Kind::Op(op, vec![self.arg(l, row.oprleft), right])
            }
            // transformAExprNullIf: `=` over the operands, each coerced to
            // its declared type.
            ast::AExprKind::AexprNullif => {
                let (l, r) = (e.lexpr.as_ref()?, e.rexpr.as_ref()?);
                let op = self.equality(self.operand_type(l)?, self.operand_type(r)?)?;
                let row = self.interp.pg_operator.get(&op)?;
                Kind::NullIf(
                    op,
                    vec![self.arg(l, row.oprleft), self.arg(r, Some(row.oprright))],
                )
            }
            // transformAExprDistinct: against an undecorated NULL, a
            // NullTest of the other side; else `=` (NOT DISTINCT: NOT of
            // it).
            ast::AExprKind::AexprDistinct | ast::AExprKind::AexprNotDistinct => {
                let (l, r) = (e.lexpr.as_ref()?, e.rexpr.as_ref()?);
                let distinct = e.kind == ast::AExprKind::AexprDistinct;
                let is_null = |n: &ast::Node| matches!(n, ast::Node::AConst(c) if c.isnull);
                if is_null(r) || is_null(l) {
                    let other = if is_null(r) { l } else { r };
                    Kind::NullTest(!distinct, Box::new(self.cook(other)))
                } else {
                    let op = self.equality(self.operand_type(l)?, self.operand_type(r)?)?;
                    let row = self.interp.pg_operator.get(&op)?;
                    let d = Cooked {
                        kind: Kind::Distinct(
                            op,
                            vec![self.arg(l, row.oprleft), self.arg(r, Some(row.oprright))],
                        ),
                        ty: Some(bool_ty()),
                    };
                    if distinct {
                        return Some(d);
                    }
                    Kind::Bool(ast::BoolExprType::NotExpr as u8, vec![d])
                }
            }
            // make_scalar_array_op: the left operand coerced to the
            // operator's left type, the array to the array of its right
            // type.
            ast::AExprKind::AexprOpAny | ast::AExprKind::AexprOpAll => {
                let op = recorded(&self.log.operators, &key)?;
                let row = self.interp.pg_operator.get(&op)?;
                let array = self.interp.array_type_of(row.oprright);
                let left = self.arg(e.lexpr.as_ref()?, row.oprleft);
                let right = self.arg(e.rexpr.as_ref()?, array);
                Kind::ScalarArrayOp(op, e.kind == ast::AExprKind::AexprOpAny, vec![left, right])
            }
            // transformAExprIn: the items without columns, when several,
            // become one `left op ANY (ARRAY[…])` over the items coerced
            // to their common type; each other item is a comparison of its
            // own, OR-ed (AND-ed for NOT IN) to what comes before.
            ast::AExprKind::AexprIn => {
                let (fold, comparisons) = recorded(&self.log.in_lists, &key)?;
                let Some(ast::Node::List(list)) = e.rexpr.as_ref() else {
                    return None;
                };
                let left = e.lexpr.as_ref()?;
                let use_or = crate::expr::extract_string_fields(&e.name).join(".") == "=";
                let mut result: Option<Cooked> = None;
                if let Some((op, common)) = fold {
                    let row = self.interp.pg_operator.get(&op)?;
                    let mut elems = Vec::new();
                    for item in &list.items {
                        let item = item.as_ref()?;
                        if !crate::expr::operators::contains_level0_column_ref(Some(item)) {
                            elems.push(self.arg(item, Some(common)));
                        }
                    }
                    let array = Cooked {
                        kind: Kind::Array(common, elems),
                        ty: self
                            .interp
                            .array_type_of(common)
                            .map(|oid| Ty { oid, typmod: None }),
                    };
                    let right = self.coerce(array, self.interp.array_type_of(row.oprright));
                    result = Some(Cooked {
                        kind: Kind::ScalarArrayOp(
                            op,
                            use_or,
                            vec![self.arg(left, row.oprleft), right],
                        ),
                        ty: Some(bool_ty()),
                    });
                }
                for (i, op) in comparisons {
                    let cmp = self.op(op, Some(left), list.items.get(i)?.as_ref()?)?;
                    result = Some(match result {
                        None => cmp,
                        Some(prev) => Cooked {
                            kind: Kind::Bool(
                                if use_or {
                                    ast::BoolExprType::OrExpr
                                } else {
                                    ast::BoolExprType::AndExpr
                                } as u8,
                                vec![prev, cmp],
                            ),
                            ty: Some(bool_ty()),
                        },
                    });
                }
                return result;
            }
            // transformAExprBetween: `a >= b AND a <= c`, `a < b OR a > c`;
            // SYMMETRIC ORs (NOT: ANDs) that with the bounds swapped.
            ast::AExprKind::AexprBetween
            | ast::AExprKind::AexprNotBetween
            | ast::AExprKind::AexprBetweenSym
            | ast::AExprKind::AexprNotBetweenSym => {
                use ast::BoolExprType::{AndExpr, OrExpr};
                let ops = recorded(&self.log.between, &key)?;
                let Some(ast::Node::List(list)) = e.rexpr.as_ref() else {
                    return None;
                };
                let [Some(b), Some(c)] = list.items.as_slice() else {
                    return None;
                };
                let left = e.lexpr.as_ref()?;
                let (inner, outer) = match e.kind {
                    ast::AExprKind::AexprBetween | ast::AExprKind::AexprBetweenSym => {
                        (AndExpr, OrExpr)
                    }
                    _ => (OrExpr, AndExpr),
                };
                let pair = |low, high, b, c| -> Option<Cooked> {
                    Some(Cooked {
                        kind: Kind::Bool(
                            inner as u8,
                            vec![self.op(low, Some(left), b)?, self.op(high, Some(left), c)?],
                        ),
                        ty: Some(bool_ty()),
                    })
                };
                match ops.as_slice() {
                    [low, high] => return pair(*low, *high, b, c),
                    [l1, h1, l2, h2] => Kind::Bool(
                        outer as u8,
                        vec![pair(*l1, *h1, b, c)?, pair(*l2, *h2, c, b)?],
                    ),
                    _ => return None,
                }
            }
        };
        Some(Cooked { kind, ty })
    }

    /// make_op: operator `op` over `left` (none for a prefix operator) and
    /// `right`, each coerced to its declared type.
    fn op(&self, op: PgOperatorOid, left: Option<&ast::Node>, right: &ast::Node) -> Option<Cooked> {
        let row = self.interp.pg_operator.get(&op)?;
        let mut args = Vec::new();
        if let Some(l) = left {
            args.push(self.arg(l, row.oprleft));
        }
        args.push(self.arg(right, Some(row.oprright)));
        let ty = row
            .oprresult
            .filter(|t| !self.is_pseudo(*t))
            .map(|oid| Ty { oid, typmod: None });
        Some(Cooked {
            kind: Kind::Op(op, args),
            ty,
        })
    }

    /// A plain function call (no aggregate, window, VARIADIC or named
    /// argument), its arguments coerced to the declared types.
    fn call(&self, node: &ast::Node, fc: &ast::FuncCall) -> Option<Cooked> {
        if fc.agg_star
            || fc.agg_distinct
            || fc.agg_within_group
            || fc.ignore_nulls != 0
            || fc.over.is_some()
            || fc.agg_filter.is_some()
            || !fc.agg_order.is_empty()
        {
            return None;
        }
        let named = fc
            .args
            .iter()
            .any(|a| matches!(a, Some(ast::Node::NamedArgExpr(_))));
        let sql_syntax = fc.funcformat == ast::CoercionForm::CoerceSqlSyntax;
        // An operand BETWEEN or IN inferred copies of has no call recorded:
        // resolved again.
        let recorded_call = recorded(&self.log.calls, &NodeKey::of(fc));
        if !named
            && !sql_syntax
            && !fc.func_variadic
            && let Some(proc) = recorded_call
            && let Some(row) = self.interp.pg_proc.get(&proc)
            && row.provariadic.is_none()
        {
            let mut args = Vec::new();
            for (i, a) in fc.args.iter().enumerate() {
                args.push(self.arg(a.as_ref()?, Some(*row.proargtypes.get(i)?)));
            }
            return Some(Cooked {
                kind: Kind::Func(proc, args),
                ty: self.ty(node),
            });
        }
        self.general_call(node, fc, sql_syntax)
    }

    /// ParseFuncOrColumn for a call with named arguments, a variadic
    /// parameter or a SQL syntax: resolved as the analysis did, each
    /// argument coerced to its declared type, and the variadic ones merged
    /// into an array (which makes it a VARIADIC call) unless the parameter
    /// is `VARIADIC "any"`.
    fn general_call(
        &self,
        node: &ast::Node,
        fc: &ast::FuncCall,
        sql_syntax: bool,
    ) -> Option<Cooked> {
        let names = crate::expr::extract_string_fields(&fc.funcname);
        let (schema, name) = match names.as_slice() {
            [n] => (None, n.as_str()),
            [s, n] => (Some(s.as_str()), n.as_str()),
            _ => return None,
        };
        let mut values: Vec<(Option<String>, &ast::Node)> = Vec::new();
        for a in &fc.args {
            match a.as_ref()? {
                ast::Node::NamedArgExpr(na) => {
                    values.push((Some(na.name.clone()), na.arg.as_ref()?))
                }
                other => values.push((None, other)),
            }
        }
        let types = values
            .iter()
            .map(|(_, v)| self.operand_type(v))
            .collect::<Option<Vec<_>>>()?;
        let notation = crate::functions::CallNotation {
            names: values.iter().filter_map(|(n, _)| n.clone()).collect(),
            variadic: fc.func_variadic,
            ..Default::default()
        };
        let resolved = crate::functions::resolve_function(
            self.interp,
            schema,
            name,
            &types,
            &notation,
            false,
            None,
        )
        .ok()?;
        if resolved.is_aggregate || resolved.is_window {
            return None;
        }
        let mut args: Vec<(Option<String>, Cooked)> = values
            .iter()
            .zip(&resolved.arg_types)
            .map(|((n, v), t)| (n.clone(), self.arg(v, Some(*t))))
            .collect();
        let mut variadic = fc.func_variadic && resolved.provariadic.is_some();
        // `"any"` (VARIADIC "any").
        const ANY: PgTypeOid = PgTypeOid::from_raw(2276);
        if resolved.nvargs > 0 && resolved.provariadic != Some(ANY) {
            let first = args.len().checked_sub(resolved.nvargs)?;
            let elems: Vec<Cooked> = args.split_off(first).into_iter().map(|(_, c)| c).collect();
            let elem = elems.first()?.ty?.oid;
            let array = self.interp.array_type_of(elem)?;
            args.push((
                None,
                Cooked {
                    kind: Kind::Array(elem, elems),
                    ty: Some(Ty {
                        oid: array,
                        typmod: None,
                    }),
                },
            ));
            variadic = true;
        }
        // A plain call resolved again is the same `FuncExpr`.
        let ty = self.ty(node).or(Some(Ty {
            oid: resolved.return_type_oid,
            typmod: None,
        }));
        if !variadic && !sql_syntax && args.iter().all(|(n, _)| n.is_none()) {
            return Some(Cooked {
                kind: Kind::Func(resolved.oid, args.into_iter().map(|(_, c)| c).collect()),
                ty,
            });
        }
        Some(Cooked {
            kind: Kind::Call(
                resolved.oid,
                args,
                CallForm {
                    variadic,
                    sql_syntax,
                },
            ),
            ty,
        })
    }
}

fn bool_ty() -> Ty {
    Ty {
        oid: oid::BOOL,
        typmod: None,
    }
}

/// `c` coerced to `t` (implicitly).
fn coerced(t: PgTypeOid, c: Cooked) -> Cooked {
    Cooked {
        kind: Kind::Coerce(t, Box::new(c), false),
        ty: Some(Ty {
            oid: t,
            typmod: None,
        }),
    }
}

/// `c` cast to `t` (an explicit coercion).
fn cast_to(t: PgTypeOid, c: Cooked) -> Cooked {
    Cooked {
        kind: Kind::Coerce(t, Box::new(c), true),
        ty: Some(Ty {
            oid: t,
            typmod: None,
        }),
    }
}

/// The planner's constant folding (`eval_const_expressions`, clauses.c) of
/// a cooked tree, as the relcache applies it to an index's expressions and
/// predicate (RelationGetIndexExpressions / RelationGetIndexPredicate) —
/// never to CHECKs, defaults or statistics, which PG compares unfolded.
///
/// Only what is certain is folded: a strict routine over a NULL constant,
/// the built-in integer arithmetic and comparisons and integer / `numeric`
/// casts over constants, relabelings (stacked ones collapsed), a domain
/// without constraints, an ARRAY of integer or boolean constants, AND / OR
/// / NOT and IS [NOT] NULL over constants, `negate_clause`, `bool =
/// constant`. Whatever else PG folds (SQL function inlining, support
/// functions, CASE / COALESCE, other types' constants) stays as is and
/// compares different — never the reverse: two trees equal here are equal
/// once PG folds them.
/// (`canonicalize_qual`, which the predicate goes through too, isn't
/// modeled either.)
struct Folder<'a> {
    interp: &'a PgCatalog,
}

/// `BooleanEqualOperator` / `BooleanNotEqualOperator` (`bool = bool`,
/// `bool <> bool`).
const BOOLEAN_EQUAL: u32 = 91;
const BOOLEAN_NOT_EQUAL: u32 = 85;

/// `bit`.
const BIT: PgTypeOid = PgTypeOid::from_raw(1560);

/// `FirstNormalObjectId`: below, what initdb made.
const FIRST_NORMAL_OBJECT_ID: u32 = 16384;

fn constant(t: PgTypeOid, value: Option<String>) -> Cooked {
    Cooked {
        kind: Kind::Const(t, value),
        ty: Some(Ty {
            oid: t,
            typmod: None,
        }),
    }
}

/// makeBoolConst.
fn bool_const(value: Option<bool>) -> Cooked {
    constant(
        oid::BOOL,
        value.map(|b| if b { "t" } else { "f" }.to_owned()),
    )
}

impl Cooked {
    /// A non-NULL boolean constant's value.
    fn as_bool(&self) -> Option<bool> {
        match &self.kind {
            Kind::Const(t, Some(v)) if *t == oid::BOOL => match v.as_str() {
                "t" => Some(true),
                "f" => Some(false),
                _ => None,
            },
            _ => None,
        }
    }

    fn is_null_const(&self) -> bool {
        matches!(self.kind, Kind::Const(_, None))
    }

    /// An integer constant's value.
    fn as_int(&self) -> Option<i128> {
        match &self.kind {
            Kind::Const(t, Some(v)) if crate::fold::IntType::of(*t).is_some() => v.parse().ok(),
            _ => None,
        }
    }
}

impl Folder<'_> {
    fn fold(&self, c: Cooked) -> Cooked {
        crate::stack::step(|| self.fold_here(c))
    }

    fn fold_all(&self, args: Vec<Cooked>) -> Vec<Cooked> {
        args.into_iter().map(|a| self.fold(a)).collect()
    }

    fn fold_here(&self, c: Cooked) -> Cooked {
        let ty = c.ty;
        match c.kind {
            Kind::Op(op, args) => {
                let args = self.fold_all(args);
                let row = self.interp.pg_operator.get(&op);
                let result = row
                    .and_then(|r| r.oprresult)
                    .filter(|t| !crate::polymorphic::is_polymorphic(*t))
                    .or(ty.map(|t| t.oid));
                if let Some(folded) = row
                    .and_then(|r| r.oprcode)
                    .and_then(|proc| self.call(proc, result, &args))
                {
                    return folded;
                }
                // simplify_boolean_equality.
                if matches!(op.get(), BOOLEAN_EQUAL | BOOLEAN_NOT_EQUAL)
                    && let [l, r] = args.as_slice()
                {
                    let equal = op.get() == BOOLEAN_EQUAL;
                    if let Some(b) = l.as_bool() {
                        return self.equal_to(b == equal, r.clone());
                    }
                    if let Some(b) = r.as_bool() {
                        return self.equal_to(b == equal, l.clone());
                    }
                }
                Cooked {
                    kind: Kind::Op(op, args),
                    ty,
                }
            }
            Kind::Func(proc, args) => {
                let args = self.fold_all(args);
                let result = self
                    .interp
                    .pg_proc
                    .get(&proc)
                    .map(|r| r.prorettype)
                    .filter(|t| !crate::polymorphic::is_polymorphic(*t))
                    .or(ty.map(|t| t.oid));
                self.call(proc, result, &args).unwrap_or(Cooked {
                    kind: Kind::Func(proc, args),
                    ty,
                })
            }
            // ece_generic_processing; the operator over constants isn't
            // evaluated.
            Kind::ScalarArrayOp(op, use_or, args) => Cooked {
                kind: Kind::ScalarArrayOp(op, use_or, self.fold_all(args)),
                ty,
            },
            Kind::Coerce(t, arg, _) => self.coerce(t, self.fold(*arg)),
            Kind::Array(elem, elems) => {
                let elems = self.fold_all(elems);
                self.array(elem, &elems, ty).unwrap_or(Cooked {
                    kind: Kind::Array(elem, elems),
                    ty,
                })
            }
            Kind::Bool(op, args) if op == ast::BoolExprType::NotExpr as u8 => {
                match <[_; 1]>::try_from(args) {
                    Ok([arg]) => self.negate(self.fold(arg)),
                    Err(args) => Cooked {
                        kind: Kind::Bool(op, args),
                        ty,
                    },
                }
            }
            Kind::Bool(op, args) => self.and_or(op == ast::BoolExprType::AndExpr as u8, args),
            Kind::NullTest(is_null, arg) => {
                let arg = self.fold(*arg);
                if !self.is_row(&arg) {
                    // A constant, or what is never NULL (expr_is_nonnullable,
                    // without a planner root: no column is).
                    let non_null = match &arg.kind {
                        Kind::Const(_, v) => Some(v.is_some()),
                        Kind::Array(..) | Kind::NullTest(..) => Some(true),
                        _ => None,
                    };
                    if let Some(non_null) = non_null {
                        return bool_const(Some(non_null != is_null));
                    }
                }
                Cooked {
                    kind: Kind::NullTest(is_null, Box::new(arg)),
                    ty,
                }
            }
            // Not folded (PG folds a CASE, a COALESCE… over constants): such
            // a tree compares different, never wrongly equal.
            kind @ (Kind::Var(_)
            | Kind::Const(..)
            | Kind::Raw(_)
            | Kind::Case(..)
            | Kind::Coalesce(..)
            | Kind::MinMax(..)
            | Kind::NullIf(..)
            | Kind::Distinct(..)
            | Kind::Row(..)
            | Kind::BoolTest(..)
            | Kind::Collate(..)
            | Kind::Subscript(..)
            | Kind::Field(..)
            | Kind::Call(..)) => Cooked { kind, ty },
        }
    }

    /// Whether `c` is of a row type (`type_is_rowtype`: NullTest's
    /// `argisrow`); unknown counts as one, which nothing folds.
    fn is_row(&self, c: &Cooked) -> bool {
        let Some(t) = c.ty else {
            return true;
        };
        let base = self.interp.unwrap_domain(t.oid);
        base == oid::RECORD
            || self
                .interp
                .get_type(base)
                .is_none_or(|ty| ty.typtype == TypType::Composite)
    }

    /// evaluate_function: routine `proc`, returning `result`, over the
    /// folded `args` — NULL when it is strict and one is a NULL constant,
    /// its value when it is built-in integer arithmetic over constants.
    fn call(&self, proc: PgProcOid, result: Option<PgTypeOid>, args: &[Cooked]) -> Option<Cooked> {
        let row = self.interp.pg_proc.get(&proc)?;
        if row.proretset || row.prorettype == oid::RECORD {
            return None;
        }
        if row.proisstrict && args.iter().any(Cooked::is_null_const) {
            return Some(constant(result?, None));
        }
        if row.provolatile != crate::pg_catalog::ProVolatile::Immutable
            || proc.get() >= FIRST_NORMAL_OBJECT_ID
            || self.interp.namespace_name(row.pronamespace) != Some("pg_catalog")
        {
            return None;
        }
        let ints = args
            .iter()
            .map(Cooked::as_int)
            .collect::<Option<Vec<_>>>()?;
        if row.prorettype == oid::BOOL
            && let [a, b] = ints.as_slice()
        {
            return crate::fold::int_comparison(&row.proname, *a, *b).map(|v| bool_const(Some(v)));
        }
        let ret = crate::fold::IntType::of(row.prorettype)?;
        match crate::fold::int_function(&row.proname, &ints, ret)? {
            Ok(crate::fold::Value::Int(v, _)) => {
                Some(constant(row.prorettype, Some(v.to_string())))
            }
            // An error (an overflow) is left to execution.
            _ => None,
        }
    }

    /// `other = b` simplified (simplify_boolean_equality): `other` itself when
    /// `b` is true, else `NOT other` (negate_clause).
    fn equal_to(&self, b: bool, other: Cooked) -> Cooked {
        if b { other } else { self.negate(other) }
    }

    /// negate_clause: `NOT c`, pushed down where PG does.
    fn negate(&self, c: Cooked) -> Cooked {
        use ast::BoolExprType::{AndExpr, NotExpr, OrExpr};
        let negator =
            |op: &PgOperatorOid| self.interp.pg_operator.get(op).and_then(|r| r.oprnegate);
        let kind = match &c.kind {
            Kind::Const(t, v)
                if *t == oid::BOOL && v.as_deref().is_none_or(|v| v == "t" || v == "f") =>
            {
                return bool_const(v.as_deref().map(|v| v == "f"));
            }
            Kind::Op(op, args) => negator(op).map(|n| Kind::Op(n, args.clone())),
            Kind::ScalarArrayOp(op, use_or, args) => {
                negator(op).map(|n| Kind::ScalarArrayOp(n, !use_or, args.clone()))
            }
            // make_orclause / make_andclause: not flattened.
            Kind::Bool(op, args) if *op == AndExpr as u8 || *op == OrExpr as u8 => {
                let flipped = if *op == AndExpr as u8 {
                    OrExpr
                } else {
                    AndExpr
                };
                Some(Kind::Bool(
                    flipped as u8,
                    args.iter().map(|a| self.negate(a.clone())).collect(),
                ))
            }
            Kind::Bool(op, args) if *op == NotExpr as u8 && args.len() == 1 => {
                return args[0].clone();
            }
            Kind::NullTest(is_null, arg) if !self.is_row(arg) => {
                Some(Kind::NullTest(!is_null, arg.clone()))
            }
            _ => None,
        };
        match kind {
            Some(kind) => Cooked { kind, ty: c.ty },
            // What negate_clause can't push into gets a NOT.
            None => Cooked {
                kind: Kind::Bool(NotExpr as u8, vec![c]),
                ty: Some(bool_ty()),
            },
        }
    }

    /// simplify_and_arguments / simplify_or_arguments over the folded
    /// arguments: nested ANDs (ORs) flattened, TRUE (FALSE) dropped, FALSE
    /// (TRUE) deciding, NULLs reduced to one NULL constant at the end; no
    /// argument left is TRUE (FALSE), one is itself.
    fn and_or(&self, is_and: bool, args: Vec<Cooked>) -> Cooked {
        let op = if is_and {
            ast::BoolExprType::AndExpr
        } else {
            ast::BoolExprType::OrExpr
        } as u8;
        let mut pending: std::collections::VecDeque<Cooked> = self.fold_all(args).into();
        let mut out = Vec::new();
        let mut have_null = false;
        while let Some(arg) = pending.pop_front() {
            if let Kind::Bool(k, sub) = &arg.kind
                && *k == op
            {
                for s in sub.iter().rev() {
                    pending.push_front(s.clone());
                }
                continue;
            }
            if arg.is_null_const() && arg.ty.is_some_and(|t| t.oid == oid::BOOL) {
                have_null = true;
                continue;
            }
            match arg.as_bool() {
                Some(b) if b != is_and => return bool_const(Some(b)),
                Some(_) => continue,
                None => out.push(arg),
            }
        }
        if have_null {
            out.push(bool_const(None));
        }
        match out.len() {
            0 => bool_const(Some(is_and)),
            1 => out.pop().expect("one argument"),
            _ => Cooked {
                kind: Kind::Bool(op, out),
                ty: Some(bool_ty()),
            },
        }
    }

    /// `arg` (folded) coerced to `t`: a constant converted where PG's
    /// coercion folds it — a relabeling (between `text`, `varchar` and
    /// `bpchar`, or a domain and its base), an integer / `numeric` cast
    /// function, a strict cast function over NULL; a domain without
    /// constraints is a relabeling (eval_const_expressions'
    /// CoerceToDomain).
    fn coerce(&self, t: PgTypeOid, arg: Cooked) -> Cooked {
        let unfolded = |arg: Cooked| coerced(t, arg);
        let is_domain = self
            .interp
            .get_type(t)
            .is_some_and(|ty| ty.typtype == TypType::Domain);
        if is_domain {
            let base = self.interp.unwrap_domain(t);
            let Some(arg_ty) = arg.ty else {
                return unfolded(arg);
            };
            // Coerced to the base type first (coerce_to_domain).
            let inner = if self.interp.unwrap_domain(arg_ty.oid) == base {
                match &arg.kind {
                    Kind::Const(_, v) => constant(base, v.clone()),
                    _ if self.is_relabel(t, Some(arg_ty)) => return self.relabel(t, arg),
                    _ => return unfolded(arg),
                }
            } else {
                self.coerce(base, arg.clone())
            };
            return match inner.kind {
                Kind::Const(_, v)
                    if !crate::ddl::tables::domain_has_constraints(self.interp, t) =>
                {
                    constant(t, v)
                }
                Kind::Const(..) => unfolded(inner),
                _ => unfolded(arg),
            };
        }
        let Kind::Const(arg_t, value) = &arg.kind else {
            if self.is_relabel(t, arg.ty) {
                return self.relabel(t, arg);
            }
            return unfolded(arg);
        };
        // A domain's value relabeled to its base first.
        let source = self.interp.unwrap_domain(*arg_t);
        if source == t {
            return constant(t, value.clone());
        }
        let text_like = [oid::TEXT, oid::VARCHAR, oid::BPCHAR];
        let numbers = [oid::INT2, oid::INT4, oid::INT8, oid::NUMERIC];
        use crate::coerce::{CoercionContext, CoercionPath, coercion_pathway};
        match coercion_pathway(t, source, CoercionContext::Explicit, self.interp) {
            Some(CoercionPath::Relabel) if value.is_none() => constant(t, None),
            Some(CoercionPath::Relabel)
                if text_like.contains(&source) && text_like.contains(&t) =>
            {
                constant(t, value.clone())
            }
            Some(CoercionPath::Func) => {
                let Some(func) = self
                    .interp
                    .cast_by_pair
                    .get(&(source, t))
                    .and_then(|c| self.interp.pg_cast.get(c))
                    .and_then(|c| c.castfunc)
                    .and_then(|f| self.interp.pg_proc.get(&f))
                else {
                    return unfolded(arg);
                };
                let builtin = func.oid.get() < FIRST_NORMAL_OBJECT_ID
                    && func.provolatile == crate::pg_catalog::ProVolatile::Immutable;
                match value {
                    None if func.proisstrict => constant(t, None),
                    Some(v) if builtin && numbers.contains(&source) && numbers.contains(&t) => {
                        let from = match crate::fold::IntType::of(source) {
                            Some(it) => v.parse().ok().map(|i| crate::fold::Value::Int(i, it)),
                            None => Some(crate::fold::Value::Numeric(v.clone())),
                        };
                        match from.and_then(|f| crate::fold::convert(&f, t, None, self.interp)) {
                            Some(Ok(crate::fold::Value::Int(i, _))) => {
                                constant(t, Some(i.to_string()))
                            }
                            Some(Ok(crate::fold::Value::Numeric(n))) => constant(t, Some(n)),
                            _ => unfolded(arg),
                        }
                    }
                    _ => unfolded(arg),
                }
            }
            _ => unfolded(arg),
        }
    }

    /// Whether coercing a value of type `src` to `t` is a `RelabelType`
    /// once folded: a binary coercion, or a domain over `src`'s base
    /// without constraints (CoerceToDomain → applyRelabelType).
    fn is_relabel(&self, t: PgTypeOid, src: Option<Ty>) -> bool {
        use crate::coerce::{CoercionContext, CoercionPath, coercion_pathway};
        let Some(src) = src else {
            return false;
        };
        let is_domain = self
            .interp
            .get_type(t)
            .is_some_and(|ty| ty.typtype == TypType::Domain);
        if is_domain {
            self.interp.unwrap_domain(t) == self.interp.unwrap_domain(src.oid)
                && !crate::ddl::tables::domain_has_constraints(self.interp, t)
        } else {
            coercion_pathway(t, src.oid, CoercionContext::Explicit, self.interp)
                == Some(CoercionPath::Relabel)
        }
    }

    /// applyRelabelType: `arg` relabeled to `t` — the relabelings already
    /// on it stripped, and none at all when what is left is of type `t`
    /// (with the default type modifier).
    fn relabel(&self, t: PgTypeOid, arg: Cooked) -> Cooked {
        let mut arg = arg;
        while let Kind::Coerce(d, inner, _) = &arg.kind
            && self.is_relabel(*d, inner.ty)
        {
            arg = (**inner).clone();
        }
        match arg.ty {
            Some(ty) if ty.oid == t && ty.typmod.is_none() => arg,
            _ => coerced(t, arg),
        }
    }

    /// An ARRAY of constants is evaluated (ece_evaluate_expr): its value in
    /// `array_out`'s form, for integer and boolean elements.
    fn array(&self, elem: PgTypeOid, elems: &[Cooked], ty: Option<Ty>) -> Option<Cooked> {
        if ![oid::INT2, oid::INT4, oid::INT8, oid::BOOL].contains(&elem) {
            return None;
        }
        let mut items = Vec::new();
        for e in elems {
            match &e.kind {
                Kind::Const(t, v) if *t == elem => items.push(v.as_deref().unwrap_or("NULL")),
                _ => return None,
            }
        }
        let array = ty?.oid;
        Some(constant(array, Some(format!("{{{}}}", items.join(",")))))
    }
}

/// `t`'s name when it is one of the reg* types (of `pg_catalog`).
fn reg_type_name(interp: &PgCatalog, t: PgTypeOid) -> Option<&'static str> {
    let ty = interp.get_type(t)?;
    if interp.namespace_name(ty.typnamespace) != Some("pg_catalog") {
        return None;
    }
    crate::reg_input::REG_TYPES
        .iter()
        .find(|n| **n == ty.typname)
        .copied()
}

/// Whether two stored expressions, over relations `rel_a` and `rel_b`, are
/// the same once transformed — coerced to `target` (a column's type) when
/// given. Two equal parse trees always are; otherwise both must analyze.
pub(crate) fn same_written(
    interp: &PgCatalog,
    (rel_a, a): (PgClassOid, Option<&ast::Node>),
    (rel_b, b): (PgClassOid, Option<&ast::Node>),
    target: Option<PgTypeOid>,
) -> bool {
    use typedpg_pg_query::Equal;
    let as_written = match (a, b) {
        (Some(a), Some(b)) => a.equal(b),
        (None, None) => true,
        _ => false,
    };
    if as_written {
        return true;
    }
    match (
        cook(interp, rel_a, a, target),
        cook(interp, rel_b, b, target),
    ) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// `expr` (an index expression or predicate over `rel`) as CompareIndexInfo
/// sees it: cooked, then constant-folded as the relcache reads it.
fn cook_index_expr(
    interp: &PgCatalog,
    rel: PgClassOid,
    expr: Option<&ast::Node>,
) -> Option<Cooked> {
    let cooked = cook(interp, rel, expr, None)?;
    Some(Folder { interp }.fold(cooked))
}

/// Whether two index expressions (or predicates), over relations `rel_a`
/// and `rel_b`, are `equal()` as CompareIndexInfo compares them: read
/// from the relcache (RelationGetIndexExpressions /
/// RelationGetIndexPredicate), so transformed and constant-folded — `a +
/// (1 + 1)` is `a + 2`. Two equal parse trees always are; otherwise both
/// must analyze.
pub(crate) fn same_index_expr(
    interp: &PgCatalog,
    (rel_a, a): (PgClassOid, Option<&ast::Node>),
    (rel_b, b): (PgClassOid, Option<&ast::Node>),
) -> bool {
    use typedpg_pg_query::Equal;
    let as_written = match (a, b) {
        (Some(a), Some(b)) => a.equal(b),
        (None, None) => true,
        _ => false,
    };
    if as_written {
        return true;
    }
    match (
        cook_index_expr(interp, rel_a, a),
        cook_index_expr(interp, rel_b, b),
    ) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// Whether an index predicate over `rel` folds to TRUE: then
/// RelationGetIndexPredicate's `make_ands_implicit` leaves no predicate at
/// all, as for an index without one.
pub(crate) fn index_predicate_is_true(
    interp: &PgCatalog,
    rel: PgClassOid,
    pred: Option<&ast::Node>,
) -> bool {
    cook_index_expr(interp, rel, pred).is_some_and(|c| c.as_bool() == Some(true))
}

// ─── Deparsing (ruleutils) ─────────────────────────────────────────────────
//
// ALTER COLUMN TYPE rebuilds the CHECK constraints and indexes reading the
// column from the text `pg_get_constraintdef` / `pg_get_indexdef` print
// for them — before the column changes — and parses that text again over
// the new type. [`Printer`] writes that text from the cooked expression as
// ruleutils' `get_rule_expr` does without pretty-printing: every operator
// expression parenthesized; the implicit coercions shown where ruleutils
// shows them (a function's or operator's arguments, a CASE result, an
// ARRAY element…) and hidden elsewhere (the expression itself, an AND / OR
// argument, a cast's argument…); a constant written as its output function
// prints it, typed unless the parser would type it so anyway
// (`get_const_expr`); routines, operators, types and collations named
// unqualified only where an unqualified lookup — with the arguments'
// types, for a routine or an operator — finds them
// (`generate_function_name`, `generate_operator_name`, `format_type_be`,
// `generate_collation_name`). [`deparse`] parses that text back (what the
// cooker doesn't model is carried through as written); [`ruleutils_text`]
// is the text itself, which the pg_sanity mirror compares with
// `pg_get_expr`'s.

/// The parse tree of what ruleutils prints for `expr`, a stored expression
/// over the row of `relid` (a CHECK constraint's, an index expression or
/// predicate): the text PG parses again when it rebuilds it. `None` when
/// it doesn't analyze.
pub(crate) fn deparse(
    interp: &PgCatalog,
    relid: PgClassOid,
    expr: Option<&ast::Node>,
) -> Option<ast::Node> {
    let cooked = cook(interp, relid, expr, None)?;
    let printer = Printer::new(interp, true);
    let text = printer.text(&cooked, false)?;
    let raws = printer.raws.into_inner();
    // The new nodes' source locations come after every original one the
    // parts kept as written carry: the bindings recorded over the tree are
    // keyed by them.
    let after = raws
        .iter()
        .flatten()
        .flat_map(|n| n.nodes().filter_map(|(n, ..)| n.location()))
        .max()
        .map_or(0, |l| l as usize + 1);
    let sql = format!("{}SELECT {text}", " ".repeat(after));
    let parsed = typedpg_pg_query::parse(&sql).ok()?;
    let stmt = parsed.tree.stmts.into_iter().next()?.stmt?;
    let ast::Node::SelectStmt(select) = stmt else {
        return None;
    };
    let Some(Some(ast::Node::ResTarget(target))) = select.target_list.first().cloned() else {
        return None;
    };
    let mut tree = ast::ParseResult {
        version: 0,
        stmts: vec![ast::RawStmt {
            stmt: target.val.clone(),
            stmt_location: 0,
            stmt_len: 0,
        }],
    };
    // The placeholders stand for the parts kept as written.
    let mut missing = false;
    tree.for_each_node_mut(|n, _| {
        if let typedpg_pg_query::NodeMut::ColumnRef(cr) = n
            && let [Some(ast::Node::String(s))] = cr.fields.as_slice()
            && let Some(i) = s.sval.strip_prefix(RAW_PLACEHOLDER)
        {
            missing |= i.parse::<usize>().ok().and_then(|i| raws.get(i)).is_none();
        }
    });
    if missing {
        return None;
    }
    let mut out = tree.stmts.pop()?.stmt;
    substitute_raws(&mut out, &raws);
    out
}

/// The text ruleutils prints for `expr` (see [`deparse`]), when every part
/// of it is modelled — its constants included, whose text the analyzer
/// prints as their output functions would; `None` otherwise.
#[cfg(any(test, feature = "pg_sanity"))]
pub(crate) fn ruleutils_text(
    interp: &PgCatalog,
    relid: PgClassOid,
    expr: Option<&ast::Node>,
) -> Option<String> {
    let cooked = cook(interp, relid, expr, None)?;
    let printer = Printer::new(interp, false);
    let text = printer.text(&cooked, false)?;
    printer.exact.get().then_some(text)
}

/// What `pg_get_indexdef(index, column)` prints for an index expression:
/// [`ruleutils_text`], parenthesized unless it reads as a function call
/// (`looks_like_function`).
#[cfg(feature = "pg_sanity")]
pub(crate) fn ruleutils_index_column_text(
    interp: &PgCatalog,
    relid: PgClassOid,
    expr: Option<&ast::Node>,
) -> Option<String> {
    // A COLLATE over the whole expression is the column's collation
    // (`indcollation`), which this form doesn't print.
    let expr = match expr {
        Some(ast::Node::CollateClause(cc)) => cc.arg.as_ref(),
        other => other,
    };
    let text = ruleutils_text(interp, relid, expr)?;
    let cooked = cook(interp, relid, expr, None)?;
    let function_like = match &cooked.kind {
        Kind::Func(..) | Kind::NullIf(..) | Kind::Coalesce(..) | Kind::MinMax(..) => true,
        // A SQL syntax prints as a function but for AT TIME ZONE, AT LOCAL,
        // OVERLAPS and IS NORMALIZED.
        Kind::Call(proc, _, form) => {
            !form.sql_syntax
                || !interp.pg_proc.get(proc).is_some_and(|p| {
                    matches!(
                        p.proname.as_str(),
                        "timezone" | "overlaps" | "is_normalized"
                    )
                })
        }
        _ => false,
    };
    Some(if function_like {
        text
    } else {
        format!("({text})")
    })
}

/// The name of the column a part kept as written stands in as.
const RAW_PLACEHOLDER: &str = "typedpg raw part ";

/// Put the parts kept as written back in place of their placeholders.
fn substitute_raws(expr: &mut Option<ast::Node>, raws: &[Option<ast::Node>]) {
    let placeholder = |n: &ast::Node| -> Option<usize> {
        let ast::Node::ColumnRef(cr) = n else {
            return None;
        };
        let [Some(ast::Node::String(s))] = cr.fields.as_slice() else {
            return None;
        };
        s.sval.strip_prefix(RAW_PLACEHOLDER)?.parse().ok()
    };
    if let Some(i) = expr.as_ref().and_then(placeholder) {
        *expr = raws.get(i).cloned().flatten();
        return;
    }
    let mut tree = ast::ParseResult {
        version: 0,
        stmts: vec![ast::RawStmt {
            stmt: expr.take(),
            stmt_location: 0,
            stmt_len: 0,
        }],
    };
    tree.for_each_node_mut(|n, _| {
        let slots: Vec<&mut Option<ast::Node>> = match n {
            typedpg_pg_query::NodeMut::AExpr(e) => vec![&mut e.lexpr, &mut e.rexpr],
            typedpg_pg_query::NodeMut::TypeCast(c) => vec![&mut c.arg],
            typedpg_pg_query::NodeMut::FuncCall(f) => f.args.iter_mut().collect(),
            typedpg_pg_query::NodeMut::BoolExpr(b) => b.args.iter_mut().collect(),
            typedpg_pg_query::NodeMut::NullTest(t) => vec![&mut t.arg],
            typedpg_pg_query::NodeMut::BooleanTest(t) => vec![&mut t.arg],
            typedpg_pg_query::NodeMut::AArrayExpr(a) => a.elements.iter_mut().collect(),
            typedpg_pg_query::NodeMut::CaseExpr(c) => {
                let mut v = vec![&mut c.arg, &mut c.defresult];
                v.extend(c.args.iter_mut());
                v
            }
            typedpg_pg_query::NodeMut::CaseWhen(w) => vec![&mut w.expr, &mut w.result],
            typedpg_pg_query::NodeMut::CoalesceExpr(c) => c.args.iter_mut().collect(),
            typedpg_pg_query::NodeMut::MinMaxExpr(m) => m.args.iter_mut().collect(),
            typedpg_pg_query::NodeMut::RowExpr(r) => r.args.iter_mut().collect(),
            typedpg_pg_query::NodeMut::CollateClause(c) => vec![&mut c.arg],
            typedpg_pg_query::NodeMut::NamedArgExpr(a) => vec![&mut a.arg],
            typedpg_pg_query::NodeMut::AIndirection(i) => vec![&mut i.arg],
            typedpg_pg_query::NodeMut::AIndices(i) => vec![&mut i.lidx, &mut i.uidx],
            typedpg_pg_query::NodeMut::List(l) => l.items.iter_mut().collect(),
            _ => Vec::new(),
        };
        for slot in slots {
            if let Some(i) = slot.as_ref().and_then(placeholder) {
                *slot = raws.get(i).cloned().flatten();
            }
        }
    });
    *expr = tree.stmts.pop().and_then(|s| s.stmt);
}

struct Printer<'a> {
    interp: &'a PgCatalog,
    /// The parts kept as written, which placeholders stand in for; `None`
    /// when such a part makes the text unprintable.
    raws: RefCell<Vec<Option<ast::Node>>>,
    placeholders: bool,
    /// Every constant printed as its output function would.
    exact: std::cell::Cell<bool>,
    /// `deparse_context.indentLevel`.
    indent: std::cell::Cell<i32>,
}

impl<'a> Printer<'a> {
    fn new(interp: &'a PgCatalog, placeholders: bool) -> Self {
        Printer {
            interp,
            raws: RefCell::new(Vec::new()),
            placeholders,
            exact: std::cell::Cell::new(true),
            indent: std::cell::Cell::new(0),
        }
    }

    fn type_of(c: &Cooked) -> Option<PgTypeOid> {
        c.ty.map(|t| t.oid)
    }

    /// `format_type_with_typemod(t, -1)`: `format_type_be`'s name, but
    /// `bpchar` (CHARACTER would mean `character(1)`).
    fn type_name(&self, t: PgTypeOid) -> String {
        let Some(ty) = self.interp.pg_type.get(&t) else {
            return String::new();
        };
        // `bit` with no modifier is printed quoted (BIT alone would mean
        // `bit(1)`).
        let unmodified = |t: &crate::pg_catalog::PgType| match t.typname.as_str() {
            "bpchar" => Some("bpchar"),
            "bit" => Some("\"bit\""),
            _ => None,
        };
        let catalog = |t: &crate::pg_catalog::PgType| {
            self.interp.namespace_name(t.typnamespace) == Some("pg_catalog")
        };
        if catalog(ty)
            && let Some(name) = unmodified(ty)
        {
            return name.to_owned();
        }
        if let Some(elem) = ty.typelem
            && self.interp.array_type_of(elem) == Some(t)
            && let Some(e) = self.interp.pg_type.get(&elem)
            && catalog(e)
            && let Some(name) = unmodified(e)
        {
            return format!("{name}[]");
        }
        crate::ddl::util::format_type_for_message(self.interp, t)
    }

    /// `format_type_with_typemod(t, typmod)` for a type with a modifier.
    fn type_name_typmod(&self, t: PgTypeOid, typmod: i32) -> String {
        use crate::typmod::DecodedTypmod;
        let name = self
            .interp
            .pg_type
            .get(&t)
            .filter(|ty| self.interp.namespace_name(ty.typnamespace) == Some("pg_catalog"))
            .map(|ty| ty.typname.as_str());
        let decoded = crate::typmod::decode(self.interp, t, Some(typmod));
        match (name, decoded) {
            (Some("varchar"), DecodedTypmod::Length(n)) => format!("character varying({n})"),
            (Some("bpchar"), DecodedTypmod::Length(n)) => format!("character({n})"),
            (Some("bit"), DecodedTypmod::Length(n)) => format!("bit({n})"),
            (Some("varbit"), DecodedTypmod::Length(n)) => format!("bit varying({n})"),
            (Some("numeric"), DecodedTypmod::Numeric { precision, scale }) => {
                format!("numeric({precision},{scale})")
            }
            (Some("timestamp"), DecodedTypmod::Precision(p)) => {
                format!("timestamp({p}) without time zone")
            }
            (Some("timestamptz"), DecodedTypmod::Precision(p)) => {
                format!("timestamp({p}) with time zone")
            }
            (Some("time"), DecodedTypmod::Precision(p)) => format!("time({p}) without time zone"),
            (Some("timetz"), DecodedTypmod::Precision(p)) => format!("time({p}) with time zone"),
            _ => {
                self.exact.set(false);
                self.type_name(t)
            }
        }
    }

    /// `get_func_sql_syntax`: the SQL syntax of built-in routine `proc`
    /// over `args`, appended to `buf` — `false` (nothing appended) for one
    /// it doesn't know.
    fn sql_syntax(&self, buf: &mut String, proc: PgProcOid, args: &[&Cooked]) -> Option<bool> {
        let row = self.interp.pg_proc.get(&proc)?;
        if self.interp.namespace_name(row.pronamespace) != Some("pg_catalog") {
            return Some(false);
        }
        let text = |c: &Cooked| self.text(c, false);
        let constant = |c: &Cooked| match &c.kind {
            Kind::Const(_, Some(v)) => Some(v.clone()),
            _ => None,
        };
        let out = match (row.proname.as_str(), args) {
            ("timezone", [zone, value]) => {
                format!("({} AT TIME ZONE {})", text(value)?, text(zone)?)
            }
            ("timezone", [value]) => format!("({} AT LOCAL)", text(value)?),
            ("overlaps", [a, b, c, d]) => format!(
                "(({}, {}) OVERLAPS ({}, {}))",
                text(a)?,
                text(b)?,
                text(c)?,
                text(d)?
            ),
            ("extract", [field, value]) => format!(
                "EXTRACT({} FROM {})",
                typedpg_core::quote_identifier(&constant(field)?),
                text(value)?
            ),
            ("is_normalized", [value]) => format!("({} IS NORMALIZED)", text(value)?),
            ("is_normalized", [value, form]) => {
                format!("({} IS {} NORMALIZED)", text(value)?, constant(form)?)
            }
            ("pg_collation_for", [value]) => format!("COLLATION FOR ({})", text(value)?),
            ("normalize", [value]) => format!("NORMALIZE({})", text(value)?),
            ("normalize", [value, form]) => {
                format!("NORMALIZE({}, {})", text(value)?, constant(form)?)
            }
            ("overlay", [a, b, c]) => format!(
                "OVERLAY({} PLACING {} FROM {})",
                text(a)?,
                text(b)?,
                text(c)?
            ),
            ("overlay", [a, b, c, d]) => format!(
                "OVERLAY({} PLACING {} FROM {} FOR {})",
                text(a)?,
                text(b)?,
                text(c)?,
                text(d)?
            ),
            ("position", [haystack, needle]) => {
                format!("POSITION(({}) IN ({}))", text(needle)?, text(haystack)?)
            }
            ("substring", [value, pattern, escape])
                if Self::type_of(pattern) == Some(oid::TEXT) =>
            {
                format!(
                    "SUBSTRING({} SIMILAR {} ESCAPE {})",
                    text(value)?,
                    text(pattern)?,
                    text(escape)?
                )
            }
            ("substring", [value, from]) => {
                format!("SUBSTRING({} FROM {})", text(value)?, text(from)?)
            }
            ("substring", [value, from, count]) => format!(
                "SUBSTRING({} FROM {} FOR {})",
                text(value)?,
                text(from)?,
                text(count)?
            ),
            (trim @ ("btrim" | "ltrim" | "rtrim"), [value, rest @ ..]) if rest.len() <= 1 => {
                let side = match trim {
                    "btrim" => "BOTH",
                    "ltrim" => "LEADING",
                    _ => "TRAILING",
                };
                let chars = match rest {
                    [chars] => format!(" {}", text(chars)?),
                    _ => String::new(),
                };
                format!("TRIM({side}{chars} FROM {})", text(value)?)
            }
            ("system_user", []) => "SYSTEM_USER".to_owned(),
            _ => return Some(false),
        };
        buf.push_str(&out);
        Some(true)
    }

    /// `quote_qualified_identifier(schema, name)`, the schema only when
    /// not `visible`.
    fn qualified(&self, ns: crate::oid::PgNamespaceOid, name: &str, visible: bool) -> String {
        match self.interp.namespace_name(ns) {
            Some(schema) if !visible => {
                crate::qualified_name::QualifiedName::new(schema, name).to_string()
            }
            _ => typedpg_core::quote_identifier(name),
        }
    }

    /// `generate_function_name`: unqualified when an unqualified call with
    /// the arguments' types (and names, and VARIADIC) resolves to `proc`.
    fn function_name(
        &self,
        proc: PgProcOid,
        args: &[(Option<String>, &Cooked)],
        variadic: bool,
    ) -> Option<String> {
        let row = self.interp.pg_proc.get(&proc)?;
        let types: Option<Vec<PgTypeOid>> = args.iter().map(|(_, a)| Self::type_of(a)).collect();
        let types = types.unwrap_or_else(|| row.proargtypes.clone());
        let notation = crate::functions::CallNotation {
            names: args.iter().filter_map(|(n, _)| n.clone()).collect(),
            variadic,
            ..Default::default()
        };
        let visible = crate::functions::resolve_function(
            self.interp,
            None,
            &row.proname,
            &types,
            &notation,
            false,
            None,
        )
        .is_ok_and(|f| f.oid == proc);
        Some(self.qualified(row.pronamespace, &row.proname, visible))
    }

    /// `generate_operator_name`: unqualified when the unqualified operator
    /// over the operands' types resolves to `op`, else `OPERATOR(s.op)`.
    fn operator_name(
        &self,
        op: PgOperatorOid,
        left: Option<PgTypeOid>,
        right: Option<PgTypeOid>,
    ) -> Option<String> {
        let row = self.interp.pg_operator.get(&op)?;
        let left = left.or(row.oprleft);
        let right = right.unwrap_or(row.oprright);
        let visible = matches!(
            self.interp.find_operator_detailed(&row.oprname, left, right),
            crate::lookup::OperatorMatch::Found(r) if r.oid == op
        );
        Some(if visible {
            row.oprname.clone()
        } else {
            format!(
                "OPERATOR({}.{})",
                typedpg_core::quote_identifier(self.interp.namespace_name(row.oprnamespace)?),
                row.oprname
            )
        })
    }

    /// `get_const_expr` (`showtype` 0, or -1 for no type label).
    fn constant(&self, t: PgTypeOid, value: Option<&str>, label: bool) -> String {
        let Some(v) = value else {
            return if label {
                format!("NULL::{}", self.type_name(t))
            } else {
                "NULL".to_owned()
            };
        };
        let quoted = |s: &str| format!("'{}'", s.replace('\'', "''"));
        if t == oid::BOOL {
            let text = if v == "t" { "true" } else { "false" }.to_owned();
            return text;
        }
        let out = match crate::ddl::stored_exprs::reg_output(self.interp, t, v) {
            Some(out) => out,
            None => match crate::ddl::const_output::output(self.interp, t, v) {
                Some(out) => out,
                // The output function isn't reproduced: the literal as
                // written stands in (the value it reads as is the same).
                None => {
                    self.exact.set(false);
                    v.to_owned()
                }
            },
        };
        let v = out.as_str();
        let (text, needlabel) = if t == oid::INT4 {
            if v.starts_with('-') {
                (quoted(v), true)
            } else {
                (v.to_owned(), false)
            }
        } else if t == oid::NUMERIC {
            if v.starts_with(|c: char| c.is_ascii_digit()) && v.contains(['.', 'e', 'E']) {
                (v.to_owned(), false)
            } else {
                (quoted(v), true)
            }
        } else {
            (quoted(v), t != oid::UNKNOWN)
        };
        if label && needlabel {
            format!("{text}::{}", self.type_name(t))
        } else {
            text
        }
    }

    /// `appendContextKeyword` with `PRETTYFLAG_INDENT` (which even the
    /// non-pretty output has): the buffer's trailing spaces removed, a
    /// newline, the indentation, the keyword.
    fn keyword(&self, buf: &mut String, word: &str, before: i32, after: i32) {
        let level = self.indent.get() + before;
        while buf.ends_with(' ') {
            buf.pop();
        }
        buf.push('\n');
        // PRETTYINDENT_LIMIT: deeper levels wrap.
        const LIMIT: i32 = 40;
        let amount = if level < LIMIT {
            level.max(0)
        } else {
            (LIMIT + (level - LIMIT) / 4) % LIMIT
        };
        buf.push_str(&" ".repeat(amount as usize));
        buf.push_str(word);
        self.indent.set((level + after).max(0));
    }

    fn list(&self, buf: &mut String, args: &[Cooked], show: bool) -> Option<()> {
        for (i, a) in args.iter().enumerate() {
            if i > 0 {
                buf.push_str(", ");
            }
            self.write(buf, a, show)?;
        }
        Some(())
    }

    /// `get_rule_expr(c, showimplicit = show)`, as a string.
    fn text(&self, c: &Cooked, show: bool) -> Option<String> {
        let mut buf = String::new();
        self.write(&mut buf, c, show)?;
        Some(buf)
    }

    /// `get_rule_expr(c, showimplicit = show)`, appended to `buf`.
    fn write(&self, buf: &mut String, c: &Cooked, show: bool) -> Option<()> {
        match &c.kind {
            Kind::Var(name) => buf.push_str(&typedpg_core::quote_identifier(name)),
            Kind::Const(t, value) => buf.push_str(&self.constant(*t, value.as_deref(), true)),
            Kind::Coerce(t, arg, explicit) => {
                if !explicit && !show {
                    return self.write(buf, arg, false);
                }
                // get_coercion_expr: a constant of the type already isn't
                // labeled twice.
                match &arg.kind {
                    Kind::Const(ct, value) if ct == t => {
                        buf.push_str(&self.constant(*ct, value.as_deref(), false));
                    }
                    _ => {
                        buf.push('(');
                        self.write(buf, arg, false)?;
                        buf.push(')');
                    }
                }
                buf.push_str("::");
                match c.ty.and_then(|ty| ty.typmod) {
                    Some(typmod) => buf.push_str(&self.type_name_typmod(*t, typmod)),
                    None => buf.push_str(&self.type_name(*t)),
                }
            }
            Kind::Op(op, args) => {
                buf.push('(');
                match args.as_slice() {
                    [l, r] => {
                        self.write(buf, l, true)?;
                        buf.push(' ');
                        buf.push_str(&self.operator_name(
                            *op,
                            Self::type_of(l),
                            Self::type_of(r),
                        )?);
                        buf.push(' ');
                        self.write(buf, r, true)?;
                    }
                    [r] => {
                        buf.push_str(&self.operator_name(*op, None, Self::type_of(r))?);
                        buf.push(' ');
                        self.write(buf, r, true)?;
                    }
                    _ => return None,
                }
                buf.push(')');
            }
            Kind::ScalarArrayOp(op, use_or, args) => {
                let [l, r] = args.as_slice() else {
                    return None;
                };
                let elem =
                    Self::type_of(r).and_then(|a| crate::expr::array_element_type(self.interp, a));
                buf.push('(');
                self.write(buf, l, true)?;
                buf.push(' ');
                buf.push_str(&self.operator_name(*op, Self::type_of(l), elem)?);
                buf.push_str(if *use_or { " ANY (" } else { " ALL (" });
                self.write(buf, r, true)?;
                buf.push_str("))");
            }
            Kind::Func(proc, args) => {
                let named: Vec<(Option<String>, &Cooked)> =
                    args.iter().map(|a| (None, a)).collect();
                buf.push_str(&self.function_name(*proc, &named, false)?);
                buf.push('(');
                self.list(buf, args, true)?;
                buf.push(')');
            }
            Kind::Call(proc, args, form) => {
                // A SQL syntax of its own (`EXTRACT(year FROM t)`): as
                // get_func_sql_syntax prints it, else as a call — the same
                // routine, but not the same text.
                if form.sql_syntax {
                    let values: Vec<&Cooked> = args.iter().map(|(_, a)| a).collect();
                    if self.sql_syntax(buf, *proc, &values)? {
                        return Some(());
                    }
                    self.exact.set(false);
                }
                let named: Vec<(Option<String>, &Cooked)> =
                    args.iter().map(|(n, a)| (n.clone(), a)).collect();
                buf.push_str(&self.function_name(*proc, &named, form.variadic)?);
                buf.push('(');
                for (i, (name, a)) in args.iter().enumerate() {
                    if i > 0 {
                        buf.push_str(", ");
                    }
                    if form.variadic && i + 1 == args.len() {
                        buf.push_str("VARIADIC ");
                    }
                    if let Some(n) = name {
                        buf.push_str(&typedpg_core::quote_identifier(n));
                        buf.push_str(" => ");
                    }
                    self.write(buf, a, true)?;
                }
                buf.push(')');
            }
            Kind::Array(elem, elems) => {
                buf.push_str("ARRAY[");
                self.list(buf, elems, true)?;
                buf.push(']');
                if elems.is_empty() {
                    buf.push_str("::");
                    buf.push_str(&self.type_name(self.interp.array_type_of(*elem)?));
                }
            }
            Kind::Bool(k, args) => {
                buf.push('(');
                if *k == ast::BoolExprType::NotExpr as u8 {
                    buf.push_str("NOT ");
                    self.write(buf, args.first()?, false)?;
                } else {
                    let sep = if *k == ast::BoolExprType::AndExpr as u8 {
                        " AND "
                    } else {
                        " OR "
                    };
                    for (i, a) in args.iter().enumerate() {
                        if i > 0 {
                            buf.push_str(sep);
                        }
                        self.write(buf, a, false)?;
                    }
                }
                buf.push(')');
            }
            Kind::NullTest(is_null, arg) => {
                buf.push('(');
                self.write(buf, arg, true)?;
                buf.push_str(if *is_null {
                    " IS NULL)"
                } else {
                    " IS NOT NULL)"
                });
            }
            Kind::BoolTest(k, arg) => {
                let test = match *k {
                    k if k == ast::BoolTestType::IsTrue as u8 => " IS TRUE)",
                    k if k == ast::BoolTestType::IsNotTrue as u8 => " IS NOT TRUE)",
                    k if k == ast::BoolTestType::IsFalse as u8 => " IS FALSE)",
                    k if k == ast::BoolTestType::IsNotFalse as u8 => " IS NOT FALSE)",
                    k if k == ast::BoolTestType::IsUnknown as u8 => " IS UNKNOWN)",
                    _ => " IS NOT UNKNOWN)",
                };
                buf.push('(');
                self.write(buf, arg, false)?;
                buf.push_str(test);
            }
            Kind::Case(arg, whens, default) => {
                // PRETTYINDENT_VAR.
                self.keyword(buf, "CASE", 0, 4);
                if let Some(a) = arg {
                    buf.push(' ');
                    self.write(buf, a, true)?;
                }
                for (cond, result) in whens {
                    self.keyword(buf, "WHEN ", 0, 0);
                    self.write(buf, cond, false)?;
                    buf.push_str(" THEN ");
                    self.write(buf, result, true)?;
                }
                self.keyword(buf, "ELSE ", 0, 0);
                self.write(buf, default, true)?;
                self.keyword(buf, "END", -4, 0);
            }
            Kind::Coalesce(args) => {
                buf.push_str("COALESCE(");
                self.list(buf, args, true)?;
                buf.push(')');
            }
            Kind::MinMax(greatest, args) => {
                buf.push_str(if *greatest { "GREATEST(" } else { "LEAST(" });
                self.list(buf, args, true)?;
                buf.push(')');
            }
            Kind::NullIf(_, args) => {
                buf.push_str("NULLIF(");
                self.list(buf, args, true)?;
                buf.push(')');
            }
            Kind::Distinct(_, args) => {
                let [l, r] = args.as_slice() else {
                    return None;
                };
                buf.push('(');
                self.write(buf, l, true)?;
                buf.push_str(" IS DISTINCT FROM ");
                self.write(buf, r, true)?;
                buf.push(')');
            }
            Kind::Row(args, cast) => {
                buf.push_str("ROW(");
                self.list(buf, args, true)?;
                buf.push(')');
                if let Some(t) = cast {
                    buf.push_str("::");
                    buf.push_str(&self.type_name(*t));
                }
            }
            Kind::Collate(collation, arg) => {
                let row = self.interp.pg_collation.get(collation)?;
                let visible = self
                    .interp
                    .resolve_collation(None, &row.collname)
                    .is_some_and(|c| c.oid == *collation);
                buf.push('(');
                self.write(buf, arg, show)?;
                buf.push_str(" COLLATE ");
                buf.push_str(&self.qualified(row.collnamespace, &row.collname, visible));
                buf.push(')');
            }
            Kind::Subscript(arg, subscripts) => {
                let parens = !matches!(arg.kind, Kind::Var(_) | Kind::Field(..));
                if parens {
                    buf.push('(');
                }
                self.write(buf, arg, show)?;
                if parens {
                    buf.push(')');
                }
                for (lower, upper, slice) in subscripts {
                    buf.push('[');
                    if *slice {
                        if let Some(l) = lower {
                            self.write(buf, l, false)?;
                        }
                        buf.push(':');
                    }
                    if let Some(u) = upper {
                        self.write(buf, u, false)?;
                    }
                    buf.push(']');
                }
            }
            Kind::Field(arg, field) => {
                let parens = !matches!(arg.kind, Kind::Subscript(..) | Kind::Field(..));
                if parens {
                    buf.push('(');
                }
                self.write(buf, arg, true)?;
                if parens {
                    buf.push(')');
                }
                buf.push('.');
                buf.push_str(&typedpg_core::quote_identifier(field));
            }
            Kind::Raw(node) => {
                if !self.placeholders {
                    return None;
                }
                self.exact.set(false);
                let mut raws = self.raws.borrow_mut();
                raws.push(crate::ddl::stored_exprs::render(self.interp, node.as_ref()));
                buf.push_str(&typedpg_core::quote_identifier(&format!(
                    "{RAW_PLACEHOLDER}{}",
                    raws.len() - 1
                )));
            }
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use crate::pg_catalog::PgCatalog;

    /// The text ruleutils prints for `expr` over table `t`.
    fn printed(db: &PgCatalog, expr: &str) -> Option<String> {
        let relid = db
            .pg_class
            .iter()
            .find(|(_, c)| c.relname == "t")
            .map(|(&o, _)| o)?;
        let parsed = typedpg_pg_query::parse(&format!("SELECT {expr}")).ok()?;
        let typedpg_pg_query::ast::Node::SelectStmt(s) = parsed.tree.stmts[0].stmt.clone()? else {
            return None;
        };
        let Some(Some(typedpg_pg_query::ast::Node::ResTarget(t))) = s.target_list.first().cloned()
        else {
            return None;
        };
        super::ruleutils_text(db, relid, t.val.as_ref())
    }

    #[test]
    fn expressions_print_as_ruleutils_prints_them() {
        // What pg_get_expr prints for these on PostgreSQL 19 (the pg_sanity
        // mirror compares every stored expression's text the same way).
        let mut db = PgCatalog::new().unwrap();
        db.apply_sql(
            "CREATE FUNCTION pos(int) RETURNS bool LANGUAGE sql IMMUTABLE AS 'SELECT $1 > 0';
             CREATE DOMAIN dbool AS bool;
             CREATE TABLE t (a int, b smallint, c varchar(10), d text, f bool, bd dbool);",
        )
        .unwrap();
        for (expr, text) in [
            ("a > '5'", "(a > 5)"),
            ("pos(b)", "pos((b)::integer)"),
            ("c = 'x'", "((c)::text = 'x'::text)"),
            ("bd AND f", "(bd AND f)"),
            (
                "COALESCE(a, b, 0) > 0",
                "(COALESCE(a, (b)::integer, 0) > 0)",
            ),
            (
                "CASE WHEN a > 0 THEN b END > 0",
                "(\nCASE\n    WHEN (a > 0) THEN b\n    ELSE NULL::smallint\nEND > 0)",
            ),
            ("d::char(3) <> ''", "((d)::character(3) <> ''::bpchar)"),
            ("a IS NOT DISTINCT FROM b", "(NOT (a IS DISTINCT FROM b))"),
        ] {
            assert_eq!(printed(&db, expr).as_deref(), Some(text), "{expr}");
        }
    }
}
