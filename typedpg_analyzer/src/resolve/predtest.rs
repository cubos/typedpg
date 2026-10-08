//! A subset of PG's `predicate_implied_by` (optimizer/util/predtest.c), used
//! to decide whether a partial unique index can arbitrate an ON CONFLICT
//! clause: the index predicate must be implied by the ON CONFLICT WHERE.
//!
//! Both sides are raw ASTs over the single target relation, compared after
//! dropping column qualifiers. Each comparison's operands are typed as PG
//! types them (see [`normalize`]), so an implicit cast and the explicit one
//! compare equal, as `equal()` ignores how a coercion was written. Proven,
//! like PG:
//! - AND / OR structure (`predicate_implied_by_recurse`), with `x IN (...)`,
//!   `x NOT IN (...)` and `x op ANY|ALL (ARRAY[...])` read as the OR / AND
//!   of their comparisons (`predicate_classify`);
//! - structurally equal atoms;
//! - `x IS NOT NULL` from a (strict) comparison on `x`;
//! - `x op1 c1` ⇒ `x op2 c2` for btree comparison operators over constants
//!   of the same kind (`operator_predicate_proof`), decided like
//!   `BT_implic_table` by containment on an ordered domain.
//!
//! Anything else is not proven, so the index doesn't qualify.

use typedpg_pg_query::protobuf::{self, node};

use super::node_fingerprint;
use crate::coerce::{CoercionContext, CoercionPath, coercion_pathway};
use crate::oid::{PgClassOid, PgTypeOid};
use crate::pg_catalog::{PgCatalog, oid};

/// The catalog a comparison is typed against, and the relation its
/// columns are of.
pub(crate) struct Types<'a> {
    pub snapshot: &'a PgCatalog,
    pub table: PgClassOid,
}

impl Types<'_> {
    fn column(&self, name: &str) -> Option<PgTypeOid> {
        self.snapshot
            .attributes_of(self.table)
            .iter()
            .find(|a| a.attname == name && a.attnum > 0)
            .map(|a| a.atttypid)
    }

    fn type_name(&self, tn: &protobuf::TypeName) -> Option<PgTypeOid> {
        crate::ddl::util::resolve_type_name(tn, self.snapshot)
    }

    /// The input types of the operator `name` resolves to for operands of
    /// types `l` and `r`.
    fn operator(&self, name: &str, l: PgTypeOid, r: PgTypeOid) -> Option<(PgTypeOid, PgTypeOid)> {
        let op = self.snapshot.find_operator(name, Some(l), r)?;
        Some((op.left_type_oid?, op.right_type_oid))
    }

    /// Is coercing `from` to `to` a RelabelType (binary compatible, to a
    /// type that is no domain)?
    fn relabel(&self, from: PgTypeOid, to: PgTypeOid) -> bool {
        self.snapshot.unwrap_domain(to) == to
            && coercion_pathway(to, from, CoercionContext::Explicit, self.snapshot)
                == Some(CoercionPath::Relabel)
    }
}

/// PG's `predicate_implied_by(predicate, clause, weak = false)`: is
/// `predicate` true whenever `clause` is? No predicate is always implied;
/// a missing clause implies nothing else.
pub(crate) fn predicate_implied_by(
    predicate: Option<&protobuf::Node>,
    clause: Option<&protobuf::Node>,
    types: &Types,
) -> bool {
    let Some(predicate) = predicate else {
        return true;
    };
    let Some(clause) = clause else {
        return false;
    };
    implied_by_recurse(
        &normalize(&unqualify(clause), types),
        &normalize(&unqualify(predicate), types),
    )
}

/// `predicate_classify` caps the arrays it expands (MAX_SAOP_ARRAY_SIZE).
const MAX_SAOP_ARRAY_SIZE: usize = 100;

