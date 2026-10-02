//! A table's CHECK constraints as knowledge about its rows.
//!
//! Every row of a table satisfies its CHECK constraints: each one is TRUE
//! or NULL — *not FALSE* — for it. A constraint is assumed to hold as
//! declared, `NOT VALID` included (only `NOT ENFORCED` ones are skipped):
//! what the schema states is taken to be true of the data.
//!
//! "Not FALSE" alone proves little about a strict test (`CHECK (x > 0)`
//! holds for a NULL `x`), but a test that is never NULL — `x IS NOT NULL`,
//! `num_nonnulls(a, b) = 1` — is TRUE when not FALSE. With what a query
//! knows of a row (its WHERE says `kind = 'a'`), alternatives of a
//! constraint get *refuted*, and what is left of them is proven:
//!
//! ```sql
//! CHECK (kind <> 'a' OR a_id IS NOT NULL)   -- with WHERE kind = 'a'
//! CHECK ((kind = 'a' AND a_id IS NOT NULL) OR (kind = 'b' AND b_id IS NOT NULL))
//! CHECK (num_nonnulls(a_id, b_id) = 1)      -- coalesce(a_id, b_id) is NOT NULL
//! ```
//!
//! Each top-level conjunct of a constraint is read as an OR of AND-arms of
//! literals ([`Lit`]); a conjunct not FALSE has an arm none of whose
//! literals is FALSE. When one arm is left, its never-NULL literals are
//! TRUE; when several are, at least one of their never-NULL literals is.

use std::collections::{BTreeSet, HashSet};

use typedpg_pg_query::protobuf::{self, node};

use super::{Col, Facts, LitKind, Literal};
use crate::oid::PgClassOid;
use crate::pg_catalog::PgCatalog;

/// One literal of a constraint, over the table's column names.
#[derive(Debug, Clone)]
enum Lit {
    /// `c IS NOT NULL` (never NULL).
    NotNull(String),
    /// `c IS NULL` (never NULL).
    IsNull(String),
    /// `c = v` (NULL for a NULL `c`).
    Eq(String, Literal),
    /// `c <> v`.
    Ne(String, Literal),
    /// `c IN (v, …)`.
    In(String, Vec<Literal>),
    /// `c NOT IN (v, …)`.
    NotIn(String, Vec<Literal>),
    /// At least one of the columns is non-NULL (`num_nonnulls(…) >= 1`;
    /// never NULL).
    SomeNonNull(BTreeSet<String>),
    /// Every column is non-NULL (`num_nulls(…) = 0`; never NULL).
    AllNonNull(BTreeSet<String>),
    /// Anything else: never refuted, proves nothing.
    Other,
}

/// A conjunct of a constraint: an OR of AND-arms.
#[derive(Debug, Clone)]
struct Clause {
    arms: Vec<Vec<Lit>>,
}

/// The CHECK constraints of one relation.
#[derive(Debug, Clone, Default)]
pub(crate) struct RelationChecks {
    clauses: Vec<Clause>,
    /// Columns whose values differ whenever their string literals do (an
    /// enum, or text / varchar under a deterministic collation).
    string_distinct: HashSet<String>,
}

/// What a query knows of one row of the relation (keyed by column name).
pub(crate) struct Knowledge<'a> {
    pub non_null: &'a dyn Fn(&str) -> bool,
    pub null: &'a dyn Fn(&str) -> bool,
    pub equals: &'a dyn Fn(&str) -> Option<Literal>,
}

/// What reading a constraint needs to know of the relation's columns.
struct Cx<'a> {
    is_composite: &'a dyn Fn(&str) -> bool,
    column_type: &'a dyn Fn(&str) -> Option<crate::oid::PgTypeOid>,
    snapshot: &'a PgCatalog,
}

/// The arms past which an OR of DNF arms grows too large to keep.
const MAX_ARMS: usize = 16;

