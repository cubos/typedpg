//! Function and aggregate resolution.

use crate::error::AnalyzeError;
use crate::oid::PgTypeOid;
use crate::pg_catalog::{ArgMode, PgCatalog, PgProc, ProKind, TypCategory, oid};

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
    /// The chosen routine's `pg_proc.oid`.
    pub oid: crate::oid::PgProcOid,
    pub return_type_oid: PgTypeOid,
    /// The coercion target of each call argument, in call order: the
    /// matched signature with its polymorphic parameters resolved (PG's
    /// `declared_arg_types` after `enforce_generic_type_consistency`).
    pub arg_types: Vec<PgTypeOid>,
    pub schema: String,
    pub is_aggregate: bool,
    /// `pg_proc.prokind == 'w'` — a true window function (`row_number`,
    /// `lag`, …), which *requires* an OVER clause at the call site.
    pub is_window: bool,
    pub is_strict: bool,
    /// `pg_proc.proretset` — the function returns a set (SRF).
    pub is_set_returning: bool,
    /// Named output columns for SRFs / OUT-arg functions, derived from the
    /// matched `pg_proc`'s `proallargtypes`/`proargmodes`/`proargnames`.
    /// Empty for plain scalar returns.
    pub out_args: Vec<OutArg>,
    /// `proname(typname,…)` of the matched overload's declared
    /// `proargtypes` — the key of the builtin nullability tables.
    pub signature: String,
    /// `pg_aggregate.aggkind` / `aggnumdirectargs` of an aggregate.
    pub aggregate: Option<(crate::pg_catalog::AggKind, i16)>,
    /// How many call arguments a variadic parameter absorbed (PG's
    /// `nvargs`; 0 when not expanded).
    pub nvargs: usize,
    /// `pg_proc.provariadic` of the matched routine (PG's `vatype`).
    pub provariadic: Option<PgTypeOid>,
}

/// What a call resolved to — PG's `FuncDetailCode` for the successful
/// cases: a routine, or (for a one-argument call named after a type) a
/// function-style cast `typename(x)`.
pub(crate) enum FuncDetail {
    Routine(ResolvedFunction),
    Coercion(PgTypeOid),
}

/// pg_catalog operators that can return NULL with non-null inputs.
const NULLABLE_PG_CATALOG_OPERATORS: &[&str] = &["->", "->>", "#>", "#>>"];

/// Resolve a function call by name and argument types, without the
/// function-style cast interpretation (see [`func_get_detail`]).
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
    match func_get_detail(snapshot, schema, name, arg_types, notation, None, span)? {
        FuncDetail::Routine(f) => Ok(f),
        FuncDetail::Coercion(_) => unreachable!("coercion interpretation not requested"),
    }
}

/// One entry of PG's `FuncCandidateList`: an overload as it would take this
/// call — named arguments re-ordered into call order, a variadic parameter
/// expanded into `nvargs` copies of its element type, parameters the call
/// omits (filled from defaults) appended after the call's own.
struct Candidate<'a> {
    proc: &'a PgProc,
    /// Parameter types in call order; the first `nargs` line up with the
    /// call's arguments, the remaining `ndargs` come from defaults.
    args: Vec<PgTypeOid>,
    ndargs: usize,
    /// The `proargtypes` positions of the defaulted parameters, in the
    /// order they follow the call's arguments in `args`.
    default_params: Vec<usize>,
    nvargs: usize,
    /// Index of the candidate's schema on the search path.
    pathpos: usize,
    /// Two same-schema overloads offered this exact signature (PG marks
    /// the survivor with an invalid OID): choosing it is `not unique`.
    ambiguous: bool,
}

