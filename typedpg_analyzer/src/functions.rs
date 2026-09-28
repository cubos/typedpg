//! Function and aggregate resolution.

use crate::error::AnalyzeError;
use crate::oid::PgTypeOid;
use crate::pg_catalog::{ArgMode, PgCatalog, PgProc, ProKind, TypCategory, TypType, oid};
use crate::polymorphic::*;

/// One output column of a SRF / OUT-arg function. Mirrors the named-field
/// shape that the analyzer needs from `pg_proc`'s `proallargtypes` /
/// `proargmodes` / `proargnames` triple.
#[derive(Debug, Clone)]
pub(crate) struct OutArg {
    pub name: String,
    pub type_oid: PgTypeOid,
    pub not_null: bool,
}

/// How a call spells its arguments: the names of its trailing
/// named-notation arguments (`f(1, b => 2)` → `["b"]`, PG's `fargnames`)
/// and whether it used the `VARIADIC` keyword.
#[derive(Debug, Default)]
pub(crate) struct CallNotation {
    pub names: Vec<String>,
    pub variadic: bool,
}

impl CallNotation {
    /// Read `fc`'s notation, enforcing PG's parse-time rules on named
    /// arguments (`ParseFuncOrColumn`): a name may be used only once, and no
    /// positional argument may follow a named one.
    pub(crate) fn of(fc: &pg_query::protobuf::FuncCall) -> Result<Self, AnalyzeError> {
        use pg_query::protobuf::node::Node;
        let mut names: Vec<String> = Vec::new();
        for arg in &fc.args {
            let span = || {
                crate::error::node_location(arg).and_then(crate::error::SourceSpan::from_node_token)
            };
            match arg.node.as_ref() {
                Some(Node::NamedArgExpr(na)) => {
                    if names.contains(&na.name) {
                        return Err(crate::pgmsg::argument_name_used_more_than_once(
                            &na.name,
                            span(),
                        )
                        .finalize_implicit());
                    }
                    names.push(na.name.clone());
                }
                _ if !names.is_empty() => {
                    return Err(
                        crate::pgmsg::positional_argument_after_named(span()).finalize_implicit()
                    );
                }
                _ => {}
            }
        }
        Ok(Self {
            names,
            variadic: fc.func_variadic,
        })
    }
}

/// The value expression of a call argument — the `x` of a named argument
/// `a => x`, the node itself otherwise.
pub(crate) fn call_arg_value(arg: &pg_query::protobuf::Node) -> &pg_query::protobuf::Node {
    match arg.node.as_ref() {
        Some(pg_query::protobuf::node::Node::NamedArgExpr(na)) => na.arg.as_deref().unwrap_or(arg),
        _ => arg,
    }
}

/// Resolved function call result.
pub(crate) struct ResolvedFunction {
    pub return_type_oid: PgTypeOid,
    /// The resolved argument types from the matched function signature.
    pub arg_types: Vec<PgTypeOid>,
    pub schema: String,
    pub is_aggregate: bool,
    /// `pg_proc.prokind == 'w'` — a true window function (`row_number`,
    /// `lag`, …), which *requires* an OVER clause at the call site.
    pub is_window: bool,
    pub is_strict: bool,
    /// Named output columns for SRFs / OUT-arg functions, derived from the
    /// matched `pg_proc`'s `proallargtypes`/`proargmodes`/`proargnames`.
    /// Empty for plain scalar returns.
    pub out_args: Vec<OutArg>,
}

/// pg_catalog strict functions that can still return NULL with non-null inputs.
const NULLABLE_STRICT_PG_CATALOG_FUNCTIONS: &[&str] = &[
    "array_position",
    "array_upper",
    "array_lower",
    "array_length",
    "regexp_match",
    "regexp_matches",
    "substring",
    "json_object_field",
    "json_object_field_text",
    "json_array_element",
    "json_array_element_text",
    "jsonb_object_field",
    "jsonb_object_field_text",
    "jsonb_array_element",
    "jsonb_array_element_text",
    "jsonb_path_query_first",
    "jsonb_path_match",
    "jsonb_extract_path",
    "jsonb_extract_path_text",
    "json_extract_path",
    "json_extract_path_text",
    "nullif",
    "obj_description",
    "col_description",
    "shobj_description",
];