impl RelationChecks {
    /// The enforced CHECK constraints of relation `relid`. A `NO INHERIT`
    /// one doesn't bind the rows of children a scan of the relation also
    /// returns, so it is skipped when the relation has any.
    pub(crate) fn of(snapshot: &PgCatalog, relid: PgClassOid) -> Option<RelationChecks> {
        let has_children = snapshot.pg_inherits.iter().any(|i| i.inhparent == relid);
        let attrs = snapshot.attributes_of(relid);
        let mut clauses = Vec::new();
        let mut constraints: Vec<_> = snapshot
            .pg_constraint
            .values()
            .filter(|c| {
                c.conrelid == relid
                    && c.contype == crate::pg_catalog::ConType::Check
                    && c.conenforced
            })
            .collect();
        if constraints.is_empty() {
            return None;
        }
        constraints.sort_by_key(|c| c.oid);
        for con in constraints {
            let Some(def) = snapshot.check_defs.get(&con.oid) else {
                continue;
            };
            if def.no_inherit && has_children {
                continue;
            }
            let crate::ddl::tables::check_inherit::StoredExpr::Written(expr) = &def.expr else {
                continue;
            };
            let is_composite = |name: &str| {
                attrs
                    .iter()
                    .find(|a| a.attname == name)
                    .is_none_or(|a| crate::coerce::is_complex(a.atttypid, snapshot))
            };
            let column_type =
                |name: &str| attrs.iter().find(|a| a.attname == name).map(|a| a.atttypid);
            let cx = Cx {
                is_composite: &is_composite,
                column_type: &column_type,
                snapshot,
            };
            for conjunct in conjuncts(expr) {
                if let Some(arms) = dnf(conjunct, &cx)
                    && arms
                        .iter()
                        .any(|arm| arm.iter().any(|l| !matches!(l, Lit::Other)))
                {
                    clauses.push(Clause { arms });
                }
            }
        }
        if clauses.is_empty() {
            return None;
        }
        let deterministic = |c: Option<crate::oid::PgCollationOid>| {
            c.is_none_or(|c| matches!(c.get(), 100 | 950 | 951))
        };
        let string_distinct = attrs
            .iter()
            .filter(|a| {
                let base = snapshot.unwrap_domain(a.atttypid);
                let is_enum = snapshot
                    .pg_type
                    .get(&base)
                    .is_some_and(|t| t.typtype == crate::pg_catalog::TypType::Enum);
                is_enum
                    || ((base == crate::pg_catalog::oid::TEXT
                        || base == crate::pg_catalog::oid::VARCHAR)
                        && deterministic(a.attcollation))
            })
            .map(|a| a.attname.clone())
            .collect();
        Some(RelationChecks {
            clauses,
            string_distinct,
        })
    }

    /// What the constraints prove of a row the query knows `k` about:
    /// columns non-NULL or NULL, and sets of columns one of which is
    /// non-NULL — over the columns of FROM entry `alias`.
    /// The constraints over the columns of a relation exposing column `c`
    /// as `rename(c)` (a view's plain columns): one it doesn't expose gets
    /// a name no column has, so nothing is known of it.
    pub(crate) fn renamed(&self, rename: impl Fn(&str) -> Option<String>) -> RelationChecks {
        let name = |c: &String| rename(c).unwrap_or_else(|| format!("\u{1}hidden:{c}"));
        let lit = |l: &Lit| match l {
            Lit::NotNull(c) => Lit::NotNull(name(c)),
            Lit::IsNull(c) => Lit::IsNull(name(c)),
            Lit::Eq(c, v) => Lit::Eq(name(c), v.clone()),
            Lit::Ne(c, v) => Lit::Ne(name(c), v.clone()),
            Lit::In(c, vs) => Lit::In(name(c), vs.clone()),
            Lit::NotIn(c, vs) => Lit::NotIn(name(c), vs.clone()),
            Lit::SomeNonNull(cs) => Lit::SomeNonNull(cs.iter().map(name).collect()),
            Lit::AllNonNull(cs) => Lit::AllNonNull(cs.iter().map(name).collect()),
            Lit::Other => Lit::Other,
        };
        RelationChecks {
            clauses: self
                .clauses
                .iter()
                .map(|cl| Clause {
                    arms: cl
                        .arms
                        .iter()
                        .map(|arm| arm.iter().map(lit).collect())
                        .collect(),
                })
                .collect(),
            string_distinct: self.string_distinct.iter().map(name).collect(),
        }
    }

