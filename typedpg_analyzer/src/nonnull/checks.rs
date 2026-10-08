//! A table's CHECK constraints as knowledge about its rows.
//!
//! Every row of a table satisfies its CHECK constraints: each one is TRUE
//! or NULL — *not FALSE* — for it. A constraint is assumed to hold as
//! declared, `NOT VALID` included (only `NOT ENFORCED` ones are skipped):
//! what the schema states is taken to be true of the data. So are the
//! constraints PG implies: a partition's bound (`get_qual_from_partbound`:
//! a range key is never NULL, a list key is one of its values) and a
//! `MATCH FULL` foreign key's all-or-nothing NULLs.
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
//! CHECK ((kind = 'a') = (a_id IS NOT NULL)) -- an iff
//! ```
//!
//! Each top-level conjunct of a constraint is read as an OR of AND-arms of
//! literals ([`Lit`]); a conjunct not FALSE has an arm none of whose
//! literals is FALSE. A sub-expression is read either as "not FALSE" or as
//! "not TRUE" (under a NOT, on the ELSE side of a CASE, either side of a
//! boolean `=`), so NOT never needs a literal of its own. Where the exact
//! reading isn't a DNF of literals (a WHEN being TRUE), a weaker one —
//! allowing more rows — stands in for it. When one arm is left, its
//! never-NULL literals are TRUE; when several are, at least one of their
//! never-NULL literals is. A conjunct left with one arm is also knowledge
//! the others are refuted with (`CHECK (kind IN ('a', 'b'))`), and a
//! column known to hold one of a few values is split into those cases.
//!
//! A comparison (and a `num_nulls` call) is read only when it resolved to
//! the built-in one when the constraint was created — as PG keeps it, by
//! OID (see [`crate::ddl::tables::check_inherit::CheckDef::cook`]): an
//! exact-signature `=` of a user's on an enum or a domain says nothing of
//! the values it compares.

use std::collections::{BTreeSet, HashMap, HashSet};

use typedpg_pg_query::protobuf::{self, node};

use super::{CmpOp, Col, Facts, LitKind, Literal, StrictNode, Trust, TrustedNodes, ValPred};
use crate::oid::{PgClassOid, PgTypeOid};
use crate::pg_catalog::PgCatalog;

/// One literal of a constraint, over the table's column names.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Lit {
    /// `c IS NOT NULL` (never NULL).
    NotNull(String),
    /// `c IS NULL` (never NULL).
    IsNull(String),
    /// A comparison of `c` with constants, not FALSE: `c` is NULL or its
    /// value satisfies the [`ValPred`].
    Val(String, ValPred),
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

impl Clause {
    /// The same, its arms read whole: an arm whose comparisons of one
    /// subject can't all hold of a value holds only of NULL (each is "NULL
    /// or …"), so it says the subject is NULL — and is impossible besides
    /// a literal saying it isn't.
    fn normalized(self, spaces: &HashMap<String, Space>) -> Clause {
        let arms = self
            .arms
            .into_iter()
            .filter_map(|arm| {
                let mut preds: HashMap<&String, Vec<ValPred>> = HashMap::new();
                for l in &arm {
                    if let Lit::Val(c, p) = l {
                        preds.entry(c).or_default().push(p.clone());
                    }
                }
                let null: Vec<String> = preds
                    .iter()
                    .filter(|(c, ps)| spaces.get(**c).is_some_and(|s| s.contradictory(ps)))
                    .map(|(c, _)| (*c).clone())
                    .collect();
                if null.is_empty() {
                    return Some(arm);
                }
                if arm
                    .iter()
                    .any(|l| matches!(l, Lit::NotNull(c) if null.contains(c)))
                {
                    return None;
                }
                let mut out: Vec<Lit> = arm
                    .into_iter()
                    .filter(|l| !matches!(l, Lit::Val(c, _) if null.contains(c)))
                    .collect();
                out.extend(null.into_iter().map(Lit::IsNull));
                Some(out)
            })
            .collect();
        Clause { arms }
    }

    /// The names its literals are about.
    fn names(&self) -> impl Iterator<Item = &String> {
        self.arms
            .iter()
            .flatten()
            .flat_map(|l| -> Box<dyn Iterator<Item = &String>> {
                match l {
                    Lit::NotNull(c) | Lit::IsNull(c) | Lit::Val(c, _) => {
                        Box::new(std::iter::once(c))
                    }
                    Lit::SomeNonNull(cs) | Lit::AllNonNull(cs) => Box::new(cs.iter()),
                    Lit::Other => Box::new(std::iter::empty()),
                }
            })
    }
}