/// pg_catalog operators that can return NULL with non-null inputs.
const NULLABLE_PG_CATALOG_OPERATORS: &[&str] = &["->", "->>", "#>", "#>>"];

/// Resolve a function call by name and argument types.
///
/// `span` covers the function reference in the original SQL — usually
/// produced by `SourceSpan::from_node_qname(FuncCall.location)`. When
/// provided, `UndefinedFunction` errors emerge with a snippet pointing at
/// the call and a "did you mean" hint computed against the catalog's
/// visible functions.
pub(crate) fn resolve_function(
    snapshot: &PgCatalog,
    schema: Option<&str>,
    name: &str,
    arg_types: &[PgTypeOid],
    notation: &CallNotation,
    _is_agg_star: bool,
    span: Option<crate::error::SourceSpan>,
) -> Result<ResolvedFunction, AnalyzeError> {
    let mut all_matches = snapshot.find_functions(schema, name);
    // A named/mixed-notation call only reaches the overloads whose parameter
    // names fit it; each survivor is re-ordered into the call's argument
    // order, so every positional matching pass below applies unchanged.
    let named_matches: Vec<PgProc>;
    if !notation.names.is_empty() {
        named_matches = all_matches
            .iter()
            .filter_map(|f| match_named_call(f, arg_types.len(), notation))
            .collect();
        all_matches = named_matches.iter().collect();
    }
    let candidates: Vec<&PgProc> = all_matches
        .iter()
        .copied()
        .filter(|f| !matches!(f.prokind, ProKind::Procedure))
        .collect();
    // PG's wording keeps the user's schema qualifier — match it so the
    // sanity-check prefix passes. `QualifiedName::Display` handles
    // identifier quoting (and round-trips through PG's rules).
    let qualified = match schema {
        Some(s) => crate::qualified_name::QualifiedName::new(s, name).to_string(),
        None => name.to_string(),
    };
    // Render the call's actual arg types in PG-style names (int4 → integer,
    // …), named arguments as `name => type`, so the message matches PG
    // verbatim (`funcname_signature_string`).
    let first_named = arg_types.len().saturating_sub(notation.names.len());
    let arg_list_actual = arg_types
        .iter()
        .enumerate()
        .map(|(i, &oid)| {
            let ty = crate::ddl::util::format_type_for_message(snapshot, oid);
            match i
                .checked_sub(first_named)
                .and_then(|j| notation.names.get(j))
            {
                Some(n) => format!("{n} => {ty}"),
                None => ty,
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    if candidates.is_empty() {
        // PG distinguishes "function not found at all" from "the name
        // resolves but only to a procedure" (SQLSTATE 42809). Mirror that
        // so the sanity check matches PG verbatim.
        if let Some(proc) = all_matches
            .iter()
            .find(|f| matches!(f.prokind, ProKind::Procedure))
        {
            let arg_list = proc
                .proargtypes
                .iter()
                .map(|oid| {
                    snapshot
                        .pg_type
                        .get(oid)
                        .map(|t| match t.typname.as_str() {
                            "int2" => "smallint".to_string(),
                            "int4" => "integer".to_string(),
                            "int8" => "bigint".to_string(),
                            "float4" => "real".to_string(),
                            "float8" => "double precision".to_string(),
                            other => other.to_string(),
                        })
                        .unwrap_or_default()
                })
                .collect::<Vec<_>>()
                .join(", ");
            // PG classifies a procedure in an expression as wrong_object_type
            // (42809), not undefined_function.
            return Err(crate::error::RawError::new(
                AnalyzeError::WrongObjectType(format!("{qualified}({arg_list}) is a procedure")),
                span,
                Some("to call a procedure, use CALL".into()),
            )
            .finalize_implicit());
        }
        return Err(undefined_function_error(
            snapshot,
            schema,
            name,
            format!("function {qualified}({arg_list_actual}) does not exist"),
            span,
        ));
    }

    // PG smashes a domain argument to its base type for function/aggregate
    // overload resolution (so `max(email)` matches `max(text)` exactly and
    // `sum(pos)` — a domain over int4 — resolves to `sum(int4)` like a plain
    // int, not to some arbitrary implicit-cast candidate). Match against the
    // unwrapped types; the original `arg_types` is still used for the
    // "does not exist" wording so it shows the domain the user wrote.
    let match_types: Vec<PgTypeOid> = arg_types
        .iter()
        .map(|&o| snapshot.unwrap_domain(o))
        .collect();
    let match_types = match_types.as_slice();

    if let Some(f) = find_exact_match(&candidates, match_types) {
        return Ok(make_resolved(f, snapshot));
    }
    if let Some(f) = find_unknown_compatible_match(&candidates, match_types) {
        return Ok(make_resolved(f, snapshot));
    }
    if let Some(f) = find_default_args_match(&candidates, match_types, snapshot) {
        return Ok(make_resolved(f, snapshot));
    }
    match find_cast_match(&candidates, match_types, snapshot) {
        CastMatch::Match(f) => return Ok(make_resolved(f, snapshot)),
        CastMatch::Ambiguous => {
            // PG (SQLSTATE 42725): `function name(types) is not unique` —
            // several candidates survived every tiebreak; choosing one
            // could silently mistype the call.
            return Err(
                crate::pgmsg::function_is_not_unique(&qualified, &arg_list_actual, span)
                    .finalize_implicit(),
            );
        }
        CastMatch::NoMatch => {}
    }
    if let Some(f) = find_polymorphic_match(&candidates, match_types, snapshot) {
        return Ok(make_resolved_polymorphic(f, match_types, snapshot));
    }

    // Whether a single declared parameter `p` plausibly accepts actual `a`. A
    // polymorphic pseudo-type (anyarray, anyenum, …) only accepts actuals that
    // satisfy its shape constraint — `array_ndims(integer)` must NOT match
    // `anyarray`. Other pseudo-types (`"any"` for `count(x)`, the `"any"`
    // element of a `VARIADIC "any"`, …) accept anything.
    let param_accepts = |p: PgTypeOid, a: PgTypeOid| {
        if p == a || a == oid::UNKNOWN {
            return true;
        }
        if is_polymorphic(p) {
            return matches_polymorphic(p, a, snapshot);
        }
        // The `record` pseudo-type only accepts an actual composite/record
        // value — NOT an arbitrary scalar. `max(record)` exists, but
        // `max(boolean)` must still fail (PG: `function max(boolean) does not
        // exist`). Other non-polymorphic pseudo-types (`"any"` for `count(x)`,
        // the `"any"` element of a `VARIADIC "any"`, …) accept anything.
        if p == oid::RECORD {
            let base = snapshot.unwrap_domain(a);
            return base == oid::RECORD
                || snapshot
                    .get_type(base)
                    .is_some_and(|t| t.typtype == TypType::Composite);
        }
        if snapshot
            .get_type(p)
            .is_some_and(|t| t.typtype == TypType::Pseudo)
        {
            return true;
        }
        casts_implicitly(a, p, snapshot)
    };

    // Whether `f`'s parameters can plausibly accept the actual `arg_types`. A
    // concrete parameter with a non-coercible actual is NOT accepted — that's a
    // genuine `does not exist`, e.g. `jsonb_typeof(numeric)`, which PG rejects.
    let args_fit = |f: &PgProc| {
        if let Some(var_elem) = f.provariadic {
            // Variadic: the first N-1 declared params are fixed and the trailing
            // slot absorbs args of the variadic element type (`provariadic`).
            // The fixed params must still match — so `concat_ws(integer)` is
            // rejected (its `text` separator can't take an int), while
            // `concat_ws(text, int, …)` is accepted.
            let n = f.proargtypes.len();
            if n == 0 {
                return true;
            }
            let fixed = n - 1;
            // A `VARIADIC "any"` function is NOT expanded like an array
            // variadic: PG requires at least one argument in the variadic
            // position, so the minimum call arity is N, not N-1. That's why
            // `concat()` and `concat_ws('x')` don't exist, while array-element
            // variadics may take zero trailing args.
            let var_is_any = snapshot
                .get_type(var_elem)
                .is_some_and(|t| t.typname == "any");
            let min_args = if var_is_any { n } else { fixed };
            if arg_types.len() < min_args {
                return false;
            }
            let fixed_ok = f.proargtypes[..fixed]
                .iter()
                .zip(arg_types)
                .all(|(&p, &a)| param_accepts(p, a));
            let tail_ok = arg_types[fixed..]
                .iter()
                .all(|&a| param_accepts(var_elem, a));
            return fixed_ok && tail_ok;
        }
        f.proargtypes.len() == arg_types.len()
            && f.proargtypes
                .iter()
                .zip(arg_types)
                .all(|(&p, &a)| param_accepts(p, a))
    };

    // A lone aggregate candidate is accepted only when its parameters actually
    // fit the call — a single `bool_or(boolean)` overload must still reject
    // `bool_or(numeric)` and `bool_or(text, timestamptz)` (wrong arity), which
    // PG reports as `function bool_or(...) does not exist`.
    let agg_candidates: Vec<_> = candidates
        .iter()
        .filter(|f| matches!(f.prokind, ProKind::Aggregate))
        .collect();
    if agg_candidates.len() == 1 && args_fit(agg_candidates[0]) {
        return Ok(make_resolved(agg_candidates[0], snapshot));
    }

    // Last-resort single-candidate match across all (non-procedure) candidates.
    let count_matches: Vec<_> = candidates.iter().filter(|f| args_fit(f)).collect();
    if count_matches.len() == 1 {
        return Ok(make_resolved(count_matches[0], snapshot));
    }

    // PG's wording: `function name(arg_types_joined) does not exist`. Match
    // it verbatim so pg_sanity's prefix check passes; append the candidate
    // count we computed as a suffix for the macro caller's diagnostic.
    Err(undefined_function_error(
        snapshot,
        schema,
        name,
        format!(
            "function {qualified}({arg_list_actual}) does not exist (found {} candidate(s))",
            candidates.len()
        ),
        span,
    ))
}

/// Build the public-facing `UndefinedFunction` error with snippet + hint.
pub(crate) fn undefined_function_error(
    snapshot: &PgCatalog,
    schema: Option<&str>,
    name: &str,
    message: String,
    span: Option<crate::error::SourceSpan>,
) -> AnalyzeError {
    let hint = crate::suggest::suggest_similar(name, snapshot.visible_function_names(schema))
        .map(|c| format!("did you mean \"{c}\"?"));
    crate::error::RawError::undefined_function(message, span, hint).finalize_implicit()
}

/// PG's `MatchNamedCall` (namespace.c): map a named/mixed-notation call
/// with `nargs` arguments onto `f`'s input parameters. Returns a copy of `f`
/// whose `proargtypes` are re-ordered into the call's argument order (the
/// omitted, defaulted parameters dropped), or `None` when the names don't
/// fit — an unknown name, a name repeating a positional argument, or an
/// omitted parameter without a default.
fn match_named_call(f: &PgProc, nargs: usize, notation: &CallNotation) -> Option<PgProc> {
    // Named notation can reach a variadic function only through an explicit
    // `VARIADIC` argument: the expanded variadic elements have no name.
    if f.provariadic.is_some() && !notation.variadic {
        return None;
    }
    let pronargs = f.proargtypes.len();
    let defaults = f.pronargdefaults.max(0) as usize;
    if nargs > pronargs || nargs + defaults < pronargs {
        return None;
    }
    // Input-parameter names in `proargtypes` order. With `proargmodes` set,
    // `proargnames` parallels `proallargtypes` and OUT/TABLE entries are
    // skipped; without it, every parameter is an input.
    let input_names: Vec<&str> = if f.proargmodes.is_empty() {
        f.proargnames.iter().map(String::as_str).collect()
    } else {
        f.proargmodes
            .iter()
            .zip(&f.proargnames)
            .filter(|(m, _)| matches!(m, ArgMode::In | ArgMode::InOut | ArgMode::Variadic))
            .map(|(_, n)| n.as_str())
            .collect()
    };
    let positional = nargs - notation.names.len();
    let mut given = vec![false; pronargs];
    given[..positional].fill(true);
    let mut order: Vec<usize> = (0..positional).collect();
    for name in &notation.names {
        let pp = input_names.iter().position(|n| n == name)?;
        if pp >= pronargs || given[pp] {
            return None;
        }
        given[pp] = true;
        order.push(pp);
    }
    // Every parameter the call leaves out must have a default — and only
    // the trailing `pronargdefaults` ones do.
    let first_default = pronargs - defaults;
    if (positional..pronargs).any(|pp| !given[pp] && pp < first_default) {
        return None;
    }
    let mut matched = f.clone();
    matched.proargtypes = order.iter().map(|&pp| f.proargtypes[pp]).collect();
    matched.pronargdefaults = 0;
    matched.provariadic = None;
    Some(matched)
}

fn find_unknown_compatible_match<'a>(
    candidates: &[&'a PgProc],
    arg_types: &[PgTypeOid],
) -> Option<&'a PgProc> {
    if !arg_types.contains(&oid::UNKNOWN) {
        return None;
    }
    let matches: Vec<_> = candidates
        .iter()
        .filter(|f| {
            f.proargtypes.len() == arg_types.len()
                && f.proargtypes
                    .iter()
                    .zip(arg_types.iter())
                    .all(|(&expected, &actual)| expected == actual || actual == oid::UNKNOWN)
        })
        .collect();
    if matches.len() == 1 {
        Some(matches[0])
    } else {
        None
    }
}

fn find_exact_match<'a>(candidates: &[&'a PgProc], arg_types: &[PgTypeOid]) -> Option<&'a PgProc> {
    candidates
        .iter()
        .find(|f| f.proargtypes == arg_types)
        .copied()
}