/// `n` with each `x IN (...)` / `x NOT IN (...)` / `x op ANY|ALL
/// (ARRAY[...])` expanded into the OR / AND of `x op element` (as
/// `predicate_classify` reads a ScalarArrayOpExpr), and each comparison
/// typed ([`typed_comparison`]).
fn normalize(n: &protobuf::Node, types: &Types) -> protobuf::Node {
    use protobuf::AExprKind as K;
    let Some(inner) = n.node.as_ref() else {
        return n.clone();
    };
    match inner {
        node::Node::BoolExpr(b) => {
            let mut b = (**b).clone();
            b.args = b.args.iter().map(|a| normalize(a, types)).collect();
            protobuf::Node {
                node: Some(node::Node::BoolExpr(Box::new(b))),
            }
        }
        node::Node::AExpr(e) if matches!(e.kind(), K::AexprIn | K::AexprOpAny | K::AexprOpAll) => {
            let elements = match (e.kind(), e.rexpr.as_deref().and_then(|r| r.node.as_ref())) {
                (K::AexprIn, Some(node::Node::List(l))) => &l.items,
                (K::AexprOpAny | K::AexprOpAll, Some(node::Node::AArrayExpr(a))) => &a.elements,
                _ => return n.clone(),
            };
            let Some(x) = e.lexpr.as_deref() else {
                return n.clone();
            };
            if elements.is_empty() || elements.len() > MAX_SAOP_ARRAY_SIZE {
                return n.clone();
            }
            // `IN` is `= ANY`, `NOT IN` is `<> ALL`.
            let any = match e.kind() {
                K::AexprIn => matches!(
                    super::expr::extract_string_fields(&e.name).as_slice(),
                    [op] if op == "="
                ),
                K::AexprOpAny => true,
                _ => false,
            };
            let args = elements
                .iter()
                .map(|el| {
                    let mut cmp = (**e).clone();
                    cmp.set_kind(K::AexprOp);
                    cmp.lexpr = Some(Box::new(x.clone()));
                    cmp.rexpr = Some(Box::new(el.clone()));
                    typed_comparison(cmp, types)
                })
                .collect();
            protobuf::Node {
                node: Some(node::Node::BoolExpr(Box::new(protobuf::BoolExpr {
                    boolop: if any {
                        protobuf::BoolExprType::OrExpr
                    } else {
                        protobuf::BoolExprType::AndExpr
                    } as i32,
                    args,
                    ..Default::default()
                }))),
            }
        }
        node::Node::AExpr(e) if e.kind() == K::AexprOp => typed_comparison((**e).clone(), types),
        _ => n.clone(),
    }
}

/// The comparison `e` with its operands as PG's `make_op` leaves them:
/// one whose type isn't the resolved operator's input type cast to it (the
/// implicit cast PG adds, which `equal()` doesn't tell from an explicit
/// one), and a constant of that type a bare literal (`'x'::text` against a
/// text operand — how pg_dump writes an index predicate — is `'x'`). Any
/// operand whose type the raw AST doesn't tell leaves `e` as written.
fn typed_comparison(mut e: protobuf::AExpr, types: &Types) -> protobuf::Node {
    let unchanged = |e: protobuf::AExpr| protobuf::Node {
        node: Some(node::Node::AExpr(Box::new(e))),
    };
    let op = match super::expr::extract_string_fields(&e.name).as_slice() {
        [op] => op.clone(),
        _ => return unchanged(e),
    };
    let (Some(l), Some(r)) = (e.lexpr.as_deref(), e.rexpr.as_deref()) else {
        return unchanged(e);
    };
    let (Some(lt), Some(rt)) = (operand_type(l, types), operand_type(r, types)) else {
        return unchanged(e);
    };
    let Some((dl, dr)) = types.operator(&op, lt, rt) else {
        return unchanged(e);
    };
    let (Some(l), Some(r)) = (
        typed_operand(l, lt, dl, types),
        typed_operand(r, rt, dr, types),
    ) else {
        return unchanged(e);
    };
    e.lexpr = Some(Box::new(l));
    e.rexpr = Some(Box::new(r));
    unchanged(e)
}

/// The operand `x`, of type `t`, as an operator taking `declared` reads it;
/// `None` when that can't be written in the canonical form.
fn typed_operand(
    x: &protobuf::Node,
    t: PgTypeOid,
    declared: PgTypeOid,
    types: &Types,
) -> Option<protobuf::Node> {
    let constant = match x.node.as_ref()? {
        node::Node::AConst(_) => Some(x),
        node::Node::TypeCast(tc) => tc
            .arg
            .as_deref()
            .filter(|a| matches!(a.node.as_ref(), Some(node::Node::AConst(_)))),
        _ => None,
    };
    if let Some(c) = constant {
        // An unknown literal takes the operator's type.
        return (t == declared || t == oid::UNKNOWN).then(|| c.clone());
    }
    Some(coerce(canonical(x, types)?, t, declared, types))
}

