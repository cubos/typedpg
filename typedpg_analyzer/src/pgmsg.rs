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

/// PG's hint on `operator does not exist` (`op_error`).
pub(crate) const NO_OPERATOR_MATCHES_HINT: &str = "No operator matches the given name and \
                                                   argument types. You might need to add \
                                                   explicit type casts.";

/// PG's hint when an assigned value doesn't coerce to its target
/// (`transformAssignedExpr`).
pub(crate) const REWRITE_OR_CAST_HINT: &str = "You will need to rewrite or cast the expression.";

/// Add to a type-mismatch error, when an explicit cast from `actual` to
/// `expected` exists (assignment coercion failed, so it is explicit-only),
/// a note saying so — `text` to `integer` gets "an explicit cast from text
/// to integer exists: `expr::integer`". When there is none, the value has
/// to be rewritten and the note would mislead.
pub(crate) fn with_explicit_cast_note(
    err: RawError,
    snapshot: &crate::pg_catalog::PgCatalog,
    actual: crate::oid::PgTypeOid,
    expected: crate::oid::PgTypeOid,
) -> RawError {
    use crate::coerce::{CoercionContext, coercion_pathway};
    if actual == crate::pg_catalog::oid::UNKNOWN
        || coercion_pathway(expected, actual, CoercionContext::Explicit, snapshot).is_none()
    {
        return err;
    }
    let from = crate::ddl::util::format_type_for_message(snapshot, actual);
    let to = crate::ddl::util::format_type_for_message(snapshot, expected);
    err.with_note(format!(
        "an explicit cast from {from} to {to} exists: `expr::{to}`"
    ))
}

/// `operator does not exist: <left> <op> <right>` — SQLSTATE 42883
/// (`undefined_function`), with PG's hint. `left`/`right` are PG-rendered
/// type names (`format_type_for_message`).
pub(crate) fn operator_does_not_exist(
    left: &str,
    op: &str,
    right: &str,
    span: Option<SourceSpan>,
) -> RawError {
    RawError::undefined_operator(
        format!("operator does not exist: {left} {op} {right}"),
        span,
        Some(NO_OPERATOR_MATCHES_HINT.to_owned()),
    )
}