fn find_default_args_match<'a>(
    candidates: &[&'a PgProc],
    arg_types: &[PgTypeOid],
    snapshot: &PgCatalog,
) -> Option<&'a PgProc> {
    let provided = arg_types.len();
    let matching: Vec<&PgProc> = candidates
        .iter()
        .filter(|f| {
            let total = f.proargtypes.len();
            let defaults = f.pronargdefaults.max(0) as usize;
            total >= provided
                && defaults >= total - provided
                && f.proargtypes
                    .iter()
                    .take(provided)
                    .zip(arg_types.iter())
                    .all(|(&expected, &actual)| {
                        expected == actual
                            || actual == oid::UNKNOWN
                            || snapshot.has_implicit_cast(actual, expected)
                    })
        })
        .copied()
        .collect();
    if matching.len() == 1 {
        Some(matching[0])
    } else {
        None
    }
}

/// `has_implicit_cast` extended with PG's element-wise rule for arrays:
/// `numeric[] → float8[]` is allowed because `numeric → float8` is. This is
/// only used by overload resolution; the bare `has_implicit_cast` query
/// stays strict so explicit cast lookups don't get accidentally relaxed.
fn casts_implicitly(source: PgTypeOid, target: PgTypeOid, snapshot: &PgCatalog) -> bool {
    if snapshot.has_implicit_cast(source, target) {
        return true;
    }
    let (Some(s), Some(t)) = (snapshot.get_type(source), snapshot.get_type(target)) else {
        return false;
    };
    if s.typcategory != TypCategory::Array || t.typcategory != TypCategory::Array {
        return false;
    }
    let (Some(se), Some(te)) = (s.typelem, t.typelem) else {
        return false;
    };
    snapshot.has_implicit_cast(se, te)
}

