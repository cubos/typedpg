//! PostgreSQL polymorphic pseudo-type handling (`anyelement`,
//! `anyarray`, `anyrange`, `anycompatible`, …): PG's
//! `check_generic_type_consistency` (does a candidate's set of polymorphic
//! parameters accept the call's actual types?) and
//! `enforce_generic_type_consistency` (resolve them to concrete types, or
//! fail), both from parse_coerce.c.
//!
//! Shared by both function resolution ([`crate::functions`]) and operator
//! resolution ([`crate::lookup`]) — PG applies the same rules to both.

use crate::coerce::{element_type, select_common_type_from_oids, verify_common_type_from_oids};
use crate::error::AnalyzeError;
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

/// PG's `type_is_array_domain`: an array, or a domain over one.
fn type_is_array_domain(t: PgTypeOid, snapshot: &PgCatalog) -> bool {
    element_type(snapshot.unwrap_domain(t), snapshot).is_some()
}

/// PG's `type_is_enum` (a domain over an enum is *not* an enum).
fn type_is_enum(t: PgTypeOid, snapshot: &PgCatalog) -> bool {
    snapshot
        .get_type(t)
        .is_some_and(|ty| ty.typtype == TypType::Enum)
}

/// PG's `check_generic_type_consistency` (parse_coerce.c): do the call's
/// actual types satisfy the candidate's polymorphic parameters *jointly*?
/// Family-1 (`anyelement`, `anyarray`, `anyrange`, …) positions must agree
/// on one element type exactly — no implicit casts; family-2
/// (`anycompatible*`) positions must have a common supertype every input
/// casts to. Unknown actuals impose nothing, but an `anyenum` that only
/// sees unknowns fails (no element type is an enum).
pub(crate) fn check_generic_type_consistency(
    actuals: &[PgTypeOid],
    declared: &[PgTypeOid],
    snapshot: &PgCatalog,
) -> bool {
    let base = |t: PgTypeOid| snapshot.unwrap_domain(t);
    let mut elem: Option<PgTypeOid> = None;
    let mut array: Option<PgTypeOid> = None;
    let mut range: Option<PgTypeOid> = None;
    let mut multirange: Option<PgTypeOid> = None;
    let mut compat_range: Option<(PgTypeOid, PgTypeOid)> = None;
    let mut compat_multirange: Option<(PgTypeOid, PgTypeOid)> = None;
    let mut have_anynonarray = false;
    let mut have_anyenum = false;
    let mut have_compat_nonarray = false;
    let mut compat_actuals: Vec<PgTypeOid> = Vec::new();

    // Record `actual` into a same-type-only slot.
    fn same(slot: &mut Option<PgTypeOid>, actual: PgTypeOid) -> bool {
        match *slot {
            Some(s) if s != actual => false,
            _ => {
                *slot = Some(actual);
                true
            }
        }
    }

    for (&decl, &actual) in declared.iter().zip(actuals) {
        match decl {
            ANYELEMENT | ANYNONARRAY | ANYENUM => {
                have_anynonarray |= decl == ANYNONARRAY;
                have_anyenum |= decl == ANYENUM;
                if actual != oid::UNKNOWN && !same(&mut elem, actual) {
                    return false;
                }
            }
            ANYARRAY if actual != oid::UNKNOWN => {
                if !same(&mut array, base(actual)) {
                    return false;
                }
            }
            ANYRANGE if actual != oid::UNKNOWN => {
                if !same(&mut range, base(actual)) {
                    return false;
                }
            }
            ANYMULTIRANGE if actual != oid::UNKNOWN => {
                if !same(&mut multirange, base(actual)) {
                    return false;
                }
            }
            ANYCOMPATIBLE | ANYCOMPATIBLENONARRAY => {
                have_compat_nonarray |= decl == ANYCOMPATIBLENONARRAY;
                if actual != oid::UNKNOWN {
                    compat_actuals.push(actual);
                }
            }
            ANYCOMPATIBLEARRAY if actual != oid::UNKNOWN => {
                let Some(e) = element_type(base(actual), snapshot) else {
                    return false;
                };
                compat_actuals.push(e);
            }
            ANYCOMPATIBLERANGE if actual != oid::UNKNOWN => {
                let actual = base(actual);
                match compat_range {
                    Some((r, _)) if r != actual => return false,
                    Some(_) => {}
                    None => {
                        let Some(sub) = snapshot.range_subtype(actual) else {
                            return false;
                        };
                        compat_range = Some((actual, sub));
                        compat_actuals.push(sub);
                    }
                }
            }
            ANYCOMPATIBLEMULTIRANGE if actual != oid::UNKNOWN => {
                let actual = base(actual);
                match compat_multirange {
                    Some((m, _)) if m != actual => return false,
                    Some(_) => {}
                    None => {
                        let Some(r) = snapshot.range_of_multirange(actual) else {
                            return false;
                        };
                        compat_multirange = Some((actual, r));
                    }
                }
            }
            _ => {}
        }
    }

    if let Some(a) = array
        && a != ANYARRAY
    {
        let Some(ae) = element_type(a, snapshot) else {
            return false;
        };
        if !same(&mut elem, ae) {
            return false;
        }
    }
    if let Some(m) = multirange {
        let Some(mr) = snapshot.range_of_multirange(m) else {
            return false;
        };
        match range {
            None => {
                if snapshot.range_subtype(mr).is_none() {
                    return false;
                }
                range = Some(mr);
            }
            Some(r) if r != mr => return false,
            Some(_) => {}
        }
    }
    if let Some(r) = range {
        let Some(sub) = snapshot.range_subtype(r) else {
            return false;
        };
        if !same(&mut elem, sub) {
            return false;
        }
    }
    if have_anynonarray && elem.is_some_and(|e| type_is_array_domain(e, snapshot)) {
        return false;
    }
    if have_anyenum && !elem.is_some_and(|e| type_is_enum(e, snapshot)) {
        return false;
    }

    if let Some((_, mr)) = compat_multirange {
        match compat_range {
            Some((r, _)) if r != mr => return false,
            Some(_) => {}
            None => {
                let Some(sub) = snapshot.range_subtype(mr) else {
                    return false;
                };
                compat_range = Some((mr, sub));
                compat_actuals.push(sub);
            }
        }
    }
    if !compat_actuals.is_empty() {
        let Some(common) = select_common_type_from_oids(&compat_actuals, snapshot) else {
            return false;
        };
        if !verify_common_type_from_oids(common, &compat_actuals, snapshot) {
            return false;
        }
        if have_compat_nonarray && type_is_array_domain(common, snapshot) {
            return false;
        }
        if let Some((_, sub)) = compat_range
            && sub != common
        {
            return false;
        }
    }
    true
}