/// How the values of a column compare with constants: what makes two
/// constants the same value, or different ones.
#[derive(Debug, Clone, Default)]
pub(crate) struct Space {
    kind: SpaceKind,
    /// Different strings are different values: an enum, or text / varchar
    /// under a deterministic collation.
    distinct_strings: bool,
    /// Every value a non-NULL column can hold, when there are few: an
    /// enum's labels (`enum_in` takes nothing else), `true` and `false`.
    domain: Option<Vec<Literal>>,
    /// An integer type (not numeric): its values are integers only.
    integral: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum SpaceKind {
    /// Constants say nothing about each other.
    #[default]
    Opaque,
    /// Integers and numeric, compared by integer constants (`int4in` /
    /// `numeric_in` read them all as the same number; numeric's NaN is
    /// above every number, so its order stays total).
    Int,
    Bool,
    /// Strings compared as written: the same text is the same value.
    Str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Value {
    Int(i128),
    Bool(bool),
    Str(String),
}

impl Space {
    /// The space of a column of type `t` under `collation`.
    pub(crate) fn of(
        t: PgTypeOid,
        collation: Option<crate::oid::PgCollationOid>,
        snapshot: &PgCatalog,
    ) -> Space {
        use crate::pg_catalog::oid;
        let base = snapshot.unwrap_domain(t);
        let deterministic = collation.is_none_or(|c| matches!(c.get(), 100 | 950 | 951));
        if [oid::INT2, oid::INT4, oid::INT8, oid::NUMERIC].contains(&base) {
            return Space {
                kind: SpaceKind::Int,
                integral: base != oid::NUMERIC,
                ..Space::default()
            };
        }
        if base == oid::BOOL {
            return Space {
                kind: SpaceKind::Bool,
                distinct_strings: false,
                domain: Some(vec![Literal::boolean(true), Literal::boolean(false)]),
                integral: false,
            };
        }
        if [oid::TEXT, oid::VARCHAR, oid::BPCHAR, oid::NAME].contains(&base) {
            return Space {
                kind: SpaceKind::Str,
                distinct_strings: deterministic && (base == oid::TEXT || base == oid::VARCHAR),
                domain: None,
                integral: false,
            };
        }
        if snapshot
            .pg_type
            .get(&base)
            .is_some_and(|ty| ty.typtype == crate::pg_catalog::TypType::Enum)
        {
            let labels = snapshot
                .pg_enum
                .get(&base)
                .map(|ls| {
                    ls.iter()
                        .map(|l| Literal {
                            text: l.enumlabel.clone(),
                            kind: LitKind::String,
                        })
                        .collect()
                })
                .unwrap_or_default();
            return Space {
                kind: SpaceKind::Str,
                distinct_strings: true,
                domain: Some(labels),
                integral: false,
            };
        }
        Space::default()
    }

    /// The space with every value one of `values` (printed as
    /// [`crate::refine::Refinement::values`] are): what a value refined so
    /// can hold.
    pub(crate) fn with_values(mut self, values: Option<&BTreeSet<String>>) -> Space {
        let Some(values) = values else {
            return self;
        };
        let kind = match self.kind {
            SpaceKind::Int => LitKind::Integer,
            SpaceKind::Bool => LitKind::Boolean,
            SpaceKind::Str => LitKind::String,
            SpaceKind::Opaque => return self,
        };
        let refined: Vec<Literal> = values
            .iter()
            .map(|v| Literal {
                text: v.clone(),
                kind,
            })
            .collect();
        self.domain = Some(match self.domain.take() {
            None => refined,
            Some(d) => d
                .into_iter()
                .filter(|x| refined.iter().any(|v| self.same(x, v)))
                .collect(),
        });
        self
    }

    /// `l` as printed in a [`crate::refine::Refinement::values`] set.
    pub(crate) fn print(&self, l: &Literal) -> Option<String> {
        Some(match self.value(l)? {
            Value::Int(i) => i.to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Str(s) => s,
        })
    }

    /// The values a non-NULL value satisfying every one of `preds` can
    /// hold, when finitely many (see [`Space::candidates`]).
    pub(crate) fn candidates_of(&self, preds: &[ValPred]) -> Option<Vec<Literal>> {
        self.candidates(preds)
    }

    fn value(&self, l: &Literal) -> Option<Value> {
        match (self.kind, l.kind) {
            (SpaceKind::Int, LitKind::Integer) => l.text.parse().ok().map(Value::Int),
            (SpaceKind::Int, LitKind::String) => {
                crate::literal_input::parse_pg_integer(&l.text).map(Value::Int)
            }
            (SpaceKind::Bool, LitKind::Boolean) => Some(Value::Bool(l.text == "true")),
            (SpaceKind::Str, LitKind::String) => Some(Value::Str(l.text.clone())),
            _ => None,
        }
    }

    /// `a` and `b` are surely the same value.
    fn same(&self, a: &Literal, b: &Literal) -> bool {
        matches!((self.value(a), self.value(b)), (Some(x), Some(y)) if x == y)
    }

    /// `a` and `b` are surely different values.
    fn distinct(&self, a: &Literal, b: &Literal) -> bool {
        match (self.value(a), self.value(b)) {
            (Some(Value::Str(x)), Some(Value::Str(y))) => self.distinct_strings && x != y,
            (Some(x), Some(y)) => x != y,
            _ => false,
        }
    }

    fn int(&self, l: &Literal) -> Option<i128> {
        match self.value(l)? {
            Value::Int(i) if self.kind == SpaceKind::Int => Some(i),
            _ => None,
        }
    }

    /// Whether a non-NULL value `x` surely fails `p`.
    fn fails(&self, p: &ValPred, x: &Literal) -> bool {
        match p {
            ValPred::In(vs) => vs.iter().all(|v| self.distinct(x, v)),
            ValPred::NotIn(vs) => vs.iter().any(|v| self.same(x, v)),
            ValPred::Cmp(op, v) => match (self.int(x), self.int(v)) {
                (Some(x), Some(v)) => !match op {
                    CmpOp::Lt => x < v,
                    CmpOp::Le => x <= v,
                    CmpOp::Gt => x > v,
                    CmpOp::Ge => x >= v,
                },
                _ => false,
            },
        }
    }

    /// The values a non-NULL column satisfying every one of `preds` can
    /// hold, when they are finitely many (and known).
    fn candidates(&self, preds: &[ValPred]) -> Option<Vec<Literal>> {
        let mut cands = self.domain.clone();
        // An integer between two near bounds is one of the few between.
        if cands.is_none()
            && let Some(crate::refine::IntRange {
                lo: Some(lo),
                hi: Some(hi),
            }) = self.bounds(preds)
            && hi >= lo
            && hi - lo < MAX_ENUMERATED
        {
            cands = Some(
                (lo..=hi)
                    .map(|v| Literal {
                        text: v.to_string(),
                        kind: LitKind::Integer,
                    })
                    .collect(),
            );
        }
        for p in preds {
            if let ValPred::In(vs) = p {
                cands = Some(match cands {
                    None => vs.clone(),
                    Some(cs) => cs
                        .into_iter()
                        .filter(|x| vs.iter().any(|v| !self.distinct(x, v)))
                        .collect(),
                });
            }
        }
        cands.map(|cs| {
            cs.into_iter()
                .filter(|x| !preds.iter().any(|p| self.fails(p, x)))
                .collect()
        })
    }

    /// The bounds the ordering comparisons and value lists among `preds`
    /// leave a non-NULL integer column (`None` for a space of another kind).
    pub(crate) fn bounds(&self, preds: &[ValPred]) -> Option<crate::refine::IntRange> {
        if self.kind != SpaceKind::Int || !self.integral {
            return None;
        }
        let mut r = crate::refine::IntRange::default();
        for p in preds {
            let b = match p {
                ValPred::Cmp(op, v) => {
                    let Some(v) = self.int(v) else { continue };
                    match op {
                        CmpOp::Gt => crate::refine::IntRange::at_least(v + 1),
                        CmpOp::Ge => crate::refine::IntRange::at_least(v),
                        CmpOp::Lt => crate::refine::IntRange {
                            lo: None,
                            hi: Some(v - 1),
                        },
                        CmpOp::Le => crate::refine::IntRange {
                            lo: None,
                            hi: Some(v),
                        },
                    }
                }
                ValPred::In(vs) => {
                    let ints: Option<Vec<i128>> = vs.iter().map(|v| self.int(v)).collect();
                    let Some(ints) = ints else { continue };
                    crate::refine::IntRange {
                        lo: ints.iter().min().copied(),
                        hi: ints.iter().max().copied(),
                    }
                }
                ValPred::NotIn(_) => continue,
            };
            r = r.meet(&b);
        }
        Some(r)
    }

    /// The interval the ordering comparisons among `preds` (and `extra`)
    /// leave a non-NULL column: whether it is surely empty.
    fn interval_empty<'p>(&self, preds: impl IntoIterator<Item = &'p ValPred>) -> bool {
        if self.kind != SpaceKind::Int {
            return false;
        }
        // (value, inclusive)
        let mut lo: Option<(i128, bool)> = None;
        let mut hi: Option<(i128, bool)> = None;
        for p in preds {
            let ValPred::Cmp(op, v) = p else { continue };
            let Some(v) = self.int(v) else { continue };
            match op {
                CmpOp::Gt | CmpOp::Ge => {
                    let b = (v, *op == CmpOp::Ge);
                    lo = Some(match lo {
                        Some(l) if l.0 > b.0 || (l.0 == b.0 && !l.1) => l,
                        _ => b,
                    });
                }
                CmpOp::Lt | CmpOp::Le => {
                    let b = (v, *op == CmpOp::Le);
                    hi = Some(match hi {
                        Some(h) if h.0 < b.0 || (h.0 == b.0 && !h.1) => h,
                        _ => b,
                    });
                }
            }
        }
        // An integer column holds no value strictly between two
        // consecutive integers.
        if self.integral {
            lo = lo.map(|(v, inc)| if inc { (v, true) } else { (v + 1, true) });
            hi = hi.map(|(v, inc)| if inc { (v, true) } else { (v - 1, true) });
        }
        match (lo, hi) {
            (Some(l), Some(h)) => l.0 > h.0 || (l.0 == h.0 && !(l.1 && h.1)),
            _ => false,
        }
    }

    /// Whether `preds` (what holds of a non-NULL column) can't all hold.
    pub(crate) fn contradictory(&self, preds: &[ValPred]) -> bool {
        self.candidates(preds).is_some_and(|c| c.is_empty()) || self.interval_empty(preds)
    }

    /// Whether `p` surely fails for a non-NULL column satisfying `preds`.
    fn refutes(&self, preds: &[ValPred], p: &ValPred) -> bool {
        if let Some(cands) = self.candidates(preds) {
            return cands.iter().all(|x| self.fails(p, x));
        }
        match p {
            // Each value is excluded by what is known.
            ValPred::In(vs) => vs.iter().all(|v| preds.iter().any(|q| self.fails(q, v))),
            ValPred::NotIn(_) => false,
            ValPred::Cmp(..) => self.interval_empty(preds.iter().chain([p])),
        }
    }
}

/// The CHECK constraints of one relation (and the ones PG implies).
#[derive(Debug, Clone, Default)]
pub(crate) struct RelationChecks {
    clauses: Vec<Clause>,
    /// The clauses about expression subjects (see [`Subject`]): read only
    /// for the facts about expressions they prove, after the columns'.
    expr_clauses: Vec<Clause>,
    /// How each column's values compare with constants.
    spaces: HashMap<String, Space>,
    /// The expression subjects literals name (see [`Subject`]).
    subjects: HashMap<String, Subject>,
    /// The subject a `jsonb_typeof(x)` subject is of, by name: non-NULL
    /// as `x` is.
    typeof_of: HashMap<String, String>,
}

/// What a query knows of one row of the relation (keyed by column name).
pub(crate) struct Knowledge<'a> {
    pub non_null: &'a dyn Fn(&str) -> bool,
    pub null: &'a dyn Fn(&str) -> bool,
    pub equals: &'a dyn Fn(&str) -> Option<Literal>,
    /// What holds of the column's value if it is non-NULL.
    pub preds: &'a dyn Fn(&str) -> Vec<ValPred>,
}

/// What reading a constraint needs to know of the relation's columns, and
/// what its operators and functions resolved to when it was created.
struct Cx<'a> {
    is_composite: &'a dyn Fn(&str) -> bool,
    column_type: &'a dyn Fn(&str) -> Option<PgTypeOid>,
    snapshot: &'a PgCatalog,
    trust: &'a TrustedNodes,
    /// The expression subjects read so far, by name (see [`Subject`]).
    subjects: &'a std::cell::RefCell<HashMap<String, Subject>>,
    /// Reading the body of a `LANGUAGE sql` function a constraint calls:
    /// what its parameters (by name and as `$n`) stand for.
    env: Option<&'a HashMap<String, Subject>>,
    /// How many function bodies deep the reading is.
    depth: usize,
}

/// How many function bodies deep a constraint is read.
const MAX_INLINE_DEPTH: usize = 8;

/// What a literal of a constraint is about: a column, or an expression
/// over one whose value a JSON constraint speaks of — a key of a jsonb
/// value (`x -> 'k'`, as text `x ->> 'k'`) or the JSON type of one
/// (`jsonb_typeof(x)`), with the built-in operators and function.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Subject {
    Column(String),
    Field(Box<Subject>, String, bool),
    Typeof(Box<Subject>),
}

impl Subject {
    /// The name literals know it by: a column's own, or one no column has.
    pub(crate) fn name(&self) -> String {
        match self {
            Subject::Column(c) => c.clone(),
            other => format!("\u{2}{other:?}"),
        }
    }

    /// The type of its values.
    fn ty(&self, column_type: &dyn Fn(&str) -> Option<PgTypeOid>) -> Option<PgTypeOid> {
        use crate::pg_catalog::oid;
        match self {
            Subject::Column(c) => column_type(c),
            Subject::Field(_, _, false) => Some(oid::JSONB),
            Subject::Field(_, _, true) | Subject::Typeof(_) => Some(oid::TEXT),
        }
    }

    /// The same, over the column `rename` gives.
    fn renamed(&self, rename: &dyn Fn(&str) -> String) -> Subject {
        match self {
            Subject::Column(c) => Subject::Column(rename(c)),
            Subject::Field(x, k, t) => Subject::Field(Box::new(x.renamed(rename)), k.clone(), *t),
            Subject::Typeof(x) => Subject::Typeof(Box::new(x.renamed(rename))),
        }
    }

    /// The expression as the parser makes it, its column qualified by
    /// `qualifier` (or bare): what a query reading it writes.
    pub(crate) fn node(&self, qualifier: Option<&str>) -> protobuf::Node {
        let string = |s: &str| protobuf::Node {
            node: Some(node::Node::String(protobuf::String { sval: s.to_owned() })),
        };
        match self {
            Subject::Column(c) => protobuf::Node {
                node: Some(node::Node::ColumnRef(protobuf::ColumnRef {
                    fields: qualifier
                        .into_iter()
                        .chain([c.as_str()])
                        .map(string)
                        .collect(),
                    location: -1,
                })),
            },
            Subject::Field(x, k, text) => protobuf::Node {
                node: Some(node::Node::AExpr(Box::new(protobuf::AExpr {
                    kind: protobuf::AExprKind::AexprOp as i32,
                    name: vec![string(if *text { "->>" } else { "->" })],
                    lexpr: Some(Box::new(x.node(qualifier))),
                    rexpr: Some(Box::new(protobuf::Node {
                        node: Some(node::Node::AConst(protobuf::AConst {
                            isnull: false,
                            val: Some(protobuf::a_const::Val::Sval(protobuf::String {
                                sval: k.clone(),
                            })),
                            location: -1,
                        })),
                    })),
                    rexpr_list_start: -1,
                    rexpr_list_end: -1,
                    location: -1,
                }))),
            },
            Subject::Typeof(x) => protobuf::Node {
                node: Some(node::Node::FuncCall(Box::new(protobuf::FuncCall {
                    funcname: vec![string("jsonb_typeof")],
                    args: vec![x.node(qualifier)],
                    funcformat: protobuf::CoercionForm::CoerceExplicitCall as i32,
                    location: -1,
                    ..Default::default()
                }))),
            },
        }
    }
}