/// `x` with its casts written canonically ([`coerce`]); `None` for a type
/// name with a modifier or array bounds, which is another expression.
fn canonical(x: &protobuf::Node, types: &Types) -> Option<protobuf::Node> {
    let Some(node::Node::TypeCast(tc)) = x.node.as_ref() else {
        return Some(x.clone());
    };
    let (arg, written) = (tc.arg.as_deref()?, tc.type_name.as_ref()?);
    if !written.typmods.is_empty() || !written.array_bounds.is_empty() || written.pct_type {
        return None;
    }
    let from = operand_type(arg, types)?;
    let to = types.type_name(written)?;
    Some(coerce(canonical(arg, types)?, from, to, types))
}

/// `x`, of type `from`, coerced to `to` as `coerce_type` leaves it — the
/// type named by its oid so that every spelling of it (`text`,
/// `pg_catalog.text`) compares equal, and an implicit coercion like an
/// explicit one. No coercion to its own type; a binary-compatible one is a
/// RelabelType, which `applyRelabelType` stacks on no other and drops when
/// the relabelings net out to nothing (`t::varchar::text` is `t`).
fn coerce(x: protobuf::Node, from: PgTypeOid, to: PgTypeOid, types: &Types) -> protobuf::Node {
    if from == to {
        return x;
    }
    if !types.relabel(from, to) {
        return cast_node(x, ["coerce".to_owned(), to.get().to_string()]);
    }
    let (inner, inner_type) = match relabeled(&x) {
        Some(r) => r,
        None => (x, from),
    };
    if inner_type == to {
        return inner;
    }
    let names = [
        "relabel".to_owned(),
        inner_type.get().to_string(),
        to.get().to_string(),
    ];
    cast_node(inner, names)
}

/// A RelabelType [`coerce`] wrote: its operand and that operand's type.
fn relabeled(x: &protobuf::Node) -> Option<(protobuf::Node, PgTypeOid)> {
    let Some(node::Node::TypeCast(tc)) = x.node.as_ref() else {
        return None;
    };
    match super::expr::extract_string_fields(&tc.type_name.as_ref()?.names).as_slice() {
        [kind, from, _] if kind == "relabel" => Some((
            (**tc.arg.as_ref()?).clone(),
            PgTypeOid::new(from.parse().ok()?)?,
        )),
        _ => None,
    }
}

fn cast_node<const N: usize>(x: protobuf::Node, names: [String; N]) -> protobuf::Node {
    protobuf::Node {
        node: Some(node::Node::TypeCast(Box::new(protobuf::TypeCast {
            arg: Some(Box::new(x)),
            type_name: Some(protobuf::TypeName {
                names: names
                    .into_iter()
                    .map(|sval| protobuf::Node {
                        node: Some(node::Node::String(protobuf::String { sval })),
                    })
                    .collect(),
                typemod: -1,
                ..Default::default()
            }),
            location: 0,
        }))),
    }
}

/// The type of an operand whose type the raw AST tells: a column's, a
/// cast's (`c::text`), a literal's.
fn operand_type(x: &protobuf::Node, types: &Types) -> Option<PgTypeOid> {
    use protobuf::a_const::Val;
    match x.node.as_ref()? {
        node::Node::ColumnRef(cr) => match cr.fields.as_slice() {
            [
                protobuf::Node {
                    node: Some(node::Node::String(s)),
                },
            ] => types.column(&s.sval),
            _ => None,
        },
        node::Node::TypeCast(tc) => types.type_name(tc.type_name.as_ref()?),
        // `make_const`: an integer literal is an int4, one too wide an
        // int8, a decimal a numeric.
        node::Node::AConst(c) if !c.isnull => Some(match c.val.as_ref()? {
            Val::Ival(_) => oid::INT4,
            Val::Fval(f) if !f.fval.contains(['.', 'e', 'E']) && f.fval.parse::<i64>().is_ok() => {
                oid::INT8
            }
            Val::Fval(_) => oid::NUMERIC,
            Val::Sval(_) => oid::UNKNOWN,
            Val::Boolval(_) => oid::BOOL,
            Val::Bsval(_) => return None,
        }),
        _ => None,
    }
}

/// How `predicate_implied_by_recurse` classifies a node.
enum Shape<'a> {
    And(&'a [protobuf::Node]),
    Or(&'a [protobuf::Node]),
    Atom,
}

fn shape(n: &protobuf::Node) -> Shape<'_> {
    match n.node.as_ref() {
        Some(node::Node::BoolExpr(b)) if b.boolop() == protobuf::BoolExprType::AndExpr => {
            Shape::And(&b.args)
        }
        Some(node::Node::BoolExpr(b)) if b.boolop() == protobuf::BoolExprType::OrExpr => {
            Shape::Or(&b.args)
        }
        _ => Shape::Atom,
    }
}