/// The concrete types a call's polymorphic parameters resolved to, one
/// per pseudo-type (as `enforce_generic_type_consistency` settles them).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PolyTypes {
    elem: Option<PgTypeOid>,
    array: Option<PgTypeOid>,
    range: Option<PgTypeOid>,
    multirange: Option<PgTypeOid>,
    compat: Option<PgTypeOid>,
    compat_array: Option<PgTypeOid>,
    compat_range: Option<PgTypeOid>,
    compat_multirange: Option<PgTypeOid>,
}

impl PolyTypes {
    /// `t` with a polymorphic pseudo-type replaced by what it resolved to
    /// (derived on demand: `anyarray` from the element, `anymultirange`
    /// from the range, …). Returns `t` itself when it isn't polymorphic or
    /// its family wasn't bound — PG's `resolve_polymorphic_tupdesc` leaves
    /// such an OUT column as the pseudo-type too.
    pub(crate) fn resolve(&self, t: PgTypeOid, snapshot: &PgCatalog) -> PgTypeOid {
        let r = match t {
            ANYELEMENT | ANYNONARRAY | ANYENUM => self.elem,
            ANYARRAY => self
                .array
                .or_else(|| self.elem.and_then(|e| snapshot.array_type_of(e))),
            ANYRANGE => self.range,
            ANYMULTIRANGE => self
                .multirange
                .or_else(|| self.range.and_then(|r| snapshot.multirange_of_range(r))),
            ANYCOMPATIBLE | ANYCOMPATIBLENONARRAY => self.compat,
            ANYCOMPATIBLEARRAY => self
                .compat_array
                .or_else(|| self.compat.and_then(|e| snapshot.array_type_of(e))),
            ANYCOMPATIBLERANGE => self.compat_range,
            ANYCOMPATIBLEMULTIRANGE => self.compat_multirange.or_else(|| {
                self.compat_range
                    .and_then(|r| snapshot.multirange_of_range(r))
            }),
            _ => None,
        };
        r.unwrap_or(t)
    }
}