    pub(crate) fn derive(&self, alias: &str, k: &Knowledge<'_>) -> Facts {
        let col = |c: &str| -> Col { (alias.to_owned(), c.to_owned()) };
        let mut out = Facts::default();
        for clause in &self.clauses {
            let survivors: Vec<&Vec<Lit>> = clause
                .arms
                .iter()
                .filter(|arm| !arm.iter().any(|l| self.refuted(l, k)))
                .collect();
            match survivors.as_slice() {
                // A contradiction: no row of the relation can be here.
                [] => {}
                [arm] => {
                    for l in arm.iter() {
                        match l {
                            Lit::NotNull(c) => out = out.union(Facts::column(alias, c)),
                            Lit::AllNonNull(cs) => {
                                for c in cs {
                                    out = out.union(Facts::column(alias, c));
                                }
                            }
                            Lit::IsNull(c) => {
                                out.nulls.insert(col(c));
                            }
                            Lit::SomeNonNull(cs) => {
                                let left: BTreeSet<Col> =
                                    cs.iter().filter(|c| !(k.null)(c)).map(|c| col(c)).collect();
                                out = out.union(Facts::disjunction(left));
                            }
                            _ => {}
                        }
                    }
                }
                arms => {
                    // One of the arms holds: one of their witnesses is
                    // non-NULL (every arm needs one).
                    let mut either: BTreeSet<Col> = BTreeSet::new();
                    for arm in arms {
                        let mut witness: BTreeSet<Col> = arm
                            .iter()
                            .flat_map(|l| match l {
                                Lit::NotNull(c) => vec![c.clone()],
                                Lit::AllNonNull(cs) => cs.iter().cloned().collect(),
                                _ => Vec::new(),
                            })
                            .map(|c| col(&c))
                            .collect();
                        if witness.is_empty()
                            && let Some(cs) = arm
                                .iter()
                                .filter_map(|l| match l {
                                    Lit::SomeNonNull(cs) => Some(cs),
                                    _ => None,
                                })
                                .min_by_key(|cs| cs.len())
                        {
                            witness = cs.iter().map(|c| col(c)).collect();
                        }
                        if witness.is_empty() {
                            either.clear();
                            break;
                        }
                        either.extend(witness);
                    }
                    either.retain(|(_, c)| !(k.null)(c));
                    if either.len() > 1 {
                        out = out.union(Facts::disjunction(either));
                    } else if either.len() == 1 {
                        // Every other witness is known NULL.
                        out = out.union(Facts::disjunction(either));
                    }
                }
            }
        }
        out
    }

    /// Whether literal `l` is FALSE for the row `k` describes.
    fn refuted(&self, l: &Lit, k: &Knowledge<'_>) -> bool {
        let distinct = |c: &str, w: &Literal, v: &Literal| match (w.kind, v.kind) {
            (LitKind::Integer, LitKind::Integer) => matches!(
                (w.text.parse::<i64>(), v.text.parse::<i64>()),
                (Ok(a), Ok(b)) if a != b
            ),
            (LitKind::Boolean, LitKind::Boolean) => w.text != v.text,
            (LitKind::String, LitKind::String) => {
                w.text != v.text && self.string_distinct.contains(c)
            }
            _ => false,
        };
        match l {
            Lit::NotNull(c) => (k.null)(c),
            Lit::IsNull(c) => (k.non_null)(c),
            Lit::Eq(c, v) => (k.equals)(c).is_some_and(|w| distinct(c, &w, v)),
            Lit::Ne(c, v) => (k.equals)(c).is_some_and(|w| w == *v),
            Lit::In(c, vs) => (k.equals)(c).is_some_and(|w| vs.iter().all(|v| distinct(c, &w, v))),
            Lit::NotIn(c, vs) => (k.equals)(c).is_some_and(|w| vs.contains(&w)),
            Lit::SomeNonNull(cs) => cs.iter().all(|c| (k.null)(c)),
            Lit::AllNonNull(cs) => cs.iter().any(|c| (k.null)(c)),
            Lit::Other => false,
        }
    }
}

/// The top-level conjuncts of `n`.
fn conjuncts(n: &protobuf::Node) -> Vec<&protobuf::Node> {
    match n.node.as_ref() {
        Some(node::Node::BoolExpr(b))
            if protobuf::BoolExprType::try_from(b.boolop)
                == Ok(protobuf::BoolExprType::AndExpr) =>
        {
            b.args.iter().flat_map(conjuncts).collect()
        }
        _ => vec![n],
    }
}

/// `n` as an OR of AND-arms; `None` past [`MAX_ARMS`].
fn dnf(n: &protobuf::Node, cx: &Cx<'_>) -> Option<Vec<Vec<Lit>>> {
    match n.node.as_ref() {
        Some(node::Node::BoolExpr(b)) => match protobuf::BoolExprType::try_from(b.boolop) {
            Ok(protobuf::BoolExprType::OrExpr) => {
                let mut arms = Vec::new();
                for a in &b.args {
                    arms.extend(dnf(a, cx)?);
                    if arms.len() > MAX_ARMS {
                        return None;
                    }
                }
                Some(arms)
            }
            Ok(protobuf::BoolExprType::AndExpr) => {
                let mut arms: Vec<Vec<Lit>> = vec![Vec::new()];
                for a in &b.args {
                    let sub = dnf(a, cx)?;
                    let mut next = Vec::new();
                    for x in &arms {
                        for y in &sub {
                            next.push(x.iter().chain(y).cloned().collect());
                        }
                    }
                    if next.len() > MAX_ARMS {
                        return None;
                    }
                    arms = next;
                }
                Some(arms)
            }
            Ok(protobuf::BoolExprType::NotExpr) => match b.args.as_slice() {
                [inner] => Some(vec![vec![negated_atom(inner, cx)]]),
                _ => Some(vec![vec![Lit::Other]]),
            },
            _ => Some(vec![vec![Lit::Other]]),
        },
        _ => Some(vec![vec![atom(n, cx)]]),
    }
}