/// PG's `func_get_detail` (parse_func.c) plus the argument checks
/// `ParseFuncOrColumn` makes right after it: gather the candidates
/// ([`func_candidates`]), take an exact match, else — for a one-argument
/// call named after a type — a function-style cast, else the candidates
/// the arguments coerce to ([`func_match_argtypes`]), narrowed by
/// [`func_select_candidate`]. The winner's polymorphic parameters are then
/// resolved by `enforce_generic_type_consistency`.
///
/// `coercion_arg` enables the cast interpretation: `Some(is_unknown_const)`
/// says whether the lone argument is an untyped literal (always a cast).
pub(crate) fn func_get_detail(
    snapshot: &PgCatalog,
    schema: Option<&str>,
    name: &str,
    arg_types: &[PgTypeOid],
    notation: &CallNotation,
    coercion_arg: Option<bool>,
    span: Option<crate::error::SourceSpan>,
) -> Result<FuncDetail, AnalyzeError> {
    if let Some(s) = schema
        && snapshot.namespace_oid(s).is_none()
    {
        return Err(crate::pgmsg::schema_does_not_exist(s, span).finalize_implicit());
    }
    let nargs = arg_types.len();
    let candidates = func_candidates(snapshot, schema, name, nargs, notation);

    // PG's wording keeps the user's schema qualifier — match it so the
    // sanity-check prefix passes. `QualifiedName::Display` handles
    // identifier quoting (and round-trips through PG's rules).
    let qualified = match schema {
        Some(s) => crate::qualified_name::QualifiedName::new(s, name).to_string(),
        None => name.to_string(),
    };
    // Render the call's actual arg types in PG-style names (int4 → integer,
    // …), named arguments as `name => type`, so the message matches PG
    // verbatim (`func_signature_string`).
    let first_named = nargs.saturating_sub(notation.names.len());
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
    let not_unique = || {
        crate::pgmsg::function_is_not_unique(&qualified, &arg_list_actual, span).finalize_implicit()
    };

    let exact = candidates
        .iter()
        .position(|c| c.args[..nargs] == *arg_types);
    let best = match exact {
        Some(i) => Some(i),
        None => {
            if let Some(unknown_const) = coercion_arg
                && nargs == 1
                && notation.names.is_empty()
                && let Some(target) =
                    func_name_as_coercion(snapshot, schema, name, arg_types[0], unknown_const)
            {
                return Ok(FuncDetail::Coercion(target));
            }
            let arg_lists: Vec<&[PgTypeOid]> =
                candidates.iter().map(|c| &c.args[..nargs]).collect();
            let matching = func_match_argtypes(arg_types, &arg_lists, snapshot);
            match matching.as_slice() {
                [] => None,
                [one] => Some(*one),
                _ => {
                    let narrowed: Vec<&[PgTypeOid]> =
                        matching.iter().map(|&i| arg_lists[i]).collect();
                    match func_select_candidate(arg_types, &narrowed, snapshot) {
                        Some(j) => Some(matching[j]),
                        None => return Err(not_unique()),
                    }
                }
            }
        }
    };
    let Some(best) = best else {
        let functions_named = snapshot.find_functions(schema, name).len();
        // PG's wording: `function name(arg_types_joined) does not exist`.
        // Match it verbatim so pg_sanity's prefix check passes; append the
        // candidate count as a suffix for the macro caller's diagnostic.
        let message = if functions_named == 0 {
            format!("function {qualified}({arg_list_actual}) does not exist")
        } else {
            format!(
                "function {qualified}({arg_list_actual}) does not exist (found {functions_named} candidate(s))"
            )
        };
        return Err(undefined_function_error(
            snapshot, schema, name, message, span,
        ));
    };
    let cand = &candidates[best];
    if cand.ambiguous {
        return Err(not_unique());
    }
    let f = cand.proc;
    if matches!(f.prokind, ProKind::Procedure) {
        // PG classifies a procedure in an expression as wrong_object_type
        // (42809), not undefined_function.
        return Err(crate::error::RawError::new(
            AnalyzeError::WrongObjectType(format!("{qualified}({arg_list_actual}) is a procedure")),
            span,
            Some("to call a procedure, use CALL".into()),
        )
        .finalize_implicit());
    }

    // `ParseFuncOrColumn`: resolve the polymorphic parameters, the omitted
    // parameters taking part with their default expressions' types.
    let mut declared = cand.args.clone();
    let mut actuals = arg_types.to_vec();
    actuals.extend(
        cand.default_params
            .iter()
            .map(|&pp| default_arg_type(f, pp)),
    );
    let rettype = aggregate_final_return(f, snapshot).unwrap_or(f.prorettype);
    let (return_type_oid, poly) = crate::polymorphic::enforce_generic_type_consistency(
        &actuals,
        &mut declared,
        rettype,
        snapshot,
    )
    .map_err(|e| crate::error::RawError::new(e, span, None).finalize_implicit())?;
    declared.truncate(nargs);

    // An explicit `VARIADIC` argument to a `VARIADIC "any"` function must
    // be an array (`ParseFuncOrColumn`).
    const ANY: PgTypeOid = PgTypeOid::from_raw(2276);
    if notation.variadic
        && f.provariadic == Some(ANY)
        && let Some(&last) = arg_types.last()
        && crate::coerce::element_type(snapshot.unwrap_domain(last), snapshot).is_none()
    {
        return Err(crate::pgmsg::variadic_argument_must_be_array(None).finalize_implicit());
    }

    let out_args = build_out_args(f)
        .into_iter()
        .map(|field| OutArg {
            type_oid: poly.resolve(field.type_oid, snapshot),
            ..field
        })
        .collect();
    Ok(FuncDetail::Routine(ResolvedFunction {
        oid: f.oid,
        aggregate: snapshot
            .pg_aggregate
            .get(&f.oid)
            .map(|a| (a.aggkind, a.aggnumdirectargs)),
        nvargs: cand.nvargs,
        provariadic: f.provariadic,
        return_type_oid,
        arg_types: declared,
        schema: snapshot
            .namespace_name(f.pronamespace)
            .map(str::to_owned)
            .unwrap_or_default(),
        is_aggregate: matches!(f.prokind, ProKind::Aggregate),
        is_window: matches!(f.prokind, ProKind::Window),
        is_strict: f.proisstrict,
        is_set_returning: f.proretset,
        out_args,
        signature: format!(
            "{}({})",
            f.proname,
            f.proargtypes
                .iter()
                .map(|t| snapshot
                    .get_type(*t)
                    .map(|ty| ty.typname.as_str())
                    .unwrap_or("?"))
                .collect::<Vec<_>>()
                .join(",")
        ),
    }))
}