impl Cx<'_> {
    /// What `n` is about, when a literal can be: a column, a parameter
    /// standing for one, or a key / JSON type of one.
    fn subject_of(&self, n: &protobuf::Node) -> Option<Subject> {
        use crate::nonnull::{JsonbBuiltin as J, StrictNode as S};
        match n.node.as_ref()? {
            node::Node::ColumnRef(_) => {
                let c = column_name(n)?;
                match self.env {
                    // A body's names are its parameters.
                    Some(env) => env.get(&c).cloned(),
                    None => ((self.column_type)(&c).is_some() && !(self.is_composite)(&c))
                        .then_some(Subject::Column(c)),
                }
            }
            node::Node::ParamRef(p) => self.env?.get(&format!("${}", p.number)).cloned(),
            // A conversion of a jsonb value (of a domain over jsonb) to
            // jsonb changes nothing.
            node::Node::TypeCast(tc) => {
                let inner = self.subject_of(tc.arg.as_deref()?)?;
                let target =
                    crate::ddl::util::resolve_type_name(tc.type_name.as_ref()?, self.snapshot)?;
                let jsonb = crate::pg_catalog::oid::JSONB;
                (target == jsonb
                    && inner
                        .ty(self.column_type)
                        .is_some_and(|t| self.snapshot.unwrap_domain(t) == jsonb))
                .then_some(inner)
            }
            node::Node::AExpr(e) => {
                let op = crate::expr::extract_string_fields(&e.name).join(".");
                let text = match op.as_str() {
                    "->" if self.trust.trusts(e.location, S::Jsonb(J::Field)) => false,
                    "->>" if self.trust.trusts(e.location, S::Jsonb(J::FieldText)) => true,
                    _ => return None,
                };
                let x = self.subject_of(e.lexpr.as_deref()?)?;
                let key = string_constant(e.rexpr.as_deref()?)?;
                Some(Subject::Field(Box::new(x), key, text))
            }
            node::Node::FuncCall(f) if self.trust.trusts(f.location, S::Jsonb(J::Typeof)) => {
                let [arg] = f.args.as_slice() else {
                    return None;
                };
                Some(Subject::Typeof(Box::new(self.subject_of(arg)?)))
            }
            _ => None,
        }
    }

    /// The name of what `n` is about (see [`Cx::subject_of`]), registered.
    fn subject(&self, n: &protobuf::Node) -> Option<String> {
        let s = self.subject_of(n)?;
        let name = s.name();
        if !matches!(s, Subject::Column(_)) {
            self.subjects.borrow_mut().insert(name.clone(), s);
        }
        Some(name)
    }

    /// The type of the values of the subject named `name`.
    fn subject_type(&self, name: &str) -> Option<PgTypeOid> {
        match self.subjects.borrow().get(name) {
            Some(s) => s.ty(self.column_type),
            None => (self.column_type)(name),
        }
    }
}

/// `f`, asked once per argument.
fn memoized<'a>(f: &'a dyn Fn(&str) -> bool) -> impl Fn(&str) -> bool + 'a {
    let cache = std::cell::RefCell::new(HashMap::<String, bool>::new());
    move |c: &str| {
        if let Some(&v) = cache.borrow().get(c) {
            return v;
        }
        let v = f(c);
        cache.borrow_mut().insert(c.to_owned(), v);
        v
    }
}

/// For each `jsonb_typeof(x)` subject of `subjects`, by name, `x`'s name.
fn typeof_of(subjects: &HashMap<String, Subject>) -> HashMap<String, String> {
    subjects
        .iter()
        .filter_map(|(n, s)| match s {
            Subject::Typeof(x) => Some((n.clone(), x.name())),
            _ => None,
        })
        .collect()
}

/// A string constant (`'k'`).
fn string_constant(n: &protobuf::Node) -> Option<String> {
    match n.node.as_ref()? {
        node::Node::AConst(c) if !c.isnull => match c.val.as_ref()? {
            protobuf::a_const::Val::Sval(s) => Some(s.sval.clone()),
            _ => None,
        },
        _ => None,
    }
}

/// The arms past which an OR of DNF arms grows too large to keep.
const MAX_ARMS: usize = 16;

/// The most integers an interval is listed as candidates for.
const MAX_ENUMERATED: i128 = 16;

/// The most values a column is split into cases over.
const MAX_CASES: usize = 8;

/// What the constraints prove of a row, as column names.
#[derive(Debug, Clone, Default)]
struct Solved {
    /// No row can be what the query knows of it.
    contradiction: bool,
    non_null: HashSet<String>,
    null: HashSet<String>,
    preds: HashMap<String, Vec<ValPred>>,
    /// Sets of columns at least one of which is non-NULL.
    disjunctions: Vec<BTreeSet<String>>,
}

impl RelationChecks {
    /// The enforced CHECK constraints of relation `relid`, the bounds of
    /// the partitions it is (and has), and its `MATCH FULL` foreign keys.
    /// A `NO INHERIT` CHECK doesn't bind the rows of children a scan of
    /// the relation also returns, so it is skipped when the relation has
    /// any; nor does a foreign key.
    pub(crate) fn of(snapshot: &PgCatalog, relid: PgClassOid) -> Option<RelationChecks> {
        // Read once per catalog state: a relation's constraints, the
        // functions they call, are the same for every query. Not while a
        // DDL statement runs: it changes the catalog as it goes (a view's
        // nullability is worked out again after a constraint is dropped).
        if crate::expr::inline::resolving() {
            return Self::build(snapshot, relid);
        }
        let cache = snapshot.relation_checks.clone();
        if let Some(hit) = cache.lock().ok().and_then(|c| c.get(&relid).cloned()) {
            return hit;
        }
        let built = Self::build(snapshot, relid);
        if let Ok(mut c) = cache.lock() {
            c.insert(relid, built.clone());
        }
        built
    }

