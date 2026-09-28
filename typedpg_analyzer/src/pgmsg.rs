//! PG-verbatim error constructors — the single home for the analyzer's
//! error-message contract.
//!
//! Every message the analyzer can emit for a query PG would also reject
//! must *start with* PG's server-side wording (see `pg_sanity`); these
//! constructors define each wording exactly once and build the
//! [`AnalyzeError`] variant whose [`AnalyzeError::sqlstate`] matches the
//! code PostgreSQL attaches, so both halves of the contract (wording
//! prefix + SQLSTATE) are pinned in one place and cross-checked by the
//! mirrored suite.
//!
//! Two families intentionally live elsewhere:
//! - literal *input-syntax* messages (`malformed … literal`, out-of-range,
//!   …) are owned by [`crate::literal_input`], next to the validators that
//!   produce them — except the shared `invalid input syntax for type …`
//!   template, defined here as [`invalid_input_syntax_for_type`];
//! - clause-coercion wording (`argument of WHERE must be type boolean …`)
//!   is owned by [`crate::clause`], keyed by `ClauseKind`.

use crate::error::{AnalyzeError, RawError, SourceSpan};

/// `invalid input syntax for type T: "content"` — SQLSTATE 22P02
/// (`invalid_text_representation`), PG's shared input-function template.
/// `type_msg_name` is the *input function's* type-name string, which
/// differs from `format_type` for the timestamp family (`timestamptz` →
/// `timestamp with time zone`). Returns a plain `String`: the
/// [`crate::literal_input`] validators compose their errors as strings and
/// the caller attaches span/kind.
pub(crate) fn invalid_input_syntax_for_type(type_msg_name: &str, content: &str) -> String {
    format!("invalid input syntax for type {type_msg_name}: \"{content}\"")
}

/// `operator does not exist: <left> <op> <right>` — SQLSTATE 42883
/// (`undefined_function`). `left`/`right` are PG-rendered type names
/// (`format_type_for_message`).
pub(crate) fn operator_does_not_exist(
    left: &str,
    op: &str,
    right: &str,
    span: Option<SourceSpan>,
) -> RawError {
    RawError::undefined_operator(
        format!("operator does not exist: {left} {op} {right}"),
        span,
        None,
    )
}

/// `operator is not unique: <left> <op> <right>` — SQLSTATE 42725
/// (`ambiguous_function`): several overloads survived every resolution
/// tiebreak.
pub(crate) fn operator_is_not_unique(
    left: &str,
    op: &str,
    right: &str,
    span: Option<SourceSpan>,
) -> RawError {
    RawError::new(
        AnalyzeError::AmbiguousFunction(format!("operator is not unique: {left} {op} {right}")),
        span,
        Some("add an explicit type cast to one side, e.g. `expr::int4`".into()),
    )
}

/// `function name(types) is not unique` — SQLSTATE 42725.
pub(crate) fn function_is_not_unique(
    qualified_name: &str,
    arg_list: &str,
    span: Option<SourceSpan>,
) -> RawError {
    RawError::new(
        AnalyzeError::AmbiguousFunction(format!(
            "function {qualified_name}({arg_list}) is not unique"
        )),
        span,
        Some("add explicit type casts to the arguments to select one overload".into()),
    )
}

/// `argument name "a" used more than once` — SQLSTATE 42601: the same
/// name given twice in one named-notation call.
pub(crate) fn argument_name_used_more_than_once(name: &str, span: Option<SourceSpan>) -> RawError {
    RawError::new(
        AnalyzeError::SyntaxError(format!("argument name \"{name}\" used more than once")),
        span,
        None,
    )
}

/// `positional argument cannot follow named argument` — SQLSTATE 42601.
pub(crate) fn positional_argument_after_named(span: Option<SourceSpan>) -> RawError {
    RawError::new(
        AnalyzeError::SyntaxError("positional argument cannot follow named argument".into()),
        span,
        None,
    )
}