/// PG's function-style cast rule in `func_get_detail`: a one-argument call
/// with no exact match, named after a type, is a cast when the argument is
/// an untyped literal or the explicit coercion is a relabeling or an I/O
/// conversion (except record → string). Function-backed casts are left to
/// the type's conversion functions (`int4(numeric)` exists in `pg_proc`).
fn func_name_as_coercion(
    snapshot: &PgCatalog,
    schema: Option<&str>,
    name: &str,
    source: PgTypeOid,
    unknown_const: bool,
) -> Option<PgTypeOid> {
    use crate::coerce::{CoercionContext, CoercionPath, coercion_pathway};
    let target = snapshot.resolve_type_by_name(schema, name)?.oid;
    let is_coercion = (source == oid::UNKNOWN && unknown_const)
        || match coercion_pathway(target, source, CoercionContext::Explicit, snapshot) {
            Some(CoercionPath::Relabel) => true,
            Some(CoercionPath::CoerceViaIo) => {
                !((source == oid::RECORD || crate::coerce::is_complex(source, snapshot))
                    && crate::coerce::type_category(target, snapshot) == Some(TypCategory::String))
            }
            _ => false,
        };
    is_coercion.then_some(target)
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

/// PG's `FuncnameGetCandidates` (namespace.c) for a call with `nargs`
/// arguments: every overload that can take that many — as written, with a
/// variadic parameter expanded (unless the call used `VARIADIC`), or with
/// trailing parameters filled from defaults — in search-path order.
///
/// When two overloads offer the same signature (ignoring defaulted
/// parameters), the one earlier on the search path wins; within one schema
/// a non-variadic match beats a variadic expansion, and any other tie
/// (`f(int)` vs `f(int, text DEFAULT …)`) leaves an *ambiguous* entry that
/// fails the call as `not unique` if chosen.
fn func_candidates<'a>(
    snapshot: &'a PgCatalog,
    schema: Option<&str>,
    name: &str,
    nargs: usize,
    notation: &CallNotation,
) -> Vec<Candidate<'a>> {
    let path = snapshot.schemas_for_lookup(schema);
    let mut out: Vec<Candidate<'a>> = Vec::new();
    for f in snapshot.find_functions(schema, name) {
        let pathpos = path
            .iter()
            .position(|&ns| ns == f.pronamespace)
            .unwrap_or(0);
        let pronargs = f.proargtypes.len();
        let defaults = f.pronargdefaults.max(0) as usize;
        let expand_variadic = !notation.variadic;
        let cand = if !notation.names.is_empty() {
            // Named notation can reach a variadic function only through an
            // explicit `VARIADIC` argument: expanded elements have no name.
            if f.provariadic.is_some() && expand_variadic {
                continue;
            }
            if pronargs < nargs || (pronargs > nargs && nargs + defaults < pronargs) {
                continue;
            }
            let Some(order) = match_named_call(f, nargs, notation) else {
                continue;
            };
            Candidate {
                proc: f,
                args: order.iter().map(|&pp| f.proargtypes[pp]).collect(),
                default_params: order[nargs..].to_vec(),
                ndargs: pronargs - nargs,
                nvargs: 0,
                pathpos,
                ambiguous: false,
            }
        } else {
            let variadic_elem = f
                .provariadic
                .filter(|_| pronargs <= nargs && expand_variadic);
            let use_defaults = pronargs > nargs;
            if use_defaults && nargs + defaults < pronargs {
                continue;
            }
            if pronargs != nargs && variadic_elem.is_none() && !use_defaults {
                continue;
            }
            let mut args = f.proargtypes.clone();
            let mut nvargs = 0;
            if let Some(elem) = variadic_elem {
                nvargs = nargs - pronargs + 1;
                args.truncate(pronargs - 1);
                args.resize(nargs, elem);
            }
            Candidate {
                proc: f,
                args,
                ndargs: pronargs.saturating_sub(nargs),
                default_params: (nargs..pronargs).collect(),
                nvargs,
                pathpos,
                ambiguous: false,
            }
        };

        let cmp = cand.args.len() - cand.ndargs;
        let prev = out
            .iter()
            .position(|p| p.args.len() - p.ndargs == cmp && p.args[..cmp] == cand.args[..cmp]);
        let Some(prev) = prev else {
            out.push(cand);
            continue;
        };
        // `preference` > 0 keeps the earlier entry, < 0 replaces it, 0
        // marks it ambiguous.
        let preference: isize = if cand.pathpos != out[prev].pathpos {
            cand.pathpos as isize - out[prev].pathpos as isize
        } else if cand.nvargs > 0 && out[prev].nvargs == 0 {
            1
        } else if cand.nvargs == 0 && out[prev].nvargs > 0 {
            -1
        } else {
            0
        };
        match preference.cmp(&0) {
            std::cmp::Ordering::Greater => {}
            std::cmp::Ordering::Less => {
                out.remove(prev);
                out.push(cand);
            }
            std::cmp::Ordering::Equal => out[prev].ambiguous = true,
        }
    }
    out
}