    fn build(snapshot: &PgCatalog, relid: PgClassOid) -> Option<RelationChecks> {
        let has_children = snapshot.pg_inherits.iter().any(|i| i.inhparent == relid);
        let partitioned = snapshot
            .pg_class
            .get(&relid)
            .is_some_and(|c| c.relkind == crate::pg_catalog::RelKind::Partitioned);
        let attrs = snapshot.attributes_of(relid);
        let is_composite = |name: &str| {
            attrs
                .iter()
                .find(|a| a.attname == name)
                .is_none_or(|a| crate::coerce::is_complex(a.atttypid, snapshot))
        };
        let column_type = |name: &str| attrs.iter().find(|a| a.attname == name).map(|a| a.atttypid);
        let untrusted = TrustedNodes::default();
        let subjects = std::cell::RefCell::new(HashMap::new());
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
            let cx = Cx {
                is_composite: &is_composite,
                column_type: &column_type,
                snapshot,
                trust: def.trusted.as_ref().unwrap_or(&untrusted),
                subjects: &subjects,
                env: None,
                depth: 0,
            };
            for conjunct in conjuncts(expr) {
                for arms in cnf(conjunct, &cx) {
                    if arms
                        .iter()
                        .any(|arm| arm.iter().any(|l| !matches!(l, Lit::Other)))
                    {
                        clauses.push(Clause { arms });
                    }
                }
            }
        }
        clauses.extend(partition_clauses(snapshot, relid));
        if !has_children || partitioned {
            clauses.extend(match_full_clauses(snapshot, relid));
        }
        if clauses.is_empty() {
            return None;
        }
        let subjects = subjects.into_inner();
        let mut spaces: HashMap<String, Space> = attrs
            .iter()
            .map(|a| {
                (
                    a.attname.clone(),
                    Space::of(a.atttypid, a.attcollation, snapshot),
                )
            })
            .collect();
        for (name, s) in &subjects {
            if let Some(t) = s.ty(&column_type) {
                spaces.insert(name.clone(), Space::of(t, None, snapshot));
            }
        }
        let typeof_of = typeof_of(&subjects);
        let clauses: Vec<Clause> = clauses.into_iter().map(|c| c.normalized(&spaces)).collect();
        let (expr_clauses, clauses): (Vec<Clause>, Vec<Clause>) = clauses
            .into_iter()
            .partition(|c| c.names().any(|n| subjects.contains_key(n)));
        Some(RelationChecks {
            clauses,
            expr_clauses,
            spaces,
            subjects,
            typeof_of,
        })
    }

    /// The constraints over the columns of a relation exposing column `c`
    /// as `rename(c)` (a view's plain columns): one it doesn't expose gets
    /// a name no column has, so nothing is known of it.
    pub(crate) fn renamed(&self, rename: impl Fn(&str) -> Option<String>) -> RelationChecks {
        let column = |c: &str| rename(c).unwrap_or_else(|| format!("\u{1}hidden:{c}"));
        // An expression subject is renamed over its renamed column.
        let renamed_subjects: HashMap<String, Subject> = self
            .subjects
            .iter()
            .map(|(n, s)| (n.clone(), s.renamed(&column)))
            .collect();
        let name = |c: &String| match renamed_subjects.get(c) {
            Some(s) => s.name(),
            None => column(c),
        };
        let lit = |l: &Lit| match l {
            Lit::NotNull(c) => Lit::NotNull(name(c)),
            Lit::IsNull(c) => Lit::IsNull(name(c)),
            Lit::Val(c, p) => Lit::Val(name(c), p.clone()),
            Lit::SomeNonNull(cs) => Lit::SomeNonNull(cs.iter().map(name).collect()),
            Lit::AllNonNull(cs) => Lit::AllNonNull(cs.iter().map(name).collect()),
            Lit::Other => Lit::Other,
        };
        let clause = |cl: &Clause| Clause {
            arms: cl
                .arms
                .iter()
                .map(|arm| arm.iter().map(lit).collect())
                .collect(),
        };
        RelationChecks {
            clauses: self.clauses.iter().map(clause).collect(),
            expr_clauses: self.expr_clauses.iter().map(clause).collect(),
            spaces: self
                .spaces
                .iter()
                .map(|(c, s)| (name(c), s.clone()))
                .collect(),
            typeof_of: typeof_of(
                &renamed_subjects
                    .values()
                    .map(|s| (s.name(), s.clone()))
                    .collect(),
            ),
            subjects: renamed_subjects
                .into_values()
                .map(|s| (s.name(), s))
                .collect(),
        }
    }

    fn space(&self, c: &str) -> Space {
        self.spaces.get(c).cloned().unwrap_or_default()
    }

    /// How column `c`'s values compare, without a copy.
    fn space_ref(&self, c: &str) -> &Space {
        static OPAQUE: std::sync::LazyLock<Space> = std::sync::LazyLock::new(Space::default);
        self.spaces.get(c).unwrap_or(&OPAQUE)
    }

    /// Run the constraints over what `k` (and `assume`: a column holding a
    /// value) says of a row, until nothing new is learned.
    fn solve(&self, k: &Knowledge<'_>, assume: Option<(&str, &Literal)>) -> Solved {
        // What the query knows of a column is asked of every literal on
        // every pass: once per column is enough.
        let (non_null, null) = (memoized(k.non_null), memoized(k.null));
        let k = &Knowledge {
            non_null: &non_null,
            null: &null,
            equals: k.equals,
            preds: k.preds,
        };
        let mut s = Solved::default();
        if let Some((c, v)) = assume {
            s.preds
                .entry(c.to_owned())
                .or_default()
                .push(ValPred::In(vec![v.clone()]));
        }
        for _ in 0..=self.clauses.len() {
            let mut changed = false;
            for clause in &self.clauses {
                let survivors: Vec<&Vec<Lit>> = clause
                    .arms
                    .iter()
                    .filter(|arm| !arm.iter().any(|l| self.refuted(l, k, &s)))
                    .collect();
                match survivors.as_slice() {
                    // No row of the relation can be here.
                    [] => {
                        s.contradiction = true;
                        return s;
                    }
                    [arm] => {
                        for l in arm.iter() {
                            changed |= learn(&mut s, l);
                        }
                    }
                    _ => {}
                }
            }
            if !changed {
                break;
            }
        }
        // What is known of a column can't hold.
        let mut cols: HashSet<String> = s.preds.keys().cloned().collect();
        cols.extend(s.non_null.iter().cloned());
        for c in &cols {
            let non_null = (k.non_null)(c) || s.non_null.contains(c);
            if non_null && ((k.null)(c) || s.null.contains(c)) {
                s.contradiction = true;
                return s;
            }
            if non_null && self.space(c).contradictory(&self.preds_of(c, k, &s)) {
                s.contradiction = true;
                return s;
            }
        }
        // One of the arms left holds: one of their witnesses is non-NULL
        // (every arm needs one).
        for clause in &self.clauses {
            let survivors: Vec<&Vec<Lit>> = clause
                .arms
                .iter()
                .filter(|arm| !arm.iter().any(|l| self.refuted(l, k, &s)))
                .collect();
            match survivors.as_slice() {
                [arm] => {
                    for l in arm.iter() {
                        if let Lit::SomeNonNull(cs) = l {
                            let left: BTreeSet<String> = cs
                                .iter()
                                .filter(|c| !(k.null)(c) && !s.null.contains(*c))
                                .cloned()
                                .collect();
                            if !left.is_empty() {
                                s.disjunctions.push(left);
                            }
                        }
                    }
                }
                arms if arms.len() > 1 => {
                    let mut either: BTreeSet<String> = BTreeSet::new();
                    for arm in arms {
                        let mut witness: BTreeSet<String> = arm
                            .iter()
                            .flat_map(|l| match l {
                                Lit::NotNull(c) => vec![c.clone()],
                                Lit::AllNonNull(cs) => cs.iter().cloned().collect(),
                                _ => Vec::new(),
                            })
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
                            witness = cs.clone();
                        }
                        if witness.is_empty() {
                            either.clear();
                            break;
                        }
                        either.extend(witness);
                    }
                    either.retain(|c| !(k.null)(c) && !s.null.contains(c));
                    if !either.is_empty() {
                        s.disjunctions.push(either);
                    }
                }
                _ => {}
            }
        }
        s
    }

    /// What the constraints say of column `c`'s value, non-NULL, in a row
    /// the query knows `k` about (`None` when no row can be one).
    pub(crate) fn column_preds(&self, c: &str, k: &Knowledge<'_>) -> Option<Vec<ValPred>> {
        let s = self.solve(k, None);
        if s.contradiction {
            return None;
        }
        let mut preds = self.preds_of(c, k, &s);
        // A clause whose every arm left says `c` is one of some values
        // (`CHECK (n = 1 OR n = 2)`): one of the arms holds, so `c` is one
        // of them all.
        for clause in &self.clauses {
            let per_arm: Option<Vec<&Vec<Literal>>> = clause
                .arms
                .iter()
                .filter(|arm| !arm.iter().any(|l| self.refuted(l, k, &s)))
                .map(|arm| {
                    arm.iter().find_map(|l| match l {
                        Lit::Val(col, ValPred::In(vs)) if col == c => Some(vs),
                        _ => None,
                    })
                })
                .collect();
            if let Some(sets) = per_arm
                && !sets.is_empty()
            {
                preds.push(ValPred::In(sets.into_iter().flatten().cloned().collect()));
            }
        }
        Some(preds)
    }

    /// What the clauses about expressions add to `s` (what the columns'
    /// proved), until nothing new is learned.
    fn learn_expressions(&self, k: &Knowledge<'_>, s: &Solved) -> Solved {
        let (non_null, null) = (memoized(k.non_null), memoized(k.null));
        let k = &Knowledge {
            non_null: &non_null,
            null: &null,
            equals: k.equals,
            preds: k.preds,
        };
        let mut s = s.clone();
        for _ in 0..=self.expr_clauses.len() {
            let mut changed = false;
            for clause in &self.expr_clauses {
                let survivors: Vec<&Vec<Lit>> = clause
                    .arms
                    .iter()
                    .filter(|arm| !arm.iter().any(|l| self.refuted(l, k, &s)))
                    .collect();
                if let [arm] = survivors.as_slice() {
                    for l in arm.iter() {
                        changed |= learn(&mut s, l);
                    }
                    continue;
                }
                // Every arm left says a subject is one of some values (a
                // CASE over a JSON discriminant): it is one of them all.
                let Some(first) = survivors.first() else {
                    continue;
                };
                for c in first.iter().filter_map(|l| match l {
                    Lit::Val(c, ValPred::In(_)) => Some(c),
                    _ => None,
                }) {
                    let sets: Option<Vec<&Vec<Literal>>> = survivors
                        .iter()
                        .map(|arm| {
                            arm.iter().find_map(|l| match l {
                                Lit::Val(d, ValPred::In(vs)) if d == c => Some(vs),
                                _ => None,
                            })
                        })
                        .collect();
                    if let Some(sets) = sets {
                        let union = Lit::Val(
                            c.clone(),
                            ValPred::In(sets.into_iter().flatten().cloned().collect()),
                        );
                        changed |= learn(&mut s, &union);
                    }
                }
            }
            if !changed {
                break;
            }
        }
        s
    }

    /// Whether `c` is known non-NULL: by the query or the constraints —
    /// or, the JSON type of a value, as that value is.
    fn non_null(&self, c: &str, k: &Knowledge<'_>, s: &Solved) -> bool {
        (k.non_null)(c)
            || s.non_null.contains(c)
            || self
                .typeof_of
                .get(c)
                .is_some_and(|x| self.non_null(x, k, s))
    }

    /// The facts about expressions the constraints prove of a row of FROM
    /// entry `alias` (see [`RelationChecks::derive`]): an expression
    /// subject proven non-NULL, and `x ->> 'k'` of a key `x -> 'k'` proven
    /// non-NULL whose JSON type isn't `null`. Each under the forms a
    /// query writes it in: qualified by the alias, or bare.
    fn expression_facts(&self, alias: &str, k: &Knowledge<'_>, s: &Solved) -> HashSet<String> {
        let keys = |subject: &Subject| {
            [Some(alias), None]
                .map(|q| super::exprs::key(&subject.node(q)))
                .into_iter()
        };
        let mut out = HashSet::new();
        for (name, subject) in &self.subjects {
            if !s.non_null.contains(name) {
                continue;
            }
            out.extend(keys(subject));
            if let Subject::Field(x, key, false) = subject {
                // Its JSON type, or the JSON values it is one of
                // (`CASE (v -> 'type') WHEN '"a"'::jsonb …`), aren't null.
                let ty = Subject::Typeof(Box::new(subject.clone())).name();
                let not_json_null = self
                    .space(&ty)
                    .candidates(&self.preds_of(&ty, k, s))
                    .is_some_and(|ts| !ts.iter().any(|t| t.text == "null"))
                    || self
                        .space(name)
                        .candidates(&self.preds_of(name, k, s))
                        .is_some_and(|vs| !vs.iter().any(|v| v.text.trim() == "null"));
                if not_json_null {
                    out.extend(keys(&Subject::Field(x.clone(), key.clone(), true)));
                }
            }
        }
        out
    }

    /// Everything known of column `c`'s value when non-NULL.
    fn preds_of(&self, c: &str, k: &Knowledge<'_>, s: &Solved) -> Vec<ValPred> {
        let mut preds = (k.preds)(c);
        if let Some(v) = (k.equals)(c) {
            preds.push(ValPred::In(vec![v]));
        }
        if let Some(learned) = s.preds.get(c) {
            preds.extend(learned.iter().cloned());
        }
        preds
    }

    /// The columns worth splitting into cases: non-NULL ones known to hold
    /// one of a few values, that the constraints compare.
    fn splits(&self, k: &Knowledge<'_>, s: &Solved) -> Vec<(String, Vec<Literal>)> {
        let mut cols: Vec<&String> = self
            .clauses
            .iter()
            .flat_map(|cl| cl.arms.iter().flatten())
            .filter_map(|l| match l {
                Lit::Val(c, _) => Some(c),
                _ => None,
            })
            .collect();
        cols.sort();
        cols.dedup();
        cols.into_iter()
            .filter(|c| (k.non_null)(c) || s.non_null.contains(*c))
            .filter_map(|c| {
                let cands = self.space(c).candidates(&self.preds_of(c, k, s))?;
                (cands.len() > 1 && cands.len() <= MAX_CASES).then(|| (c.clone(), cands))
            })
            .collect()
    }

    /// What the constraints prove of a row the query knows `k` about:
    /// columns non-NULL or NULL, and sets of columns one of which is
    /// non-NULL — over the columns of FROM entry `alias`.
    pub(crate) fn derive(&self, alias: &str, k: &Knowledge<'_>) -> Facts {
        let col = |c: &str| -> Col { (alias.to_owned(), c.to_owned()) };
        let s = self.solve(k, None);
        let mut out = Facts::default();
        if s.contradiction {
            return out;
        }
        let mut non_null = s.non_null.clone();
        // A column non-NULL whichever value the split column holds.
        for (c, cands) in self.splits(k, &s) {
            let mut common: Option<HashSet<String>> = None;
            for x in &cands {
                let case = self.solve(k, Some((&c, x)));
                if case.contradiction {
                    continue;
                }
                common = Some(match common {
                    None => case.non_null,
                    Some(prev) => prev.intersection(&case.non_null).cloned().collect(),
                });
            }
            non_null.extend(common.unwrap_or_default());
        }
        for c in &non_null {
            out = out.union(Facts::column(alias, c));
        }
        if !self.expr_clauses.is_empty() {
            let se = self.learn_expressions(k, &s);
            out.exprs.extend(self.expression_facts(alias, k, &se));
        }
        for c in &s.null {
            out.nulls.insert(col(c));
        }
        for d in &s.disjunctions {
            out = out.union(Facts::disjunction(d.iter().map(|c| col(c)).collect()));
        }
        out
    }

    /// Whether no row of the relation can satisfy the constraints, with
    /// only its columns' own NOT NULL (`base_not_null`) known.
    pub(crate) fn contradict_alone(&self, base_not_null: &HashMap<String, bool>) -> bool {
        let non_null = |c: &str| base_not_null.get(c) == Some(&true);
        let nothing = |_: &str| false;
        let no_value = |_: &str| None;
        let no_preds = |_: &str| Vec::new();
        self.contradicts(&Knowledge {
            non_null: &non_null,
            null: &nothing,
            equals: &no_value,
            preds: &no_preds,
        })
    }

    /// Whether no row of the relation can be what `k` says of it.
    pub(crate) fn contradicts(&self, k: &Knowledge<'_>) -> bool {
        let s = self.solve(k, None);
        s.contradiction
            || self.splits(k, &s).iter().any(|(c, cands)| {
                cands
                    .iter()
                    .all(|x| self.solve(k, Some((c, x))).contradiction)
            })
    }

    /// Whether at least one of `cols` is non-NULL in every row `k`
    /// describes — case by case over a column holding one of a few values
    /// (`CHECK (kind IN ('a', 'b'))` with a CHECK per kind).
    pub(crate) fn some_non_null(&self, k: &Knowledge<'_>, cols: &[String]) -> bool {
        let proves = |s: &Solved| {
            s.contradiction
                || cols
                    .iter()
                    .any(|c| s.non_null.contains(c) || (k.non_null)(c))
                || s.disjunctions
                    .iter()
                    .any(|d| d.iter().all(|c| cols.contains(c)))
        };
        let s = self.solve(k, None);
        if s.contradiction {
            return false;
        }
        if proves(&s) {
            return true;
        }
        self.splits(k, &s)
            .iter()
            .any(|(c, cands)| cands.iter().all(|x| proves(&self.solve(k, Some((c, x))))))
    }

    /// Whether literal `l` is FALSE for the row `k` (and what was learned,
    /// `s`) describes.
    fn refuted(&self, l: &Lit, k: &Knowledge<'_>, s: &Solved) -> bool {
        let null = |c: &str| (k.null)(c) || s.null.contains(c);
        let non_null = |c: &str| self.non_null(c, k, s);
        match l {
            Lit::NotNull(c) => null(c),
            Lit::IsNull(c) => non_null(c),
            Lit::Val(c, p) => non_null(c) && self.space_ref(c).refutes(&self.preds_of(c, k, s), p),
            Lit::SomeNonNull(cs) => cs.iter().all(|c| null(c)),
            Lit::AllNonNull(cs) => cs.iter().any(|c| null(c)),
            Lit::Other => false,
        }
    }
}

