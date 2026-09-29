//! Type coercion and common-type resolution.

use crate::oid::PgTypeOid;
use crate::pg_catalog::{CastContext, CastMethod, PgCatalog, TypCategory, oid};

/// Describes the level of implicit coercion allowed in a given context.
///
/// Mirrors PostgreSQL's `CoercionContext` enum in `primnodes.h`.
/// - `Implicit`: only casts registered as implicit in `pg_cast` are allowed
///   (used inside operator/function argument matching).
/// - `Assignment`: implicit **and** assignment casts are allowed
///   (used for INSERT/UPDATE target columns, WHERE, LIMIT, OFFSET —
///   matches PG's `COERCION_ASSIGNMENT`).
/// - `Explicit`: every `pg_cast` entry plus I/O conversion from a string
///   type (a `CAST`, or a function-style `typename(x)`); only consulted by
///   [`coercion_pathway`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CoercionContext {
    Implicit,
    Assignment,
    Explicit,
}

/// Check whether a cast from `source` to `target` is permitted under
/// the given coercion context — PG's `can_coerce_type` for one concrete
/// pair, i.e. whether [`coercion_pathway`] finds a path.
///
/// Domains are smashed to their base types on *both* sides, which makes two
/// distinct domains over the same base coercible to each other. That
/// matches PG (verified on 18): `find_coercion_pathway` reduces the source
/// domain to its base, and coercing the base *to* the target domain is
/// "allowed whenever a coercion to the base type would be". Beyond the
/// `pg_cast` rows this includes element-wise array coercion (`int[]` to
/// `numeric[]` implicitly, `numeric[]` to `int[]` on assignment) and, from
/// assignment up, the I/O conversion to a string-category target
/// (`UPDATE t SET text_col = 780`).
pub(crate) fn can_coerce(
    source: PgTypeOid,
    target: PgTypeOid,
    context: CoercionContext,
    snapshot: &PgCatalog,
) -> bool {
    source == target || coercion_pathway(target, source, context, snapshot).is_some()
}

/// `pg_type.typcategory` of `t` — PG's `TypeCategory`: `unknown` is
/// category `X`, a type missing from the catalog has none.
pub(crate) fn type_category(t: PgTypeOid, snapshot: &PgCatalog) -> Option<TypCategory> {
    snapshot.get_type(t).map(|ty| ty.typcategory)
}

/// `(typcategory, typispreferred)` of `t` — PG's
/// `get_type_category_preferred`.
pub(crate) fn type_category_preferred(
    t: PgTypeOid,
    snapshot: &PgCatalog,
) -> (Option<TypCategory>, bool) {
    match snapshot.get_type(t) {
        Some(ty) => (Some(ty.typcategory), ty.typispreferred),
        None => (None, false),
    }
}

/// Element type of a *true* array type — PG's `get_element_type`, which
/// requires `typelem` and the array subscript handler (category `A`: the
/// `_foo` arrays plus `int2vector`/`oidvector`, but not `name` or `point`,
/// whose `typelem` only drives raw subscripting).
pub(crate) fn element_type(t: PgTypeOid, snapshot: &PgCatalog) -> Option<PgTypeOid> {
    snapshot
        .get_type(t)
        .filter(|ty| ty.typcategory == TypCategory::Array)
        .and_then(|ty| ty.typelem)
}

/// PG's `ISCOMPLEX` (`typeOrDomainTypeRelid`): a composite type, or a
/// domain over one.
pub(crate) fn is_complex(t: PgTypeOid, snapshot: &PgCatalog) -> bool {
    snapshot
        .get_type(snapshot.unwrap_domain(t))
        .is_some_and(|ty| ty.typrelid.is_some())
}

/// The kind of coercion [`coercion_pathway`] found — PG's
/// `CoercionPathType` (minus `COERCION_PATH_NONE`, which is `None`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CoercionPath {
    /// Binary-compatible relabeling (also domain <-> base).
    Relabel,
    /// A `pg_cast` function.
    Func,
    /// Element-wise coercion of an array.
    ArrayCoerce,
    /// Output/input function round-trip.
    CoerceViaIo,
}