/// A column a constraint names (bare, or qualified by its table).
fn column_name(n: &protobuf::Node) -> Option<String> {
    let Some(node::Node::ColumnRef(c)) = n.node.as_ref() else {
        return None;
    };
    if c.fields
        .iter()
        .any(|f| matches!(f.node.as_ref(), Some(node::Node::AStar(_))))
    {
        return None;
    }
    crate::expr::extract_string_fields(&c.fields)
        .last()
        .cloned()
}

fn atom(n: &protobuf::Node, cx: &Cx<'_>) -> Lit {
    let scalar =
        |a: Option<&protobuf::Node>| a.and_then(column_name).filter(|c| !(cx.is_composite)(c));
    let lit = |n: &protobuf::Node, c: &str| {
        (cx.column_type)(c).and_then(|t| super::literal_for(n, t, cx.snapshot))
    };
    match n.node.as_ref() {
        // A boolean column used as a condition: `done` is `done = true`.
        Some(node::Node::ColumnRef(_)) => match column_name(n) {
            Some(c) => Lit::Eq(c, Literal::boolean(true)),
            None => Lit::Other,
        },
        Some(node::Node::NullTest(t)) => {
            let Some(c) = scalar(t.arg.as_deref()) else {
                return Lit::Other;
            };
            match protobuf::NullTestType::try_from(t.nulltesttype) {
                Ok(protobuf::NullTestType::IsNotNull) => Lit::NotNull(c),
                Ok(protobuf::NullTestType::IsNull) => Lit::IsNull(c),
                _ => Lit::Other,
            }
        }
        Some(node::Node::AExpr(e)) => {
            use protobuf::AExprKind as K;
            if let Some((args, at_least, _)) = super::null_count_bounds(e) {
                let cols: Option<BTreeSet<String>> = args.iter().map(column_name).collect();
                return match cols {
                    Some(cols) if at_least as usize >= cols.len() && !cols.is_empty() => {
                        Lit::AllNonNull(cols)
                    }
                    Some(cols) if at_least >= 1 => Lit::SomeNonNull(cols),
                    _ => Lit::Other,
                };
            }
            let op = crate::expr::extract_string_fields(&e.name).join(".");
            let (Some(l), Some(r)) = (e.lexpr.as_deref(), e.rexpr.as_deref()) else {
                return Lit::Other;
            };
            match K::try_from(e.kind) {
                Ok(K::AexprOp) => {
                    let pair = match (column_name(l), column_name(r)) {
                        (Some(c), None) => lit(r, &c).map(|v| (c, v)),
                        (None, Some(c)) => lit(l, &c).map(|v| (c, v)),
                        _ => None,
                    };
                    match (pair, op.as_str()) {
                        (Some((c, v)), "=") => Lit::Eq(c, v),
                        (Some((c, v)), "<>") => Lit::Ne(c, v),
                        _ => Lit::Other,
                    }
                }
                Ok(K::AexprIn) => {
                    let (Some(c), Some(node::Node::List(list))) = (column_name(l), r.node.as_ref())
                    else {
                        return Lit::Other;
                    };
                    let vs: Option<Vec<Literal>> = list.items.iter().map(|i| lit(i, &c)).collect();
                    match (vs, op.as_str()) {
                        (Some(vs), "=") => Lit::In(c, vs),
                        (Some(vs), "<>") => Lit::NotIn(c, vs),
                        _ => Lit::Other,
                    }
                }
                _ => Lit::Other,
            }
        }
        _ => Lit::Other,
    }
}

/// `NOT n` as a literal.
fn negated_atom(n: &protobuf::Node, cx: &Cx<'_>) -> Lit {
    match atom(n, cx) {
        Lit::NotNull(c) => Lit::IsNull(c),
        Lit::IsNull(c) => Lit::NotNull(c),
        // NOT is strict: `NOT (c = v)` is `c <> v`.
        Lit::Eq(c, v) => Lit::Ne(c, v),
        Lit::Ne(c, v) => Lit::Eq(c, v),
        Lit::In(c, vs) => Lit::NotIn(c, vs),
        Lit::NotIn(c, vs) => Lit::In(c, vs),
        _ => Lit::Other,
    }
}