fn implied_by_recurse(clause: &protobuf::Node, pred: &protobuf::Node) -> bool {
    match (shape(clause), shape(pred)) {
        (_, Shape::And(ps)) => ps.iter().all(|p| implied_by_recurse(clause, p)),
        (Shape::And(cs), Shape::Or(ps)) => {
            ps.iter().any(|p| implied_by_recurse(clause, p))
                || cs.iter().any(|c| implied_by_recurse(c, pred))
        }
        (Shape::And(cs), Shape::Atom) => cs.iter().any(|c| implied_by_recurse(c, pred)),
        (Shape::Or(cs), Shape::Or(ps)) => cs
            .iter()
            .all(|c| ps.iter().any(|p| implied_by_recurse(c, p))),
        (Shape::Or(cs), Shape::Atom) => cs.iter().all(|c| implied_by_recurse(c, pred)),
        (Shape::Atom, Shape::Or(ps)) => ps.iter().any(|p| implied_by_recurse(clause, p)),
        (Shape::Atom, Shape::Atom) => simple_clause_implied(clause, pred),
    }
}

/// `predicate_implied_by_simple_clause`.
fn simple_clause_implied(clause: &protobuf::Node, pred: &protobuf::Node) -> bool {
    if node_fingerprint(clause) == node_fingerprint(pred) {
        return true;
    }
    // `x IS NOT NULL` follows from any strict comparison on x.
    if let Some(node::Node::NullTest(nt)) = pred.node.as_ref()
        && nt.nulltesttype() == protobuf::NullTestType::IsNotNull
        && !nt.argisrow
        && let Some(arg) = nt.arg.as_deref()
        && let Some((x, _, _)) = comparison(clause)
        && node_fingerprint(&x) == node_fingerprint(arg)
    {
        return true;
    }
    operator_predicate_proof(clause, pred)
}

/// A btree comparison operator.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cmp {
    Lt,
    Le,
    Eq,
    Ge,
    Gt,
    Ne,
}

impl Cmp {
    fn parse(op: &str) -> Option<Cmp> {
        Some(match op {
            "<" => Cmp::Lt,
            "<=" => Cmp::Le,
            "=" => Cmp::Eq,
            ">=" => Cmp::Ge,
            ">" => Cmp::Gt,
            "<>" | "!=" => Cmp::Ne,
            _ => return None,
        })
    }

    /// The operator with its operands swapped (`c < x` is `x > c`).
    fn commute(self) -> Cmp {
        match self {
            Cmp::Lt => Cmp::Gt,
            Cmp::Le => Cmp::Ge,
            Cmp::Gt => Cmp::Lt,
            Cmp::Ge => Cmp::Le,
            other => other,
        }
    }

    fn holds(self, a: &Constant, b: &Constant) -> Option<bool> {
        // Unknown-typed literals only relate by equality (their ordering
        // depends on the type and collation they resolve to).
        if let (Constant::Text(a), Constant::Text(b)) = (a, b) {
            return match self {
                Cmp::Eq => Some(a == b),
                Cmp::Ne => Some(a != b),
                _ => None,
            };
        }
        let ord = a.compare(b)?;
        Some(match self {
            Cmp::Lt => ord.is_lt(),
            Cmp::Le => ord.is_le(),
            Cmp::Eq => ord.is_eq(),
            Cmp::Ge => ord.is_ge(),
            Cmp::Gt => ord.is_gt(),
            Cmp::Ne => ord.is_ne(),
        })
    }
}

/// A constant operand. Only constants of the same kind are compared: an
/// integer and a decimal literal resolve to different operand types, which
/// PG may not relate through one operator family. A decimal literal is a
/// `numeric`, compared exactly.
enum Constant {
    Int(i64),
    Decimal(crate::decimal::Decimal),
    Text(String),
}

impl Constant {
    fn of(n: &protobuf::Node) -> Option<Constant> {
        use protobuf::a_const::Val;
        let Some(node::Node::AConst(c)) = n.node.as_ref() else {
            return None;
        };
        if c.isnull {
            return None;
        }
        Some(match c.val.as_ref()? {
            Val::Ival(i) => Constant::Int(i64::from(i.ival)),
            Val::Fval(f) => match f.fval.parse::<i64>() {
                // An integer too wide for int4 is still an integer literal.
                Ok(i) if !f.fval.contains(['.', 'e', 'E']) => Constant::Int(i),
                _ => Constant::Decimal(crate::decimal::Decimal::parse(&f.fval)?),
            },
            Val::Sval(s) => Constant::Text(s.sval.clone()),
            _ => return None,
        })
    }