/// PG's `find_coercion_pathway` (parse_coerce.c): domains are smashed to
/// their base types (a domain always coerces to and from its base), a
/// `pg_cast` row counts when its context is allowed, and — only without a
/// `pg_cast` row — an element-wise array coercion or the I/O conversion
/// (to a string type from assignment up, from one when explicit) applies.
pub(crate) fn coercion_pathway(
    target: PgTypeOid,
    source: PgTypeOid,
    context: CoercionContext,
    snapshot: &PgCatalog,
) -> Option<CoercionPath> {
    let source = snapshot.unwrap_domain(source);
    let target = snapshot.unwrap_domain(target);
    if source == target {
        return Some(CoercionPath::Relabel);
    }
    if let Some(cast) = snapshot
        .cast_by_pair
        .get(&(source, target))
        .and_then(|oid| snapshot.pg_cast.get(oid))
    {
        let allowed = match cast.castcontext {
            CastContext::Implicit => true,
            CastContext::Assignment => context != CoercionContext::Implicit,
            CastContext::Explicit => context == CoercionContext::Explicit,
        };
        return allowed.then_some(match cast.castmethod {
            CastMethod::Function => CoercionPath::Func,
            CastMethod::Binary => CoercionPath::Relabel,
            CastMethod::InOut => CoercionPath::CoerceViaIo,
        });
    }
    const INT2VECTOR: PgTypeOid = PgTypeOid::from_raw(22);
    const OIDVECTOR: PgTypeOid = PgTypeOid::from_raw(30);
    if target != OIDVECTOR
        && target != INT2VECTOR
        && let (Some(te), Some(se)) = (
            element_type(target, snapshot),
            element_type(source, snapshot),
        )
        && coercion_pathway(te, se, context, snapshot).is_some()
    {
        return Some(CoercionPath::ArrayCoerce);
    }
    let io = (context != CoercionContext::Implicit
        && type_category(target, snapshot) == Some(TypCategory::String))
        || (context == CoercionContext::Explicit
            && type_category(source, snapshot) == Some(TypCategory::String));
    io.then_some(CoercionPath::CoerceViaIo)
}

/// PG's `can_coerce_type` (parse_coerce.c) under `COERCION_IMPLICIT`: can
/// every `inputs[i]` be coerced to `targets[i]`? Unknown inputs coerce to
/// anything, `"any"` accepts anything, polymorphic targets are accepted
/// position-wise and then cross-checked together by
/// [`crate::polymorphic::check_generic_type_consistency`], and `record`
/// relabels to and from composites.
pub(crate) fn can_coerce_types(
    inputs: &[PgTypeOid],
    targets: &[PgTypeOid],
    snapshot: &PgCatalog,
) -> bool {
    const ANY: PgTypeOid = PgTypeOid::from_raw(2276);
    const RECORDARRAY: PgTypeOid = PgTypeOid::from_raw(2287);
    let mut have_generics = false;
    for (&input, &target) in inputs.iter().zip(targets) {
        if input == target || target == ANY {
            continue;
        }
        if crate::polymorphic::is_polymorphic(target) {
            have_generics = true;
            continue;
        }
        if input == oid::UNKNOWN
            || coercion_pathway(target, input, CoercionContext::Implicit, snapshot).is_some()
            || (input == oid::RECORD && is_complex(target, snapshot))
            || (target == oid::RECORD && is_complex(input, snapshot))
            || (target == RECORDARRAY
                && element_type(input, snapshot).is_some_and(|e| is_complex(e, snapshot)))
            || type_inherits_from(input, target, snapshot)
        {
            continue;
        }
        return false;
    }
    !have_generics || crate::polymorphic::check_generic_type_consistency(inputs, targets, snapshot)
}

/// PG's `typeInheritsFrom`: `sub` is the row type of a table that inherits
/// (transitively) from the table whose row type is `sup`.
fn type_inherits_from(sub: PgTypeOid, sup: PgTypeOid, snapshot: &PgCatalog) -> bool {
    let relid = |t: PgTypeOid| snapshot.get_type(t).and_then(|ty| ty.typrelid);
    let (Some(sub_rel), Some(sup_rel)) = (relid(sub), relid(sup)) else {
        return false;
    };
    let mut frontier = vec![sub_rel];
    let mut seen = Vec::new();
    while let Some(rel) = frontier.pop() {
        for inh in snapshot.pg_inherits.iter().filter(|i| i.inhrelid == rel) {
            if inh.inhparent == sup_rel {
                return true;
            }
            if !seen.contains(&inh.inhparent) {
                seen.push(inh.inhparent);
                frontier.push(inh.inhparent);
            }
        }
    }
    false
}

