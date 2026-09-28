//! PostgreSQL polymorphic pseudo-type handling (`anyelement`,
//! `anyarray`, `anyrange`, `anycompatible`, …): matching actual args
//! against a pseudo-type, binding the concrete types implied by a call,
//! and substituting them back into a result type.
//!
//! Shared by both function resolution ([`crate::functions`]) and operator
//! resolution ([`crate::lookup`]) — PG applies the same rules to both.

use crate::oid::PgTypeOid;
use crate::pg_catalog::{PgCatalog, TypCategory, TypType, oid};

// Polymorphic pseudo-type OIDs (stable across PG versions).
const ANYELEMENT: PgTypeOid = PgTypeOid::from_raw(2283);
const ANYARRAY: PgTypeOid = PgTypeOid::from_raw(2277);
const ANYNONARRAY: PgTypeOid = PgTypeOid::from_raw(2776);
const ANYENUM: PgTypeOid = PgTypeOid::from_raw(3500);
const ANYRANGE: PgTypeOid = PgTypeOid::from_raw(3831);
const ANYMULTIRANGE: PgTypeOid = PgTypeOid::from_raw(4537);
const ANYCOMPATIBLE: PgTypeOid = PgTypeOid::from_raw(5077);
const ANYCOMPATIBLEARRAY: PgTypeOid = PgTypeOid::from_raw(5078);
const ANYCOMPATIBLENONARRAY: PgTypeOid = PgTypeOid::from_raw(5079);
const ANYCOMPATIBLERANGE: PgTypeOid = PgTypeOid::from_raw(5080);
const ANYCOMPATIBLEMULTIRANGE: PgTypeOid = PgTypeOid::from_raw(4538);

/// The `provariadic` element type of a parameter declared `VARIADIC t`, as
/// PG's `CreateFunction` derives it: the element of an array type, with the
/// polymorphic arrays mapping to their element pseudo-types and `"any"`
/// standing for itself. `None` when `t` is not an array — PG rejects that
/// declaration (`VARIADIC parameter must be an array`).
pub(crate) fn variadic_element_type(
    declared: PgTypeOid,
    snapshot: &PgCatalog,
) -> Option<PgTypeOid> {
    const ANY: PgTypeOid = PgTypeOid::from_raw(2276);
    match declared {
        ANYARRAY => Some(ANYELEMENT),
        ANYCOMPATIBLEARRAY => Some(ANYCOMPATIBLE),
        ANY => Some(ANY),
        _ => snapshot
            .get_type(declared)
            .filter(|t| t.typcategory == TypCategory::Array)
            .and_then(|t| t.typelem),
    }
}

/// The concrete types a polymorphic call binds, one slot per pseudo-type
/// family. Binding any slot derives the related ones where the catalog
/// knows the relation (array → element, range → subtype element,
/// multirange → range → element), mirroring PG's
/// `enforce_generic_type_consistency`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PolyBindings {
    pub element: Option<PgTypeOid>,
    pub array: Option<PgTypeOid>,
    pub range: Option<PgTypeOid>,
    pub multirange: Option<PgTypeOid>,
}

/// Is `expected` a polymorphic pseudo-type that PG would accept `actual` for?
pub(crate) fn matches_polymorphic(
    expected: PgTypeOid,
    actual: PgTypeOid,
    snapshot: &PgCatalog,
) -> bool {
    const INT2VECTOR: PgTypeOid = PgTypeOid::from_raw(22);
    const OIDVECTOR: PgTypeOid = PgTypeOid::from_raw(30);

    let actual_is_array = matches!(
        snapshot.get_type(actual).map(|t| t.typcategory),
        Some(TypCategory::Array)
    ) || actual == INT2VECTOR
        || actual == OIDVECTOR;

    match expected {
        ANYELEMENT | ANYCOMPATIBLE => true,
        ANYARRAY | ANYCOMPATIBLEARRAY => actual_is_array,
        ANYNONARRAY | ANYCOMPATIBLENONARRAY => !actual_is_array,
        ANYENUM => matches!(
            snapshot.get_type(actual).map(|t| t.typtype),
            Some(TypType::Enum)
        ),
        ANYRANGE | ANYCOMPATIBLERANGE => matches!(
            snapshot.get_type(actual).map(|t| t.typtype),
            Some(TypType::Range)
        ),
        ANYMULTIRANGE | ANYCOMPATIBLEMULTIRANGE => matches!(
            snapshot.get_type(actual).map(|t| t.typtype),
            Some(TypType::Multirange)
        ),
        _ => false,
    }
}

pub(crate) fn is_polymorphic(oid: PgTypeOid) -> bool {
    matches!(
        oid,
        ANYELEMENT
            | ANYARRAY
            | ANYNONARRAY
            | ANYENUM
            | ANYRANGE
            | ANYMULTIRANGE
            | ANYCOMPATIBLE
            | ANYCOMPATIBLEARRAY
            | ANYCOMPATIBLENONARRAY
            | ANYCOMPATIBLERANGE
            | ANYCOMPATIBLEMULTIRANGE
    )
}

