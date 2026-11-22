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
            Kind::Coerce(t, arg) => self.coerce(t, self.fold(*arg)),
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
            kind @ (Kind::Var(_) | Kind::Const(..) | Kind::Raw(_)) => Cooked { kind, ty },
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
        while let Kind::Coerce(d, inner) = &arg.kind
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
