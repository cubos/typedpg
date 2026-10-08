//! Expression type inference.
//!
//! Walks typedpg_pg_query AST expression nodes and infers their type (OID) and
//! nullability based on the schema snapshot and current scope.
//!
//! Every expression evaluation receives a [`TypeGoal`] describing the type
//! expected by the enclosing context (e.g. `BOOL` for `WHERE`, `INT8` for
//! `LIMIT`).  When the result is a `ParamRef` whose type is still `UNKNOWN`,
//! the goal type is recorded as a constraint — this is the single mechanism
//! that replaces all ad-hoc parameter recording.  After inference, a
//! compatibility check verifies that the result type can be coerced to the
//! goal under the allowed coercion context.

use typedpg_pg_query::protobuf::{self, a_const, node};

use crate::coerce::{self, CoercionContext, can_coerce};
use crate::error::AnalyzeError;
use crate::functions;
use crate::functions::OutArg;
use crate::nullability::NullabilityContext;
use crate::oid::PgTypeOid;
use crate::param_collector::ParamCollector;
use crate::pg_catalog::{PgCatalog, TypCategory, TypType, oid};
use crate::scope::Scope;

// Built-in type OIDs the expression layer needs beyond `pg_catalog::oid`
// (fixed by PG's `pg_type.dat`).
pub(crate) const BIT: PgTypeOid = PgTypeOid::from_raw(1560);

// ──────────────────────────────────────────────────────────────────────────────
// Inference context
// ──────────────────────────────────────────────────────────────────────────────

/// The immutable context threaded through every expression-inference call:
/// the name-resolution [`Scope`], the outer-join [`NullabilityContext`], and
/// the catalog [`PgCatalog`] snapshot. Bundled into one `Copy` handle so the
/// inference functions take `(node, ctx, params, goal)` instead of repeating
/// the same four references at every call. The mutable [`ParamCollector`] is
/// kept separate (it can't share a struct with the shared borrows).
#[derive(Clone, Copy)]
pub(crate) struct Ctx<'a> {
    pub scope: &'a Scope,
    pub null_ctx: &'a NullabilityContext,
    pub snapshot: &'a PgCatalog,
    /// When set, every function the expression runs — called directly,
    /// through an operator or through an explicit cast — is recorded: what
    /// DDL needs to check an index / generation expression's mutability
    /// the way PG's CheckMutability does.
    pub used_procs: Option<&'a std::cell::RefCell<Vec<crate::oid::PgProcOid>>>,
    /// When set, the strictness of the operators, functions and casts the
    /// expression resolves is recorded — what [`crate::nonnull`] needs to
    /// tell what a qual proves non-NULL.
    pub strict_log: Option<&'a crate::nonnull::StrictLog>,
}

impl<'a> Ctx<'a> {
    /// Build a context from its three parts.
    pub fn new(
        scope: &'a Scope,
        null_ctx: &'a NullabilityContext,
        snapshot: &'a PgCatalog,
    ) -> Self {
        Ctx {
            scope,
            null_ctx,
            snapshot,
            used_procs: None,
            strict_log: None,
        }
    }

    /// The same context, recording strictness into `log`.
    pub fn logging_strictness(self, log: &'a crate::nonnull::StrictLog) -> Self {
        Ctx {
            strict_log: Some(log),
            ..self
        }
    }

    /// The same context over another nullability context.
    pub fn with_null_ctx(self, null_ctx: &'a NullabilityContext) -> Self {
        Ctx { null_ctx, ..self }
    }

    /// Record the strictness of the node of `kind` at `location`.
    pub fn note_strict(&self, location: i32, kind: crate::nonnull::StrictNode, strict: bool) {
        if let Some(log) = self.strict_log {
            log.note(location, kind, strict);
        }
    }

    /// Record that the untyped string literal at `location` was coerced to
    /// `type_oid`.
    pub fn note_literal_type(&self, location: i32, type_oid: PgTypeOid) {
        if let Some(log) = self.strict_log {
            log.note_literal_type(location, type_oid);
        }
    }

    /// Whether `proc` is strict.
    pub fn proc_is_strict(&self, proc: Option<crate::oid::PgProcOid>) -> bool {
        proc.and_then(|p| self.snapshot.pg_proc.get(&p))
            .is_some_and(|p| p.proisstrict)
    }

    /// Whether the implicit coercion from `from` to `to` (an argument
    /// coerced to a declared type) maps NULL to NULL: no cast function, or
    /// a strict one.
    pub fn coercion_is_strict(&self, from: PgTypeOid, to: PgTypeOid) -> bool {
        if from == to || from == oid::UNKNOWN {
            return true;
        }
        let key = (
            self.snapshot.unwrap_domain(from),
            self.snapshot.unwrap_domain(to),
        );
        match self
            .snapshot
            .cast_by_pair
            .get(&key)
            .and_then(|oid| self.snapshot.pg_cast.get(oid))
            .and_then(|c| c.castfunc)
        {
            Some(f) => self.proc_is_strict(Some(f)),
            None => true,
        }
    }

    /// The same context, recording the functions the expression runs.
    pub fn recording(self, used_procs: &'a std::cell::RefCell<Vec<crate::oid::PgProcOid>>) -> Self {
        Ctx {
            used_procs: Some(used_procs),
            ..self
        }
    }

    /// Record that the expression runs function `oid`.
    pub fn note_proc(&self, oid: Option<crate::oid::PgProcOid>) {
        if let (Some(cell), Some(oid)) = (self.used_procs, oid) {
            cell.borrow_mut().push(oid);
        }
    }