pub(crate) fn polymorphic_specificity(oid: PgTypeOid) -> u8 {
    match oid {
        ANYELEMENT | ANYCOMPATIBLE => 1,
        ANYARRAY | ANYNONARRAY | ANYCOMPATIBLEARRAY | ANYCOMPATIBLENONARRAY => 2,
        ANYENUM | ANYRANGE | ANYMULTIRANGE | ANYCOMPATIBLERANGE | ANYCOMPATIBLEMULTIRANGE => 3,
        _ => 10,
    }
}

pub(crate) fn bind_polymorphic_from(
    expected: PgTypeOid,
    actual: PgTypeOid,
    snapshot: &PgCatalog,
    bindings: &mut PolyBindings,
) {
    // PG (`enforce_generic_type_consistency`) skips UNKNOWN actuals when
    // unifying polymorphic args — the unknown is coerced to the resolved
    // polymorphic type *after* binding, so letting it land as the bound
    // type would freeze the resolution at UNKNOWN and propagate to the
    // other side (e.g. `'admin'::unknown = role::user_role` would resolve
    // both anyenum slots to UNKNOWN instead of `user_role`).
    if actual == oid::UNKNOWN {
        return;
    }
    match expected {
        ANYELEMENT | ANYNONARRAY | ANYENUM | ANYCOMPATIBLE | ANYCOMPATIBLENONARRAY => {
            bindings.element.get_or_insert(actual);
        }
        ANYARRAY | ANYCOMPATIBLEARRAY => {
            bindings.array.get_or_insert(actual);
            if let Some(t) = snapshot.get_type(actual)
                && t.typcategory == TypCategory::Array
                && let Some(elem) = t.typelem
            {
                bindings.element.get_or_insert(elem);
            }
        }
        ANYRANGE | ANYCOMPATIBLERANGE => {
            bindings.range.get_or_insert(actual);
            if let Some(sub) = snapshot.range_subtype(actual) {
                bindings.element.get_or_insert(sub);
            }
        }
        ANYMULTIRANGE | ANYCOMPATIBLEMULTIRANGE => {
            bindings.multirange.get_or_insert(actual);
            if let Some(r) = snapshot.range_of_multirange(actual) {
                bindings.range.get_or_insert(r);
                if let Some(sub) = snapshot.range_subtype(r) {
                    bindings.element.get_or_insert(sub);
                }
            }
        }
        _ => {}
    }
}

pub(crate) fn substitute_polymorphic(
    oid: PgTypeOid,
    bindings: &PolyBindings,
    snapshot: &PgCatalog,
) -> PgTypeOid {
    match oid {
        ANYELEMENT | ANYNONARRAY | ANYENUM | ANYCOMPATIBLE | ANYCOMPATIBLENONARRAY => {
            bindings.element.unwrap_or(oid)
        }
        ANYARRAY | ANYCOMPATIBLEARRAY => bindings
            .array
            .or_else(|| bindings.element.and_then(|e| snapshot.array_type_of(e)))
            .unwrap_or(oid),
        ANYRANGE | ANYCOMPATIBLERANGE => bindings.range.unwrap_or(oid),
        ANYMULTIRANGE | ANYCOMPATIBLEMULTIRANGE => bindings
            .multirange
            .or_else(|| bindings.range.and_then(|r| snapshot.multirange_of_range(r)))
            .unwrap_or(oid),
        _ => oid,
    }
}

/// PG's `enforce_generic_type_consistency` for the strict (non-
/// `anycompatible`) family: once the call's bindings are known, a
/// *concrete* actual at a polymorphic position must be the resolved type
/// itself or implicitly castable to it. This is what rejects
/// `tstzrange @> 1` (anyelement resolves to `timestamptz`, no implicit
/// `integer` cast) while keeping `array_position(int8[], int4)` (implicit
/// int4 → int8). The `anycompatible*` family instead promotes through
/// `select_common_type` and is not checked here. UNKNOWN actuals are
/// always consistent — they're coerced to the resolved type afterwards.
fn binding_consistent(
    expected: PgTypeOid,
    actual: PgTypeOid,
    bindings: &PolyBindings,
    snapshot: &PgCatalog,
) -> bool {
    if actual == oid::UNKNOWN {
        return true;
    }
    match expected {
        ANYELEMENT | ANYNONARRAY | ANYENUM | ANYARRAY | ANYRANGE | ANYMULTIRANGE => {
            let resolved = substitute_polymorphic(expected, bindings, snapshot);
            resolved == actual
                || (resolved != expected && snapshot.has_implicit_cast(actual, resolved))
        }
        _ => true,
    }
}

/// Bind every polymorphic position of a call and verify the strict-family
/// consistency rule across all of them. Returns the bindings on success,
/// `None` when some concrete actual contradicts the resolution.
pub(crate) fn unify_polymorphic_call(
    declared: &[PgTypeOid],
    actuals: &[PgTypeOid],
    snapshot: &PgCatalog,
) -> Option<PolyBindings> {
    let mut bindings = PolyBindings::default();
    for (&expected, &actual) in declared.iter().zip(actuals.iter()) {
        if is_polymorphic(expected) {
            bind_polymorphic_from(expected, actual, snapshot, &mut bindings);
        }
    }
    for (&expected, &actual) in declared.iter().zip(actuals.iter()) {
        if is_polymorphic(expected) && !binding_consistent(expected, actual, &bindings, snapshot) {
            return None;
        }
    }
    Some(bindings)
}
