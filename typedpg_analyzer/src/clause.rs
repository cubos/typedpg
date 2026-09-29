//! Clause-context tracking — the analyzer's analogue of PostgreSQL's
//! `ParseExprKind`.
//!
//! Every SQL position that coerces an expression to a specific type carries
//! a [`ClauseKind`]: it owns the PG-verbatim wording (`argument of WHERE
//! must be type boolean, not type X`), the coercion target, and whether
//! aggregate/window calls are forbidden there. [`coerce_clause_expr`] is the
//! one walker implementing PG's error ordering for all of them:
//!
//! 1. bottom-up resolution failures win (`function … does not exist` from
//!    inside the expression beats any clause-level complaint);
//! 2. the aggregate/window placement rule fires next (`aggregate functions
//!    are not allowed in WHERE` outranks the boolean complaint for
//!    `WHERE min(id)`);
//! 3. the clause's own coercion wording comes last.
//!
//! Before this module the rewrite existed in six hand-rolled copies (WHERE/
//! HAVING/JOIN ON, LIMIT/OFFSET, FILTER, CASE/WHEN, NOT/AND/OR, IS TRUE…) —
//! each a chance to diverge on ordering or wording.

use typedpg_pg_query::protobuf;

use crate::error::AnalyzeError;
use crate::expr::{self, Ctx, TypeGoal};
use crate::oid::PgTypeOid;
use crate::param_collector::ParamCollector;
use crate::pg_catalog::{PgCatalog, oid};

/// The clause (or clause-like construct) coercing an expression. Mirrors the
/// distinctions PG's `ParseExprKind` draws for error wording and placement
/// rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClauseKind {
    Where,
    Having,
    JoinOn,
    Filter,
    Limit,
    Offset,
    /// A searched CASE's WHEN condition.
    CaseWhen,
    /// One operand of NOT / AND / OR (PG names the operator in the message).
    Not,
    And,
    Or,
    /// `IS [NOT] TRUE/FALSE/UNKNOWN` — the payload is PG's spelling of the
    /// test (`"IS TRUE"`, …).
    BoolTest(&'static str),
    /// A ROWS / GROUPS window frame offset (the payload names the frame
    /// mode) — transformFrameOffset coerces it to bigint like LIMIT.
    FrameOffset(&'static str),
}

impl ClauseKind {
    /// PG's name for the construct, as it appears in `argument of {label}
    /// must be type …`.
    fn label(self) -> &'static str {
        match self {
            ClauseKind::Where => "WHERE",
            ClauseKind::Having => "HAVING",
            ClauseKind::JoinOn => "JOIN/ON",
            ClauseKind::Filter => "FILTER",
            ClauseKind::Limit => "LIMIT",
            ClauseKind::Offset => "OFFSET",
            ClauseKind::CaseWhen => "CASE/WHEN",
            ClauseKind::Not => "NOT",
            ClauseKind::And => "AND",
            ClauseKind::Or => "OR",
            ClauseKind::BoolTest(label) => label,
            ClauseKind::FrameOffset(mode) => mode,
        }
    }

    /// The type the clause coerces its expression to, with PG's name for it
    /// in the message.
    fn expected(self) -> (PgTypeOid, &'static str) {
        match self {
            ClauseKind::Limit | ClauseKind::Offset | ClauseKind::FrameOffset(_) => {
                (oid::INT8, "bigint")
            }
            _ => (oid::BOOL, "boolean"),
        }
    }

    /// `Some(context)` when PG forbids aggregate/window calls in this
    /// position (`aggregate functions are not allowed in {context}`). The
    /// expression-level kinds (CASE/WHEN, NOT, …) return `None`: the
    /// enclosing clause owns that rule.
    fn aggregate_context(self) -> Option<&'static str> {
        match self {
            ClauseKind::Where => Some("WHERE"),
            // PG's ParseExprKindName for EXPR_KIND_JOIN_ON.
            ClauseKind::JoinOn => Some("JOIN conditions"),
            ClauseKind::Limit => Some("LIMIT"),
            ClauseKind::Offset => Some("OFFSET"),
            ClauseKind::FrameOffset("ROWS") => Some("window ROWS"),
            ClauseKind::FrameOffset(_) => Some("window GROUPS"),
            _ => None,
        }
    }

    /// `Some(context)` when PG forbids window calls in this position but
    /// allows aggregates (`window functions are not allowed in HAVING`).
    fn window_only_context(self) -> Option<&'static str> {
        match self {
            ClauseKind::Having => Some("HAVING"),
            _ => None,
        }
    }

    /// Whether this is a clause of its query level (as opposed to an
    /// expression inside one): an aggregate belonging to the level that a
    /// sublink in the clause holds is checked against it.
    fn is_level_clause(self) -> bool {
        matches!(
            self,
            ClauseKind::Where
                | ClauseKind::Having
                | ClauseKind::JoinOn
                | ClauseKind::Limit
                | ClauseKind::Offset
                | ClauseKind::FrameOffset(_)
        )
    }

    /// Whether the diagnostic carries a caret label under the offending
    /// expression. (LIMIT/OFFSET historically render bare; the boolean
    /// clauses annotate.)
    fn caret_label(self) -> bool {
        !matches!(
            self,
            ClauseKind::Limit | ClauseKind::Offset | ClauseKind::FrameOffset(_)
        )
    }
}