/// PG's `MatchNamedCall` (namespace.c): map a named/mixed-notation call
/// with `nargs` arguments onto `f`'s input parameters. Returns the
/// parameter positions in the call's argument order, followed by the
/// omitted (defaulted) parameters in declaration order, or `None` when the
/// names don't fit — an unknown name, a name repeating a positional
/// argument, or an omitted parameter without a default.
fn match_named_call(f: &PgProc, nargs: usize, notation: &CallNotation) -> Option<Vec<usize>> {
    let pronargs = f.proargtypes.len();
    let defaults = f.pronargdefaults.max(0) as usize;
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
    let first_default = pronargs - defaults.min(pronargs);
    for (pp, &was_given) in given.iter().enumerate().skip(positional) {
        if !was_given {
            if pp < first_default {
                return None;
            }
            order.push(pp);
        }
    }
    Some(order)
}

/// The type of the default expression of `f`'s parameter `pp`
/// (`proargdefaults` covers the trailing `pronargdefaults` parameters).
fn default_arg_type(f: &PgProc, pp: usize) -> PgTypeOid {
    let first_default = f
        .proargtypes
        .len()
        .saturating_sub(f.pronargdefaults.max(0) as usize);
    pp.checked_sub(first_default)
        .and_then(|i| f.proargdefaulttypes.get(i))
        .copied()
        .unwrap_or(f.proargtypes[pp])
}