/// `GROUP BY position N is not in select list` / `ORDER BY position N is
/// not in select list` — SQLSTATE 42P10 (`invalid_column_reference`).
pub(crate) fn position_not_in_select_list(
    clause: &str,
    position: i64,
    span: Option<SourceSpan>,
) -> RawError {
    RawError::new(
        AnalyzeError::InvalidColumnReference(format!(
            "{clause} position {position} is not in select list"
        )),
        span,
        None,
    )
}

/// `for SELECT DISTINCT, ORDER BY expressions must appear in select list`
/// — SQLSTATE 42P10.
pub(crate) fn distinct_order_by_not_in_select_list(span: Option<SourceSpan>) -> RawError {
    RawError::new(
        AnalyzeError::InvalidColumnReference(
            "for SELECT DISTINCT, ORDER BY expressions must appear in select list".to_string(),
        ),
        span,
        None,
    )
}

/// `table name "u" specified more than once` — SQLSTATE 42712
/// (`duplicate_alias`).
pub(crate) fn duplicate_table_alias(alias: &str) -> RawError {
    RawError::new(
        AnalyzeError::DuplicateAlias(format!("table name \"{alias}\" specified more than once")),
        None,
        None,
    )
}

/// `table "t" has N columns available but M columns specified` — SQLSTATE
/// 42P10: a FROM column-alias list longer than the relation's width.
pub(crate) fn too_many_column_aliases(alias: &str, available: usize, specified: usize) -> RawError {
    RawError::new(
        AnalyzeError::InvalidColumnReference(format!(
            "table \"{alias}\" has {available} columns available but {specified} columns specified"
        )),
        None,
        None,
    )
}

/// `VALUES lists must all be the same length` — SQLSTATE 42601
/// (`syntax_error`).
pub(crate) fn values_lists_length(first_arity: usize, row_arity: usize) -> RawError {
    RawError::new(
        AnalyzeError::SyntaxError("VALUES lists must all be the same length".to_string()),
        None,
        Some(format!(
            "the first row has {first_arity} column(s), a later row has {row_arity}"
        )),
    )
}

/// `window "w" does not exist` — SQLSTATE 42704 (`undefined_object`): a
/// named-window reference with no matching WINDOW-clause definition.
pub(crate) fn window_does_not_exist(name: &str) -> RawError {
    RawError::new(
        AnalyzeError::UndefinedObject(format!("window \"{name}\" does not exist")),
        None,
        Some("define it in a WINDOW clause, e.g. `WINDOW w AS (ORDER BY …)`".into()),
    )
}

/// `column "x" specified in USING clause does not exist in left table`
/// (or `right table`) — SQLSTATE 42703 (`undefined_column`).
pub(crate) fn using_column_missing(column: &str, side: &str) -> RawError {
    RawError::new(
        AnalyzeError::UndefinedColumn(format!(
            "column \"{column}\" specified in USING clause does not exist in {side} table"
        )),
        None,
        None,
    )
}

/// `JOIN/USING types X and Y cannot be matched` — SQLSTATE 42804
/// (`datatype_mismatch`).
pub(crate) fn join_using_types_mismatch(left: &str, right: &str) -> RawError {
    RawError::new(
        AnalyzeError::DatatypeMismatch(format!(
            "JOIN/USING types {left} and {right} cannot be matched"
        )),
        None,
        None,
    )
}

/// `operator does not exist: X = Y (NULLIF types X and Y cannot be
/// matched)` — SQLSTATE 42883: NULLIF resolves `=` over its arguments, so
/// PG reports the operator-lookup failure; the parenthesized tail is our
/// extra detail (allowed by the prefix contract).
pub(crate) fn nullif_types_mismatch(left: &str, right: &str) -> AnalyzeError {
    AnalyzeError::UndefinedOperator(format!(
        "operator does not exist: {left} = {right} \
         (NULLIF types {left} and {right} cannot be matched)"
    ))
}

/// `{construct} types A and B cannot be matched` — SQLSTATE 42804. The
/// construct label and argument order are the caller's: CASE reports the
/// *last* branch first, COALESCE/GREATEST/UNION report source order, and
/// UNION appends a column suffix through `extra`.
pub(crate) fn types_cannot_be_matched(
    construct: &str,
    first: &str,
    second: &str,
    extra: &str,
    hint: Option<String>,
) -> RawError {
    RawError::new(
        AnalyzeError::DatatypeMismatch(format!(
            "{construct} types {first} and {second} cannot be matched{extra}"
        )),
        None,
        hint,
    )
}