/// Coerce a clause expression to the kind's expected type with PG's error
/// ordering and wording. See the module docs for the three-step order.
pub(crate) fn coerce_clause_expr(
    node: &protobuf::Node,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
    kind: ClauseKind,
) -> Result<expr::ExprType, AnalyzeError> {
    let (goal_oid, goal_name) = kind.expected();
    // Sublinks analyzed inside a clause of the level hand the aggregates
    // they hold for the level to the clause's placement rule.
    let inferred = if kind.is_level_clause() {
        crate::grouping::with_clause(kind.aggregate_context(), || {
            expr::infer_expr(node, ctx, params, TypeGoal::assignment(goal_oid))
        })
    } else {
        expr::infer_expr(node, ctx, params, TypeGoal::assignment(goal_oid))
    };
    let placement = || -> Result<(), AnalyzeError> {
        if let Some(c) = kind.aggregate_context() {
            check_level_calls(node, ctx, c, true)?;
        }
        if let Some(c) = kind.window_only_context() {
            check_level_calls(node, ctx, c, false)?;
        }
        Ok(())
    };
    let e = match inferred {
        Ok(t) => {
            placement()?;
            if matches!(kind, ClauseKind::Where | ClauseKind::JoinOn) {
                expr::check_regex_restrictions(node, ctx)?;
            }
            return Ok(t);
        }
        Err(e) => e,
    };
    if !matches!(e, AnalyzeError::TypeMismatch { .. }) {
        return Err(e);
    }
    // The expression resolved but isn't the expected type — placement still
    // outranks the coercion complaint (`WHERE min(id)` is "aggregate
    // functions are not allowed in WHERE", not "argument of WHERE…").
    placement()?;
    // Re-infer with no goal (on a scratch collector) to learn the actual
    // type for the message.
    let mut params2 = params.clone();
    let actual_oid = expr::infer_expr(node, ctx, &mut params2, TypeGoal::NONE)
        .map(|t| t.type_oid)
        .unwrap_or(oid::UNKNOWN);
    let actual_pg = crate::ddl::util::format_type_for_message(ctx.snapshot, actual_oid);
    let span =
        crate::error::node_location(node).and_then(crate::error::SourceSpan::from_node_qname);
    let raw = crate::error::RawError::new(
        AnalyzeError::DatatypeMismatch(format!(
            "argument of {} must be type {goal_name}, not type {actual_pg}",
            kind.label()
        )),
        span,
        None,
    );
    let raw = if kind.caret_label() {
        raw.with_primary_label(format!("this is {actual_pg}, expected {goal_name}"))
    } else {
        raw
    };
    Err(raw.finalize_implicit())
}

