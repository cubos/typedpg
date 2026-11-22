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
//! (An index's expressions and predicate reach CompareIndexInfo from the
//! relcache, which folds their constants — RelationGetIndexExpressions —
//! so there `(a + (1 + 1))` is `(a + 2)`; that folding isn't modeled, and
//! such indexes compare different.)

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
    /// follows from the two types.
    Coerce(PgTypeOid, Box<Cooked>),
    /// An `ArrayExpr` of an element type.
    Array(PgTypeOid, Vec<Cooked>),
    /// A `BoolExpr` (AND / OR / NOT).
    Bool(u8, Vec<Cooked>),
    /// A `NullTest` (IS NULL when true).
    NullTest(bool, Box<Cooked>),
    /// Anything else, as written.
    Raw(Option<ast::Node>),
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
            (Kind::Coerce(t1, a1), Kind::Coerce(t2, a2)) => t1 == t2 && a1 == a2,
            (Kind::Array(t1, e1), Kind::Array(t2, e2)) => t1 == t2 && e1 == e2,
            (Kind::Bool(k1, a1), Kind::Bool(k2, a2)) => k1 == k2 && a1 == a2,
            (Kind::NullTest(n1, a1), Kind::NullTest(n2, a2)) => n1 == n2 && a1 == a2,
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
            _ => return None,
        };
        Some(Cooked { kind, ty })
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
            ast::AConstValue::String(s) => {
                let t = recorded(&self.log.literals, &NodeKey::of(ac))
                    .filter(|t| *t != oid::UNKNOWN)?;
                return self.literal(t, Some(&s.sval));
            }
            _ => return None,
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
        let target = crate::nonnull::cast_target(tc, self.interp)?;
        let arg = tc.arg.as_ref()?;
        if let Some((_, text)) = Self::untyped(arg) {
            return self.literal(target, text);
        }
        if let ast::Node::AArrayExpr(a) = arg {
            let elem = crate::expr::array_element_type(self.interp, target)?;
            return self.array(a, target, elem);
        }
        let c = self.cook(arg);
        let t = c.ty?;
        if t.oid == oid::UNKNOWN {
            return None;
        }
        if t.oid != target {
            return Some(coerced(target, c));
        }
        // The same type: no node when the value's type modifier is -1
        // already, as it is for a constant or an operator's or function's
        // result.
        let default_typmod = match &c.kind {
            Kind::Var(_) => t.typmod.is_none(),
            Kind::Const(..)
            | Kind::Op(..)
            | Kind::ScalarArrayOp(..)
            | Kind::Func(..)
            | Kind::Coerce(..)
            | Kind::Bool(..)
            | Kind::NullTest(..) => true,
            Kind::Array(..) | Kind::Raw(_) => false,
        };
        Some(if default_typmod {
            c
        } else {
            coerced(target, c)
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
            _ => return None,
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
            || fc.func_variadic
            || fc.ignore_nulls != 0
            || fc.over.is_some()
            || fc.agg_filter.is_some()
            || !fc.agg_order.is_empty()
        {
            return None;
        }
        let proc = recorded(&self.log.calls, &NodeKey::of(fc))?;
        let row = self.interp.pg_proc.get(&proc)?;
        if row.provariadic.is_some() {
            return None;
        }
        let mut args = Vec::new();
        for (i, a) in fc.args.iter().enumerate() {
            let a = a.as_ref()?;
            if matches!(a, ast::Node::NamedArgExpr(_)) {
                return None;
            }
            args.push(self.arg(a, Some(*row.proargtypes.get(i)?)));
        }
        Some(Cooked {
            kind: Kind::Func(proc, args),
            ty: self.ty(node),
        })
    }
}

fn bool_ty() -> Ty {
    Ty {
        oid: oid::BOOL,
        typmod: None,
    }
}

/// `c` coerced to `t`.
fn coerced(t: PgTypeOid, c: Cooked) -> Cooked {
    Cooked {
        kind: Kind::Coerce(t, Box::new(c)),
        ty: Some(Ty {
            oid: t,
            typmod: None,
        }),
    }
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