/// `each UNION query must have the same number of columns` (likewise
/// INTERSECT / EXCEPT) — SQLSTATE 42601.
pub(crate) fn set_op_column_count(op_label: &str, left: usize, right: usize) -> RawError {
    RawError::new(
        AnalyzeError::SyntaxError(format!(
            "each {op_label} query must have the same number of columns"
        )),
        None,
        Some(format!(
            "the left side produces {left} column(s), the right side {right}"
        )),
    )
}

/// `could not find array type for data type T` — SQLSTATE 42704: typing a
/// bare parameter as `T[]` when no such array type exists (T is itself an
/// array).
pub(crate) fn no_array_type_for(type_name: &str) -> AnalyzeError {
    AnalyzeError::UndefinedObject(format!(
        "could not find array type for data type {type_name}"
    ))
}

/// `recursive query "r" column N has type X in non-recursive term but type
/// Y overall` — SQLSTATE 42804: PG fixes a recursive CTE's column types
/// from the non-recursive term alone.
pub(crate) fn recursive_query_column_type(
    cte_name: &str,
    column: usize,
    seed_type: &str,
    overall_type: &str,
) -> RawError {
    RawError::new(
        AnalyzeError::DatatypeMismatch(format!(
            "recursive query \"{cte_name}\" column {column} has type {seed_type} in \
             non-recursive term but type {overall_type} overall"
        )),
        None,
        Some(format!(
            "cast the non-recursive term's column to {overall_type}"
        )),
    )
}

/// `schema "s" does not exist` — SQLSTATE 3F000 (`invalid_schema_name`):
/// a qualified function/operator name whose schema is missing.
pub(crate) fn schema_does_not_exist(schema: &str, span: Option<SourceSpan>) -> RawError {
    RawError::new(
        AnalyzeError::UndefinedSchema(format!("schema \"{schema}\" does not exist")),
        span,
        None,
    )
}

/// `VARIADIC argument must be an array` — SQLSTATE 42804: an explicit
/// `VARIADIC` argument to a `VARIADIC "any"` function that isn't an array.
pub(crate) fn variadic_argument_must_be_array(span: Option<SourceSpan>) -> RawError {
    RawError::new(
        AnalyzeError::DatatypeMismatch("VARIADIC argument must be an array".into()),
        span,
        None,
    )
}

// ── Polymorphic argument resolution (`enforce_generic_type_consistency`) ──
// Every message below is SQLSTATE 42804 (`datatype_mismatch`).

/// `could not determine polymorphic type because input has type unknown`,
/// or with a family name (`… polymorphic type anycompatiblerange because
/// …`) for the families that can't fall back to `text`.
pub(crate) fn polymorphic_type_from_unknown(family: Option<&str>) -> AnalyzeError {
    AnalyzeError::DatatypeMismatch(match family {
        None => "could not determine polymorphic type because input has type unknown".into(),
        Some(f) => {
            format!("could not determine polymorphic type {f} because input has type unknown")
        }
    })
}

/// `arguments declared "anyelement" are not all alike`.
pub(crate) fn polymorphic_args_not_alike(declared: &str) -> AnalyzeError {
    AnalyzeError::DatatypeMismatch(format!(
        "arguments declared \"{declared}\" are not all alike"
    ))
}

/// `argument declared anyarray is not consistent with argument declared
/// anyelement`.
pub(crate) fn polymorphic_args_inconsistent(declared: &str, other: &str) -> AnalyzeError {
    AnalyzeError::DatatypeMismatch(format!(
        "argument declared {declared} is not consistent with argument declared {other}"
    ))
}

/// `argument declared anyarray is not an array but type integer` (`kind`
/// is `an array`, `a range type` or `a multirange type`).
pub(crate) fn polymorphic_arg_wrong_kind(declared: &str, kind: &str, actual: &str) -> AnalyzeError {
    AnalyzeError::DatatypeMismatch(format!(
        "argument declared {declared} is not {kind} but type {actual}"
    ))
}