/// PG's placement rules (check_agglevels_and_constraints,
/// transformWindowFuncCall) for the calls of the current level in `node`:
/// with `aggregates`, aggregate and GROUPING calls are rejected as well as
/// window calls, else only window calls. An aggregate belonging to an outer
/// level (its arguments only reference that level) is that level's
/// business.
pub(crate) fn check_level_calls(
    node: &protobuf::Node,
    ctx: Ctx<'_>,
    context: &str,
    aggregates: bool,
) -> Result<(), AnalyzeError> {
    let calls = crate::grouping::level_calls(node, ctx.scope, ctx.snapshot);
    if aggregates {
        if let Some(loc) = calls.aggregate {
            return Err(aggregate_not_allowed(context, Some(loc)));
        }
        if let Some(loc) = calls.grouping {
            return Err(grouping_not_allowed(context, Some(loc)));
        }
    }
    if let Some(loc) = calls.window {
        return Err(window_not_allowed(context, Some(loc)));
    }
    Ok(())
}

/// `aggregate functions are not allowed in {context}` (42803).
pub(crate) fn aggregate_not_allowed(context: &str, location: Option<i32>) -> AnalyzeError {
    // The classic fix for an aggregate in WHERE is a HAVING clause; for the
    // other clauses there's no single rewrite, so just point at the call.
    let hint = (context == "WHERE")
        .then(|| "to filter on an aggregate, use a HAVING clause instead of WHERE".to_string());
    crate::error::RawError::new(
        AnalyzeError::GroupingError(format!("aggregate functions are not allowed in {context}")),
        location.and_then(crate::error::SourceSpan::from_node_qname),
        hint,
    )
    .with_primary_label("aggregate not allowed here")
    .finalize_implicit()
}

/// `grouping operations are not allowed in {context}` (42803).
pub(crate) fn grouping_not_allowed(context: &str, location: Option<i32>) -> AnalyzeError {
    crate::error::RawError::new(
        AnalyzeError::GroupingError(format!("grouping operations are not allowed in {context}")),
        location.and_then(crate::error::SourceSpan::from_node_qname),
        None,
    )
    .with_primary_label("GROUPING not allowed here")
    .finalize_implicit()
}

/// `window functions are not allowed in {context}` (42P20).
fn window_not_allowed(context: &str, location: Option<i32>) -> AnalyzeError {
    crate::error::RawError::new(
        AnalyzeError::WindowingError(format!("window functions are not allowed in {context}")),
        location.and_then(crate::error::SourceSpan::from_node_qname),
        None,
    )
    .with_primary_label("window function not allowed here")
    .finalize_implicit()
}