/// Add what the TRUE (or, for a [`Lit::Val`], not FALSE) literal `l`
/// says; whether it was new.
fn learn(s: &mut Solved, l: &Lit) -> bool {
    match l {
        Lit::NotNull(c) => s.non_null.insert(c.clone()),
        Lit::AllNonNull(cs) => {
            let mut changed = false;
            for c in cs {
                changed |= s.non_null.insert(c.clone());
            }
            changed
        }
        Lit::IsNull(c) => s.null.insert(c.clone()),
        Lit::Val(c, p) => {
            let preds = s.preds.entry(c.clone()).or_default();
            if preds.contains(p) {
                false
            } else {
                preds.push(p.clone());
                true
            }
        }
        _ => false,
    }
}

/// The top-level conjuncts of the enforced CHECK constraints of relation
/// `relid` that bind every row a scan of it returns (a `NO INHERIT` one
/// doesn't when the relation has children, as in [`RelationChecks::of`]),
/// each with what its constraint resolved to when it was created.
fn binding_conjuncts(
    snapshot: &PgCatalog,
    relid: PgClassOid,
) -> Vec<(&protobuf::Node, &TrustedNodes)> {
    static UNTRUSTED: std::sync::LazyLock<TrustedNodes> =
        std::sync::LazyLock::new(TrustedNodes::default);
    let has_children = snapshot.pg_inherits.iter().any(|i| i.inhparent == relid);
    let mut out = Vec::new();
    for con in snapshot.pg_constraint.values().filter(|c| {
        c.conrelid == relid && c.contype == crate::pg_catalog::ConType::Check && c.conenforced
    }) {
        let Some(def) = snapshot.check_defs.get(&con.oid) else {
            continue;
        };
        if def.no_inherit && has_children {
            continue;
        }
        let crate::ddl::tables::check_inherit::StoredExpr::Written(expr) = &def.expr else {
            continue;
        };
        let trust = def.trusted.as_ref().unwrap_or(&UNTRUSTED);
        out.extend(conjuncts(expr).into_iter().map(|c| (c, trust)));
    }
    out
}

/// The array columns of relation `relid` an enforced CHECK constraint keeps
/// NULL elements out of (see [`null_free_arrays`]).
pub(crate) fn null_free_array_columns(snapshot: &PgCatalog, relid: PgClassOid) -> HashSet<String> {
    binding_conjuncts(snapshot, relid)
        .into_iter()
        .filter_map(|(c, trust)| null_free_array(c, trust))
        .collect()
}

/// The columns of relation `relid` an enforced CHECK constraint keeps
/// finite (see [`finite_column`]).
pub(crate) fn finite_columns(snapshot: &PgCatalog, relid: PgClassOid) -> HashSet<String> {
    binding_conjuncts(snapshot, relid)
        .into_iter()
        .filter_map(|(c, trust)| finite_column(c, trust))
        .collect()
}

/// The column a constraint conjunct keeps finite: `isfinite(c)`, with the
/// built-in `isfinite`. Not FALSE, it is TRUE (`c` is finite) or NULL (`c`
/// is NULL).
pub(crate) fn finite_column(n: &protobuf::Node, trust: &impl Trust) -> Option<String> {
    let Some(node::Node::FuncCall(f)) = n.node.as_ref() else {
        return None;
    };
    let [arg] = f.args.as_slice() else {
        return None;
    };
    trust
        .trusts(f.location, StrictNode::IsFinite)
        .then(|| column_name(arg))
        .flatten()
}

/// What a constraint expression allows its column `c`, of type `t` under
/// `collation`, when non-NULL: the values its conjuncts that are `c = v`,
/// `c IN (…)` or an OR of them leave of the type's, and the bounds its
/// comparisons with constants set (built-in ones, see [`dnf`]). `None`
/// when the type doesn't print values as they compare
/// ([`crate::refine::Exact`]).
pub(crate) fn allowed_values(
    expr: &protobuf::Node,
    c: &str,
    t: PgTypeOid,
    collation: Option<crate::oid::PgCollationOid>,
    snapshot: &PgCatalog,
    trust: &TrustedNodes,
) -> Option<crate::refine::Refinement> {
    crate::refine::Exact::of(t, collation, snapshot)?;
    let is_composite = |_: &str| false;
    let column_type = |n: &str| (n == c).then_some(t);
    let subjects = std::cell::RefCell::new(HashMap::new());
    let cx = Cx {
        is_composite: &is_composite,
        column_type: &column_type,
        snapshot,
        trust,
        subjects: &subjects,
        env: None,
        depth: 0,
    };
    let space = Space::of(t, collation, snapshot);
    let mut preds = Vec::new();
    for conjunct in conjuncts(expr) {
        let Some(arms) = dnf(conjunct, true, &cx) else {
            continue;
        };
        // Every arm says `c` is one of some values: it is one of them all.
        let per_arm: Option<Vec<&Vec<Literal>>> = arms
            .iter()
            .map(|arm| {
                arm.iter().find_map(|l| match l {
                    Lit::Val(col, ValPred::In(vs)) if col == c => Some(vs),
                    _ => None,
                })
            })
            .collect();
        if let Some(sets) = per_arm {
            preds.push(ValPred::In(sets.into_iter().flatten().cloned().collect()));
        }
        // A single arm: everything it says of `c` holds (`VALUE > 0`).
        if let [arm] = arms.as_slice() {
            preds.extend(arm.iter().filter_map(|l| match l {
                Lit::Val(col, p @ ValPred::Cmp(..)) if col == c => Some(p.clone()),
                _ => None,
            }));
        }
    }
    let values = space
        .candidates(&preds)
        .and_then(|cands| cands.iter().map(|l| space.print(l)).collect());
    let range = space
        .bounds(&preds)
        .filter(|r| *r != crate::refine::IntRange::default());
    Some(crate::refine::Refinement {
        values,
        range,
        ..crate::refine::Refinement::NONE
    })
}

/// The columns a constraint expression keeps finite (see [`finite_column`]).
pub(crate) fn finite_columns_of(expr: &protobuf::Node, trust: &impl Trust) -> Vec<String> {
    conjuncts(expr)
        .into_iter()
        .filter_map(|c| finite_column(c, trust))
        .collect()
}

/// The columns a constraint expression says hold no NULL element (see
/// [`null_free_array`]).
pub(crate) fn null_free_arrays(expr: &protobuf::Node, trust: &impl Trust) -> Vec<String> {
    conjuncts(expr)
        .into_iter()
        .filter_map(|c| null_free_array(c, trust))
        .collect()
}