/// `type matched to anynonarray is an array type: integer[]` /
/// `type matched to anyenum is not an enum type: integer`.
pub(crate) fn polymorphic_match_wrong_kind(declared: &str, what: &str, ty: &str) -> AnalyzeError {
    AnalyzeError::DatatypeMismatch(format!("type matched to {declared} {what}: {ty}"))
}

/// `cannot determine element type of "anyarray" argument`.
pub(crate) fn anyarray_element_undetermined() -> AnalyzeError {
    AnalyzeError::DatatypeMismatch("cannot determine element type of \"anyarray\" argument".into())
}

/// `arguments of anycompatible family cannot be cast to a common type`.
pub(crate) fn anycompatible_no_common_type() -> AnalyzeError {
    AnalyzeError::DatatypeMismatch(
        "arguments of anycompatible family cannot be cast to a common type".into(),
    )
}

/// `anycompatiblerange type int4range does not match anycompatible type
/// numeric` (also for `anycompatiblemultirange`).
pub(crate) fn anycompatible_range_mismatch(
    family: &str,
    range: &str,
    common: &str,
) -> AnalyzeError {
    AnalyzeError::DatatypeMismatch(format!(
        "{family} type {range} does not match anycompatible type {common}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every constructor must build a variant whose `sqlstate()` is the
    /// code its rustdoc documents — keeps the constructors and the
    /// variant → SQLSTATE mapping from drifting apart. The codes
    /// themselves are validated against the live PG by the `pg_sanity`
    /// suite.
    #[test]
    fn constructors_carry_their_documented_sqlstates() {
        let cases: Vec<(AnalyzeError, &str)> = vec![
            (
                operator_does_not_exist("integer", "+", "point", None).kind,
                "42883",
            ),
            (
                operator_is_not_unique("unknown", "+", "unknown", None).kind,
                "42725",
            ),
            (
                function_is_not_unique("mod", "unknown, unknown", None).kind,
                "42725",
            ),
            (
                position_not_in_select_list("GROUP BY", 9, None).kind,
                "42P10",
            ),
            (
                position_not_in_select_list("ORDER BY", 9, None).kind,
                "42P10",
            ),
            (distinct_order_by_not_in_select_list(None).kind, "42P10"),
            (duplicate_table_alias("u").kind, "42712"),
            (too_many_column_aliases("t", 1, 2).kind, "42P10"),
            (values_lists_length(2, 1).kind, "42601"),
            (window_does_not_exist("w").kind, "42704"),
            (using_column_missing("id", "left").kind, "42703"),
            (join_using_types_mismatch("integer", "point").kind, "42804"),
            (nullif_types_mismatch("integer", "point"), "42883"),
            (
                types_cannot_be_matched("CASE", "integer", "point", "", None).kind,
                "42804",
            ),
            (set_op_column_count("UNION", 2, 1).kind, "42601"),
            (no_array_type_for("integer[]"), "42704"),
            (schema_does_not_exist("s", None).kind, "3F000"),
            (variadic_argument_must_be_array(None).kind, "42804"),
            (polymorphic_type_from_unknown(None), "42804"),
            (polymorphic_args_not_alike("anyelement"), "42804"),
            (
                polymorphic_args_inconsistent("anyarray", "anyelement"),
                "42804",
            ),
            (
                polymorphic_arg_wrong_kind("anyarray", "an array", "integer"),
                "42804",
            ),
            (
                polymorphic_match_wrong_kind("anyenum", "is not an enum type", "integer"),
                "42804",
            ),
            (anyarray_element_undetermined(), "42804"),
            (anycompatible_no_common_type(), "42804"),
            (
                anycompatible_range_mismatch("anycompatiblerange", "int4range", "numeric"),
                "42804",
            ),
            (
                recursive_query_column_type("r", 1, "integer", "text").kind,
                "42804",
            ),
        ];
        for (err, want) in cases {
            assert_eq!(
                err.sqlstate(),
                Some(want),
                "constructor for {:?} should carry SQLSTATE {want}",
                err.to_string().lines().next().unwrap_or(""),
            );
        }
    }
}