/// PG's `select_common_type_from_oids` (parse_coerce.c) in `noerror` mode:
/// the common supertype of `types`, or `None` when two inputs fall in
/// different type categories. Identical inputs keep their type (domains
/// included); otherwise domains are smashed and the running choice moves
/// to a later type only when it isn't preferred and the implicit cast is
/// one-way. All-unknown resolves to `text`.
pub(crate) fn select_common_type_from_oids(
    types: &[PgTypeOid],
    snapshot: &PgCatalog,
) -> Option<PgTypeOid> {
    let first = *types.first()?;
    let mut i = 1;
    if first != oid::UNKNOWN {
        while i < types.len() && types[i] == first {
            i += 1;
        }
        if i == types.len() {
            return Some(first);
        }
    }
    let mut ptype = snapshot.unwrap_domain(first);
    let (mut pcategory, mut pispreferred) = type_category_preferred(ptype, snapshot);
    for &t in &types[i..] {
        let ntype = snapshot.unwrap_domain(t);
        if ntype == oid::UNKNOWN || ntype == ptype {
            continue;
        }
        let (ncategory, nispreferred) = type_category_preferred(ntype, snapshot);
        if ptype == oid::UNKNOWN {
            (ptype, pcategory, pispreferred) = (ntype, ncategory, nispreferred);
        } else if ncategory != pcategory {
            return None;
        } else if !pispreferred
            && can_coerce_types(&[ptype], &[ntype], snapshot)
            && !can_coerce_types(&[ntype], &[ptype], snapshot)
        {
            (ptype, pcategory, pispreferred) = (ntype, ncategory, nispreferred);
        }
    }
    Some(if ptype == oid::UNKNOWN {
        oid::TEXT
    } else {
        ptype
    })
}

/// PG's `verify_common_type_from_oids`: every input coerces implicitly to
/// `common`.
pub(crate) fn verify_common_type_from_oids(
    common: PgTypeOid,
    types: &[PgTypeOid],
    snapshot: &PgCatalog,
) -> bool {
    types
        .iter()
        .all(|&t| t == common || can_coerce_types(&[t], &[common], snapshot))
}

/// Whether an *explicit* cast (`x::T` / `CAST(x AS T)`) from `source` to
/// `target` is legal, mirroring PostgreSQL's `can_coerce_type` under
/// `COERCION_EXPLICIT`. Deliberately more permissive than [`can_coerce`]: in
/// addition to any registered `pg_cast` entry, PG allows an explicit I/O cast
/// whenever either side is a string-category type, and relabels freely between
/// domains/base, composites/record, and element-castable arrays.
///
/// Errs toward allowing: it returns `false` only for clear-cut scalar
/// refusals (e.g. `boolean → double precision`), so callers never reject a
/// cast PG would have accepted.
pub(crate) fn can_cast_explicit(
    source: PgTypeOid,
    target: PgTypeOid,
    snapshot: &PgCatalog,
) -> bool {
    if source == target {
        return true;
    }
    let s = snapshot.unwrap_domain(source);
    let t = snapshot.unwrap_domain(target);
    if s == t {
        return true;
    }
    // Untyped literals (`unknown`) coerce to anything; a pseudo `any`-style
    // target accepts anything.
    if s == oid::UNKNOWN || t == oid::UNKNOWN {
        return true;
    }
    // Any registered cast — implicit, assignment, explicit, or
    // binary-coercible — makes it legal (`cast_by_pair` is keyed by pair,
    // independent of context).
    if snapshot.cast_by_pair.contains_key(&(s, t)) {
        return true;
    }
    let scat = snapshot.get_type(s).map(|ty| ty.typcategory);
    let tcat = snapshot.get_type(t).map(|ty| ty.typcategory);
    // Explicit I/O cast: PG allows casting to or from any string-category type.
    if scat == Some(TypCategory::String) || tcat == Some(TypCategory::String) {
        return true;
    }
    // Pseudo-types (incl. `record`, `any*`) and unknowns cast freely — they're
    // resolved structurally and PG accepts almost anything to/from them.
    let pseudo_or_unknown =
        |cat: Option<TypCategory>| matches!(cat, Some(TypCategory::Pseudo | TypCategory::Unknown));
    if pseudo_or_unknown(scat) || pseudo_or_unknown(tcat) {
        return true;
    }
    // Casting *to* a composite is allowed from `record` (a pseudo source,
    // handled above), a string type (handled above), or the row type of a
    // table inheriting from the target's (`typeInheritsFrom`, a
    // ConvertRowtypeExpr) — never from an unrelated composite or a scalar:
    // `(u.*)::t` and `numeric::some_composite` are `cannot cast type X to
    // Y` (can_coerce_type has no other composite rule and pg_cast no rows).
    if tcat == Some(TypCategory::Composite) {
        return scat == Some(TypCategory::Composite) && type_inherits_from(s, t, snapshot);
    }
    // A composite *source* to a non-composite target is rare; err toward
    // allowing (composite→record/text are already covered above).
    if scat == Some(TypCategory::Composite) {
        return true;
    }
    // Array → array: legal when the element types are themselves castable.
    if scat == Some(TypCategory::Array) && tcat == Some(TypCategory::Array) {
        return match (
            snapshot.get_type(s).and_then(|ty| ty.typelem),
            snapshot.get_type(t).and_then(|ty| ty.typelem),
        ) {
            (Some(se), Some(te)) => can_cast_explicit(se, te, snapshot),
            _ => true,
        };
    }
    false
}