fn find_polymorphic_match<'a>(
    candidates: &[&'a PgProc],
    arg_types: &[PgTypeOid],
    snapshot: &PgCatalog,
) -> Option<&'a PgProc> {
    // First pass: only exact / UNKNOWN / polymorphic matches — what PG calls
    // a "type-conformant" candidate without any cast.
    let strict: Vec<&PgProc> = candidates
        .iter()
        .filter(|f| f.proargtypes.len() == arg_types.len())
        .filter(|f| {
            f.proargtypes
                .iter()
                .zip(arg_types.iter())
                .all(|(&expected, &actual)| {
                    expected == actual
                        || actual == oid::UNKNOWN
                        || matches_polymorphic(expected, actual, snapshot)
                })
        })
        // Per-position matching passed; the call as a whole must also
        // unify (anyelement == the range's subtype, etc.).
        .filter(|f| unify_polymorphic_call(&f.proargtypes, arg_types, snapshot).is_some())
        .copied()
        .collect();
    if strict.len() == 1 {
        return Some(strict[0]);
    }
    if !strict.is_empty() {
        return None;
    }

    // Second pass: allow implicit casts on the non-polymorphic args. This is
    // what makes `percentile_disc(0.5) WITHIN GROUP (ORDER BY int_col)`
    // resolve — the direct arg is `numeric` but the candidate expects
    // `float8`, while the ordered arg matches the polymorphic `anyelement`.
    let lax: Vec<&PgProc> = candidates
        .iter()
        .filter(|f| f.proargtypes.len() == arg_types.len())
        .filter(|f| {
            f.proargtypes
                .iter()
                .zip(arg_types.iter())
                .all(|(&expected, &actual)| {
                    expected == actual
                        || actual == oid::UNKNOWN
                        || matches_polymorphic(expected, actual, snapshot)
                        || (!is_polymorphic(expected)
                            && casts_implicitly(actual, expected, snapshot))
                })
        })
        .copied()
        .collect();
    if lax.len() == 1 { Some(lax[0]) } else { None }
}