/// Reject aggregate / window function calls in a context where PG forbids
/// them. Matches PG's `aggregate functions are not allowed in WHERE` /
/// `window functions are not allowed in WHERE` errors. `context` goes into
/// the error message (e.g. `"WHERE"`, `"GROUP BY"`, `"JOIN/ON"`). Blind to
/// query levels — [`check_level_calls`] is the level-aware form.
pub(crate) fn check_no_aggregates_or_windows(
    node: &protobuf::Node,
    snapshot: &PgCatalog,
    context: &str,
) -> Result<(), AnalyzeError> {
    let kinds = expr::detect_func_kinds(node, snapshot);
    if kinds.has_aggregate {
        return Err(aggregate_not_allowed(context, kinds.agg_location));
    }
    if kinds.has_grouping {
        return Err(grouping_not_allowed(context, kinds.grouping_location));
    }
    if kinds.has_window {
        return Err(window_not_allowed(context, kinds.window_location));
    }
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────────
// Sort / group keys
// ──────────────────────────────────────────────────────────────────────────────

/// Whether `t` has an ordering operator: PG's typcache `TYPECACHE_LT_OPR`,
/// the `<` of its default btree operator class — which an array or a
/// composite only has when its element / every field has one too.
pub(crate) fn has_ordering_operator(snapshot: &PgCatalog, t: PgTypeOid) -> bool {
    type_has_operators(snapshot, t, false, 0)
}

/// Whether `t` has an equality operator: PG's typcache `TYPECACHE_EQ_OPR`,
/// from its default btree operator class or else its default hash one
/// (element- / field-wise for arrays and composites).
pub(crate) fn has_equality_operator(snapshot: &PgCatalog, t: PgTypeOid) -> bool {
    type_has_operators(snapshot, t, true, 0)
}

fn type_has_operators(snapshot: &PgCatalog, t: PgTypeOid, equality: bool, depth: u32) -> bool {
    let t = snapshot.unwrap_domain(t);
    // An unknown literal becomes text; an anonymous record is only
    // checked field by field at run time.
    if depth > 32 || t == oid::UNKNOWN || t == oid::RECORD {
        return true;
    }
    let Some(ty) = snapshot.get_type(t) else {
        return true;
    };
    if ty.typcategory == crate::pg_catalog::TypCategory::Array
        && let Some(elem) = ty.typelem
    {
        return type_has_operators(snapshot, elem, equality, depth + 1);
    }
    if ty.typtype == crate::pg_catalog::TypType::Composite
        && let Some(relid) = ty.typrelid
    {
        return snapshot
            .attributes_of(relid)
            .iter()
            .filter(|a| a.attnum > 0)
            .all(|a| type_has_operators(snapshot, a.atttypid, equality, depth + 1));
    }
    let has = |am| crate::ddl::opclass::default_opclass_intype(snapshot, t, am).is_some();
    has("btree") || (equality && has("hash"))
}

/// How a sort / group key is used, which decides the operators
/// get_sort_group_operators requires of its type.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyUse {
    /// ORDER BY (the query's, a window's or an aggregate's): `<`.
    Sort,
    /// GROUP BY, DISTINCT [ON], PARTITION BY, set operations: `=`.
    Group,
    /// An aggregate's DISTINCT argument: `=`, and `<` to sort its input.
    AggDistinct,
}

/// get_sort_group_operators for a key of type `t`.
pub(crate) fn check_key_operators(
    snapshot: &PgCatalog,
    t: PgTypeOid,
    usage: KeyUse,
    location: Option<i32>,
) -> Result<(), AnalyzeError> {
    let span = location.and_then(crate::error::SourceSpan::from_node_qname);
    let name = || crate::ddl::util::format_type_for_message(snapshot, t);
    if matches!(usage, KeyUse::Group | KeyUse::AggDistinct) && !has_equality_operator(snapshot, t) {
        return Err(crate::pgmsg::no_equality_operator(&name(), span).finalize_implicit());
    }
    if matches!(usage, KeyUse::Sort | KeyUse::AggDistinct) && !has_ordering_operator(snapshot, t) {
        return Err(
            crate::pgmsg::no_ordering_operator(&name(), usage == KeyUse::Sort, span)
                .finalize_implicit(),
        );
    }
    Ok(())
}

/// PG's collation strengths (parse_collate.c), weakest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Strength {
    None,
    Implicit,
    Conflict,
    Explicit,
}

/// An expression's collation state: its collation and strength, and for a
/// conflict the second collation.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CollationState {
    strength: Strength,
    collation: Option<crate::oid::PgCollationOid>,
    other: Option<crate::oid::PgCollationOid>,
}

impl CollationState {
    const NONE: CollationState = CollationState {
        strength: Strength::None,
        collation: None,
        other: None,
    };

    /// The state of a value of type `t` carrying `collation` (its type's
    /// when `None`), explicitly (`COLLATE`) or not.
    fn of_value(
        snapshot: &PgCatalog,
        t: PgTypeOid,
        collation: Option<crate::oid::PgCollationOid>,
        explicit: bool,
    ) -> Self {
        let Some(type_collation) = snapshot.get_type(t).and_then(|ty| ty.typcollation) else {
            return Self::NONE;
        };
        CollationState {
            strength: if explicit {
                Strength::Explicit
            } else {
                Strength::Implicit
            },
            collation: Some(collation.unwrap_or(type_collation)),
            other: None,
        }
    }