/// The column a constraint conjunct says holds no NULL element:
/// `array_position(c, NULL) IS NULL`, with the built-in `array_position`.
/// The test is never NULL, so not FALSE is TRUE: the search found no NULL
/// — `c` is NULL, or a one-dimensional array (a multidimensional one fails
/// the search) none of whose elements is.
fn null_free_array(n: &protobuf::Node, trust: &impl Trust) -> Option<String> {
    let is_null_const = |n: &protobuf::Node| match n.node.as_ref() {
        Some(node::Node::AConst(c)) => c.isnull,
        Some(node::Node::TypeCast(tc)) => tc
            .arg
            .as_deref()
            .and_then(|a| a.node.as_ref())
            .is_some_and(|a| matches!(a, node::Node::AConst(c) if c.isnull)),
        _ => false,
    };
    let Some(node::Node::NullTest(t)) = n.node.as_ref() else {
        return None;
    };
    if protobuf::NullTestType::try_from(t.nulltesttype) != Ok(protobuf::NullTestType::IsNull) {
        return None;
    }
    let Some(node::Node::FuncCall(f)) = t.arg.as_deref()?.node.as_ref() else {
        return None;
    };
    let [array, needle] = f.args.as_slice() else {
        return None;
    };
    (trust.trusts(f.location, StrictNode::ArrayPosition) && is_null_const(needle))
        .then(|| column_name(array))
        .flatten()
}

/// The constraint partition `relid`'s bound and its ancestors' put on its
/// rows (`get_qual_from_partbound`), and — for a partitioned table — what
/// every one of its partitions' bounds does.
fn partition_clauses(snapshot: &PgCatalog, relid: PgClassOid) -> Vec<Clause> {
    use crate::ddl::tables::partbound::bound_facts;
    let mut out = Vec::new();
    let name = |rel: PgClassOid, attnum: i16| {
        snapshot
            .attributes_of(rel)
            .iter()
            .find(|a| a.attnum == attnum)
            .map(|a| a.attname.clone())
    };
    let mut part = relid;
    for _ in 0..64 {
        let Some(parent) = partition_parent(snapshot, part) else {
            break;
        };
        if let Some(f) = bound_facts(snapshot, parent, part) {
            for attnum in f.not_null {
                if let Some(c) = name(parent, attnum) {
                    out.push(Clause {
                        arms: vec![vec![Lit::NotNull(c)]],
                    });
                }
            }
            if let Some((attnum, values)) = f.values
                && let Some(c) = name(parent, attnum)
            {
                out.push(Clause {
                    arms: vec![vec![Lit::Val(c, ValPred::In(values))]],
                });
            }
        }
        part = parent;
    }
    for c in below_not_null(snapshot, relid, 0) {
        out.push(Clause {
            arms: vec![vec![Lit::NotNull(c)]],
        });
    }
    out
}

/// The partitioned table `part` is a partition of.
fn partition_parent(snapshot: &PgCatalog, part: PgClassOid) -> Option<PgClassOid> {
    if !snapshot.partition_bounds.contains_key(&part) {
        return None;
    }
    snapshot
        .pg_inherits
        .iter()
        .find(|i| i.inhrelid == part)
        .map(|i| i.inhparent)
}

/// The columns every partition of partitioned table `rel` (recursively)
/// keeps non-NULL by its bound: a row is in one of them. Nothing for one
/// without partitions.
fn below_not_null(snapshot: &PgCatalog, rel: PgClassOid, depth: u32) -> HashSet<String> {
    use crate::ddl::tables::partbound::bound_facts;
    if depth > 32 || !snapshot.partition_specs.contains_key(&rel) {
        return HashSet::new();
    }
    let attrs = snapshot.attributes_of(rel);
    let mut common: Option<HashSet<String>> = None;
    for child in crate::ddl::tables::inherit::children_of(snapshot, rel) {
        let mut here: HashSet<String> = bound_facts(snapshot, rel, child)
            .map(|f| {
                f.not_null
                    .iter()
                    .filter_map(|&n| attrs.iter().find(|a| a.attnum == n))
                    .map(|a| a.attname.clone())
                    .collect()
            })
            .unwrap_or_default();
        here.extend(below_not_null(snapshot, child, depth + 1));
        common = Some(match common {
            None => here,
            Some(prev) => prev.intersection(&here).cloned().collect(),
        });
    }
    common.unwrap_or_default()
}