/// Outcome of the implicit-cast overload filter: a unique winner, no
/// surviving candidate, or several candidates left tied after every
/// tiebreak — PG reports the last as `function X(…) is not unique`
/// (SQLSTATE 42725) rather than picking one arbitrarily.
enum CastMatch<'a> {
    Match(&'a PgProc),
    NoMatch,
    Ambiguous,
}

fn find_cast_match<'a>(
    candidates: &[&'a PgProc],
    arg_types: &[PgTypeOid],
    snapshot: &PgCatalog,
) -> CastMatch<'a> {
    let matching: Vec<&PgProc> = candidates
        .iter()
        .filter(|f| f.proargtypes.len() == arg_types.len())
        .filter(|f| {
            f.proargtypes
                .iter()
                .zip(arg_types.iter())
                .all(|(&expected, &actual)| {
                    expected == actual
                        || actual == oid::UNKNOWN
                        || casts_implicitly(actual, expected, snapshot)
                })
        })
        .copied()
        .collect();

    if matching.is_empty() {
        return CastMatch::NoMatch;
    }
    if matching.len() == 1 {
        return CastMatch::Match(matching[0]);
    }

    if !arg_types.contains(&oid::UNKNOWN) {
        // PG §10.3 step 4c/4d for concretely-typed arguments: keep the
        // candidates with the most exact type matches, then those accepting
        // the preferred type of each argument's category at the most
        // converted positions. This is what makes `floor(bigint)` resolve to
        // `floor(double precision)` (float8 is the preferred numeric type),
        // not `floor(numeric)`.
        let exact = |f: &PgProc| -> usize {
            f.proargtypes
                .iter()
                .zip(arg_types)
                .filter(|&(p, a)| p == a)
                .count()
        };
        let best_exact = matching.iter().copied().map(exact).max().unwrap_or(0);
        let mut narrowed: Vec<&PgProc> = matching
            .iter()
            .copied()
            .filter(|f| exact(f) == best_exact)
            .collect();
        if narrowed.len() > 1 {
            // "Accepts the preferred type" — shared with operator resolution,
            // see `PgCatalog::is_preferred_for`.
            let preferred_hits = |f: &PgProc| -> usize {
                f.proargtypes
                    .iter()
                    .zip(arg_types)
                    .filter(|&(&p, &a)| snapshot.is_preferred_for(p, a))
                    .count()
            };
            let best_pref = narrowed
                .iter()
                .copied()
                .map(preferred_hits)
                .max()
                .unwrap_or(0);
            if best_pref > 0 {
                narrowed.retain(|f| preferred_hits(f) == best_pref);
            }
        }
        // PG fails with `is not unique` when several candidates survive its
        // tiebreaks; picking one would risk silently mistyping the call
        // (`mod(int2,int2)` vs `mod(int8,int8)` for smallint inputs differ
        // in result type).
        return match narrowed.len() {
            0 => CastMatch::NoMatch,
            1 => CastMatch::Match(narrowed[0]),
            _ => CastMatch::Ambiguous,
        };
    }

    // Unknown-argument resolution (PG's func_select_candidate): prefer
    // candidates accepting the STRING category at every unknown position…
    let string_compatible: Vec<&PgProc> = matching
        .iter()
        .filter(|f| {
            f.proargtypes
                .iter()
                .zip(arg_types.iter())
                .all(|(&param_oid, &actual)| {
                    if actual != oid::UNKNOWN {
                        return true;
                    }
                    snapshot
                        .get_type(param_oid)
                        .is_some_and(|t| t.typcategory == TypCategory::String)
                })
        })
        .copied()
        .collect();

    if string_compatible.len() == 1 {
        return CastMatch::Match(string_compatible[0]);
    }
    if string_compatible.len() > 1 {
        let preferred: Vec<&PgProc> = string_compatible
            .iter()
            .filter(|f| {
                f.proargtypes
                    .iter()
                    .zip(arg_types.iter())
                    .all(|(&param_oid, &actual)| {
                        if actual != oid::UNKNOWN {
                            return true;
                        }
                        snapshot
                            .get_type(param_oid)
                            .is_some_and(|t| t.typispreferred)
                    })
            })
            .copied()
            .collect();
        return match preferred.len() {
            1 => CastMatch::Match(preferred[0]),
            // Several string-accepting candidates left — PG gives up with
            // `is not unique` instead of guessing.
            _ => CastMatch::Ambiguous,
        };
    }

    // …otherwise every unknown slot's candidates must agree on a single
    // type category; within it, candidates carrying the category's
    // *preferred* type win (this is what resolves `round('1.5')` to
    // `round(float8)` while `mod('5','2')` — int/numeric variants only,
    // none preferred — stays ambiguous).
    let unknown_slots: Vec<usize> = arg_types
        .iter()
        .enumerate()
        .filter(|&(_, &a)| a == oid::UNKNOWN)
        .map(|(i, _)| i)
        .collect();
    let mut narrowed = matching.clone();
    for &i in &unknown_slots {
        let categories: Vec<Option<TypCategory>> = narrowed
            .iter()
            .map(|f| {
                f.proargtypes
                    .get(i)
                    .and_then(|&p| snapshot.get_type(p))
                    .map(|t| t.typcategory)
            })
            .collect();
        if categories.windows(2).any(|w| w[0] != w[1]) {
            // Candidates disagree on the unknown slot's category — PG's
            // func_select_candidate returns "multiple" here.
            return CastMatch::Ambiguous;
        }
        let has_preferred = narrowed.iter().any(|f| {
            f.proargtypes
                .get(i)
                .and_then(|&p| snapshot.get_type(p))
                .is_some_and(|t| t.typispreferred)
        });
        if has_preferred {
            narrowed.retain(|f| {
                f.proargtypes
                    .get(i)
                    .and_then(|&p| snapshot.get_type(p))
                    .is_some_and(|t| t.typispreferred)
            });
        }
    }
    if narrowed.len() == 1 {
        return CastMatch::Match(narrowed[0]);
    }

    // Last-gasp rule: when every *known* argument shares one type, assume
    // the unknowns are of that type too and look for the single candidate
    // that accepts it everywhere.
    let known: Vec<PgTypeOid> = arg_types
        .iter()
        .copied()
        .filter(|&a| a != oid::UNKNOWN)
        .collect();
    if let Some(&t) = known.first()
        && known.iter().all(|&k| k == t)
    {
        let assumed: Vec<&PgProc> = narrowed
            .iter()
            .filter(|f| {
                unknown_slots
                    .iter()
                    .all(|&i| f.proargtypes.get(i) == Some(&t))
            })
            .copied()
            .collect();
        if assumed.len() == 1 {
            return CastMatch::Match(assumed[0]);
        }
    }
    CastMatch::Ambiguous
}