    /// PG's merge_collation_state: fold a sibling's state into this one.
    fn merge(&mut self, s: CollationState) {
        if s.strength > self.strength {
            *self = s;
        } else if s.strength == self.strength
            && s.strength == Strength::Implicit
            && s.collation != self.collation
        {
            if self.collation == Some(DEFAULT_COLLATION) {
                *self = s;
            } else if s.collation != Some(DEFAULT_COLLATION) {
                self.strength = Strength::Conflict;
                self.other = s.collation;
            }
        }
    }

    /// The state PG's select_common_collation merges for two values: a
    /// set operation's column from its two arms.
    pub(crate) fn merged(mut self, other: CollationState) -> CollationState {
        self.merge(other);
        self
    }

    /// A column value of type `t` with collation `collation` (implicit
    /// unless `explicit`).
    pub(crate) fn column(
        snapshot: &PgCatalog,
        t: PgTypeOid,
        collation: Option<crate::oid::PgCollationOid>,
        explicit: bool,
    ) -> Self {
        Self::of_value(snapshot, t, collation, explicit)
    }

    /// The collation, when explicit.
    pub(crate) fn explicit(self) -> Option<crate::oid::PgCollationOid> {
        (self.strength == Strength::Explicit)
            .then_some(self.collation)
            .flatten()
    }

    /// The resolved collation, `None` for a conflict or no collation.
    pub(crate) fn collation(self) -> Option<crate::oid::PgCollationOid> {
        match self.strength {
            Strength::Implicit | Strength::Explicit => self.collation,
            _ => None,
        }
    }

    /// `collation mismatch between implicit collations …` when the state
    /// is a conflict.
    pub(crate) fn check_determinate(
        self,
        snapshot: &PgCatalog,
        location: Option<i32>,
    ) -> Result<(), AnalyzeError> {
        if self.strength != Strength::Conflict {
            return Ok(());
        }
        let name = |c: Option<crate::oid::PgCollationOid>| {
            c.and_then(|c| snapshot.pg_collation.get(&c))
                .map(|c| c.collname.clone())
                .unwrap_or_default()
        };
        Err(crate::pgmsg::collation_mismatch_implicit(
            &name(self.collation),
            &name(self.other),
            location.and_then(crate::error::SourceSpan::from_node_qname),
        )
        .finalize_implicit())
    }
}

/// PG's `DEFAULT_COLLATION_OID`.
const DEFAULT_COLLATION: crate::oid::PgCollationOid = crate::oid::PgCollationOid::from_raw(100);

/// PG's assign_collations_walker for one expression of the current level:
/// explicit (`COLLATE`) beats implicit, a non-default implicit collation
/// beats the default, and two different non-default implicit ones are a
/// conflict that bubbles up through every node whose result is
/// collatable. Types come from inference on scratch parameters.
pub(crate) fn collation_state(
    node: &protobuf::Node,
    ctx: Ctx<'_>,
    params: &ParamCollector,
) -> CollationState {
    let infer = |n: &protobuf::Node| {
        let mut scratch = params.clone();
        expr::infer_expr(n, ctx, &mut scratch, TypeGoal::NONE).ok()
    };
    if let Some(protobuf::node::Node::CollateClause(_)) = node.node.as_ref() {
        return infer(node).map_or(CollationState::NONE, |t| {
            CollationState::of_value(ctx.snapshot, t.type_oid, t.collation, true)
        });
    }
    let children = crate::resolve::expr_children(node);
    if children.is_empty() {
        return infer(node).map_or(CollationState::NONE, |t| {
            CollationState::of_value(ctx.snapshot, t.type_oid, t.collation, t.explicit_collation)
        });
    }
    let mut merged = CollationState::NONE;
    for c in children {
        merged.merge(collation_state(c, ctx, params));
    }
    match infer(node) {
        // A node whose result isn't collatable absorbs its inputs' state.
        Some(t) => match ctx
            .snapshot
            .get_type(t.type_oid)
            .and_then(|ty| ty.typcollation)
        {
            None => CollationState::NONE,
            Some(_) if merged.strength > Strength::None => merged,
            Some(tc) => CollationState {
                strength: Strength::Implicit,
                collation: Some(tc),
                other: None,
            },
        },
        None => merged,
    }
}