/// Find the common supertype for a list of types.
///
/// Used for CASE, COALESCE, UNION, ARRAY, and VALUES column reconciliation.
///
/// Mirrors PG's `select_common_type` (parse_coerce.c): start from the first
/// concrete type and switch the running candidate to a later type only when
/// the candidate isn't its category's preferred type and the implicit cast
/// between them is *one-way* (candidate → next but not back). The
/// directionality matters: `varchar` then `text` keeps **varchar** (the casts
/// are bidirectional), while `int4` then `int8` promotes to **int8**. A
/// category mismatch — or a survivor some input can't implicitly reach —
/// yields `None`, which callers render as PG's "X and Y cannot be matched".
pub(crate) fn find_common_type(types: &[PgTypeOid], snapshot: &PgCatalog) -> Option<PgTypeOid> {
    select_common_type(types, snapshot).ok()
}

/// Why [`select_common_type`] found no common type — the two ways PG fails.
#[derive(Debug, Clone, Copy)]
pub(crate) enum CommonTypeError {
    /// Two inputs of different categories (base types): select_common_type's
    /// own `X types A and B cannot be matched`.
    Mismatch(PgTypeOid, PgTypeOid),
    /// Every category agrees, but input `from` has no implicit cast to the
    /// selected type `to`: PG picks `to` and the later coerce_to_common_type
    /// fails with `X could not convert type A to B`.
    CannotConvert { from: PgTypeOid, to: PgTypeOid },
}

/// [`find_common_type`] with PG's failure detail.
pub(crate) fn select_common_type(
    types: &[PgTypeOid],
    snapshot: &PgCatalog,
) -> Result<PgTypeOid, CommonTypeError> {
    if types.is_empty() {
        return Err(CommonTypeError::Mismatch(oid::UNKNOWN, oid::UNKNOWN));
    }

    // PG's first pass: when *every* input — unknowns/NULLs included — is the
    // exact same type, keep it as-is. This is the only path that preserves a
    // domain: `COALESCE(d, d)` is `d`.
    if types[0] != oid::UNKNOWN && types.iter().all(|&t| t == types[0]) {
        return Ok(types[0]);
    }

    let concrete: Vec<PgTypeOid> = types
        .iter()
        .copied()
        .filter(|&t| t != oid::UNKNOWN)
        .collect();
    if concrete.is_empty() {
        return Ok(oid::TEXT);
    }

    // Any mixed input — even just a NULL alongside a single domain — goes
    // through PG's main loop, which smashes every input to its base type
    // up front (`getBaseType`): `COALESCE(d, NULL)` is the *base*, and the
    // "cannot be matched" wording reports base names. Verified on PG 18.
    let concrete: Vec<PgTypeOid> = concrete
        .iter()
        .map(|&t| snapshot.unwrap_domain(t))
        .collect();

    if concrete.iter().all(|&t| t == concrete[0]) {
        return Ok(concrete[0]);
    }

    let category = |t: PgTypeOid| snapshot.get_type(t).map(|ty| ty.typcategory);
    let preferred = |t: PgTypeOid| snapshot.get_type(t).is_some_and(|ty| ty.typispreferred);

    let mut ptype = concrete[0];
    let pcategory = category(ptype).ok_or(CommonTypeError::Mismatch(ptype, ptype))?;
    for &n in &concrete[1..] {
        if n == ptype {
            continue;
        }
        if category(n) != Some(pcategory) {
            return Err(CommonTypeError::Mismatch(ptype, n));
        }
        // can_coerce_type under COERCION_IMPLICIT — `pg_cast` rows plus
        // element-wise array coercion (`integer[]` → `numeric[]`).
        if !preferred(ptype)
            && can_coerce_types(&[ptype], &[n], snapshot)
            && !can_coerce_types(&[n], &[ptype], snapshot)
        {
            ptype = n;
        }
    }

    // PG defers this check to the per-value coercion step (callers that only
    // need a yes/no fold it into `None`; see `CommonTypeError`).
    match concrete
        .iter()
        .find(|&&t| t != ptype && !can_coerce_types(&[t], &[ptype], snapshot))
    {
        None => Ok(ptype),
        Some(&from) => Err(CommonTypeError::CannotConvert { from, to: ptype }),
    }
}