/// A `MATCH FULL` foreign key of `relid` over several columns: its
/// columns are all NULL or none is (`RI_FKey_check` rejects a mix). Not
/// with the relation's RI triggers disabled.
fn match_full_clauses(snapshot: &PgCatalog, relid: PgClassOid) -> Vec<Clause> {
    if crate::ddl::tables::inherit::fk_triggers_disabled(snapshot, relid) {
        return Vec::new();
    }
    let attrs = snapshot.attributes_of(relid);
    let mut out = Vec::new();
    let mut fks: Vec<_> = snapshot
        .pg_constraint
        .values()
        .filter(|c| {
            c.conrelid == relid
                && c.contype == crate::pg_catalog::ConType::ForeignKey
                && c.conenforced
                && c.conkey.len() > 1
                && snapshot
                    .fk_details
                    .get(&c.oid)
                    .is_some_and(|d| d.match_full())
        })
        .collect();
    fks.sort_by_key(|c| c.oid);
    for fk in fks {
        let cols: Option<Vec<String>> = fk
            .conkey
            .iter()
            .map(|&n| {
                attrs
                    .iter()
                    .find(|a| a.attnum == n)
                    .map(|a| a.attname.clone())
            })
            .collect();
        let Some(cols) = cols else { continue };
        out.push(Clause {
            arms: vec![
                cols.iter().map(|c| Lit::IsNull(c.clone())).collect(),
                cols.iter().map(|c| Lit::NotNull(c.clone())).collect(),
            ],
        });
    }
    out
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

/// `n` read not FALSE as a conjunction of clauses (each an OR of AND-arms,
/// see [`dnf`]): an AND is each of its arms; a CASE whose results but one
/// are constants is `(P AND R1 AND R2) OR Q` — the guard `P` of the one,
/// `Q` the other ways — so `(P OR Q) AND (R1 OR Q) AND (R2 OR Q)`; a call
/// of a `LANGUAGE sql` function, its body. What doesn't split is one
/// clause, as [`dnf`] reads it (none, past its size).
fn cnf(n: &protobuf::Node, cx: &Cx<'_>) -> Vec<Vec<Vec<Lit>>> {
    use protobuf::BoolExprType as B;
    match n.node.as_ref() {
        Some(node::Node::BoolExpr(b)) if B::try_from(b.boolop) == Ok(B::AndExpr) => {
            b.args.iter().flat_map(|a| cnf(a, cx)).collect()
        }
        Some(node::Node::CaseExpr(c)) if c.arg.is_none() => {
            case_cnf(c, cx).unwrap_or_else(|| dnf(n, true, cx).into_iter().collect())
        }
        Some(node::Node::FuncCall(f)) => match inline_cnf(f, cx) {
            Some(clauses) => clauses,
            None => dnf(n, true, cx).into_iter().collect(),
        },
        _ => dnf(n, true, cx).into_iter().collect(),
    }
}

/// A searched CASE with one result that isn't a constant (see [`cnf`]).
fn case_cnf(c: &protobuf::CaseExpr, cx: &Cx<'_>) -> Option<Vec<Vec<Vec<Lit>>>> {
    let constant = |n: &protobuf::Node| matches!(n.node.as_ref(), Some(node::Node::AConst(_)));
    let mut whens = Vec::new();
    for arg in &c.args {
        let Some(node::Node::CaseWhen(w)) = arg.node.as_ref() else {
            return None;
        };
        whens.push((w.expr.as_deref()?, w.result.as_deref()?));
    }
    let default = c.defresult.as_deref();
    let open: Vec<usize> = whens
        .iter()
        .enumerate()
        .filter(|(_, (_, r))| !constant(r))
        .map(|(i, _)| i)
        .collect();
    let default_open = default.is_some_and(|d| !constant(d));
    // `P`: the guard of the one non-constant result, `Q`: every other way.
    let (guard, rest, result) = match (open.as_slice(), default_open) {
        ([i], false) => {
            let mut none_before = vec![Vec::new()];
            let mut q: Vec<Vec<Lit>> = Vec::new();
            let mut p = None;
            for (j, (w, r)) in whens.iter().enumerate() {
                let taken = product(&none_before, &dnf(w, true, cx)?)?;
                if j == *i {
                    p = Some(taken);
                } else {
                    q = either(q, product(&taken, &dnf(r, true, cx)?)?)?;
                }
                none_before = product(&none_before, &dnf(w, false, cx)?)?;
            }
            let d = match default {
                Some(d) => dnf(d, true, cx)?,
                None => vec![Vec::new()],
            };
            q = either(q, product(&none_before, &d)?)?;
            (p?, q, whens[*i].1)
        }
        ([], true) => {
            let mut none_before = vec![Vec::new()];
            let mut q: Vec<Vec<Lit>> = Vec::new();
            for (w, r) in &whens {
                let taken = product(&none_before, &dnf(w, true, cx)?)?;
                q = either(q, product(&taken, &dnf(r, true, cx)?)?)?;
                none_before = product(&none_before, &dnf(w, false, cx)?)?;
            }
            (none_before, q, default?)
        }
        _ => return None,
    };
    let mut out = vec![either(guard, rest.clone())?];
    for clause in cnf(result, cx) {
        out.push(either(clause, rest.clone())?);
    }
    Some(out)
}

/// A call of a `LANGUAGE sql` function of one expression (the one the
/// constraint resolved to when created, see [`TrustedNodes::calls`]): its
/// body read over what the arguments are about, with what the body
/// resolves to, as clauses. A STRICT one is also NULL — neither FALSE nor
/// TRUE — for a NULL argument. `None` when the call isn't one, or past a
/// depth.
fn inline_cnf(f: &protobuf::FuncCall, cx: &Cx<'_>) -> Option<Vec<Vec<Vec<Lit>>>> {
    let snapshot = cx.snapshot;
    if cx.depth >= MAX_INLINE_DEPTH {
        return None;
    }
    let oid = cx.trust.call(f.location)?;
    if snapshot.procs_with_config.contains(&oid) {
        return None;
    }
    let proc = snapshot.pg_proc.get(&oid)?;
    let body = snapshot.inline_sql_bodies.get(&oid)?;
    let trust = crate::expr::inline::cached_body_trust(snapshot, oid)?;
    if f.args.len() != proc.proargtypes.len()
        || f.args
            .iter()
            .any(|a| matches!(a.node.as_ref(), Some(node::Node::NamedArgExpr(_))))
    {
        return None;
    }
    let names: Vec<&String> = proc
        .proargnames
        .iter()
        .enumerate()
        .filter(|(i, _)| {
            proc.proargmodes.get(*i).is_none_or(|m| {
                matches!(
                    m,
                    crate::pg_catalog::ArgMode::In
                        | crate::pg_catalog::ArgMode::InOut
                        | crate::pg_catalog::ArgMode::Variadic
                )
            })
        })
        .map(|(_, n)| n)
        .collect();
    let mut env = HashMap::new();
    let mut strict_nulls: Vec<Vec<Lit>> = Vec::new();
    for (i, arg) in f.args.iter().enumerate() {
        let s = cx.subject_of(arg);
        if proc.proisstrict {
            strict_nulls.push(match &s {
                Some(s) => {
                    let name = s.name();
                    if !matches!(s, Subject::Column(_)) {
                        cx.subjects.borrow_mut().insert(name.clone(), s.clone());
                    }
                    vec![Lit::IsNull(name)]
                }
                None => vec![Lit::Other],
            });
        }
        let Some(s) = s else { continue };
        env.insert(format!("${}", i + 1), s.clone());
        if let Some(n) = names.get(i).filter(|n| !n.is_empty()) {
            env.insert((*n).clone(), s);
        }
    }
    let sub = Cx {
        is_composite: cx.is_composite,
        column_type: cx.column_type,
        snapshot,
        trust: &trust,
        subjects: cx.subjects,
        env: Some(&env),
        depth: cx.depth + 1,
    };
    let mut clauses = cnf(body, &sub);
    if !strict_nulls.is_empty() {
        clauses = clauses
            .into_iter()
            .filter_map(|c| either(c, strict_nulls.clone()))
            .collect();
    }
    Some(clauses)
}

/// Every arm of `a` joined with every arm of `b`; `None` past
/// [`MAX_ARMS`].
fn product(a: &[Vec<Lit>], b: &[Vec<Lit>]) -> Option<Vec<Vec<Lit>>> {
    let mut out = Vec::new();
    for x in a {
        for y in b {
            out.push(x.iter().chain(y).cloned().collect());
        }
    }
    (out.len() <= MAX_ARMS).then_some(out)
}

/// Both arm lists; `None` past [`MAX_ARMS`].
fn either(mut a: Vec<Vec<Lit>>, b: Vec<Vec<Lit>>) -> Option<Vec<Vec<Lit>>> {
    a.extend(b);
    (a.len() <= MAX_ARMS).then_some(a)
}

/// One arm of one literal.
fn one(l: Lit) -> Vec<Vec<Lit>> {
    vec![vec![l]]
}

/// `n` not FALSE (`positive`) or not TRUE, as an OR of AND-arms — or a
/// weaker one, allowing more rows; `None` past [`MAX_ARMS`]. No arm: never;
/// one empty arm: always.
fn dnf(n: &protobuf::Node, positive: bool, cx: &Cx<'_>) -> Option<Vec<Vec<Lit>>> {
    use protobuf::BoolExprType as B;
    match n.node.as_ref() {
        Some(node::Node::BoolExpr(b)) => match B::try_from(b.boolop) {
            // Not FALSE: every arm not FALSE. Not TRUE: some arm not TRUE.
            Ok(B::AndExpr) | Ok(B::OrExpr) => {
                let and = B::try_from(b.boolop) == Ok(B::AndExpr);
                let mut arms: Vec<Vec<Lit>> = if and == positive {
                    vec![Vec::new()]
                } else {
                    Vec::new()
                };
                for a in &b.args {
                    let sub = dnf(a, positive, cx)?;
                    arms = if and == positive {
                        product(&arms, &sub)?
                    } else {
                        either(arms, sub)?
                    };
                }
                Some(arms)
            }
            Ok(B::NotExpr) => match b.args.as_slice() {
                [inner] => dnf(inner, !positive, cx),
                _ => Some(one(Lit::Other)),
            },
            _ => Some(one(Lit::Other)),
        },
        Some(node::Node::AConst(c)) => {
            use typedpg_pg_query::protobuf::a_const::Val;
            match c.val.as_ref() {
                // NULL is neither FALSE nor TRUE.
                _ if c.isnull => Some(vec![Vec::new()]),
                Some(Val::Boolval(v)) if v.boolval == positive => Some(vec![Vec::new()]),
                Some(Val::Boolval(_)) => Some(Vec::new()),
                _ => Some(one(Lit::Other)),
            }
        }
        Some(node::Node::CaseExpr(c)) => case_dnf(c, positive, cx),
        Some(node::Node::BooleanTest(t)) => {
            use protobuf::BoolTestType as T;
            let Some(arg) = t.arg.as_deref() else {
                return Some(one(Lit::Other));
            };
            // `P IS TRUE` FALSE is `P` not TRUE, exactly; TRUE is `P`
            // TRUE, which `P` not FALSE stands in for.
            match T::try_from(t.booltesttype) {
                Ok(T::IsTrue | T::IsNotFalse) => dnf(arg, positive, cx),
                Ok(T::IsFalse | T::IsNotTrue) => dnf(arg, !positive, cx),
                _ => Some(one(Lit::Other)),
            }
        }
        Some(node::Node::AExpr(e)) if boolean_equality(e, cx) => {
            let (Some(l), Some(r)) = (e.lexpr.as_deref(), e.rexpr.as_deref()) else {
                return Some(one(Lit::Other));
            };
            // `P = Q` is NULL when either side is, else TRUE when they
            // agree: not FALSE when both are not FALSE or both not TRUE
            // (a NULL side is both), not TRUE when one is not FALSE and the
            // other not TRUE.
            let (lt, lf) = (dnf(l, true, cx)?, dnf(l, false, cx)?);
            let (rt, rf) = (dnf(r, true, cx)?, dnf(r, false, cx)?);
            let agree = either(product(&lt, &rt)?, product(&lf, &rf)?)?;
            let differ = either(product(&lt, &rf)?, product(&lf, &rt)?)?;
            let eq = crate::expr::extract_string_fields(&e.name).join(".") == "=";
            Some(if eq == positive { agree } else { differ })
        }
        _ => Some(atom(n, positive, cx)),
    }
}

/// `P = Q` / `P <> Q` between two conditions (the built-in `bool` ones).
fn boolean_equality(e: &protobuf::AExpr, cx: &Cx<'_>) -> bool {
    let op = crate::expr::extract_string_fields(&e.name).join(".");
    protobuf::AExprKind::try_from(e.kind) == Ok(protobuf::AExprKind::AexprOp)
        && (op == "=" || op == "<>")
        && cx.trust.trusts(e.location, StrictNode::BuiltinCompare)
        && e.lexpr.as_deref().is_some_and(|n| is_condition(n, cx))
        && e.rexpr.as_deref().is_some_and(|n| is_condition(n, cx))
}

/// A boolean-valued expression: a test, a comparison, a boolean column.
fn is_condition(n: &protobuf::Node, cx: &Cx<'_>) -> bool {
    use typedpg_pg_query::protobuf::a_const::Val;
    match n.node.as_ref() {
        Some(node::Node::BoolExpr(_) | node::Node::NullTest(_) | node::Node::BooleanTest(_)) => {
            true
        }
        Some(node::Node::AConst(c)) => matches!(c.val, Some(Val::Boolval(_))),
        Some(node::Node::AExpr(e)) => {
            use protobuf::AExprKind as K;
            match K::try_from(e.kind) {
                Ok(K::AexprOp) => {
                    let op = crate::expr::extract_string_fields(&e.name).join(".");
                    matches!(op.as_str(), "=" | "<>" | "<" | "<=" | ">" | ">=")
                }
                Ok(
                    K::AexprIn
                    | K::AexprOpAny
                    | K::AexprOpAll
                    | K::AexprDistinct
                    | K::AexprNotDistinct
                    | K::AexprBetween
                    | K::AexprNotBetween,
                ) => true,
                _ => false,
            }
        }
        Some(node::Node::ColumnRef(_)) => column_name(n)
            .and_then(|c| (cx.column_type)(&c))
            .is_some_and(|t| cx.snapshot.unwrap_domain(t) == crate::pg_catalog::oid::BOOL),
        _ => false,
    }
}

/// A searched CASE (or a simple one over a column) with condition-valued
/// results: the first WHEN that is TRUE picks its result, the ELSE (an
/// omitted one is NULL) applies when none is. A WHEN being TRUE is read as
/// it being not FALSE (more rows).
fn case_dnf(c: &protobuf::CaseExpr, positive: bool, cx: &Cx<'_>) -> Option<Vec<Vec<Lit>>> {
    let test = c.arg.as_deref().map(|t| cx.subject(t));
    let when_dnf = |when: &protobuf::CaseWhen, w: &protobuf::Node, pos: bool| {
        match &test {
            None => dnf(w, pos, cx),
            Some(None) => Some(one(Lit::Other)),
            // `test = value`, by the built-in `=` (resolved at the WHEN).
            Some(Some(_)) if !cx.trust.trusts(when.location, StrictNode::BuiltinCompare) => {
                Some(one(Lit::Other))
            }
            Some(Some(col)) => {
                let v = cx
                    .subject_type(col)
                    .and_then(|t| super::literal_for(w, t, cx.snapshot));
                Some(match v {
                    Some(v) => {
                        let p = ValPred::In(vec![v]);
                        one(Lit::Val(col.clone(), if pos { p } else { p.negated() }))
                    }
                    None => one(Lit::Other),
                })
            }
        }
    };
    let mut none_before: Vec<Vec<Lit>> = vec![Vec::new()];
    let mut out: Vec<Vec<Lit>> = Vec::new();
    for arg in &c.args {
        let Some(node::Node::CaseWhen(w)) = arg.node.as_ref() else {
            return Some(one(Lit::Other));
        };
        let (Some(cond), Some(result)) = (w.expr.as_deref(), w.result.as_deref()) else {
            return Some(one(Lit::Other));
        };
        let taken = product(&none_before, &when_dnf(w, cond, true)?)?;
        out = either(out, product(&taken, &dnf(result, positive, cx)?)?)?;
        none_before = product(&none_before, &when_dnf(w, cond, false)?)?;
    }
    let default = match c.defresult.as_deref() {
        Some(d) => dnf(d, positive, cx)?,
        None => vec![Vec::new()],
    };
    either(out, product(&none_before, &default)?)
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

/// `x ? 'k'` / `x ?& ARRAY['k', …]` (the built-in `jsonb_exists[_all]`):
/// TRUE when `x` holds the keys — for an object, that `x -> 'k'` isn't
/// NULL; an array holding the string `'k'` is TRUE too. So not FALSE:
/// `x` is NULL, not an object, or has the keys; not TRUE (one key): `x`
/// is NULL, not an object, or hasn't it.
fn json_key_atom(
    e: &protobuf::AExpr,
    op: &str,
    l: &protobuf::Node,
    r: &protobuf::Node,
    positive: bool,
    cx: &Cx<'_>,
) -> Option<Vec<Vec<Lit>>> {
    use crate::nonnull::{JsonbBuiltin as J, StrictNode as S};
    let keys: Vec<String> = match op {
        "?" if cx.trust.trusts(e.location, S::Jsonb(J::Exists)) => vec![string_constant(r)?],
        "?&" if positive && cx.trust.trusts(e.location, S::Jsonb(J::ExistsAll)) => {
            string_array_constant(r)?
        }
        _ => return None,
    };
    let x = cx.subject_of(l)?;
    let register = |s: Subject| {
        let name = s.name();
        if !matches!(s, Subject::Column(_)) {
            cx.subjects.borrow_mut().insert(name.clone(), s);
        }
        name
    };
    let ty = register(Subject::Typeof(Box::new(x.clone())));
    let fields: Vec<String> = keys
        .into_iter()
        .map(|k| register(Subject::Field(Box::new(x.clone()), k, false)))
        .collect();
    let x = register(x);
    let not_object = Lit::Val(ty, ValPred::NotIn(vec![json_type("object")]));
    let has = if positive {
        fields.into_iter().map(Lit::NotNull).collect()
    } else {
        fields.into_iter().map(Lit::IsNull).collect()
    };
    Some(vec![vec![Lit::IsNull(x)], vec![not_object], has])
}

/// The name `jsonb_typeof` gives a JSON type, as a text literal.
fn json_type(t: &str) -> Literal {
    Literal {
        text: t.to_owned(),
        kind: LitKind::String,
    }
}

/// `ARRAY['a', 'b']` (maybe cast to `text[]`) of string constants.
fn string_array_constant(n: &protobuf::Node) -> Option<Vec<String>> {
    match n.node.as_ref()? {
        node::Node::TypeCast(tc) => string_array_constant(tc.arg.as_deref()?),
        node::Node::AArrayExpr(a) => a.elements.iter().map(string_constant).collect(),
        _ => None,
    }
}

/// A literal (or arms of literals) for `n`, not FALSE (`positive`) or not
/// TRUE.
fn atom(n: &protobuf::Node, positive: bool, cx: &Cx<'_>) -> Vec<Vec<Lit>> {
    let scalar = |a: &protobuf::Node| cx.subject(a);
    let lit = |n: &protobuf::Node, c: &str| {
        cx.subject_type(c)
            .and_then(|t| super::literal_for(n, t, cx.snapshot))
    };
    let val = |c: String, p: ValPred| one(Lit::Val(c, if positive { p } else { p.negated() }));
    let all_null = |cols: &BTreeSet<String>| -> Vec<Vec<Lit>> {
        if cols.iter().any(|c| (cx.is_composite)(c)) {
            return one(Lit::Other);
        }
        vec![cols.iter().map(|c| Lit::IsNull(c.clone())).collect()]
    };
    let other = one(Lit::Other);
    match n.node.as_ref() {
        // A boolean column used as a condition: `done` is `done = true`.
        Some(node::Node::ColumnRef(_)) => match column_name(n) {
            Some(c) => val(c, ValPred::In(vec![Literal::boolean(true)])),
            None => other,
        },
        Some(node::Node::NullTest(t)) => {
            let Some(arg) = t.arg.as_deref() else {
                return other;
            };
            let is_null = match protobuf::NullTestType::try_from(t.nulltesttype) {
                Ok(protobuf::NullTestType::IsNull) => true,
                Ok(protobuf::NullTestType::IsNotNull) => false,
                _ => return other,
            };
            // `x IS NULL` is never NULL: not FALSE is TRUE, not TRUE FALSE.
            let null_wanted = is_null == positive;
            if let Some(c) = scalar(arg) {
                return one(if null_wanted {
                    Lit::IsNull(c)
                } else {
                    Lit::NotNull(c)
                });
            }
            // `coalesce(a, b) IS NOT NULL`: one of them is non-NULL.
            if let Some(node::Node::CoalesceExpr(co)) = arg.node.as_ref() {
                let cols: Option<BTreeSet<String>> = co.args.iter().map(scalar).collect();
                if let Some(cols) = cols.filter(|c| !c.is_empty()) {
                    return if null_wanted {
                        all_null(&cols)
                    } else {
                        one(Lit::SomeNonNull(cols))
                    };
                }
            }
            other
        }
        Some(node::Node::AExpr(e)) => {
            use protobuf::AExprKind as K;
            if let Some((args, lo, hi)) = super::null_count_bounds(e, cx.trust) {
                let cols: Option<BTreeSet<String>> = args.iter().map(column_name).collect();
                let Some(cols) = cols.filter(|c| !c.is_empty()) else {
                    return other;
                };
                let m = cols.len() as i64;
                if args.len() as i64 != m {
                    return other;
                }
                return match (positive, lo, hi) {
                    (true, lo, _) if lo >= m => one(Lit::AllNonNull(cols)),
                    (true, lo, _) if lo >= 1 => one(Lit::SomeNonNull(cols)),
                    (true, _, 0) => all_null(&cols),
                    // Fewer than `lo` (and no upper bound): one is NULL, or
                    // all are.
                    (false, lo, hi) if hi >= m && lo == m => cols
                        .iter()
                        .map(|c| {
                            if (cx.is_composite)(c) {
                                vec![Lit::Other]
                            } else {
                                vec![Lit::IsNull(c.clone())]
                            }
                        })
                        .collect(),
                    (false, 1, hi) if hi >= m => all_null(&cols),
                    _ => other,
                };
            }
            let op = crate::expr::extract_string_fields(&e.name).join(".");
            let (Some(l), Some(r)) = (e.lexpr.as_deref(), e.rexpr.as_deref()) else {
                return other;
            };
            if let Some(arms) = json_key_atom(e, &op, l, r, positive, cx) {
                return arms;
            }
            // Every comparison read below, as the built-in one: a
            // user-defined `=` may hold for different values.
            if !cx.trust.trusts(e.location, StrictNode::BuiltinCompare) {
                return other;
            }
            match K::try_from(e.kind) {
                Ok(K::AexprOp) => {
                    let (c, v, flipped) = match (scalar(l), scalar(r)) {
                        (Some(c), None) => match lit(r, &c) {
                            Some(v) => (c, v, false),
                            None => return other,
                        },
                        (None, Some(c)) => match lit(l, &c) {
                            Some(v) => (c, v, true),
                            None => return other,
                        },
                        _ => return other,
                    };
                    let p = match op.as_str() {
                        "=" => ValPred::In(vec![v]),
                        "<>" => ValPred::NotIn(vec![v]),
                        o => match CmpOp::parse(o) {
                            Some(cmp) => ValPred::Cmp(if flipped { cmp.flipped() } else { cmp }, v),
                            None => return other,
                        },
                    };
                    val(c, p)
                }
                // A NULL in the list makes `IN` NULL rather than FALSE:
                // such a list says nothing.
                Ok(K::AexprIn) => {
                    let (Some(c), Some(node::Node::List(list))) = (scalar(l), r.node.as_ref())
                    else {
                        return other;
                    };
                    let Some(t) = cx.subject_type(&c) else {
                        return other;
                    };
                    match (
                        super::constants(list.items.iter(), t, false, cx.snapshot),
                        op.as_str(),
                    ) {
                        (Some(vs), "=") => val(c, ValPred::In(vs)),
                        (Some(vs), "<>") => val(c, ValPred::NotIn(vs)),
                        _ => other,
                    }
                }
                Ok(kind @ (K::AexprOpAny | K::AexprOpAll)) => {
                    let Some(c) = scalar(l) else {
                        return other;
                    };
                    let Some(t) = cx.subject_type(&c) else {
                        return other;
                    };
                    match (
                        kind,
                        op.as_str(),
                        super::array_constants(r, t, false, cx.snapshot),
                    ) {
                        (K::AexprOpAny, "=", Some(vs)) => val(c, ValPred::In(vs)),
                        (K::AexprOpAll, "<>", Some(vs)) => val(c, ValPred::NotIn(vs)),
                        _ => other,
                    }
                }
                // `c IS DISTINCT FROM v` (never NULL): `c` is NULL, or
                // non-NULL and not `v`.
                Ok(kind @ (K::AexprDistinct | K::AexprNotDistinct)) => {
                    let (c, v) = match (scalar(l), scalar(r)) {
                        (Some(c), None) => match lit(r, &c) {
                            Some(v) => (c, v),
                            None => return other,
                        },
                        (None, Some(c)) => match lit(l, &c) {
                            Some(v) => (c, v),
                            None => return other,
                        },
                        _ => return other,
                    };
                    let distinct = (kind == K::AexprDistinct) == positive;
                    if distinct {
                        vec![
                            vec![Lit::IsNull(c.clone())],
                            vec![
                                Lit::NotNull(c.clone()),
                                Lit::Val(c, ValPred::NotIn(vec![v])),
                            ],
                        ]
                    } else {
                        vec![vec![
                            Lit::NotNull(c.clone()),
                            Lit::Val(c, ValPred::In(vec![v])),
                        ]]
                    }
                }
                // `c BETWEEN a AND b` is `c >= a AND c <= b`.
                Ok(kind @ (K::AexprBetween | K::AexprNotBetween)) => {
                    let (Some(c), Some(node::Node::List(list))) = (scalar(l), r.node.as_ref())
                    else {
                        return other;
                    };
                    let (Some(a), Some(b)) = (
                        list.items.first().and_then(|n| lit(n, &c)),
                        list.items.get(1).and_then(|n| lit(n, &c)),
                    ) else {
                        return other;
                    };
                    let ge = Lit::Val(c.clone(), ValPred::Cmp(CmpOp::Ge, a.clone()));
                    let le = Lit::Val(c.clone(), ValPred::Cmp(CmpOp::Le, b.clone()));
                    if (kind == K::AexprBetween) == positive {
                        vec![vec![ge, le]]
                    } else {
                        vec![
                            vec![Lit::Val(c.clone(), ValPred::Cmp(CmpOp::Lt, a))],
                            vec![Lit::Val(c, ValPred::Cmp(CmpOp::Gt, b))],
                        ]
                    }
                }
                _ => other,
            }
        }
        _ => other,
    }
}