/// PG's `func_match_argtypes` (parse_func.c): the candidates (indexes into
/// `candidates`) whose parameters the actual arguments can be implicitly
/// coerced to (`can_coerce_type`).
pub(crate) fn func_match_argtypes(
    inputs: &[PgTypeOid],
    candidates: &[&[PgTypeOid]],
    snapshot: &PgCatalog,
) -> Vec<usize> {
    candidates
        .iter()
        .enumerate()
        .filter(|(_, args)| crate::coerce::can_coerce_types(inputs, args, snapshot))
        .map(|(i, _)| i)
        .collect()
}

/// PG's `IsPreferredType`: `t` is its category's preferred type and in
/// `category` (a missing category matches any).
fn is_preferred_type(category: Option<TypCategory>, t: PgTypeOid, snapshot: &PgCatalog) -> bool {
    let (cat, preferred) = crate::coerce::type_category_preferred(t, snapshot);
    (category.is_none() || category == cat) && preferred
}

/// PG's `func_select_candidate` (parse_func.c), shared by function and
/// operator resolution: pick one of several `candidates` (parameter lists,
/// all accepting `inputs`) or `None` when the call is ambiguous. With
/// domains smashed to their base types, keep the candidates with the most
/// exact matches, then with the most exact-or-preferred matches at known
/// positions; then give each unknown position a type category (STRING if
/// any candidate takes one there, else the one all candidates agree on)
/// and keep the candidates taking that category (its preferred type, when
/// one does); last, if all known inputs share one type, assume the unknowns
/// have it too.
pub(crate) fn func_select_candidate(
    inputs: &[PgTypeOid],
    candidates: &[&[PgTypeOid]],
    snapshot: &PgCatalog,
) -> Option<usize> {
    use crate::coerce::{type_category, type_category_preferred};
    let nargs = inputs.len();
    let base: Vec<PgTypeOid> = inputs
        .iter()
        .map(|&t| {
            if t == oid::UNKNOWN {
                t
            } else {
                snapshot.unwrap_domain(t)
            }
        })
        .collect();
    let nunknowns = base.iter().filter(|&&t| t == oid::UNKNOWN).count();
    let mut live: Vec<usize> = (0..candidates.len()).collect();
    let keep_best = |live: &mut Vec<usize>, score: &dyn Fn(usize) -> usize| {
        let best = live.iter().map(|&c| score(c)).max().unwrap_or(0);
        live.retain(|&c| score(c) == best);
    };

    // Most exact matches on the known inputs.
    keep_best(&mut live, &|c| {
        (0..nargs)
            .filter(|&i| base[i] != oid::UNKNOWN && candidates[c][i] == base[i])
            .count()
    });
    if let [one] = live[..] {
        return Some(one);
    }

    // Most exact-or-preferred matches (preferred within the input's own
    // category) on the known inputs.
    let slot_category: Vec<Option<TypCategory>> =
        base.iter().map(|&t| type_category(t, snapshot)).collect();
    keep_best(&mut live, &|c| {
        (0..nargs)
            .filter(|&i| {
                base[i] != oid::UNKNOWN
                    && (candidates[c][i] == base[i]
                        || is_preferred_type(slot_category[i], candidates[c][i], snapshot))
            })
            .count()
    });
    if let [one] = live[..] {
        return Some(one);
    }
    if nunknowns == 0 {
        return None;
    }

    // Resolve a category for every unknown position.
    let mut slots: Vec<(usize, Option<TypCategory>, bool)> = Vec::new();
    let mut resolved_unknowns = true;
    for i in (0..nargs).filter(|&i| base[i] == oid::UNKNOWN) {
        let mut category: Option<Option<TypCategory>> = None;
        let mut has_preferred = false;
        let mut conflict = false;
        for &c in &live {
            let (cat, preferred) = type_category_preferred(candidates[c][i], snapshot);
            match category {
                None => {
                    category = Some(cat);
                    has_preferred = preferred;
                }
                Some(sc) if sc == cat => has_preferred |= preferred,
                Some(_) if cat == Some(TypCategory::String) => {
                    // STRING always wins if available.
                    category = Some(cat);
                    has_preferred = preferred;
                }
                Some(_) => conflict = true,
            }
        }
        let category = category.flatten();
        if conflict && category != Some(TypCategory::String) {
            resolved_unknowns = false;
            break;
        }
        slots.push((i, category, has_preferred));
    }
    if resolved_unknowns {
        let kept: Vec<usize> = live
            .iter()
            .copied()
            .filter(|&c| {
                slots.iter().all(|&(i, category, has_preferred)| {
                    let (cat, preferred) = type_category_preferred(candidates[c][i], snapshot);
                    cat == category && (!has_preferred || preferred)
                })
            })
            .collect();
        if !kept.is_empty() {
            live = kept;
        }
        if let [one] = live[..] {
            return Some(one);
        }
    }

    // Last gasp: all known inputs share one type — assume the unknowns
    // have it too and look for a unique candidate accepting that.
    if nunknowns < nargs {
        let mut known = base.iter().copied().filter(|&t| t != oid::UNKNOWN);
        let first = known.next()?;
        if known.all(|t| t == first) {
            let assumed = vec![first; nargs];
            let mut fits = live
                .iter()
                .copied()
                .filter(|&c| crate::coerce::can_coerce_types(&assumed, candidates[c], snapshot));
            if let (Some(one), None) = (fits.next(), fits.next()) {
                return Some(one);
            }
        }
    }
    None
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

pub(crate) fn is_nullable_operator(name: &str) -> bool {
    NULLABLE_PG_CATALOG_OPERATORS.contains(&name)
}

/// Whether a `pg_catalog` routine's result can be NULL, given which of the
/// call's arguments can be (`builtin_nullability` has the derivation). A
/// strict function is NULL exactly on a NULL argument unless it is one of
/// the known NULL-returning overloads; a non-strict one is nullable unless
/// known never to be, or to be NULL only on NULL arguments.
pub(crate) fn builtin_result_nullable(
    resolved: &ResolvedFunction,
    args_nullable: &[bool],
    variadic_keyword: bool,
) -> bool {
    use crate::builtin_nullability::*;
    let sig = resolved.signature.as_str();
    let any_nullable = args_nullable.iter().any(|&n| n);
    if resolved.is_strict {
        any_nullable || NULLABLE_STRICT.contains(&sig)
    } else if NEVER_NULL_NONSTRICT.contains(&sig) {
        false
    } else if NULL_ONLY_ON_VARIADIC_NULL.contains(&sig) {
        variadic_keyword && any_nullable
    } else if sig == "concat_ws(text,any)" {
        // A NULL separator makes the result NULL; other NULLs are skipped.
        args_nullable.first() == Some(&true) || (variadic_keyword && any_nullable)
    } else {
        !NULL_ONLY_ON_NULL_ARG.contains(&sig) || any_nullable
    }
}