    /// Record the cast function an implicit coercion from `from` to `to`
    /// runs (an argument coerced to a function's / operator's declared
    /// type), if any.
    pub fn note_coercion(&self, from: PgTypeOid, to: PgTypeOid) {
        if self.used_procs.is_none() || from == to || from == oid::UNKNOWN {
            return;
        }
        let key = (
            self.snapshot.unwrap_domain(from),
            self.snapshot.unwrap_domain(to),
        );
        self.note_proc(
            self.snapshot
                .cast_by_pair
                .get(&key)
                .and_then(|oid| self.snapshot.pg_cast.get(oid))
                .and_then(|c| c.castfunc),
        );
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// TypeGoal
// ──────────────────────────────────────────────────────────────────────────────

/// The type expected by the enclosing context.
///
/// Mirrors PostgreSQL's approach where each clause (`WHERE`, `LIMIT`, `INSERT
/// VALUES`, …) tells the parser "I expect this expression to produce type X
/// with coercion level Y".
#[derive(Debug, Clone)]
pub(crate) struct TypeGoal {
    pub type_oid: PgTypeOid,
    pub coercion: CoercionContext,
    /// Optional byte range (post-lex SQL) covering the *source* of this
    /// expectation — the column being assigned, the other side of a
    /// comparison, etc. When present, surfaces as a secondary label in
    /// `TypeMismatch` diagnostics ("expected here").
    pub source_span: Option<crate::error::SourceSpan>,
    /// When the expectation comes from a named column (INSERT VALUES,
    /// UPDATE SET), the column's name. Used to produce PG's exact wording:
    /// `column "X" is of type Y but expression is of type Z`.
    pub source_col_name: Option<String>,
    /// The type modifier the coercion hands the target's input function
    /// (`None` is PG's `-1`): an assigned column's typmod. Every other
    /// coercion of an untyped literal passes `-1`.
    pub typmod: Option<i32>,
}

impl TypeGoal {
    /// No type expectation (e.g. SELECT target list).
    pub const NONE: Self = Self {
        type_oid: oid::UNKNOWN,
        coercion: CoercionContext::Implicit,
        source_span: None,
        source_col_name: None,
        typmod: None,
    };

    /// Expression context — only implicit casts allowed
    /// (operator/function argument matching).
    pub fn implicit(type_oid: PgTypeOid) -> Self {
        Self {
            type_oid,
            coercion: CoercionContext::Implicit,
            source_span: None,
            source_col_name: None,
            typmod: None,
        }
    }

    /// Assignment context — implicit + assignment casts allowed
    /// (WHERE, LIMIT, INSERT, UPDATE — matches PG's `COERCION_ASSIGNMENT`).
    pub fn assignment(type_oid: PgTypeOid) -> Self {
        Self {
            type_oid,
            coercion: CoercionContext::Assignment,
            source_span: None,
            source_col_name: None,
            typmod: None,
        }
    }

    /// Attach a `source_span` (the range that establishes this expectation,
    /// e.g. the column reference being assigned to). Used to render a
    /// secondary label in type-mismatch diagnostics.
    pub fn with_source(mut self, span: crate::error::SourceSpan) -> Self {
        self.source_span = Some(span);
        self
    }

    /// Attach the target's type modifier (see [`Self::typmod`]).
    pub fn with_typmod(mut self, typmod: Option<i32>) -> Self {
        self.typmod = typmod;
        self
    }

    /// Attach the name of the column whose type drove this goal. Used by
    /// `check_goal_compatibility` to render PG's exact wording for
    /// INSERT/UPDATE assignments.
    pub fn with_source_column(mut self, name: impl Into<String>) -> Self {
        self.source_col_name = Some(name.into());
        self
    }

    pub fn has_expectation(&self) -> bool {
        self.type_oid != oid::UNKNOWN
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// ExprType
// ──────────────────────────────────────────────────────────────────────────────

/// Result of inferring an expression's type.
///
/// `record_fields` is populated for anonymous records whose attribute list is
/// statically determinable (e.g. `ROW(1, 'x')` produces `Some([(f1, int4),
/// (f2, text)])`). Mirrors PostgreSQL's typmod + RecordCacheArray: when the
/// shape can't be determined (opaque `RETURNS RECORD`, UNION of mismatched
/// shapes, casts to `record`/`text`), the field is `None` and the value
/// behaves like a typmod=-1 dynamic record.
#[derive(Debug, Clone)]
pub(crate) struct ExprType {
    pub type_oid: PgTypeOid,
    pub nullable: bool,
    /// `pg_attribute.atttypmod`-shaped type modifier carried through the
    /// inference. `None` (PG's `-1`) is the default; only column refs, direct
    /// casts, and uniform CASE/UNION branches actually propagate a value.
    /// Functions, operators, and aggregates strip it (PG matching).
    pub typmod: Option<i32>,
    /// `pg_collation.oid` carried through the inference. `None` for non-
    /// collatable types and untagged sites; `Some` only when a real
    /// collation is pinned (column attcollation, explicit `COLLATE "x"`,
    /// or propagation through a binary op / case branch). Operator and
    /// function outputs typically clear this — PG's full collation
    /// derivation rules ("explicit > implicit > none") are approximated
    /// here as "left-or-right wins" in binary contexts.
    pub collation: Option<crate::oid::PgCollationOid>,
    /// Whether `collation` was derived from an explicit `COLLATE` clause
    /// (PG's COLLATE_EXPLICIT strength) rather than implicitly from a
    /// column. Explicit collations win over implicit ones, and two
    /// different explicit ones meeting in one expression are an error.
    pub explicit_collation: bool,
    pub record_fields: Option<RecordShape>,
    /// For an array value, whether its elements can be NULL, where known
    /// (see [`crate::types::Type::Array`]); `None` otherwise.
    pub elem_nullable: Option<bool>,
    /// What every non-NULL value is (see [`crate::refine`]).
    pub refine: crate::refine::Refinement,
}

/// The row shape of a record value: its fields, and whether PostgreSQL
/// sees them too while parsing.
///
/// The analyzer keeps a shape PG loses: a field selected out of a record
/// that is itself a record (`(ROW(1, ROW(2, 3))).f2`) is a `FieldSelect`
/// whose result PG types as plain `record` (typmod -1), with no tuple
/// descriptor `get_expr_result_tupdesc` could find. The value still comes
/// back with its fields (the shape describes it to the macro), but PG
/// rejects selecting from it again: `could not identify column … in record
/// data type` on the expression, `record type has not been registered`
/// through a subquery's column (`expandRecordVariable`) or for `.*`.
#[derive(Debug, Clone)]
pub(crate) struct RecordShape {
    pub fields: Vec<RecordField>,
    /// PG has no tuple descriptor for this shape.
    pub hidden: bool,
}

impl RecordShape {
    /// The shape PG can't see, of a value selected out of another record
    /// or read from a VALUES list.
    pub fn hidden(fields: Vec<RecordField>) -> Self {
        Self {
            fields,
            hidden: true,
        }
    }
}

/// The row shape of a set operation's record column from its arms'. PG
/// takes the left arm's (what `expandRecordVariable` follows), so whether
/// it is hidden is the left's; the analyzer keeps a shape only when both
/// arms agree on it field for field, a field NULL where a row that can
/// come out (either arm's for UNION, the left one's otherwise) has it NULL.
pub(crate) fn merge_set_op_shapes(
    left: Option<&RecordShape>,
    right: Option<&RecordShape>,
    union: bool,
) -> Option<RecordShape> {
    let (left, right) = (left?, right?);
    if left.len() != right.len() {
        return None;
    }
    let mut fields = Vec::with_capacity(left.len());
    for (l, r) in left.iter().zip(right.iter()) {
        if l.ty.type_oid != r.ty.type_oid {
            return None;
        }
        let mut ty = l.ty.clone();
        if union {
            ty.nullable |= r.ty.nullable;
            ty.elem_nullable = merge_elem_nullable([l.ty.elem_nullable, r.ty.elem_nullable]);
            ty.refine = crate::refine::Refinement::either([&l.ty.refine, &r.ty.refine]);
            ty.record_fields = match (&l.ty.record_fields, &r.ty.record_fields) {
                (None, None) => None,
                (l, r) => merge_set_op_shapes(l.as_ref(), r.as_ref(), true),
            };
        }
        fields.push(RecordField {
            name: l.name.clone(),
            ty,
        });
    }
    Some(RecordShape {
        fields,
        hidden: left.hidden,
    })
}

impl From<Vec<RecordField>> for RecordShape {
    fn from(fields: Vec<RecordField>) -> Self {
        Self {
            fields,
            hidden: false,
        }
    }
}

impl std::ops::Deref for RecordShape {
    type Target = [RecordField];

    fn deref(&self) -> &[RecordField] {
        &self.fields
    }
}

/// One element of an anonymous record's static shape, as it flows through
/// inference. Recursive via `ty: ExprType` — nested rows like
/// `ROW(1, ROW(2, 3))` survive without a special `nested_fields` channel.
///
/// SRF / OUT-arg outputs live as [`OutArg`]. The `from_*` constructor below
/// bridges that form into the expression-side shape used during inference.
/// Composite-type fields are read directly from `pg_attribute` via
/// [`PgCatalog::composite_fields_of`] in the call sites that need them.
#[derive(Debug, Clone)]
pub(crate) struct RecordField {
    pub name: String,
    pub ty: ExprType,
}

impl RecordField {
    /// Convert an SRF / OUT-arg field into the expression form.
    pub fn from_out_arg(a: &OutArg) -> Self {
        Self {
            name: a.name.clone(),
            ty: ExprType::scalar(a.type_oid, !a.not_null),
        }
    }

    pub fn from_out_args(args: &[OutArg]) -> Vec<Self> {
        args.iter().map(Self::from_out_arg).collect()
    }
}

impl ExprType {
    /// Construct a scalar (non-record) ExprType. The vast majority of call
    /// sites use this; only ROW constructors and shape-propagating helpers
    /// build with `record_fields: Some(...)`.
    pub fn scalar(type_oid: PgTypeOid, nullable: bool) -> Self {
        Self {
            type_oid,
            nullable,
            typmod: None,
            collation: None,
            explicit_collation: false,
            record_fields: None,
            elem_nullable: None,
            refine: crate::refine::Refinement::NONE,
        }
    }

    /// Construct a scalar with a known `pg_attribute.atttypmod` value. Used
    /// by `infer_column_ref` and `infer_type_cast` to thread the modifier
    /// through the inference chain.
    pub fn scalar_with_typmod(type_oid: PgTypeOid, nullable: bool, typmod: Option<i32>) -> Self {
        Self {
            type_oid,
            nullable,
            typmod,
            collation: None,
            explicit_collation: false,
            record_fields: None,
            elem_nullable: None,
            refine: crate::refine::Refinement::NONE,
        }
    }

    /// Construct a scalar with a known typmod *and* collation. Used by
    /// `infer_column_ref` (column attcollation) and the `CollateClause`
    /// arm of `infer_expr` (explicit decoration overrides the inferred
    /// collation regardless of source).
    pub fn scalar_with_collation(
        type_oid: PgTypeOid,
        nullable: bool,
        typmod: Option<i32>,
        collation: Option<crate::oid::PgCollationOid>,
    ) -> Self {
        Self {
            type_oid,
            nullable,
            typmod,
            collation,
            explicit_collation: false,
            record_fields: None,
            elem_nullable: None,
            refine: crate::refine::Refinement::NONE,
        }
    }

    /// The same value, refined.
    pub fn with_refine(mut self, refine: crate::refine::Refinement) -> Self {
        self.refine = refine;
        self
    }

    /// The same value, with its array elements' nullability.
    pub fn with_elem_nullable(mut self, elem_nullable: Option<bool>) -> Self {
        self.elem_nullable = elem_nullable;
        self
    }

    /// The same value with the collation state PG's `assign_collations`
    /// derives for it (see [`derive_collation`]).
    pub fn with_collation(mut self, state: (Option<crate::oid::PgCollationOid>, bool)) -> Self {
        (self.collation, self.explicit_collation) = state;
        self
    }

    /// Account for this value being implicitly coerced to `to` (a call
    /// argument to its parameter's type, a branch to a common type): the
    /// cast function the coercion runs may map it — or, coercing an array
    /// element by element, one of its elements — to NULL. The type is left
    /// alone; only the nullability the coerced value can have changes.
    pub(crate) fn note_coerced_to(&mut self, to: PgTypeOid, snapshot: &PgCatalog) {
        if literals::coercion_can_return_null(self.type_oid, to, snapshot) {
            self.nullable = true;
        }
        if literals::coercion_can_null_elements(self.type_oid, to, snapshot) {
            self.elem_nullable = Some(true);
        }
        self.refine = self.refine.converted(self.type_oid, to, snapshot);
    }
}

/// The element nullability of an array built from parts whose element (or
/// value) nullability is `parts`: NULL-able if any part's is, known
/// non-NULL only if every part's is.
pub(crate) fn merge_elem_nullable(parts: impl IntoIterator<Item = Option<bool>>) -> Option<bool> {
    let mut all_known = true;
    for part in parts {
        match part {
            Some(true) => return Some(true),
            Some(false) => {}
            None => all_known = false,
        }
    }
    all_known.then_some(false)
}

/// PG's `DEFAULT_COLLATION_OID`.
const DEFAULT_COLLATION: crate::oid::PgCollationOid = crate::oid::PgCollationOid::from_raw(100);

/// PG's collation derivation for one expression node (`assign_collations_walker`
/// and `merge_collation_state`, parse_collate.c), given its inputs' states
/// and its result type:
///
/// - explicit (`COLLATE`) inputs dominate; two different ones are `collation
///   mismatch between explicit collations "A" and "B"` (42P21) whatever the
///   result type;
/// - otherwise a non-default implicit collation beats the default one, and
///   two different non-default ones leave the collation undetermined (PG only
///   fails later, if a collation is actually needed);
/// - a non-collatable result has no collation.
pub(crate) fn derive_collation<'a>(
    inputs: impl IntoIterator<Item = &'a ExprType>,
    result_type: PgTypeOid,
    snapshot: &PgCatalog,
) -> Result<(Option<crate::oid::PgCollationOid>, bool), AnalyzeError> {
    let mut explicit: Option<crate::oid::PgCollationOid> = None;
    let mut implicit: Option<crate::oid::PgCollationOid> = None;
    let mut implicit_conflict = false;
    for t in inputs {
        let Some(c) = t.collation else {
            continue;
        };
        if t.explicit_collation {
            match explicit {
                Some(e) if e != c => {
                    let name = |o| {
                        snapshot
                            .pg_collation
                            .get(&o)
                            .map(|c| c.collname.clone())
                            .unwrap_or_default()
                    };
                    return Err(crate::pgmsg::collation_mismatch_explicit(
                        &name(e),
                        &name(c),
                    ));
                }
                _ => explicit = Some(c),
            }
        } else if c != DEFAULT_COLLATION {
            match implicit {
                Some(i) if i != c => implicit_conflict = true,
                _ => implicit = Some(c),
            }
        }
    }
    let collatable = snapshot
        .get_type(result_type)
        .is_some_and(|t| t.typcollation.is_some());
    if !collatable {
        return Ok((None, false));
    }
    Ok(match explicit {
        Some(e) => (Some(e), true),
        None if implicit_conflict => (None, false),
        None => (implicit, false),
    })
}

// ──────────────────────────────────────────────────────────────────────────────
// Context-rule validation
// ──────────────────────────────────────────────────────────────────────────────

/// Kind of function call an expression tree contains. Used to enforce PG's
/// placement rules (`no aggregate in WHERE`, `no window function in WHERE`,
/// `no nested aggregates`).
#[derive(Default, Debug, Clone, Copy)]
pub(crate) struct FuncKindPresence {
    pub has_aggregate: bool,
    pub has_window: bool,
    /// Location of the first aggregate / window call seen, so a placement
    /// error can point its caret at the offending call.
    pub agg_location: Option<i32>,
    pub window_location: Option<i32>,
    /// A `GROUPING(…)` call — aggregate-like for PG's placement rules, but
    /// with its own wording (`grouping operations are not allowed in …`).
    pub has_grouping: bool,
    pub grouping_location: Option<i32>,
}

/// Walk an expression AST and report whether it contains aggregate calls
/// (`COUNT(*)`, `SUM(x)`, …) or window function calls (`RANK() OVER …`),
/// without resolving anything against the schema. Used up-front by clauses
/// that forbid those constructs (WHERE, GROUP BY, JOIN ON, HAVING for the
/// nested-agg case).
impl FuncKindPresence {
    /// Where the first aggregate (or `GROUPING`) call found is — PG
    /// positions placement errors at the offending call's name.
    pub(crate) fn aggregate_span(&self) -> Option<crate::error::SourceSpan> {
        self.agg_location
            .or(self.grouping_location)
            .and_then(crate::error::SourceSpan::from_node_qname)
    }

    /// Where the first window-function call found is.
    pub(crate) fn window_span(&self) -> Option<crate::error::SourceSpan> {
        self.window_location
            .and_then(crate::error::SourceSpan::from_node_qname)
    }
}

pub(crate) fn detect_func_kinds(node: &protobuf::Node, snapshot: &PgCatalog) -> FuncKindPresence {
    let mut out = FuncKindPresence::default();
    walk(node, snapshot, &mut out);
    out
}

fn walk(node: &protobuf::Node, snapshot: &PgCatalog, out: &mut FuncKindPresence) {
    let Some(inner) = node.node.as_ref() else {
        return;
    };
    match inner {
        node::Node::FuncCall(fc) => {
            if fc.over.is_some() {
                out.has_window = true;
                out.window_location.get_or_insert(fc.location);
            } else {
                // Aggregate check via pg_proc.is_aggregate — resolved by name
                // against the snapshot.
                let parts = extract_string_fields(&fc.funcname);
                let (schema, name) = match parts.as_slice() {
                    [n] => (None, n.as_str()),
                    [s, n] => (Some(s.as_str()), n.as_str()),
                    _ => (None, ""),
                };
                if !name.is_empty() {
                    // Only an aggregate the call's argument count can
                    // reach: `max()` is no call to `max(anyarray)` — PG
                    // fails it with `function max() does not exist`
                    // before any placement rule.
                    // (An ordered-set aggregate's WITHIN GROUP columns
                    // are arguments too.)
                    let nargs = fc.args.len()
                        + if fc.agg_within_group {
                            fc.agg_order.len()
                        } else {
                            0
                        };
                    let candidates = snapshot.find_functions(schema, name);
                    if candidates.iter().any(|f| {
                        let declared = f.proargtypes.len();
                        let required = declared.saturating_sub(f.pronargdefaults.max(0) as usize);
                        matches!(f.prokind, crate::pg_catalog::ProKind::Aggregate)
                            && ((required..=declared).contains(&nargs)
                                || (f.provariadic.is_some() && nargs + 1 >= declared))
                    }) {
                        out.has_aggregate = true;
                        out.agg_location.get_or_insert(fc.location);
                    }
                }
            }
            for arg in &fc.args {
                walk(arg, snapshot, out);
            }
            if let Some(f) = &fc.agg_filter {
                walk(f, snapshot, out);
            }
            for o in &fc.agg_order {
                walk(o, snapshot, out);
            }
        }
        node::Node::NamedArgExpr(na) => {
            if let Some(a) = &na.arg {
                walk(a, snapshot, out);
            }
        }
        node::Node::GroupingFunc(g) => {
            out.has_grouping = true;
            out.grouping_location.get_or_insert(g.location);
        }
        // JSON_OBJECTAGG / JSON_ARRAYAGG are aggregates (window functions
        // with OVER).
        node::Node::JsonObjectAgg(_) | node::Node::JsonArrayAgg(_) => {
            let (ctor, args): (Option<&protobuf::JsonAggConstructor>, Vec<&protobuf::Node>) =
                match inner {
                    node::Node::JsonObjectAgg(a) => (
                        a.constructor.as_deref(),
                        a.arg
                            .as_deref()
                            .map(|kv| {
                                kv.key
                                    .as_deref()
                                    .into_iter()
                                    .chain(kv.value.as_deref().and_then(|v| v.raw_expr.as_deref()))
                                    .collect()
                            })
                            .unwrap_or_default(),
                    ),
                    node::Node::JsonArrayAgg(a) => (
                        a.constructor.as_deref(),
                        a.arg
                            .as_deref()
                            .and_then(|v| v.raw_expr.as_deref())
                            .into_iter()
                            .collect(),
                    ),
                    _ => (None, Vec::new()),
                };
            let location = ctor.map_or(-1, |c| c.location);
            if ctor.is_some_and(|c| c.over.is_some()) {
                out.has_window = true;
                out.window_location.get_or_insert(location);
            } else {
                out.has_aggregate = true;
                out.agg_location.get_or_insert(location);
            }
            for a in args {
                walk(a, snapshot, out);
            }
        }
        node::Node::JsonObjectConstructor(c) => {
            for e in &c.exprs {
                walk(e, snapshot, out);
            }
        }
        node::Node::JsonArrayConstructor(c) => {
            for e in &c.exprs {
                walk(e, snapshot, out);
            }
        }
        node::Node::JsonKeyValue(kv) => {
            if let Some(k) = &kv.key {
                walk(k, snapshot, out);
            }
            if let Some(v) = kv.value.as_deref().and_then(|v| v.raw_expr.as_deref()) {
                walk(v, snapshot, out);
            }
        }
        node::Node::JsonValueExpr(v) => {
            if let Some(e) = &v.raw_expr {
                walk(e, snapshot, out);
            }
        }
        node::Node::JsonIsPredicate(p) => {
            if let Some(e) = &p.expr {
                walk(e, snapshot, out);
            }
        }
        node::Node::JsonScalarExpr(s) => {
            if let Some(e) = &s.expr {
                walk(e, snapshot, out);
            }
        }
        node::Node::JsonFuncExpr(f) => {
            if let Some(e) = f
                .context_item
                .as_deref()
                .and_then(|v| v.raw_expr.as_deref())
            {
                walk(e, snapshot, out);
            }
            if let Some(p) = &f.pathspec {
                walk(p, snapshot, out);
            }
        }
        node::Node::AExpr(e) => {
            if let Some(l) = &e.lexpr {
                walk(l, snapshot, out);
            }
            if let Some(r) = &e.rexpr {
                walk(r, snapshot, out);
            }
        }
        node::Node::BoolExpr(b) => {
            for a in &b.args {
                walk(a, snapshot, out);
            }
        }
        node::Node::NullTest(t) => {
            if let Some(a) = &t.arg {
                walk(a, snapshot, out);
            }
        }
        node::Node::BooleanTest(t) => {
            if let Some(a) = &t.arg {
                walk(a, snapshot, out);
            }
        }
        node::Node::CoalesceExpr(c) => {
            for a in &c.args {
                walk(a, snapshot, out);
            }
        }
        node::Node::CaseExpr(c) => {
            for w in &c.args {
                walk(w, snapshot, out);
            }
            if let Some(d) = &c.defresult {
                walk(d, snapshot, out);
            }
        }
        node::Node::CaseWhen(w) => {
            if let Some(e) = &w.expr {
                walk(e, snapshot, out);
            }
            if let Some(r) = &w.result {
                walk(r, snapshot, out);
            }
        }
        node::Node::TypeCast(c) => {
            if let Some(a) = &c.arg {
                walk(a, snapshot, out);
            }
        }
        node::Node::List(l) => {
            for i in &l.items {
                walk(i, snapshot, out);
            }
        }
        node::Node::SubLink(_) => {
            // Do NOT descend into subqueries — a SubLink is its own scope
            // and aggregates/windows inside it belong to that scope, not
            // the one we're validating.
        }
        _ => {}
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Main entry point
// ──────────────────────────────────────────────────────────────────────────────

/// Infer the type and nullability of an AST expression node.
///
/// `goal` describes the type expected by the enclosing context.  When the
/// expression is a `ParamRef` whose type is still unknown, the goal type is
/// recorded as a constraint.  After inference, the result is checked for
/// compatibility with the goal (raising `TypeMismatch` on failure).
pub(crate) fn infer_expr(
    node: &protobuf::Node,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
    goal: TypeGoal,
) -> Result<ExprType, AnalyzeError> {
    // An error raised with no location points at the innermost expression
    // being inferred when it was.
    let mut t = infer_expr_unlocated(node, ctx, params, goal)
        .map_err(|e| crate::error::with_fallback_span(e, || crate::error::expr_span(node)))?;
    // The same expression a qual proved non-NULL for this row (`WHERE
    // j ->> 'k' IS NOT NULL`, `HAVING max(b) > 0`) — see `nonnull::exprs`.
    if t.nullable
        && ctx.null_ctx.has_expr_facts()
        && !matches!(
            node.node.as_ref(),
            Some(node::Node::ColumnRef(_) | node::Node::AConst(_) | node::Node::ParamRef(_))
        )
        && ctx
            .null_ctx
            .expr_proven_non_null(&crate::nonnull::exprs::key(node))
    {
        t.nullable = false;
    }
    // A grouped expression some grouping set leaves out is NULL in that
    // set's rows (`GROUP BY ROLLUP (g + 1)`'s total row).
    if !t.nullable
        && !matches!(node.node.as_ref(), Some(node::Node::ColumnRef(_)))
        && ctx.null_ctx.grouping_omits_exprs()
        && ctx.null_ctx.grouping_omitted.contains(&(
            crate::grouping::EXPR_KEY.to_owned(),
            crate::grouping::expr_key(node, ctx.scope, ctx.snapshot),
        ))
    {
        t.nullable = true;
    }
    Ok(t)
}

fn infer_expr_unlocated(
    node: &protobuf::Node,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
    goal: TypeGoal,
) -> Result<ExprType, AnalyzeError> {
    let Ctx { snapshot, .. } = ctx;
    let inner = node
        .node
        .as_ref()
        .ok_or_else(|| AnalyzeError::Unsupported("empty node".into()))?;

    let result = match inner {
        node::Node::ColumnRef(col_ref) => infer_column_ref(col_ref, ctx),
        node::Node::AConst(a_const) => infer_a_const(a_const),
        node::Node::TypeCast(cast) => infer_type_cast(cast, ctx, params),
        node::Node::FuncCall(func) => infer_func_call(func, ctx, params),
        // `name => value` only occurs as a function-call argument; its type is
        // the value's. The enclosing call resolves the name.
        node::Node::NamedArgExpr(na) => match na.arg.as_deref() {
            Some(arg) => infer_expr(arg, ctx, params, goal.clone()),
            None => Err(AnalyzeError::Unsupported(
                "named argument without a value".into(),
            )),
        },
        node::Node::GroupingFunc(g) => {
            // `GROUPING(expr, …)` — returns int4 indicating which of the
            // listed expressions are *missing* from the current grouping
            // set. Always defined → NOT NULL. Walk the args so params get
            // typed and column refs / typos surface as errors.
            for arg in &g.args {
                infer_expr(arg, ctx, params, TypeGoal::NONE)?;
            }
            Ok(ExprType::scalar(oid::INT4, false))
        }
        node::Node::AExpr(expr) => infer_a_expr(expr, ctx, params),
        node::Node::BoolExpr(expr) => infer_bool_expr(expr, ctx, params),
        node::Node::NullTest(t) => {
            if let Some(arg) = &t.arg {
                infer_expr(arg, ctx, params, TypeGoal::NONE)?;
                // IS [NOT] NULL accepts any type, so it pins nothing — and
                // PG *locks* the parameter's type at this first untyped use:
                // `SELECT $1 IS NULL, $1 = 1` is `could not determine data
                // type of parameter $1` (42P08) even though the later use
                // would pin int4. A param typed *before* this point is fine.
                if let Some(node::Node::ParamRef(p)) = arg.node.as_ref() {
                    params.mark_indeterminate_locked(p.number);
                }
            }
            Ok(ExprType::scalar(oid::BOOL, false))
        }
        node::Node::BooleanTest(t) => {
            // `x IS [NOT] TRUE/FALSE/UNKNOWN` coerces its operand to boolean
            // (PG's coerce_to_boolean) — so a bare `$1 IS TRUE` pins the
            // param as bool, and a non-boolean operand gets PG's wording
            // from the shared clause walker.
            if let Some(arg) = &t.arg {
                let label = match protobuf::BoolTestType::try_from(t.booltesttype) {
                    Ok(protobuf::BoolTestType::IsTrue) => "IS TRUE",
                    Ok(protobuf::BoolTestType::IsNotTrue) => "IS NOT TRUE",
                    Ok(protobuf::BoolTestType::IsFalse) => "IS FALSE",
                    Ok(protobuf::BoolTestType::IsNotFalse) => "IS NOT FALSE",
                    Ok(protobuf::BoolTestType::IsUnknown) => "IS UNKNOWN",
                    _ => "IS NOT UNKNOWN",
                };
                crate::clause::coerce_clause_expr(
                    arg,
                    ctx,
                    params,
                    crate::clause::ClauseKind::BoolTest(label),
                )?;
            }
            Ok(ExprType::scalar(oid::BOOL, false))
        }
        node::Node::CoalesceExpr(expr) => infer_coalesce(expr, ctx, params),
        node::Node::CaseExpr(expr) => infer_case(expr, ctx, params),
        node::Node::SubLink(sub) => infer_sublink(sub, ctx, params),
        node::Node::ParamRef(p) => {
            if let Some(arg) = params.bound_arg(p.number) {
                return Ok(arg.clone());
            }
            params.see(p.number);
            // If the param is still untyped and the context provides a goal,
            // record the goal type — this is our equivalent of PG's
            // p_coerce_param_hook.
            if params.get(p.number) == oid::UNKNOWN && goal.has_expectation() {
                params.record(p.number, goal.type_oid);
            }
            let type_oid = params.get(p.number);
            Ok(ExprType::scalar(type_oid, params.read_nullable(p.number)))
        }
        node::Node::MinMaxExpr(mm) => {
            // `GREATEST`/`LEAST` are non-strict: they skip NULL args and
            // return NULL only when every arg is NULL. typedpg_pg_query's AST
            // doesn't fill in `minmaxtype` without full parse analysis —
            // we resolve the common type from the args and track per-arg
            // nullability.
            let label = match protobuf::MinMaxOp::try_from(mm.op) {
                Ok(protobuf::MinMaxOp::IsLeast) => "LEAST",
                _ => "GREATEST",
            };
            let mut args = Vec::with_capacity(mm.args.len());
            for arg in &mm.args {
                args.push(infer_expr(arg, ctx, params, TypeGoal::NONE)?);
            }
            let types: Vec<PgTypeOid> = args.iter().map(|t| t.type_oid).collect();
            let resolved_type = match PgTypeOid::new(mm.minmaxtype) {
                Some(t) if t != oid::UNKNOWN => t,
                _ => {
                    let nodes: Vec<&protobuf::Node> = mm.args.iter().collect();
                    select_common_type(label, &types, &nodes, snapshot)?
                }
            };
            // ExecInitExprRec looks up the type's btree comparison function
            // when the executor starts — every execution fails without one.
            if !crate::clause::has_ordering_operator(snapshot, resolved_type) {
                return Err(crate::pgmsg::no_comparison_function(
                    &crate::ddl::util::format_type_for_message(snapshot, resolved_type),
                )
                .finalize_implicit());
            }
            // Back-fill UNKNOWN args with the resolved common type so
            // embedded params get pinned and string-literal contents are
            // validated (PG rejects `GREATEST(1, 'x')` at parse time).
            for (arg, t) in mm.args.iter().zip(&args) {
                if t.type_oid == oid::UNKNOWN {
                    coerce_unknown_to(arg, ctx, params, resolved_type)?;
                }
            }
            // Each argument is coerced to the common type, which may map it
            // to NULL.
            let kept = conditional::coerce_branches(&mut args, resolved_type, snapshot);
            // GREATEST/LEAST over ≥1 NOT NULL arg are never NULL.
            let nullable = args.is_empty()
                || (args.iter().all(|t| t.nullable)
                    && !conditional::some_column_non_null(
                        conditional::kept_nodes(&mm.args, &kept),
                        ctx,
                    ));
            let typmod = agreed_typmod(&args, resolved_type);
            ctx.note_strict(
                mm.location,
                crate::nonnull::StrictNode::ExactFold,
                crate::nonnull::subst::folds_exactly(resolved_type, typmod, snapshot),
            );
            ctx.note_strict(
                mm.location,
                crate::nonnull::StrictNode::FloatFold,
                crate::nonnull::subst::folds_as_float(resolved_type, snapshot),
            );
            let branches: Vec<(&protobuf::Node, &ExprType)> = mm.args.iter().zip(&args).collect();
            let mut refine = conditional::branches_refine(&branches, resolved_type, snapshot);
            // GREATEST is at least each non-NULL argument (LEAST at most):
            // a NOT NULL one bounds it whatever the others are.
            if crate::refine::Exact::of(resolved_type, None, snapshot)
                == Some(crate::refine::Exact::Int)
            {
                let mut range = refine.range.unwrap_or_default();
                for a in args.iter().filter(|a| !a.nullable) {
                    let Some(b) = a.refine.int_bounds() else {
                        continue;
                    };
                    if label == "GREATEST" {
                        range.lo = range.lo.max(b.lo);
                    } else if let Some(hi) = b.hi {
                        range.hi = Some(range.hi.map_or(hi, |h| h.min(hi)));
                    }
                }
                refine.range = (range != crate::refine::IntRange::default()).then_some(range);
            }
            Ok(
                ExprType::scalar_with_typmod(resolved_type, nullable, typmod)
                    .with_collation(derive_collation(&args, resolved_type, snapshot)?)
                    .with_refine(refine),
            )
        }
        node::Node::AIndirection(ind) => {
            let t = infer_indirection(ind, ctx, params)?;
            // PG's coerce_type can implicitly convert an `unknown` literal or
            // parameter, but not an `unknown` field selected from an
            // anonymous record; only an explicit cast (via I/O) works.
            if t.type_oid == oid::UNKNOWN
                && goal.has_expectation()
                && goal.coercion != CoercionContext::Explicit
            {
                let target = crate::ddl::util::format_type_for_message(snapshot, goal.type_oid);
                return Err(crate::pgmsg::unknown_field_not_coercible(
                    &target,
                    crate::error::node_location(node)
                        .and_then(crate::error::SourceSpan::from_node_qname),
                )
                .finalize_implicit());
            }
            Ok(t)
        }
        node::Node::AArrayExpr(arr) => infer_array_expr(arr, ctx, params),
        node::Node::RowExpr(row) => {
            let expanded;
            let row: &protobuf::RowExpr = match expand_row_args(&row.args, ctx, params) {
                std::borrow::Cow::Borrowed(_) => row,
                std::borrow::Cow::Owned(args) => {
                    expanded = protobuf::RowExpr {
                        args,
                        ..(**row).clone()
                    };
                    &expanded
                }
            };
            // `ROW(a, b, …)` constructs an anonymous composite. The ROW
            // value itself is never NULL — empty `ROW()` still yields a
            // record.
            //
            // When the enclosing context expects a registered composite
            // type of matching arity (UPDATE composite_col = ROW(...) or
            // INSERT INTO t (composite_col) VALUES (ROW(...))), we type
            // each element against the composite's declared field type so
            // params get pinned correctly and the result adopts the goal's
            // OID — exactly what PG does in `coerce_record_to_complex`.
            // Otherwise the ROW types as the pseudo `record` with shape
            // captured statically so downstream operators/indirection can
            // see through.
            let composite_goal = if goal.has_expectation() {
                let target = snapshot.unwrap_domain(goal.type_oid);
                snapshot.get_type(target).and_then(|te| {
                    if te.typtype == TypType::Composite
                        && let Some(relid) = te.typrelid
                    {
                        let fields = snapshot.attributes_of(relid).to_vec();
                        if fields.len() == row.args.len() {
                            Some((target, fields))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                })
            } else {
                None
            };

            if let Some((composite_oid, composite_fields)) = composite_goal {
                let mut shape = Vec::with_capacity(row.args.len());
                for (i, (arg, field)) in row.args.iter().zip(composite_fields.iter()).enumerate() {
                    // coerce_record_to_complex: a field that doesn't
                    // coerce — in the context of the record's own coercion:
                    // explicit for a cast, assignment for a stored value —
                    // fails the whole record's cast, worded as such
                    // (42846), not as the field's own mismatch.
                    let mut scratch = params.clone();
                    let own = infer_expr(arg, ctx, &mut scratch, TypeGoal::NONE)?;
                    if own.type_oid != oid::UNKNOWN
                        && !crate::coerce::can_coerce(
                            own.type_oid,
                            field.atttypid,
                            goal.coercion,
                            snapshot,
                        )
                    {
                        let typname = snapshot
                            .get_type(composite_oid)
                            .map_or_else(String::new, |t| t.typname.clone());
                        return Err(crate::error::RawError::invalid(
                            format!("cannot cast type record to {typname}"),
                            crate::error::SourceSpan::from_node_token(row.location),
                            Some(format!(
                                "Cannot cast type {} to {} in column {}.",
                                crate::ddl::util::format_type_for_message(snapshot, own.type_oid),
                                crate::ddl::util::format_type_for_message(snapshot, field.atttypid),
                                i + 1,
                            )),
                        )
                        .with_primary_label("record value")
                        .finalize_implicit());
                    }
                    let t = infer_expr(
                        arg,
                        ctx,
                        params,
                        TypeGoal {
                            coercion: goal.coercion,
                            ..TypeGoal::assignment(field.atttypid)
                        },
                    )?;
                    // coerce_record_to_complex keeps each value, coerced to
                    // its field's type: still non-NULL when no cast runs
                    // (same type) or an untyped literal is read by the
                    // field type's input function. Any other cast may map a
                    // value to NULL (`int4(jsonb)` on a JSON null).
                    let literal = matches!(
                        arg.node.as_ref(),
                        Some(node::Node::AConst(c)) if !c.isnull
                    );
                    let kept =
                        own.type_oid == field.atttypid || (own.type_oid == oid::UNKNOWN && literal);
                    shape.push(RecordField {
                        name: field.attname.clone(),
                        ty: ExprType::scalar_with_typmod(
                            field.atttypid,
                            t.nullable || !kept,
                            field.atttypmod,
                        ),
                    });
                }
                // ROW value is never NULL; its fields' nullability is the
                // composite's shape (what `(ROW(a, b)::pair).x` reads).
                return Ok(ExprType {
                    record_fields: Some(shape.into()),
                    ..ExprType::scalar(composite_oid, false)
                });
            }

            // PG names anonymous ROW elements `f1`, `f2`, ... by position.
            // The element's full ExprType (with any nested record shape)
            // goes straight onto the field — recursion handled by ExprType.
            // For each bare `$N` element, mark the param as
            // indeterminate-required: PG refuses to default these to text
            // (`SELECT ROW($1)` raises `could not determine data type of
            // parameter $1`). The marker is harmless if a later inference
            // site (ROW=ROW back-fill, composite-cast pre-pass, …) pins
            // the param to a concrete type.
            let mut fields = Vec::with_capacity(row.args.len());
            for (i, arg) in row.args.iter().enumerate() {
                let ty = infer_expr(arg, ctx, params, TypeGoal::NONE)?;
                if let Some(node::Node::ParamRef(p)) = arg.node.as_ref() {
                    params.mark_indeterminate_required(p.number);
                }
                fields.push(RecordField {
                    name: format!("f{}", i + 1),
                    ty,
                });
            }
            Ok(ExprType {
                type_oid: oid::RECORD,
                nullable: false,
                typmod: None,
                collation: None,
                explicit_collation: false,
                record_fields: Some(fields.into()),
                elem_nullable: None,
                refine: crate::refine::Refinement::NONE,
            })
        }
        node::Node::SetToDefault(d) => {
            // `DEFAULT` is only meaningful as the whole value of an INSERT /
            // UPDATE target, which the DML analyzers handle before reaching
            // here; anywhere else PG's transformExprRecurse rejects it.
            Err(crate::error::RawError::new(
                AnalyzeError::SyntaxError("DEFAULT is not allowed in this context".into()),
                crate::error::SourceSpan::from_node_token(d.location),
                None,
            )
            .finalize_implicit())
        }
        node::Node::CollateClause(c) => {
            // `expr COLLATE "x"` is metadata-only — it changes how the
            // surrounding operator compares strings, not the result type or
            // nullability. Forward the goal so a `$param COLLATE "x"`
            // placeholder still picks up its expected type.
            let arg = c
                .arg
                .as_ref()
                .ok_or_else(|| AnalyzeError::Internal("CollateClause without arg".into()))?;
            // PG rejects unknown collation names up front; mirror that.
            // `collname` is a list of identifier nodes (`["pg_catalog", "C"]`
            // when fully qualified, just `["C"]` otherwise).
            let parts: Vec<&str> = c
                .collname
                .iter()
                .filter_map(|n| match n.node.as_ref()? {
                    node::Node::String(s) => Some(s.sval.as_str()),
                    _ => None,
                })
                .collect();
            let (schema, name) = match parts.as_slice() {
                [n] => (None, *n),
                [s, n] => (Some(*s), *n),
                _ => {
                    return Err(AnalyzeError::Invalid("malformed COLLATE clause".into()));
                }
            };
            let resolved_collation = if parts.is_empty() {
                None
            } else {
                if let Some(schema) = schema
                    && snapshot.namespace_oid(schema).is_none()
                {
                    return Err(
                        crate::pgmsg::schema_does_not_exist(schema, None).finalize_implicit()
                    );
                }
                let r = snapshot
                    .resolve_collation(schema, name)
                    .ok_or_else(|| crate::pgmsg::collation_does_not_exist(&parts.join(".")))?;
                Some(r.oid)
            };
            let result = infer_expr(arg, ctx, params, goal)?;
            // PG rejects `COLLATE` on non-collatable types with
            // `collations are not supported by type X`. Collatable means
            // string-category — or an *array* of a collatable element
            // (`tags COLLATE "C"` is valid; the collation applies to the
            // elements). Accept UNKNOWN (untyped literal/param) — the
            // parser already coerces it through the surrounding goal.
            if result.type_oid != oid::UNKNOWN {
                let base = snapshot.unwrap_domain(result.type_oid);
                let category_of = |t: PgTypeOid| {
                    snapshot
                        .get_type(t)
                        .map(|ty| ty.typcategory)
                        .unwrap_or(TypCategory::UserDefined)
                };
                let mut effective = base;
                if category_of(effective) == TypCategory::Array
                    && let Some(elem) = snapshot.get_type(effective).and_then(|t| t.typelem)
                {
                    effective = snapshot.unwrap_domain(elem);
                }
                let category = category_of(effective);
                if category != TypCategory::String {
                    // PG renders the bare type name here (search-path
                    // aware) — keep the SQL-standard aliases for built-ins
                    // (`int4` → `integer`) but drop the schema prefix for
                    // user types so a `public.address` column reads the
                    // same way PG would: `... by type address`.
                    let formatted = crate::ddl::util::format_type_for_message(snapshot, base);
                    let type_name = match formatted.rsplit_once('.') {
                        Some((_, bare)) => bare.to_owned(),
                        None => formatted,
                    };
                    let span = crate::error::node_location(arg)
                        .and_then(crate::error::SourceSpan::from_node_qname);
                    return Err(crate::error::RawError::invalid(
                        format!("collations are not supported by type {type_name}"),
                        span,
                        None,
                    )
                    .with_primary_label(format!("this is {type_name}, not a collatable type"))
                    .finalize_implicit());
                }
            }
            // Explicit COLLATE overrides whatever collation was inherited
            // from the inner expression (PG's "explicit" derivation tier).
            return Ok(ExprType {
                explicit_collation: resolved_collation.is_some(),
                ..ExprType::scalar_with_collation(
                    result.type_oid,
                    result.nullable,
                    result.typmod,
                    resolved_collation,
                )
            });
        }
        node::Node::SqlvalueFunction(svf) => {
            // SQL value functions: `CURRENT_DATE`, `CURRENT_TIMESTAMP`,
            // `CURRENT_USER`, `CURRENT_SCHEMA`, `LOCALTIME`, … typedpg_pg_query leaves
            // the result OID at 0 in the raw tree, so map the op ourselves
            // (PG's gram.y assigns these). All but CURRENT_SCHEMA are never NULL.
            use protobuf::SqlValueFunctionOp as Op;
            let op = protobuf::SqlValueFunctionOp::try_from(svf.op)
                .unwrap_or(Op::SqlvalueFunctionOpUndefined);
            let type_oid = match op {
                Op::SvfopCurrentDate => oid::DATE,
                Op::SvfopCurrentTime | Op::SvfopCurrentTimeN => oid::TIMETZ,
                Op::SvfopCurrentTimestamp | Op::SvfopCurrentTimestampN => oid::TIMESTAMPTZ,
                Op::SvfopLocaltime | Op::SvfopLocaltimeN => oid::TIME,
                Op::SvfopLocaltimestamp | Op::SvfopLocaltimestampN => oid::TIMESTAMP,
                Op::SvfopCurrentRole
                | Op::SvfopCurrentUser
                | Op::SvfopUser
                | Op::SvfopSessionUser
                | Op::SvfopCurrentCatalog
                | Op::SvfopCurrentSchema => oid::NAME,
                Op::SqlvalueFunctionOpUndefined => {
                    return Err(AnalyzeError::Unsupported(
                        "unknown SQL value function".into(),
                    ));
                }
            };
            // The `(n)` precision variants carry a typmod; the base type is
            // unchanged. Forward it so e.g. `current_time(3)` keeps its typmod.
            // PG's transformSQLValueFunction runs it through
            // `any{time,timestamp}_typmod_check`, which only warns above the
            // maximum and clamps (`CURRENT_TIMESTAMP(7)` is timestamptz(6)).
            let typmod =
                (svf.typmod >= 0).then_some(svf.typmod.min(crate::typmod::MAX_TIMESTAMP_PRECISION));
            // `CURRENT_SCHEMA` evaluates `current_schema()`, which is NULL
            // when no schema on the search path exists.
            let nullable = op == Op::SvfopCurrentSchema;
            // The current date and time are finite.
            Ok(ExprType::scalar_with_typmod(type_oid, nullable, typmod)
                .with_refine(crate::refine::Refinement::FINITE))
        }
        node::Node::MergeSupportFunc(f) => crate::resolve::infer_merge_support_func(f),
        // `WHERE CURRENT OF cursor` (UPDATE / DELETE only, by grammar): a
        // boolean test against the cursor's current row.
        node::Node::CurrentOfExpr(_) => Ok(ExprType::scalar(oid::BOOL, false)),
        node::Node::JsonFuncExpr(f) => infer_json_func_expr(f, ctx, params),
        node::Node::JsonParseExpr(p) => infer_json_parse(p, ctx, params),
        node::Node::JsonScalarExpr(s) => infer_json_scalar(s, ctx, params),
        node::Node::JsonSerializeExpr(s) => infer_json_serialize(s, ctx, params),
        node::Node::JsonObjectConstructor(c) => infer_json_object(c, ctx, params),
        node::Node::JsonArrayConstructor(c) => infer_json_array(c, ctx, params),
        node::Node::JsonArrayQueryConstructor(c) => infer_json_array_query(c, ctx, params),
        node::Node::JsonObjectAgg(a) => infer_json_objectagg(a, ctx, params),
        node::Node::JsonArrayAgg(a) => infer_json_arrayagg(a, ctx, params),
        node::Node::JsonIsPredicate(p) => infer_json_is_predicate(p, ctx, params),
        node::Node::XmlExpr(x) => infer_xml_expr(x, ctx, params),
        node::Node::XmlSerialize(xs) => infer_xml_serialize(xs, ctx, params),
        _ => Err(crate::error::RawError::unsupported(
            format!(
                "typedpg does not support {} expressions yet",
                crate::error::node_kind(inner)
            ),
            crate::error::expr_span(node),
            None,
        )
        .finalize_implicit()),
    }?;

    // PG runs the target type's input function on untyped string-literal
    // constants the moment a context coerces them to a concrete type
    // (`coerce_type` → `stringTypeDatum`), so `WHERE int_col = 'x'` fails at
    // parse time with `invalid input syntax for type integer: "x"`. Mirror
    // it: a string literal whose type stayed UNKNOWN meeting a concrete goal
    // gets its *content* validated here.
    if goal.has_expectation()
        && result.type_oid == oid::UNKNOWN
        && let Some(node::Node::AConst(ac)) = node.node.as_ref()
        && !ac.isnull
        && matches!(ac.val, Some(a_const::Val::Sval(_)))
    {
        ctx.note_literal_type(ac.location, goal.type_oid);
    }
    if goal.has_expectation()
        && result.type_oid == oid::UNKNOWN
        && let Some(node::Node::AConst(ac)) = node.node.as_ref()
        && !ac.isnull
        && let Some(a_const::Val::Sval(sv)) = &ac.val
        && let Err(msg) = crate::literal_input::validate_with_typmod(
            &sv.sval,
            goal.type_oid,
            goal.typmod,
            snapshot,
        )
    {
        let span =
            crate::error::node_location(node).and_then(crate::error::SourceSpan::from_node_token);
        return Err(crate::error::RawError::invalid_literal(msg, span).finalize_implicit());
    }

    // Verify result is compatible with the goal type. Pass the location
    // of the offending expression so a `TypeMismatch` carries a snippet.
    check_goal_compatibility(&result, &goal, snapshot, crate::error::node_location(node))?;

    Ok(result)
}

/// Filter for *speculative* re-inference sites (operator / CASE / COALESCE /
/// function-argument back-fills): most failures there just mean the candidate
/// goal didn't fit and are deliberately swallowed, but a literal-content
/// rejection is exactly the error PG itself raises from that coercion, so it
/// must survive — as must `coerce_type`'s failure on an `unknown` value
/// that is no literal ([`crate::pgmsg::unknown_field_not_coercible`]).
/// Returns `Err` only for those two.
fn swallow_unless_literal<T>(r: Result<T, AnalyzeError>) -> Result<(), AnalyzeError> {
    match r {
        Err(e @ (AnalyzeError::InvalidLiteral(_) | AnalyzeError::PgInternalError(_))) => Err(e),
        _ => Ok(()),
    }
}

/// PG's `coerce_type` for a pass-2 back-fill: an expression whose bottom-up
/// inference stayed UNKNOWN adopts the type its context resolved. A bare
/// `$N` is pinned to `target`, an untyped string literal has its *content*
/// validated against `target`'s input function (both via the goal-driven
/// re-walk through [`infer_expr`]), and any other shape is walked under the
/// goal so nested unknowns resolve the same way.
///
/// Re-walk failures other than a literal-content rejection are swallowed:
/// the walk is speculative (the enclosing construct owns its own error
/// reporting), but the literal rejection is exactly the parse-time error PG
/// raises from this coercion.
///
/// This is **the** primitive every two-pass construct (operator/function
/// arguments, CASE/COALESCE/GREATEST branches, ARRAY elements, VALUES
/// cells, set-operation projections, …) must use for its back-fill —
/// open-coded goal walks are how parameters historically ended up typed
/// differently from PG's Describe.
pub(crate) fn coerce_unknown_to(
    node: &protobuf::Node,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
    target: PgTypeOid,
) -> Result<(), AnalyzeError> {
    swallow_unless_literal(infer_expr(node, ctx, params, TypeGoal::implicit(target)))
}

/// The bare `$N` output columns of a SELECT list that PG transforms while
/// `$N` is still untyped. PG transforms the target list right after FROM
/// (`transformSelectStmt`), left to right, so a parameter first typed by
/// WHERE, GROUP BY, HAVING, ORDER BY — or by a *later* target entry — is
/// still `unknown` when a bare `$N` entry is transformed, and that entry
/// stays `unknown` until `resolveTargetListUnknowns`. The analyzer walks
/// those clauses in a different order, so this replays the target list on a
/// scratch collector at PG's point (the caller runs it right after FROM) and
/// records each such occurrence ([`ParamCollector::mark_untyped_output`]);
/// [`resolve_untyped_output_params`] then applies PG's coercion to text.
///
/// An entry that fails to resolve there fails in PG before any later
/// clause is looked at: its error is returned, so a query wrong in both
/// its select list and its WHERE reports the select list's, as PG does.
pub(crate) fn note_untyped_output_params(
    target_list: &[protobuf::Node],
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
    let mut scratch = params.clone();
    for target in target_list {
        let Some(node::Node::ResTarget(rt)) = target.node.as_ref() else {
            continue;
        };
        let Some(val) = rt.val.as_deref() else {
            continue;
        };
        if let Some(node::Node::ParamRef(p)) = val.node.as_ref() {
            if scratch.get(p.number) == oid::UNKNOWN {
                params.mark_untyped_output(p.location);
            }
            scratch.see(p.number);
            continue;
        }
        // `*` / `t.*` / `(expr).*` expand rather than resolve as one
        // expression; the final pass handles them.
        let star = |fields: &[protobuf::Node]| {
            fields
                .iter()
                .any(|f| matches!(f.node.as_ref(), Some(node::Node::AStar(_))))
        };
        match val.node.as_ref() {
            Some(node::Node::ColumnRef(c)) if star(&c.fields) => continue,
            Some(node::Node::AIndirection(i)) if star(&i.indirection) => continue,
            _ => {}
        }
        infer_expr(val, ctx, &mut scratch, TypeGoal::NONE)?;
    }
    Ok(())
}

/// PG's `resolveTargetListUnknowns` for the bare-parameter output columns
/// [`note_untyped_output_params`] recorded: each such occurrence is coerced
/// to `text` through `variable_coerce_param_hook`, which fails with
/// `inconsistent types deduced for parameter $N` when a later clause already
/// deduced another type (`SELECT $1 FROM t WHERE id = $1`). Only for the
/// contexts that resolve unknowns to text — a top-level SELECT, a subquery,
/// a sublink or a CTE body; set-operation arms and `INSERT … SELECT` coerce
/// their unknowns to another type instead.
pub(crate) fn resolve_untyped_output_params(
    sel: &protobuf::SelectStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
    if sel.op != protobuf::SetOperation::SetopNone as i32 || !sel.values_lists.is_empty() {
        return Ok(());
    }
    for target in &sel.target_list {
        let Some(node::Node::ResTarget(rt)) = target.node.as_ref() else {
            continue;
        };
        let Some(node::Node::ParamRef(p)) = rt.val.as_deref().and_then(|v| v.node.as_ref()) else {
            continue;
        };
        if !params.is_untyped_output(p.location) {
            continue;
        }
        if let Err(deduced) = params.coerce_untyped(p.number, oid::TEXT) {
            return Err(inconsistent_param_error(
                p.number,
                deduced,
                oid::TEXT,
                p.location,
                snapshot,
            ));
        }
    }
    Ok(())
}

/// [`crate::pgmsg::inconsistent_parameter_types`] with PG's type names,
/// pointing at the parameter occurrence being coerced.
pub(crate) fn inconsistent_param_error(
    num: i32,
    deduced: PgTypeOid,
    target: PgTypeOid,
    location: i32,
    snapshot: &PgCatalog,
) -> AnalyzeError {
    crate::pgmsg::inconsistent_parameter_types(
        num,
        &crate::ddl::util::format_type_for_message(snapshot, deduced),
        &crate::ddl::util::format_type_for_message(snapshot, target),
        crate::error::SourceSpan::from_node_token(location),
    )
    .finalize_implicit()
}

thread_local! {
    static PLAN_TIME_CHECKS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `f` analyzing a statement that will be *planned and executed* (an
/// application query), as opposed to DDL that only stores an expression
/// (a view, a default, a function body): the checks for errors the
/// planner raises on every execution — constant folding, selectivity
/// estimation — apply only here.
pub(crate) fn with_plan_time_checks<R>(f: impl FnOnce() -> R) -> R {
    let prev = PLAN_TIME_CHECKS.with(|c| c.replace(true));
    let out = f();
    PLAN_TIME_CHECKS.with(|c| c.set(prev));
    out
}

/// Whether [`with_plan_time_checks`] is in effect.
pub(crate) fn plan_time_checks() -> bool {
    PLAN_TIME_CHECKS.with(std::cell::Cell::get)
}

/// PG's `transformExpressionList` for a `ROW(...)` constructor's arguments:
/// a `rel.*` column reference expands to one reference per column of `rel`
/// (`ExpandColumnRefStar`) and a `(expr).*` indirection to one field
/// selection per field (`ExpandIndirectionStar`), so `ROW(t.*)` is
/// `ROW(t.a, t.b, …)`, not a one-field record holding the whole row.
/// Borrowed when no argument is a star.
pub(crate) fn expand_row_args<'a>(
    args: &'a [protobuf::Node],
    ctx: Ctx<'_>,
    params: &ParamCollector,
) -> std::borrow::Cow<'a, [protobuf::Node]> {
    let ends_in_star = |fields: &[protobuf::Node]| {
        matches!(
            fields.last().and_then(|f| f.node.as_ref()),
            Some(node::Node::AStar(_))
        )
    };
    let is_star = |a: &protobuf::Node| match a.node.as_ref() {
        Some(node::Node::ColumnRef(cr)) => ends_in_star(&cr.fields),
        Some(node::Node::AIndirection(ind)) => ends_in_star(&ind.indirection),
        _ => false,
    };
    if !args.iter().any(is_star) {
        return std::borrow::Cow::Borrowed(args);
    }
    let string = |s: &str| protobuf::Node {
        node: Some(node::Node::String(protobuf::String { sval: s.to_owned() })),
    };
    let mut out = Vec::with_capacity(args.len());
    for arg in args {
        match arg.node.as_ref() {
            Some(node::Node::ColumnRef(cr)) if ends_in_star(&cr.fields) => {
                // The relation is named by the last qualifier (`s.t.*` → t);
                // an unknown one stays as written so the column-reference
                // walk reports PG's error for it.
                let rel = cr.fields.iter().rev().find_map(|f| match f.node.as_ref()? {
                    node::Node::String(s) => Some(s.sval.as_str()),
                    _ => None,
                });
                match rel.and_then(|r| ctx.scope.find_source(r)) {
                    Some(src) => out.extend(src.columns.iter().map(|c| protobuf::Node {
                        node: Some(node::Node::ColumnRef(protobuf::ColumnRef {
                            fields: vec![string(&src.alias), string(&c.name)],
                            location: cr.location,
                        })),
                    })),
                    None => out.push(arg.clone()),
                }
            }
            Some(node::Node::AIndirection(ind)) if ends_in_star(&ind.indirection) => {
                let mut scratch = params.clone();
                match expand_indirection_star(ind, ctx, &mut scratch) {
                    Ok(Some(fields)) => {
                        let prefix = &ind.indirection[..ind.indirection.len() - 1];
                        out.extend(fields.iter().map(|(name, _)| {
                            protobuf::Node {
                                node: Some(node::Node::AIndirection(Box::new(
                                    protobuf::AIndirection {
                                        arg: ind.arg.clone(),
                                        indirection: prefix
                                            .iter()
                                            .cloned()
                                            .chain(std::iter::once(string(name)))
                                            .collect(),
                                    },
                                ))),
                            }
                        }));
                    }
                    _ => out.push(arg.clone()),
                }
            }
            _ => out.push(arg.clone()),
        }
    }
    std::borrow::Cow::Owned(out)
}

// ──────────────────────────────────────────────────────────────────────────────
// Goal compatibility check
// ──────────────────────────────────────────────────────────────────────────────

/// Verify that `result` can be coerced to `goal` under the allowed coercion
/// context.  Returns `Ok(())` when:
/// - There is no goal expectation (`goal.type_oid == UNKNOWN`).
/// - The result is `UNKNOWN` (untyped literals / unresolved params coerce to
///   anything, per SQL spec).
/// - The types match (after domain unwrapping).
/// - A registered cast exists at the required coercion level.
fn check_goal_compatibility(
    result: &ExprType,
    goal: &TypeGoal,
    snapshot: &PgCatalog,
    location: Option<i32>,
) -> Result<(), AnalyzeError> {
    if !goal.has_expectation() {
        return Ok(());
    }
    if result.type_oid == oid::UNKNOWN {
        return Ok(());
    }
    if result.type_oid == goal.type_oid {
        return Ok(());
    }
    if can_coerce(result.type_oid, goal.type_oid, goal.coercion, snapshot) {
        return Ok(());
    }
    // PG uses a distinct wording when the source is the pseudo `record`
    // type and the target is a registered composite (e.g. assigning
    // `ROW($p1, $p2)` to an `address` column with the wrong arity). Mirror
    // it so pg_sanity's prefix check passes — the rest of the cases keep
    // the generic `cannot coerce` form.
    if result.type_oid == oid::RECORD
        && let Some(target_te) = snapshot.get_type(goal.type_oid)
        && target_te.typtype == TypType::Composite
    {
        // PG renders the bare composite name (search-path aware) here, not
        // the schema-qualified form `format_type_for_message` would produce.
        let span = location.and_then(crate::error::SourceSpan::from_node_qname);
        return Err(crate::error::RawError::invalid(
            format!("cannot cast type record to {}", target_te.typname),
            span,
            Some(format!(
                "the ROW(...) shape doesn't match `{}` — check the field count and types",
                target_te.typname
            )),
        )
        .with_primary_label("record value")
        .finalize_implicit());
    }
    // PG's user-facing type names (e.g. `int4` → `integer`, `bool` →
    // `boolean`) — these appear verbatim in the message so the sanity
    // prefix match works.
    let actual_pg = crate::ddl::util::format_type_for_message(snapshot, result.type_oid);
    let expected_pg = crate::ddl::util::format_type_for_message(snapshot, goal.type_oid);

    // Internal short forms kept around so the introspection-style
    // `actual`/`expected` fields on `TypeMismatch` still carry the OID's
    // type name (what tests/macros use).
    let actual = type_display_name(result.type_oid, snapshot);
    let expected = type_display_name(goal.type_oid, snapshot);

    // PG-verbatim message. When the expectation comes from a named column
    // (INSERT VALUES, UPDATE SET), use PG's exact wording so pg_sanity
    // passes; otherwise fall back to the generic `cannot coerce` form.
    let context = match &goal.source_col_name {
        Some(col) => format!(
            "column \"{col}\" is of type {expected_pg} but expression is of type {actual_pg}"
        ),
        None => format!("cannot coerce {actual_pg} to {expected_pg}"),
    };

    let primary_span = location.and_then(|loc| {
        // `from_node_token` covers identifiers, numeric literals, and
        // quoted strings — TypeMismatch can fire on any of those.
        crate::error::SourceSpan::from_node_token(loc)
            .or_else(|| crate::error::SourceSpan::from_location(loc))
    });

    // When the goal carries a `source_span` (the column being assigned, the
    // operand setting the expectation, …), surface it as a secondary label
    // so the diagnostic shows both sides.
    let secondary = goal
        .source_span
        .map(|s| crate::error::DiagnosticLabel::new(s, format!("expected {expected_pg} here")));

    // transformAssignedExpr's hint, for the column wording.
    let hint = goal
        .source_col_name
        .as_ref()
        .map(|_| crate::pgmsg::REWRITE_OR_CAST_HINT.to_owned());
    let err = crate::error::RawError::type_mismatch(
        actual,
        expected,
        &actual_pg,
        &expected_pg,
        context,
        primary_span,
        secondary,
        hint,
    );
    Err(
        crate::pgmsg::with_explicit_cast_note(err, snapshot, result.type_oid, goal.type_oid)
            .finalize_implicit(),
    )
}

fn type_display_name(oid: PgTypeOid, snapshot: &PgCatalog) -> String {
    snapshot
        .get_type(oid)
        .map(|t| t.typname.clone())
        .unwrap_or_else(|| format!("oid:{}", oid.get()))
}

/// PG's `get_base_element_type`: the element type of a (domain over a)
/// true array type — one with a `typelem` and the array subscript handler.
/// That includes `_record` (category P) but not `point` / `name`, whose
/// `typelem` makes them subscriptable without being arrays.
pub(crate) fn array_element_type(snapshot: &PgCatalog, t: PgTypeOid) -> Option<PgTypeOid> {
    let base = snapshot.unwrap_domain(t);
    let entry = snapshot.get_type(base)?;
    let elem = entry.typelem?;
    (entry.typcategory == TypCategory::Array || snapshot.array_type_of(elem) == Some(base))
        .then_some(elem)
}

/// Return `text` when `node` is an untyped string literal (`'x'`) and its
/// inferred type is still UNKNOWN. Used by constructs that need to treat a
/// bare string constant as text for type-compatibility checks — NULLIF,
/// CASE / COALESCE / ARRAY[...] branch merging, UNION column reconciliation.
pub(crate) fn unknown_literal_as_text(
    node: Option<&protobuf::Node>,
    inferred_oid: PgTypeOid,
) -> PgTypeOid {
    if inferred_oid != oid::UNKNOWN {
        return inferred_oid;
    }
    let is_string_literal = node.is_some_and(|n| {
        matches!(
            n.node.as_ref(),
            Some(node::Node::AConst(ac))
                if !ac.isnull && matches!(ac.val, Some(a_const::Val::Sval(_)))
        )
    });
    if is_string_literal {
        oid::TEXT
    } else {
        oid::UNKNOWN
    }
}

mod column_refs;
mod conditional;
mod func_call;
mod indirection;
pub(crate) mod inline;
mod json;
pub(crate) mod literals;
pub(crate) mod operators;
mod sublink;
mod xml;

use column_refs::*;
pub(crate) use column_refs::{
    SqlFunctionParams, check_column_ref_length, with_sql_function_params,
};
pub(crate) use conditional::failing_input_span;
use conditional::*;
use func_call::*;
pub(crate) use func_call::{
    aggregate_reads_rows, arg_coercions_nullable, backfill_call_args, check_window_clause,
    effective_frame_options,
};
use indirection::*;
pub(crate) use indirection::{expand_indirection_star, transform_container_subscripts};
use json::*;
pub(crate) use literals::assignment_nullable;
use literals::*;
pub(crate) use literals::{coercion_can_null_elements, coercion_can_return_null};
pub(crate) use operators::check_regex_restrictions;
use operators::*;
use sublink::*;
use xml::*;

// ──────────────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Extract string values from a list of nodes.
pub(crate) fn extract_string_fields(nodes: &[protobuf::Node]) -> Vec<String> {
    nodes
        .iter()
        .filter_map(|n| match n.node.as_ref()? {
            node::Node::String(s) => Some(s.sval.clone()),
            _ => None,
        })
        .collect()
}

/// PG's `DeconstructQualifiedName` over an already-split name (a function
/// or type name): `name`, `schema.name`, or `catalog.schema.name` — whose
/// catalog must be the current database (see
/// [`crate::pgmsg::cross_database_reference`]); anything longer is an
/// improper qualified name.
pub(crate) fn deconstruct_qualified_name(
    parts: &[String],
    span: Option<crate::error::SourceSpan>,
) -> Result<(Option<&str>, &str), AnalyzeError> {
    match parts {
        [name] => Ok((None, name.as_str())),
        [schema, name] => Ok((Some(schema.as_str()), name.as_str())),
        [_, _, _] => {
            Err(crate::pgmsg::cross_database_reference(&parts.join("."), span).finalize_implicit())
        }
        _ => Err(crate::pgmsg::improper_qualified_name(&parts.join("."), span).finalize_implicit()),
    }
}

/// Resolve a TypeName to a type OID.
fn resolve_type_name(
    type_name: Option<&protobuf::TypeName>,
    snapshot: &PgCatalog,
) -> Result<PgTypeOid, AnalyzeError> {
    let tn = type_name.ok_or_else(|| AnalyzeError::Unsupported("missing TypeName".into()))?;

    if let Some(oid) = PgTypeOid::new(tn.type_oid) {
        return Ok(oid);
    }

    let parts = extract_string_fields(&tn.names);
    let (schema, name) = deconstruct_qualified_name(&parts, None)?;

    let is_array = !tn.array_bounds.is_empty();

    // LookupTypeName: a qualified name's schema must exist first.
    if let Some(schema) = schema
        && snapshot.namespace_oid(schema).is_none()
    {
        return Err(crate::pgmsg::schema_does_not_exist(schema, None).finalize_implicit());
    }

    let type_entry = snapshot.resolve_type_by_name(schema, name).ok_or_else(|| {
        // Build a snippet + "did you mean" hint for the unknown type name.
        let hint = crate::suggest::suggest_similar(name, snapshot.visible_type_names(schema))
            .map(|c| format!("did you mean \"{c}\"?"));
        let span = crate::error::SourceSpan::from_node_qname(tn.location);
        let qualified = parts.join(".");
        crate::error::RawError {
            kind: AnalyzeError::UndefinedType(format!("type \"{qualified}\" does not exist")),
            primary: span.map(|s| crate::error::DiagnosticLabel::new(s, "type does not exist")),
            secondaries: Vec::new(),
            notes: Vec::new(),
            hint,
        }
        .finalize_implicit()
    })?;

    crate::ddl::depend::note(crate::ddl::depend::ObjectAddress::type_(type_entry.oid));
    // typenameType: a shell type can't be used.
    if !type_entry.typisdefined {
        return Err(AnalyzeError::UndefinedType(format!(
            "type \"{}\" is only a shell",
            parts.join(".")
        )));
    }

    // The array type is the element's `typarray`, whatever its name
    // (makeArrayTypeName can't always use `_name`).
    if is_array && let Some(arr_oid) = snapshot.array_type_of(type_entry.oid) {
        return Ok(arr_oid);
    }

    Ok(type_entry.oid)
}