    fn compare(&self, other: &Constant) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (Constant::Int(a), Constant::Int(b)) => Some(a.cmp(b)),
            (Constant::Decimal(a), Constant::Decimal(b)) => a.compare(*b),
            _ => None,
        }
    }
}

/// `x op c` / `c op x` as `(x, op, c)` with the constant on the right.
fn comparison(n: &protobuf::Node) -> Option<(protobuf::Node, Cmp, Constant)> {
    let Some(node::Node::AExpr(e)) = n.node.as_ref() else {
        return None;
    };
    if e.kind() != protobuf::AExprKind::AexprOp {
        return None;
    }
    let op = match super::expr::extract_string_fields(&e.name).as_slice() {
        [op] => Cmp::parse(op)?,
        [schema, op] if schema == "pg_catalog" => Cmp::parse(op)?,
        _ => return None,
    };
    let (l, r) = (e.lexpr.as_deref()?, e.rexpr.as_deref()?);
    match (Constant::of(l), Constant::of(r)) {
        (None, Some(c)) => Some((l.clone(), op, c)),
        (Some(c), None) => Some((r.clone(), op.commute(), c)),
        _ => None,
    }
}

/// `operator_predicate_proof`: does `x op1 c1` imply `x op2 c2`? Decided
/// as containment of the value sets on an ordered domain, which is what
/// `BT_implic_table`'s test operators encode.
fn operator_predicate_proof(clause: &protobuf::Node, pred: &protobuf::Node) -> bool {
    let (Some((cx, op1, c1)), Some((px, op2, c2))) = (comparison(clause), comparison(pred)) else {
        return false;
    };
    if node_fingerprint(&cx) != node_fingerprint(&px) {
        return false;
    }
    let test = |op: Cmp, a: &Constant, b: &Constant| op.holds(a, b).unwrap_or(false);
    match op1 {
        // {c1} ⊆ S2 iff c1 itself satisfies the predicate.
        Cmp::Eq => test(op2, &c1, &c2),
        Cmp::Lt => match op2 {
            Cmp::Lt | Cmp::Le | Cmp::Ne => test(Cmp::Le, &c1, &c2),
            _ => false,
        },
        Cmp::Le => match op2 {
            Cmp::Lt | Cmp::Ne => test(Cmp::Lt, &c1, &c2),
            Cmp::Le => test(Cmp::Le, &c1, &c2),
            _ => false,
        },
        Cmp::Gt => match op2 {
            Cmp::Gt | Cmp::Ge | Cmp::Ne => test(Cmp::Ge, &c1, &c2),
            _ => false,
        },
        Cmp::Ge => match op2 {
            Cmp::Gt | Cmp::Ne => test(Cmp::Gt, &c1, &c2),
            Cmp::Ge => test(Cmp::Ge, &c1, &c2),
            _ => false,
        },
        Cmp::Ne => op2 == Cmp::Ne && test(Cmp::Eq, &c1, &c2),
    }
}

/// `node` with every qualified column reference (`t.v`) reduced to its
/// column name — both sides refer to the one target relation.
pub(crate) fn unqualify(node: &protobuf::Node) -> protobuf::Node {
    let mut tree = protobuf::ParseResult {
        version: 0,
        stmts: vec![protobuf::RawStmt {
            stmt: Some(Box::new(node.clone())),
            stmt_location: 0,
            stmt_len: 0,
        }],
    };
    // SAFETY: the tree is neither moved nor dropped while the pointers are
    // used, and only column references are written through them; a column
    // reference contains no other column reference, so no pointer that is
    // dereferenced points into a vector that has been modified.
    unsafe {
        let column_refs: Vec<_> = tree
            .nodes_mut()
            .into_iter()
            .filter_map(|(n, _)| match n {
                typedpg_pg_query::NodeMut::ColumnRef(cr) => Some(cr),
                _ => None,
            })
            .collect();
        for cr in column_refs {
            let fields = &mut (*cr).fields;
            if fields.len() > 1
                && matches!(
                    fields.last().and_then(|f| f.node.as_ref()),
                    Some(node::Node::String(_))
                )
            {
                let last = fields.pop();
                fields.clear();
                fields.extend(last);
            }
        }
    }
    tree.stmts
        .pop()
        .and_then(|s| s.stmt)
        .map(|b| *b)
        .unwrap_or_default()
}