/// Build the named output-argument list for an SRF / OUT-arg function from
/// its `pg_proc` row. Returns an empty vec when the function has no OUT-like
/// args.
fn build_out_args(p: &PgProc) -> Vec<OutArg> {
    if p.proargmodes.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let len = p
        .proallargtypes
        .len()
        .min(p.proargmodes.len())
        .min(p.proargnames.len());
    for i in 0..len {
        let mode = p.proargmodes[i];
        if !matches!(mode, ArgMode::Out | ArgMode::InOut | ArgMode::Table) {
            continue;
        }
        let name = &p.proargnames[i];
        if name.is_empty() {
            continue;
        }
        out.push(OutArg {
            name: name.clone(),
            type_oid: p.proallargtypes[i],
            not_null: false,
        });
    }
    out
}

/// Effective return type for an aggregate: when `pg_aggregate.aggfinalfn`
/// points at a real proc, use *that* proc's `prorettype`; otherwise the
/// caller falls back to the aggregate's own `prorettype`. PG derives this
/// the same way at lookup time — we don't cache the type on `pg_aggregate`.
fn aggregate_final_return(f: &PgProc, snapshot: &PgCatalog) -> Option<PgTypeOid> {
    let agg = snapshot.pg_aggregate.get(&f.oid)?;
    let final_oid = agg.aggfinalfn?;
    snapshot.pg_proc.get(&final_oid).map(|p| p.prorettype)
}

