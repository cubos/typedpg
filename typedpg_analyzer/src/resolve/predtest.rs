//! A subset of PG's `predicate_implied_by` (optimizer/util/predtest.c), used
//! to decide whether a partial unique index can arbitrate an ON CONFLICT
//! clause: the index predicate must be implied by the ON CONFLICT WHERE.
//!
//! Both sides are raw ASTs over the single target relation, compared after
//! dropping column qualifiers. Proven, like PG:
//! - AND / OR structure (`predicate_implied_by_recurse`);
//! - structurally equal atoms;
//! - `x IS NOT NULL` from a (strict) comparison on `x`;
//! - `x op1 c1` ⇒ `x op2 c2` for btree comparison operators over constants
//!   of the same kind (`operator_predicate_proof`), decided like
//!   `BT_implic_table` by containment on an ordered domain.
//!
//! Anything else is not proven, so the index doesn't qualify.

use typedpg_pg_query::protobuf::{self, node};

use super::node_fingerprint;

/// PG's `predicate_implied_by(predicate, clause, weak = false)`: is
/// `predicate` true whenever `clause` is? No predicate is always implied;
/// a missing clause implies nothing else.
pub(crate) fn predicate_implied_by(
    predicate: Option<&protobuf::Node>,
    clause: Option<&protobuf::Node>,
) -> bool {
    let Some(predicate) = predicate else {
        return true;
    };
    let Some(clause) = clause else {
        return false;
    };
    implied_by_recurse(&unqualify(clause), &unqualify(predicate))
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
/// PG may not relate through one operator family.
enum Constant {
    Int(i64),
    Decimal(f64),
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
                _ => Constant::Decimal(f.fval.parse().ok()?),
            },
            Val::Sval(s) => Constant::Text(s.sval.clone()),
            _ => return None,
        })
    }

    fn compare(&self, other: &Constant) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (Constant::Int(a), Constant::Int(b)) => Some(a.cmp(b)),
            (Constant::Decimal(a), Constant::Decimal(b)) => a.partial_cmp(b),
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
fn unqualify(node: &protobuf::Node) -> protobuf::Node {
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