/// `operator is only a shell: <left> <op> <right>` — SQLSTATE 42883
/// (`undefined_function`): the chosen operator was only named as another
/// one's commutator or negator (`make_op`). A prefix operator has no left.
pub(crate) fn operator_is_only_a_shell(left: Option<&str>, op: &str, right: &str) -> RawError {
    let signature = match left {
        Some(left) => format!("{left} {op} {right}"),
        None => format!("{op} {right}"),
    };
    RawError::undefined_operator(format!("operator is only a shell: {signature}"), None, None)
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

/// `operator does not exist: <op> <right>` — SQLSTATE 42883, the prefix
/// form (PG's `op_signature_string` omits the missing left operand).
pub(crate) fn prefix_operator_does_not_exist(
    op: &str,
    right: &str,
    span: Option<SourceSpan>,
) -> RawError {
    RawError::undefined_operator(
        format!("operator does not exist: {op} {right}"),
        span,
        Some(NO_OPERATOR_MATCHES_HINT.to_owned()),
    )
}

/// `operator is not unique: <op> <right>` — SQLSTATE 42725, prefix form.
pub(crate) fn prefix_operator_is_not_unique(
    op: &str,
    right: &str,
    span: Option<SourceSpan>,
) -> RawError {
    RawError::new(
        AnalyzeError::AmbiguousFunction(format!("operator is not unique: {op} {right}")),
        span,
        Some("add an explicit type cast to the operand, e.g. `expr::int4`".into()),
    )
}

/// `procedure p(args) is not unique` — SQLSTATE 42725: a `CALL` whose
/// overload resolution left several candidates (ParseFuncOrColumn with
/// `proc_call`).
pub(crate) fn procedure_is_not_unique(
    qualified_name: &str,
    arg_list: &str,
    span: Option<SourceSpan>,
) -> RawError {
    RawError::new(
        AnalyzeError::AmbiguousFunction(format!(
            "procedure {qualified_name}({arg_list}) is not unique"
        )),
        span,
        Some("add explicit type casts to the arguments to select one overload".into()),
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

/// rewriteTargetListIU (INSERT): `cannot insert a non-DEFAULT value into
/// column "c"` — SQLSTATE 428C9. `identity` selects the GENERATED ALWAYS
/// identity detail (and PG's OVERRIDING hint) over the generated-column one.
pub(crate) fn insert_non_default_into_generated(column: &str, identity: bool) -> RawError {
    let detail = if identity {
        format!("Column \"{column}\" is an identity column defined as GENERATED ALWAYS.")
    } else {
        format!("Column \"{column}\" is a generated column.")
    };
    RawError::new(
        AnalyzeError::GeneratedAlways(format!(
            "cannot insert a non-DEFAULT value into column \"{column}\" ({detail})"
        )),
        None,
        identity.then(|| "Use OVERRIDING SYSTEM VALUE to override.".to_owned()),
    )
}

/// rewriteTargetListIU (UPDATE): `column "c" can only be updated to
/// DEFAULT` — SQLSTATE 428C9.
pub(crate) fn update_generated_to_non_default(column: &str, identity: bool) -> RawError {
    let detail = if identity {
        format!("Column \"{column}\" is an identity column defined as GENERATED ALWAYS.")
    } else {
        format!("Column \"{column}\" is a generated column.")
    };
    RawError::new(
        AnalyzeError::GeneratedAlways(format!(
            "column \"{column}\" can only be updated to DEFAULT ({detail})"
        )),
        None,
        None,
    )
}

/// error_view_not_updatable: `cannot insert into view "v"` / `cannot update
/// view "v"` / `cannot delete from view "v"` — SQLSTATE 55000, with the
/// reason as detail and PG's hint (`merge` picks MERGE's trigger-only hint).
pub(crate) fn view_not_updatable(
    command: crate::resolve::DmlEvent,
    view: &str,
    detail: &str,
    merge: bool,
) -> RawError {
    use crate::resolve::DmlEvent;
    let (message, verb, event) = match command {
        DmlEvent::Insert => (
            format!("cannot insert into view \"{view}\""),
            "inserting into",
            "INSERT",
        ),
        DmlEvent::Update => (
            format!("cannot update view \"{view}\""),
            "updating",
            "UPDATE",
        ),
        DmlEvent::Delete => (
            format!("cannot delete from view \"{view}\""),
            "deleting from",
            "DELETE",
        ),
    };
    let hint = if merge {
        format!("To enable {verb} the view using MERGE, provide an INSTEAD OF {event} trigger.")
    } else {
        format!(
            "To enable {verb} the view, provide an INSTEAD OF {event} trigger or an unconditional \
             ON {event} DO INSTEAD rule."
        )
    };
    RawError::new(
        AnalyzeError::ObjectNotInPrerequisiteState(format!("{message} ({detail})")),
        None,
        Some(hint),
    )
}

/// rewriteTargetView: `cannot insert into column "c" of view "v"` (`update`,
/// `merge into`) — SQLSTATE 0A000, with view_col_is_auto_updatable's reason.
pub(crate) fn view_column_not_updatable(
    verb: &str,
    column: &str,
    view: &str,
    detail: &str,
) -> RawError {
    RawError::new(
        AnalyzeError::FeatureNotSupported(format!(
            "cannot {verb} column \"{column}\" of view \"{view}\" ({detail})"
        )),
        None,
        None,
    )
}

/// rewriteTargetView: `cannot merge into view "v"` when only some MERGE
/// actions have INSTEAD OF triggers — SQLSTATE 0A000.
pub(crate) fn merge_view_partial_instead_triggers(view: &str) -> RawError {
    RawError::new(
        AnalyzeError::FeatureNotSupported(format!(
            "cannot merge into view \"{view}\" (MERGE is not supported for views with INSTEAD OF \
             triggers for some actions but not all.)"
        )),
        None,
        Some(
            "To enable merging into the view, either provide a full set of INSTEAD OF triggers \
             or drop the existing INSTEAD OF triggers."
                .to_owned(),
        ),
    )
}

/// validate_relation_kind (table_open): `cannot open relation "i"` for an
/// index or a composite type — SQLSTATE 42809.
pub(crate) fn cannot_open_relation(relation: &str, kind: crate::pg_catalog::RelKind) -> RawError {
    RawError::new(
        AnalyzeError::WrongObjectType(format!(
            "cannot open relation \"{relation}\" (This operation is not supported for {}.)",
            kind.plural()
        )),
        None,
        None,
    )
}

/// transformMergeStmt: `cannot execute MERGE on relation "r"` for a
/// relation other than a table or a view — SQLSTATE 0A000.
pub(crate) fn merge_on_relation_kind(relation: &str, kind: crate::pg_catalog::RelKind) -> RawError {
    RawError::new(
        AnalyzeError::FeatureNotSupported(format!(
            "cannot execute MERGE on relation \"{relation}\" (This operation is not supported \
             for {}.)",
            kind.plural()
        )),
        None,
        None,
    )
}

/// CheckValidResultRel: `cannot change materialized view "mv"` / `cannot
/// change sequence "s"` — SQLSTATE 42809.
pub(crate) fn cannot_change_relation(kind: &str, relation: &str) -> RawError {
    RawError::new(
        AnalyzeError::WrongObjectType(format!("cannot change {kind} \"{relation}\"")),
        None,
        None,
    )
}

/// RewriteQuery: `infinite recursion detected in rules for relation "r"` —
/// SQLSTATE 42P17.
pub(crate) fn infinite_rule_recursion(relation: &str) -> RawError {
    RawError::new(
        AnalyzeError::InvalidObjectDefinition(format!(
            "infinite recursion detected in rules for relation \"{relation}\""
        )),
        None,
        None,
    )
}

/// matchLocks: `cannot execute MERGE on relation "r"` — SQLSTATE 0A000.
pub(crate) fn merge_on_relation_with_rules(relation: &str) -> RawError {
    RawError::new(
        AnalyzeError::FeatureNotSupported(format!(
            "cannot execute MERGE on relation \"{relation}\" (MERGE is not supported for \
             relations with rules.)"
        )),
        None,
        None,
    )
}

/// RewriteQuery: `INSERT with ON CONFLICT clause cannot be used with table
/// that has INSERT or UPDATE rules` — SQLSTATE 0A000.
pub(crate) fn on_conflict_with_rules() -> RawError {
    RawError::new(
        AnalyzeError::FeatureNotSupported(
            "INSERT with ON CONFLICT clause cannot be used with table that has INSERT or UPDATE \
             rules"
                .into(),
        ),
        None,
        None,
    )
}

/// RewriteQuery: `cannot perform INSERT RETURNING on relation "r"` (UPDATE,
/// DELETE) when an INSTEAD rule without RETURNING replaces the statement —
/// SQLSTATE 0A000.
pub(crate) fn returning_without_instead_rule_returning(
    command: crate::resolve::DmlEvent,
    relation: &str,
) -> RawError {
    let event = match command {
        crate::resolve::DmlEvent::Insert => "INSERT",
        crate::resolve::DmlEvent::Update => "UPDATE",
        crate::resolve::DmlEvent::Delete => "DELETE",
    };
    RawError::new(
        AnalyzeError::FeatureNotSupported(format!(
            "cannot perform {event} RETURNING on relation \"{relation}\""
        )),
        None,
        Some(format!(
            "You need an unconditional ON {event} DO INSTEAD rule with a RETURNING clause."
        )),
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

/// `cannot assign to system column "ctid"` — SQLSTATE 0A000: an UPDATE /
/// ON CONFLICT DO UPDATE / MERGE UPDATE SET item naming a system column
/// (`transformAssignedExpr`).
pub(crate) fn cannot_assign_to_system_column(column: &str, span: Option<SourceSpan>) -> RawError {
    RawError::new(
        AnalyzeError::FeatureNotSupported(format!("cannot assign to system column \"{column}\"")),
        span,
        None,
    )
}

/// `name "t" specified more than once` — SQLSTATE 42712: a MERGE whose
/// target and data source share a name (`transformMergeStmt`).
pub(crate) fn merge_name_specified_twice(name: &str) -> RawError {
    RawError::new(
        AnalyzeError::DuplicateAlias(format!(
            "name \"{name}\" specified more than once (The name is used both as MERGE target \
             table and data source.)"
        )),
        None,
        None,
    )
}

/// `unreachable WHEN clause specified after unconditional WHEN clause` —
/// SQLSTATE 42601 (`transformMergeStmt`).
pub(crate) fn merge_unreachable_when_clause() -> RawError {
    RawError::new(
        AnalyzeError::SyntaxError(
            "unreachable WHEN clause specified after unconditional WHEN clause".into(),
        ),
        None,
        None,
    )
}

/// `INSERT has more expressions than target columns` — SQLSTATE 42601
/// (`transformInsertRow`).
pub(crate) fn insert_more_expressions_than_targets() -> RawError {
    RawError::new(
        AnalyzeError::SyntaxError("INSERT has more expressions than target columns".into()),
        None,
        None,
    )
}

/// `INSERT has more target columns than expressions` — SQLSTATE 42601
/// (`transformInsertRow`).
pub(crate) fn insert_more_targets_than_expressions() -> RawError {
    RawError::new(
        AnalyzeError::SyntaxError("INSERT has more target columns than expressions".into()),
        None,
        None,
    )
}

/// `OLD cannot be specified multiple times` (or `NEW …`) — SQLSTATE 42601:
/// a `RETURNING WITH (…)` list naming the same row twice
/// (`transformReturningClause`).
pub(crate) fn returning_option_repeated(kind: &str, span: Option<SourceSpan>) -> RawError {
    RawError::new(
        AnalyzeError::SyntaxError(format!("{kind} cannot be specified multiple times")),
        span,
        None,
    )
}

/// `RETURNING must have at least one column` — SQLSTATE 42601: a nonempty
/// RETURNING list whose stars expanded to nothing (a zero-column table).
pub(crate) fn returning_without_columns(span: Option<SourceSpan>) -> RawError {
    RawError::new(
        AnalyzeError::SyntaxError("RETURNING must have at least one column".into()),
        span,
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

/// `join expression "j" has 4 columns available but 5 columns specified` —
/// SQLSTATE 42P10 (`addRangeTableEntryForJoin`): an aliased JOIN's column
/// alias list is longer than its output.
pub(crate) fn too_many_join_column_aliases(
    alias: &str,
    available: usize,
    specified: usize,
) -> RawError {
    RawError::new(
        AnalyzeError::InvalidColumnReference(format!(
            "join expression \"{alias}\" has {available} columns available but {specified} columns specified"
        )),
        None,
        None,
    )
}

/// `there is no unique or exclusion constraint matching the ON CONFLICT
/// specification` — SQLSTATE 42P10 (`infer_arbiter_indexes`). The table
/// name is our trailing detail; PG's message ends before it.
pub(crate) fn no_on_conflict_arbiter(table: &str) -> RawError {
    RawError::new(
        AnalyzeError::InvalidColumnReference(format!(
            "there is no unique or exclusion constraint matching the ON CONFLICT \
             specification on table \"{table}\""
        )),
        None,
        None,
    )
}

/// `failed to find conversion function from unknown to text` — SQLSTATE
/// XX000, raised by PG's `coerce_type` when an `unknown` value that is
/// neither a literal nor a parameter must be coerced implicitly: a field
/// selected from an anonymous record over an untyped literal,
/// `(ROW(1, 'x')).f2`, used as an output column or an argument.
pub(crate) fn unknown_field_not_coercible(target: &str, span: Option<SourceSpan>) -> RawError {
    RawError::new(
        AnalyzeError::PgInternalError(format!(
            "failed to find conversion function from unknown to {target}"
        )),
        span,
        Some("cast the field explicitly, e.g. `(r).f2::text`".into()),
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

/// `common column name "x" appears more than once in left table` (or
/// `right table`) — SQLSTATE 42702 (`ambiguous_column`).
pub(crate) fn using_column_ambiguous(column: &str, side: &str) -> RawError {
    RawError::new(
        AnalyzeError::AmbiguousColumn(format!(
            "common column name \"{column}\" appears more than once in {side} table"
        )),
        None,
        None,
    )
}

/// `column name "x" appears more than once in USING clause` — SQLSTATE
/// 42701 (`duplicate_column`).
pub(crate) fn using_column_listed_twice(column: &str) -> RawError {
    RawError::new(
        AnalyzeError::DuplicateColumn(format!(
            "column name \"{column}\" appears more than once in USING clause"
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

/// A parse error from `typedpg_pg_query`, which carries PG's message but not its
/// SQLSTATE. Most grammar errors are `syntax_error` (42601), but gram.y
/// raises a few with another code; those messages (from PG 18's gram.y,
/// the query-level ones) get the variant carrying that code — the frame
/// bound checks of `opt_frame_clause` are `windowing_error` (42P20), a
/// non-constant JSON_TABLE path `feature_not_supported` (0A000).
pub(crate) fn grammar_error(message: String, span: Option<SourceSpan>) -> RawError {
    const WINDOWING: &[&str] = &[
        "frame start cannot be UNBOUNDED FOLLOWING",
        "frame starting from following row cannot end with current row",
        "frame end cannot be UNBOUNDED PRECEDING",
        "frame starting from current row cannot have preceding rows",
        "frame starting from following row cannot have preceding rows",
    ];
    const FEATURE_NOT_SUPPORTED: &[&str] = &[
        "UNIQUE predicate is not yet implemented",
        "only string constants are supported in JSON_TABLE path specification",
    ];
    // `invalid_parameter_value` (22023) has no dedicated variant: the
    // multi-code `Invalid` bucket is compared on wording only.
    const INVALID_PARAMETER_VALUE: &[&str] = &[
        "precision for type float must be at least 1 bit",
        "precision for type float must be less than 54 bits",
        "unrecognized JSON encoding: ",
    ];
    let kind = if WINDOWING.iter().any(|m| message.starts_with(m)) {
        AnalyzeError::WindowingError(message)
    } else if FEATURE_NOT_SUPPORTED.iter().any(|m| message.starts_with(m)) {
        AnalyzeError::FeatureNotSupported(message)
    } else if INVALID_PARAMETER_VALUE
        .iter()
        .any(|m| message.starts_with(m))
    {
        AnalyzeError::Invalid(message)
    } else {
        AnalyzeError::Parse(message)
    };
    RawError::new(kind, span, None)
}

/// `cross-database references are not implemented: a.b.c` — SQLSTATE
/// 0A000: a name qualified by a database (catalog) name. PG accepts the
/// qualifier only when it names the current database, which the analyzer
/// cannot know — the application's database name is not part of its
/// migrations — so every catalog qualifier is taken as another database.
pub(crate) fn cross_database_reference(name: &str, span: Option<SourceSpan>) -> RawError {
    RawError::new(
        AnalyzeError::FeatureNotSupported(format!(
            "cross-database references are not implemented: {name}"
        )),
        span,
        Some(
            "drop the database qualifier: typedpg cannot tell whether it names the              application's database"
                .into(),
        ),
    )
}

/// `improper qualified name (too many dotted names): a.b.c.d` — SQLSTATE
/// 42601 (DeconstructQualifiedName, transformColumnRef).
pub(crate) fn improper_qualified_name(name: &str, span: Option<SourceSpan>) -> RawError {
    RawError::new(
        AnalyzeError::SyntaxError(format!(
            "improper qualified name (too many dotted names): {name}"
        )),
        span,
        None,
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

// ── Call modifiers on the wrong kind of routine (ParseFuncOrColumn) ──────

/// `DISTINCT specified, but lower is not an aggregate function` (also
/// `lower(*) specified`, `WITHIN GROUP`, `ORDER BY`, `FILTER`) — SQLSTATE
/// 42809.
pub(crate) fn not_an_aggregate(modifier: &str, name: &str, span: Option<SourceSpan>) -> RawError {
    RawError::new(
        AnalyzeError::WrongObjectType(format!(
            "{modifier} specified, but {name} is not an aggregate function"
        )),
        span,
        None,
    )
}

/// `WITHIN GROUP is required for ordered-set aggregate mode` — 42809.
pub(crate) fn within_group_required(name: &str, span: Option<SourceSpan>) -> RawError {
    RawError::new(
        AnalyzeError::WrongObjectType(format!(
            "WITHIN GROUP is required for ordered-set aggregate {name}"
        )),
        span,
        None,
    )
}

/// `count is not an ordered-set aggregate, so it cannot have WITHIN GROUP`
/// — 42809.
pub(crate) fn not_an_ordered_set_aggregate(name: &str, span: Option<SourceSpan>) -> RawError {
    RawError::new(
        AnalyzeError::WrongObjectType(format!(
            "{name} is not an ordered-set aggregate, so it cannot have WITHIN GROUP"
        )),
        span,
        None,
    )
}

/// `window function rank cannot have WITHIN GROUP` — 42809.
pub(crate) fn window_function_within_group(name: &str, span: Option<SourceSpan>) -> RawError {
    RawError::new(
        AnalyzeError::WrongObjectType(format!("window function {name} cannot have WITHIN GROUP")),
        span,
        None,
    )
}

/// `count(*) must be used to call a parameterless aggregate function` —
/// 42809.
pub(crate) fn parameterless_aggregate_needs_star(name: &str, span: Option<SourceSpan>) -> RawError {
    RawError::new(
        AnalyzeError::WrongObjectType(format!(
            "{name}(*) must be used to call a parameterless aggregate function"
        )),
        span,
        None,
    )
}

/// A PG `feature_not_supported` (0A000) message for aggregate / window
/// call shapes the parser accepts but PG doesn't implement — the wording is
/// one of [`NOT_IMPLEMENTED_CALL_SHAPES`].
pub(crate) fn call_shape_not_implemented(message: &str, span: Option<SourceSpan>) -> RawError {
    debug_assert!(
        NOT_IMPLEMENTED_CALL_SHAPES
            .iter()
            .any(|m| message.starts_with(m))
    );
    RawError::new(
        AnalyzeError::FeatureNotSupported(message.into()),
        span,
        None,
    )
}

/// PG's 0A000 wordings for aggregate / window call shapes.
pub(crate) const NOT_IMPLEMENTED_CALL_SHAPES: &[&str] = &[
    "DISTINCT is not implemented for window functions",
    "aggregate ORDER BY is not implemented for window functions",
    "FILTER is not implemented for non-aggregate window functions",
    "aggregates cannot use named arguments",
    "OVER is not supported for ordered-set aggregate ",
];

/// `in an aggregate with DISTINCT, ORDER BY expressions must appear in
/// argument list` — SQLSTATE 42P10.
pub(crate) fn distinct_aggregate_order_by_not_in_args(span: Option<SourceSpan>) -> RawError {
    RawError::new(
        AnalyzeError::InvalidColumnReference(
            "in an aggregate with DISTINCT, ORDER BY expressions must appear in argument list"
                .into(),
        ),
        span,
        None,
    )
}

/// `aggregate functions are not allowed in FILTER` — SQLSTATE 42803.
pub(crate) fn aggregate_in_filter(span: Option<SourceSpan>) -> RawError {
    RawError::new(
        AnalyzeError::GroupingError("aggregate functions are not allowed in FILTER".into()),
        span,
        None,
    )
}

/// `window functions are not allowed in FILTER` — SQLSTATE 42P20.
pub(crate) fn window_in_filter(span: Option<SourceSpan>) -> RawError {
    RawError::new(
        AnalyzeError::WindowingError("window functions are not allowed in FILTER".into()),
        span,
        None,
    )
}

// ── Window definitions (transformWindowDefinitions, transformFrameOffset) ─

fn windowing(message: String, span: Option<SourceSpan>, hint: Option<String>) -> RawError {
    RawError::new(AnalyzeError::WindowingError(message), span, hint)
}

/// `window functions are not allowed in window definitions` — 42P20.
pub(crate) fn window_in_window_definition(span: Option<SourceSpan>) -> RawError {
    windowing(
        "window functions are not allowed in window definitions".into(),
        span,
        None,
    )
}

/// `window "w" is already defined` — 42P20.
pub(crate) fn window_already_defined(name: &str, span: Option<SourceSpan>) -> RawError {
    windowing(format!("window \"{name}\" is already defined"), span, None)
}

/// `cannot override PARTITION BY clause of window "w"` (or `ORDER BY`) —
/// 42P20.
pub(crate) fn cannot_override_window_clause(
    clause: &str,
    window: &str,
    span: Option<SourceSpan>,
) -> RawError {
    windowing(
        format!("cannot override {clause} clause of window \"{window}\""),
        span,
        None,
    )
}

/// `cannot copy window "w" because it has a frame clause` — 42P20; a bare
/// `OVER (w)` gets PG's hint to drop the parentheses.
pub(crate) fn cannot_copy_window_with_frame(
    window: &str,
    bare_over: bool,
    span: Option<SourceSpan>,
) -> RawError {
    windowing(
        format!("cannot copy window \"{window}\" because it has a frame clause"),
        span,
        bare_over.then(|| "Omit the parentheses in this OVER clause.".into()),
    )
}

/// `RANGE with offset PRECEDING/FOLLOWING requires exactly one ORDER BY
/// column` — 42P20.
pub(crate) fn range_offset_needs_one_order_by(span: Option<SourceSpan>) -> RawError {
    windowing(
        "RANGE with offset PRECEDING/FOLLOWING requires exactly one ORDER BY column".into(),
        span,
        None,
    )
}

/// `GROUPS mode requires an ORDER BY clause` — 42P20.
pub(crate) fn groups_needs_order_by(span: Option<SourceSpan>) -> RawError {
    windowing("GROUPS mode requires an ORDER BY clause".into(), span, None)
}

/// `argument of ROWS must not contain variables` (RANGE / GROUPS too) —
/// SQLSTATE 42P10.
pub(crate) fn frame_offset_has_variables(construct: &str, span: Option<SourceSpan>) -> RawError {
    RawError::new(
        AnalyzeError::InvalidColumnReference(format!(
            "argument of {construct} must not contain variables"
        )),
        span,
        None,
    )
}

/// `RANGE with offset PRECEDING/FOLLOWING is not supported for column type
/// text` (with `and offset type integer` when support exists for other
/// offset types) — SQLSTATE 0A000.
pub(crate) fn range_offset_unsupported(
    key: &str,
    offset: Option<&str>,
    span: Option<SourceSpan>,
) -> RawError {
    let (message, hint) = match offset {
        None => (
            format!("RANGE with offset PRECEDING/FOLLOWING is not supported for column type {key}"),
            None,
        ),
        Some(o) => (
            format!(
                "RANGE with offset PRECEDING/FOLLOWING is not supported for column type {key} \
                 and offset type {o}"
            ),
            Some("Cast the offset value to an appropriate type.".into()),
        ),
    };
    RawError::new(AnalyzeError::FeatureNotSupported(message), span, hint)
}

/// `RANGE with offset PRECEDING/FOLLOWING has multiple interpretations for
/// column type X and offset type Y` — SQLSTATE 0A000.
pub(crate) fn range_offset_ambiguous(
    key: &str,
    offset: &str,
    span: Option<SourceSpan>,
) -> RawError {
    RawError::new(
        AnalyzeError::FeatureNotSupported(format!(
            "RANGE with offset PRECEDING/FOLLOWING has multiple interpretations for column \
             type {key} and offset type {offset}"
        )),
        span,
        Some("Cast the offset value to the exact intended type.".into()),
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

/// `a column definition list is required for functions returning "record"`
/// — SQLSTATE 42601 (`addRangeTableEntryForFunction`).
pub(crate) fn coldeflist_required() -> RawError {
    RawError::new(
        AnalyzeError::SyntaxError(
            "a column definition list is required for functions returning \"record\"".into(),
        ),
        None,
        Some("add one after the alias, e.g. `AS x(a int, b text)`".into()),
    )
}

/// `a column definition list is only allowed for functions returning
/// "record"` — SQLSTATE 42601.
pub(crate) fn coldeflist_only_for_record() -> RawError {
    RawError::new(
        AnalyzeError::SyntaxError(
            "a column definition list is only allowed for functions returning \"record\"".into(),
        ),
        None,
        None,
    )
}

/// `a column definition list is redundant for a function with OUT
/// parameters` — SQLSTATE 42601.
pub(crate) fn coldeflist_redundant_out_params() -> RawError {
    RawError::new(
        AnalyzeError::SyntaxError(
            "a column definition list is redundant for a function with OUT parameters".into(),
        ),
        None,
        None,
    )
}

/// `a column definition list is redundant for a function returning a named
/// composite type` — SQLSTATE 42601.
pub(crate) fn coldeflist_redundant_composite() -> RawError {
    RawError::new(
        AnalyzeError::SyntaxError(
            "a column definition list is redundant for a function returning a named composite type"
                .into(),
        ),
        None,
        None,
    )
}

/// `ROWS FROM() with multiple functions cannot have a column definition
/// list` — SQLSTATE 42601 (`transformRangeFunction`).
pub(crate) fn rows_from_multiple_coldeflist() -> RawError {
    RawError::new(
        AnalyzeError::SyntaxError(
            "ROWS FROM() with multiple functions cannot have a column definition list".into(),
        ),
        None,
        Some("Put a separate column definition list for each function inside ROWS FROM().".into()),
    )
}

/// `UNNEST() with multiple arguments cannot have a column definition list`
/// — SQLSTATE 42601.
pub(crate) fn unnest_multiple_coldeflist() -> RawError {
    RawError::new(
        AnalyzeError::SyntaxError(
            "UNNEST() with multiple arguments cannot have a column definition list".into(),
        ),
        None,
        Some(
            "Use separate UNNEST() calls inside ROWS FROM(), and attach a column definition \
             list to each one."
                .into(),
        ),
    )
}

/// `WITH ORDINALITY cannot be used with a column definition list` —
/// SQLSTATE 42601.
pub(crate) fn ordinality_with_coldeflist() -> RawError {
    RawError::new(
        AnalyzeError::SyntaxError(
            "WITH ORDINALITY cannot be used with a column definition list".into(),
        ),
        None,
        Some("Put the column definition list inside ROWS FROM().".into()),
    )
}

/// `multiple column definition lists are not allowed for the same function`
/// — SQLSTATE 42601.
pub(crate) fn multiple_coldeflists() -> RawError {
    RawError::new(
        AnalyzeError::SyntaxError(
            "multiple column definition lists are not allowed for the same function".into(),
        ),
        None,
        None,
    )
}

/// `column name "a" specified more than once` — SQLSTATE 42701
/// (`duplicate_column`, `CheckAttributeNamesTypes`).
pub(crate) fn duplicate_column_name(name: &str) -> RawError {
    RawError::new(
        AnalyzeError::DuplicateColumn(format!("column name \"{name}\" specified more than once")),
        None,
        None,
    )
}

/// `subquery must return only one column` — SQLSTATE 42601: a scalar
/// `(SELECT …)` or `ARRAY(SELECT …)` sublink whose subquery has more than
/// one output column (PG's transformSubLink).
pub(crate) fn subquery_must_return_one_column(span: Option<SourceSpan>) -> RawError {
    RawError::new(
        AnalyzeError::SyntaxError("subquery must return only one column".to_string()),
        span,
        None,
    )
}

/// `collation "x" for encoding "UTF8" does not exist` — SQLSTATE 42704
/// (get_collation_oid). `name` is the collation name as written, qualified
/// names joined with a dot (NameListToString).
pub(crate) fn collation_does_not_exist(name: &str) -> AnalyzeError {
    AnalyzeError::UndefinedObject(format!(
        "collation \"{name}\" for encoding \"UTF8\" does not exist"
    ))
}

/// `collation mismatch between explicit collations "A" and "B"` — SQLSTATE
/// 42P21: merge_collation_state meeting two different COLLATE clauses.
pub(crate) fn collation_mismatch_explicit(a: &str, b: &str) -> AnalyzeError {
    AnalyzeError::CollationMismatch(format!(
        "collation mismatch between explicit collations \"{a}\" and \"{b}\""
    ))
}

/// `cannot subscript type T because it does not support subscripting` —
/// SQLSTATE 42804: the (domain-unwrapped) container type has no subscript
/// handler (PG's transformContainerSubscripts).
pub(crate) fn cannot_subscript_type(type_name: &str) -> RawError {
    RawError::new(
        AnalyzeError::DatatypeMismatch(format!(
            "cannot subscript type {type_name} because it does not support subscripting"
        )),
        None,
        None,
    )
}

/// `jsonb subscript does not support slices` — SQLSTATE 42804.
pub(crate) fn jsonb_subscript_does_not_support_slices(span: Option<SourceSpan>) -> RawError {
    RawError::new(
        AnalyzeError::DatatypeMismatch("jsonb subscript does not support slices".to_string()),
        span,
        None,
    )
}

/// `subscript type T is not supported` — SQLSTATE 42804: a jsonb subscript
/// coercible to neither integer nor text.
pub(crate) fn subscript_type_not_supported(type_name: &str, span: Option<SourceSpan>) -> RawError {
    RawError::new(
        AnalyzeError::DatatypeMismatch(format!("subscript type {type_name} is not supported")),
        span,
        Some("jsonb subscript must be coercible to either integer or text.".to_string()),
    )
}

/// `cannot determine type of empty array` — SQLSTATE 42P18: an `ARRAY[]`
/// with no cast to give it a type.
pub(crate) fn cannot_determine_type_of_empty_array(span: Option<SourceSpan>) -> RawError {
    RawError::new(
        AnalyzeError::IndeterminateType("cannot determine type of empty array".to_string()),
        span,
        Some("Explicitly cast to the desired type, for example ARRAY[]::integer[].".to_string()),
    )
}

/// `<CONTEXT> could not convert type A to B` — SQLSTATE 42846
/// (`cannot_coerce`, no dedicated variant): PG's coerce_to_common_type
/// after select_common_type chose B for a construct (ARRAY, CASE, …).
pub(crate) fn could_not_convert_type(context: &str, from: &str, to: &str) -> RawError {
    RawError::new(
        AnalyzeError::Invalid(format!("{context} could not convert type {from} to {to}")),
        None,
        None,
    )
}

/// `inconsistent types deduced for parameter $N` — SQLSTATE 42P08
/// (`ambiguous_parameter`): PG's `variable_coerce_param_hook` coercing an
/// untyped parameter occurrence to `target` after an earlier coercion
/// already deduced `deduced`. PG's DETAIL (`integer versus text`) is
/// carried as the hint.
pub(crate) fn inconsistent_parameter_types(
    num: i32,
    deduced: &str,
    target: &str,
    span: Option<SourceSpan>,
) -> RawError {
    RawError::new(
        AnalyzeError::AmbiguousParameter(format!(
            "inconsistent types deduced for parameter ${num}"
        )),
        span,
        Some(format!("{deduced} versus {target}")),
    )
}

/// `array subscript must have type integer` — SQLSTATE 42804.
pub(crate) fn array_subscript_must_be_integer(span: Option<SourceSpan>) -> RawError {
    RawError::new(
        AnalyzeError::DatatypeMismatch("array subscript must have type integer".to_string()),
        span,
        None,
    )
}

/// `non-integer constant in ORDER BY` (GROUP BY, DISTINCT ON) — SQLSTATE
/// 42601: findTargetlistEntrySQL92 takes a bare constant as a select-list
/// position, and only an integer can be one.
pub(crate) fn non_integer_constant(clause: &str, span: Option<SourceSpan>) -> RawError {
    RawError::new(
        AnalyzeError::SyntaxError(format!("non-integer constant in {clause}")),
        span,
        None,
    )
}

/// `ORDER BY "a" is ambiguous` (GROUP BY, DISTINCT ON) — SQLSTATE 42702: a
/// bare name matching several differing output columns
/// (findTargetlistEntrySQL92).
pub(crate) fn clause_name_ambiguous(
    clause: &str,
    name: &str,
    span: Option<SourceSpan>,
) -> RawError {
    RawError::new(
        AnalyzeError::AmbiguousColumn(format!("{clause} \"{name}\" is ambiguous")),
        span,
        None,
    )
}

/// `row count cannot be null in FETCH FIRST ... WITH TIES clause` —
/// SQLSTATE 2201W (transformLimitClause).
pub(crate) fn with_ties_null_row_count(span: Option<SourceSpan>) -> RawError {
    RawError::new(
        AnalyzeError::InvalidRowCountInLimitClause(
            "row count cannot be null in FETCH FIRST ... WITH TIES clause".to_string(),
        ),
        span,
        None,
    )
}

/// `could not identify an ordering operator for type T` — SQLSTATE 42883:
/// a sort key whose type has no default btree opclass
/// (get_sort_group_operators). `hint` adds PG's errhint for sort clauses.
pub(crate) fn no_ordering_operator(
    type_name: &str,
    hint: bool,
    span: Option<SourceSpan>,
) -> RawError {
    RawError::new(
        AnalyzeError::UndefinedFunction(format!(
            "could not identify an ordering operator for type {type_name}"
        )),
        span,
        hint.then(|| "Use an explicit ordering operator or modify the query.".to_string()),
    )
}

/// `could not identify an equality operator for type T` — SQLSTATE 42883:
/// a grouping / DISTINCT / set-operation key whose type has neither a
/// btree nor a hash default opclass (get_sort_group_operators).
pub(crate) fn no_equality_operator(type_name: &str, span: Option<SourceSpan>) -> RawError {
    RawError::new(
        AnalyzeError::UndefinedFunction(format!(
            "could not identify an equality operator for type {type_name}"
        )),
        span,
        None,
    )
}

/// `could not identify a comparison function for type T` — SQLSTATE
/// 42883: GREATEST / LEAST over a type without a btree comparison
/// function (ExecInitExprRec, at executor start).
pub(crate) fn no_comparison_function(type_name: &str) -> RawError {
    RawError::new(
        AnalyzeError::UndefinedFunction(format!(
            "could not identify a comparison function for type {type_name}"
        )),
        None,
        None,
    )
}

/// `collation mismatch between implicit collations "A" and "B"` —
/// SQLSTATE 42P21: a sort / group key (or a set operation's column) whose
/// collation is indeterminate.
pub(crate) fn collation_mismatch_implicit(a: &str, b: &str, span: Option<SourceSpan>) -> RawError {
    RawError::new(
        AnalyzeError::CollationMismatch(format!(
            "collation mismatch between implicit collations \"{a}\" and \"{b}\""
        )),
        span,
        Some(
            "You can choose the collation by applying the COLLATE clause to one or both \
             expressions."
                .to_string(),
        ),
    )
}

/// `subquery uses ungrouped column "t.c" from outer query` — SQLSTATE
/// 42803 (check_ungrouped_columns_walker inside a sublink).
pub(crate) fn subquery_uses_ungrouped_column(
    alias: &str,
    column: &str,
    span: Option<SourceSpan>,
) -> RawError {
    RawError::new(
        AnalyzeError::GroupingError(format!(
            "subquery uses ungrouped column \"{alias}.{column}\" from outer query"
        )),
        span,
        None,
    )
}

/// `GROUPING must have fewer than 32 arguments` — SQLSTATE 54023
/// (transformGroupingFunc).
pub(crate) fn grouping_too_many_arguments(span: Option<SourceSpan>) -> RawError {
    RawError::new(
        AnalyzeError::TooManyArguments("GROUPING must have fewer than 32 arguments".to_string()),
        span,
        None,
    )
}

/// `CUBE is limited to 12 elements` — SQLSTATE 54011 (the grammar's
/// limit, enforced by transformGroupClause).
pub(crate) fn cube_too_many_elements(span: Option<SourceSpan>) -> RawError {
    RawError::new(
        AnalyzeError::TooManyColumns("CUBE is limited to 12 elements".to_string()),
        span,
        None,
    )
}

/// `too many grouping sets present (maximum 4096)` — SQLSTATE 54001
/// (parseCheckAggregates).
pub(crate) fn too_many_grouping_sets() -> RawError {
    RawError::new(
        AnalyzeError::StatementTooComplex(
            "too many grouping sets present (maximum 4096)".to_string(),
        ),
        None,
        None,
    )
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
            (procedure_is_not_unique("p", "unknown", None).kind, "42725"),
            (cross_database_reference("x.s.t", None).kind, "0A000"),
            (improper_qualified_name("a.b.c.d", None).kind, "42601"),
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
            (unknown_field_not_coercible("text", None).kind, "XX000"),
            (no_on_conflict_arbiter("t").kind, "42P10"),
            (insert_non_default_into_generated("g", false).kind, "428C9"),
            (update_generated_to_non_default("g", true).kind, "428C9"),
            (infinite_rule_recursion("t").kind, "42P17"),
            (cannot_change_relation("sequence", "s").kind, "42809"),
            (
                view_not_updatable(crate::resolve::DmlEvent::Insert, "v", "x", false).kind,
                "55000",
            ),
            (
                view_column_not_updatable("update", "c", "v", "x").kind,
                "0A000",
            ),
            (merge_view_partial_instead_triggers("v").kind, "0A000"),
            (merge_on_relation_with_rules("t").kind, "0A000"),
            (on_conflict_with_rules().kind, "0A000"),
            (
                returning_without_instead_rule_returning(crate::resolve::DmlEvent::Delete, "t")
                    .kind,
                "0A000",
            ),
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
            (
                prefix_operator_does_not_exist("-", "text", None).kind,
                "42883",
            ),
            (
                prefix_operator_is_not_unique("-", "unknown", None).kind,
                "42725",
            ),
            (schema_does_not_exist("s", None).kind, "3F000"),
            (window_in_window_definition(None).kind, "42P20"),
            (window_already_defined("w", None).kind, "42P20"),
            (
                cannot_override_window_clause("ORDER BY", "w", None).kind,
                "42P20",
            ),
            (cannot_copy_window_with_frame("w", true, None).kind, "42P20"),
            (range_offset_needs_one_order_by(None).kind, "42P20"),
            (groups_needs_order_by(None).kind, "42P20"),
            (frame_offset_has_variables("ROWS", None).kind, "42P10"),
            (range_offset_unsupported("text", None, None).kind, "0A000"),
            (
                range_offset_ambiguous("integer", "numeric", None).kind,
                "0A000",
            ),
            (not_an_aggregate("DISTINCT", "lower", None).kind, "42809"),
            (within_group_required("mode", None).kind, "42809"),
            (not_an_ordered_set_aggregate("count", None).kind, "42809"),
            (window_function_within_group("rank", None).kind, "42809"),
            (
                parameterless_aggregate_needs_star("count", None).kind,
                "42809",
            ),
            (
                call_shape_not_implemented(NOT_IMPLEMENTED_CALL_SHAPES[0], None).kind,
                "0A000",
            ),
            (distinct_aggregate_order_by_not_in_args(None).kind, "42P10"),
            (aggregate_in_filter(None).kind, "42803"),
            (window_in_filter(None).kind, "42P20"),
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
            (coldeflist_required().kind, "42601"),
            (coldeflist_only_for_record().kind, "42601"),
            (coldeflist_redundant_out_params().kind, "42601"),
            (coldeflist_redundant_composite().kind, "42601"),
            (rows_from_multiple_coldeflist().kind, "42601"),
            (unnest_multiple_coldeflist().kind, "42601"),
            (ordinality_with_coldeflist().kind, "42601"),
            (multiple_coldeflists().kind, "42601"),
            (duplicate_column_name("a").kind, "42701"),
            (using_column_ambiguous("id", "left").kind, "42702"),
            (using_column_listed_twice("id").kind, "42701"),
            (
                inconsistent_parameter_types(1, "integer", "text", None).kind,
                "42P08",
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