fn make_resolved(f: &PgProc, snapshot: &PgCatalog) -> ResolvedFunction {
    let agg_final = aggregate_final_return(f, snapshot);
    ResolvedFunction {
        return_type_oid: agg_final.unwrap_or(f.prorettype),
        arg_types: f.proargtypes.clone(),
        schema: snapshot
            .namespace_name(f.pronamespace)
            .map(str::to_owned)
            .unwrap_or_default(),
        is_aggregate: matches!(f.prokind, ProKind::Aggregate),
        is_window: matches!(f.prokind, ProKind::Window),
        is_strict: f.proisstrict,
        out_args: build_out_args(f),
    }
}

fn make_resolved_polymorphic(
    f: &PgProc,
    actual_args: &[PgTypeOid],
    snapshot: &PgCatalog,
) -> ResolvedFunction {
    let mut bindings = PolyBindings::default();
    for (&expected, &actual) in f.proargtypes.iter().zip(actual_args.iter()) {
        bind_polymorphic_from(expected, actual, snapshot, &mut bindings);
    }

    let agg_final = aggregate_final_return(f, snapshot);
    let return_type_oid =
        substitute_polymorphic(agg_final.unwrap_or(f.prorettype), &bindings, snapshot);
    let out_args = build_out_args(f)
        .into_iter()
        .map(|field| OutArg {
            name: field.name,
            type_oid: substitute_polymorphic(field.type_oid, &bindings, snapshot),
            not_null: field.not_null,
        })
        .collect();

    ResolvedFunction {
        return_type_oid,
        arg_types: f.proargtypes.clone(),
        schema: snapshot
            .namespace_name(f.pronamespace)
            .map(str::to_owned)
            .unwrap_or_default(),
        is_aggregate: matches!(f.prokind, ProKind::Aggregate),
        is_window: matches!(f.prokind, ProKind::Window),
        is_strict: f.proisstrict,
        out_args,
    }
}