/// PG's `enforce_generic_type_consistency` (parse_coerce.c, with
/// `allow_poly = false` as `ParseFuncOrColumn`/`make_op` call it): once a
/// candidate is chosen, resolve its polymorphic parameters from the actual
/// argument types, rewriting `declared` in place to the concrete coercion
/// targets (unknown actuals included — that is what types a bare `$1` in a
/// polymorphic slot), and return the concrete result type. Fails with PG's
/// wording when the actuals conflict or leave a family unresolvable (all
/// inputs `unknown`).
///
/// `declared` may be longer than `actuals` (parameters filled in from
/// defaults); those trailing positions contribute no actual type.
pub(crate) fn enforce_generic_type_consistency(
    actuals: &[PgTypeOid],
    declared: &mut [PgTypeOid],
    rettype: PgTypeOid,
    snapshot: &PgCatalog,
) -> Result<(PgTypeOid, PolyTypes), AnalyzeError> {
    use crate::pgmsg;
    let fmt = |t: PgTypeOid| crate::ddl::util::format_type_for_message(snapshot, t);
    let base = |t: PgTypeOid| snapshot.unwrap_domain(t);

    let mut p = PolyTypes::default();
    let mut have_poly_anycompatible = false;
    let mut have_poly_unknowns = false;
    let mut have_anynonarray = rettype == ANYNONARRAY;
    let mut have_anyenum = rettype == ANYENUM;
    let have_anymultirange = rettype == ANYMULTIRANGE;
    let mut have_compat_nonarray = rettype == ANYCOMPATIBLENONARRAY;
    let mut have_compat_array = rettype == ANYCOMPATIBLEARRAY;
    let mut have_compat_range = rettype == ANYCOMPATIBLERANGE;
    let mut have_compat_multirange = rettype == ANYCOMPATIBLEMULTIRANGE;
    let mut n_poly_args = 0usize;
    let mut compat_actuals: Vec<PgTypeOid> = Vec::new();
    let mut compat_range_elem: Option<PgTypeOid> = None;
    let mut compat_multirange_range: Option<PgTypeOid> = None;

    // A same-type-only slot of the family-1 resolution.
    let alike = |slot: &mut Option<PgTypeOid>, actual: PgTypeOid, name: &str| {
        match *slot {
            Some(s) if s != actual => return Err(pgmsg::polymorphic_args_not_alike(name)),
            _ => *slot = Some(actual),
        }
        Ok(())
    };

    for (&decl, &actual) in declared.iter().zip(actuals) {
        match decl {
            ANYELEMENT | ANYNONARRAY | ANYENUM => {
                n_poly_args += 1;
                have_anynonarray |= decl == ANYNONARRAY;
                have_anyenum |= decl == ANYENUM;
                if actual == oid::UNKNOWN {
                    have_poly_unknowns = true;
                    continue;
                }
                alike(&mut p.elem, actual, "anyelement")?;
            }
            ANYARRAY | ANYRANGE | ANYMULTIRANGE => {
                n_poly_args += 1;
                if actual == oid::UNKNOWN {
                    have_poly_unknowns = true;
                    continue;
                }
                let (slot, name) = match decl {
                    ANYARRAY => (&mut p.array, "anyarray"),
                    ANYRANGE => (&mut p.range, "anyrange"),
                    _ => (&mut p.multirange, "anymultirange"),
                };
                alike(slot, base(actual), name)?;
            }
            ANYCOMPATIBLE | ANYCOMPATIBLENONARRAY => {
                have_poly_anycompatible = true;
                have_compat_nonarray |= decl == ANYCOMPATIBLENONARRAY;
                if actual != oid::UNKNOWN {
                    compat_actuals.push(actual);
                }
            }
            ANYCOMPATIBLEARRAY => {
                have_poly_anycompatible = true;
                have_compat_array = true;
                if actual == oid::UNKNOWN {
                    continue;
                }
                let actual = base(actual);
                let Some(e) = element_type(actual, snapshot) else {
                    return Err(pgmsg::polymorphic_arg_wrong_kind(
                        "anycompatiblearray",
                        "an array",
                        &fmt(actual),
                    ));
                };
                compat_actuals.push(e);
            }
            ANYCOMPATIBLERANGE => {
                have_poly_anycompatible = true;
                have_compat_range = true;
                if actual == oid::UNKNOWN {
                    continue;
                }
                let actual = base(actual);
                match p.compat_range {
                    Some(r) if r != actual => {
                        return Err(pgmsg::polymorphic_args_not_alike("anycompatiblerange"));
                    }
                    Some(_) => {}
                    None => {
                        let Some(sub) = snapshot.range_subtype(actual) else {
                            return Err(pgmsg::polymorphic_arg_wrong_kind(
                                "anycompatiblerange",
                                "a range type",
                                &fmt(actual),
                            ));
                        };
                        p.compat_range = Some(actual);
                        compat_range_elem = Some(sub);
                        compat_actuals.push(sub);
                    }
                }
            }
            ANYCOMPATIBLEMULTIRANGE => {
                have_poly_anycompatible = true;
                have_compat_multirange = true;
                if actual == oid::UNKNOWN {
                    continue;
                }
                let actual = base(actual);
                match p.compat_multirange {
                    Some(m) if m != actual => {
                        return Err(pgmsg::polymorphic_args_not_alike("anycompatiblemultirange"));
                    }
                    Some(_) => {}
                    None => {
                        let Some(r) = snapshot.range_of_multirange(actual) else {
                            return Err(pgmsg::polymorphic_arg_wrong_kind(
                                "anycompatiblemultirange",
                                "a multirange type",
                                &fmt(actual),
                            ));
                        };
                        p.compat_multirange = Some(actual);
                        compat_multirange_range = Some(r);
                    }
                }
            }
            _ => {}
        }
    }

    // Fast track: no polymorphic arguments, so nothing to resolve.
    if n_poly_args == 0 && !have_poly_anycompatible {
        return Ok((rettype, p));
    }

    if n_poly_args > 0 {
        if let Some(a) = p.array {
            let elem = if a == ANYARRAY {
                // An `anyarray` actual (e.g. `pg_stats.most_common_vals`)
                // is only usable when nothing else needs its element type.
                if n_poly_args != 1 || (rettype != ANYARRAY && is_family1(rettype)) {
                    return Err(pgmsg::anyarray_element_undetermined());
                }
                ANYELEMENT
            } else {
                element_type(a, snapshot).ok_or_else(|| {
                    pgmsg::polymorphic_arg_wrong_kind("anyarray", "an array", &fmt(a))
                })?
            };
            match p.elem {
                None => p.elem = Some(elem),
                Some(e) if e != elem => {
                    return Err(pgmsg::polymorphic_args_inconsistent(
                        "anyarray",
                        "anyelement",
                    ));
                }
                Some(_) => {}
            }
        }
        if let Some(m) = p.multirange {
            let mr = snapshot.range_of_multirange(m).ok_or_else(|| {
                pgmsg::polymorphic_arg_wrong_kind("anymultirange", "a multirange type", &fmt(m))
            })?;
            match p.range {
                None => p.range = Some(mr),
                Some(r) if r != mr => {
                    return Err(pgmsg::polymorphic_args_inconsistent(
                        "anymultirange",
                        "anyrange",
                    ));
                }
                Some(_) => {}
            }
        } else if have_anymultirange && let Some(r) = p.range {
            p.multirange = snapshot.multirange_of_range(r);
        }
        if let Some(r) = p.range {
            let sub = snapshot.range_subtype(r).ok_or_else(|| {
                pgmsg::polymorphic_arg_wrong_kind("anyrange", "a range type", &fmt(r))
            })?;
            match p.elem {
                None => p.elem = Some(sub),
                Some(e) if e != sub => {
                    return Err(pgmsg::polymorphic_args_inconsistent(
                        "anyrange",
                        "anyelement",
                    ));
                }
                Some(_) => {}
            }
        }
        // Only reachable when every family-1 argument is unknown.
        let Some(elem) = p.elem else {
            return Err(pgmsg::polymorphic_type_from_unknown(None));
        };
        if have_anynonarray && elem != ANYELEMENT && type_is_array_domain(elem, snapshot) {
            return Err(pgmsg::polymorphic_match_wrong_kind(
                "anynonarray",
                "is an array type",
                &fmt(elem),
            ));
        }
        if have_anyenum && elem != ANYELEMENT && !type_is_enum(elem, snapshot) {
            return Err(pgmsg::polymorphic_match_wrong_kind(
                "anyenum",
                "is not an enum type",
                &fmt(elem),
            ));
        }
    }

    if have_poly_anycompatible {
        if let Some(mr) = compat_multirange_range {
            match p.compat_range {
                Some(r) if r != mr => {
                    return Err(pgmsg::polymorphic_args_inconsistent(
                        "anycompatiblemultirange",
                        "anycompatiblerange",
                    ));
                }
                Some(_) => {}
                None => {
                    let sub = snapshot.range_subtype(mr).ok_or_else(|| {
                        pgmsg::polymorphic_arg_wrong_kind(
                            "anycompatiblemultirange",
                            "a multirange type",
                            &fmt(mr),
                        )
                    })?;
                    p.compat_range = Some(mr);
                    compat_range_elem = Some(sub);
                    compat_actuals.push(sub);
                }
            }
        } else if have_compat_multirange && let Some(r) = p.compat_range {
            p.compat_multirange = snapshot.multirange_of_range(r);
        }

        if !compat_actuals.is_empty() {
            let common = select_common_type_from_oids(&compat_actuals, snapshot)
                .filter(|&c| verify_common_type_from_oids(c, &compat_actuals, snapshot))
                .ok_or_else(pgmsg::anycompatible_no_common_type)?;
            if have_compat_array {
                p.compat_array = Some(
                    snapshot
                        .array_type_of(common)
                        .ok_or_else(|| pgmsg::no_array_type_for(&fmt(common)))?,
                );
            }
            if have_compat_range {
                let Some(r) = p.compat_range else {
                    return Err(pgmsg::polymorphic_type_from_unknown(Some(
                        "anycompatiblerange",
                    )));
                };
                if compat_range_elem != Some(common) {
                    return Err(pgmsg::anycompatible_range_mismatch(
                        "anycompatiblerange",
                        &fmt(r),
                        &fmt(common),
                    ));
                }
            }
            if have_compat_multirange {
                let Some(m) = p.compat_multirange else {
                    return Err(pgmsg::polymorphic_type_from_unknown(Some(
                        "anycompatiblemultirange",
                    )));
                };
                if compat_range_elem != Some(common) {
                    return Err(pgmsg::anycompatible_range_mismatch(
                        "anycompatiblemultirange",
                        &fmt(m),
                        &fmt(common),
                    ));
                }
            }
            if have_compat_nonarray && type_is_array_domain(common, snapshot) {
                return Err(pgmsg::polymorphic_match_wrong_kind(
                    "anycompatiblenonarray",
                    "is an array type",
                    &fmt(common),
                ));
            }
            p.compat = Some(common);
        } else {
            // All family-2 inputs are unknown: resolve to text, like
            // `select_common_type` — which doesn't license a text range.
            if have_compat_range {
                return Err(pgmsg::polymorphic_type_from_unknown(Some(
                    "anycompatiblerange",
                )));
            }
            if have_compat_multirange {
                return Err(pgmsg::polymorphic_type_from_unknown(Some(
                    "anycompatiblemultirange",
                )));
            }
            p.compat = Some(oid::TEXT);
            p.compat_array = snapshot.array_type_of(oid::TEXT);
        }
        for d in declared.iter_mut() {
            if matches!(
                *d,
                ANYCOMPATIBLE
                    | ANYCOMPATIBLENONARRAY
                    | ANYCOMPATIBLEARRAY
                    | ANYCOMPATIBLERANGE
                    | ANYCOMPATIBLEMULTIRANGE
            ) {
                *d = p.resolve(*d, snapshot);
            }
        }
    }

    // Unknown actuals in family-1 positions adopt the resolved types.
    if have_poly_unknowns {
        for (d, &actual) in declared.iter_mut().zip(actuals) {
            if actual != oid::UNKNOWN {
                continue;
            }
            match *d {
                ANYELEMENT | ANYNONARRAY | ANYENUM => *d = p.elem.unwrap_or(*d),
                ANYARRAY => {
                    if p.array.is_none() {
                        let elem = p.elem.unwrap_or(ANYELEMENT);
                        p.array = Some(
                            snapshot
                                .array_type_of(elem)
                                .ok_or_else(|| pgmsg::no_array_type_for(&fmt(elem)))?,
                        );
                    }
                    *d = p.array.unwrap_or(*d);
                }
                ANYRANGE => {
                    *d = p
                        .range
                        .ok_or_else(|| pgmsg::polymorphic_type_from_unknown(Some("anyrange")))?;
                }
                ANYMULTIRANGE => {
                    *d = p.multirange.ok_or_else(|| {
                        pgmsg::polymorphic_type_from_unknown(Some("anymultirange"))
                    })?;
                }
                _ => {}
            }
        }
    }

    // The result type.
    let ret = match rettype {
        ANYARRAY if p.array.is_none() => {
            let elem = p.elem.unwrap_or(ANYELEMENT);
            let a = snapshot
                .array_type_of(elem)
                .ok_or_else(|| pgmsg::no_array_type_for(&fmt(elem)))?;
            p.array = Some(a);
            a
        }
        ANYCOMPATIBLEARRAY if p.compat_array.is_none() => {
            let elem = p.compat.unwrap_or(ANYCOMPATIBLE);
            let a = snapshot
                .array_type_of(elem)
                .ok_or_else(|| pgmsg::no_array_type_for(&fmt(elem)))?;
            p.compat_array = Some(a);
            a
        }
        ANYRANGE if p.range.is_none() => {
            return Err(pgmsg::polymorphic_type_from_unknown(Some("anyrange")));
        }
        ANYMULTIRANGE if p.multirange.is_none() => {
            return Err(pgmsg::polymorphic_type_from_unknown(Some("anymultirange")));
        }
        t => p.resolve(t, snapshot),
    };
    Ok((ret, p))
}

/// PG's `IsPolymorphicTypeFamily1`.
fn is_family1(t: PgTypeOid) -> bool {
    matches!(
        t,
        ANYELEMENT | ANYARRAY | ANYNONARRAY | ANYENUM | ANYRANGE | ANYMULTIRANGE
    )
}