pub(crate) fn is_nullable_strict_exception(name: &str) -> bool {
    NULLABLE_STRICT_PG_CATALOG_FUNCTIONS.contains(&name)
}

pub(crate) fn is_nullable_operator(name: &str) -> bool {
    NULLABLE_PG_CATALOG_OPERATORS.contains(&name)
}

const NOT_NULL_NONSTRICT_PG_CATALOG_FUNCTIONS: &[&str] = &[
    "concat",
    "format",
    "now",
    "transaction_timestamp",
    "statement_timestamp",
    "clock_timestamp",
    "timeofday",
    "current_timestamp",
    "current_date",
    "localtime",
    "localtimestamp",
    "current_user",
    "session_user",
    "current_schema",
    "current_database",
    "current_catalog",
    "inet_client_addr",
    "inet_server_addr",
    "pg_backend_pid",
    "pg_postmaster_start_time",
    "version",
    "random",
    "gen_random_uuid",
    "setseed",
    "nextval",
    "currval",
    "lastval",
    "setval",
    "txid_current",
    "txid_current_if_assigned",
    "array_cat",
    "array_append",
    "array_prepend",
    "coalesce",
    "greatest",
    "least",
    "json_build_object",
    "jsonb_build_object",
    "json_build_array",
    "jsonb_build_array",
    "row_number",
    "rank",
    "dense_rank",
    "percent_rank",
    "cume_dist",
    "ntile",
];

pub(crate) fn is_not_null_nonstrict(name: &str) -> bool {
    NOT_NULL_NONSTRICT_PG_CATALOG_FUNCTIONS.contains(&name)
}
