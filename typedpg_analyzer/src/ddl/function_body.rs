//! Validation of `LANGUAGE sql` function bodies at CREATE FUNCTION time.
//!
//! Mirrors `fmgr_sql_validator` (`executor/functions.c`): unless
//! `check_function_bodies` is off (and always for a `BEGIN ATOMIC` /
//! `RETURN` body), every statement of the body is parse-analyzed with the
//! function's parameters in scope, and the final statement's result must fit
//! the declared return type (`check_sql_fn_retval`). Functions with
//! polymorphic arguments are only parsed, as in PG.
//!
//! Parameters are referenced as `$n` or by name. The body is analyzed by the
//! regular query analyzer after rewriting each such reference to
//! `($n::type)`, so the analyzer sees the declared parameter types. A name
//! that also matches a column of a relation the statement reads is left
//! alone and the statement not validated (PG resolves the column first; the
//! rewrite can't tell them apart), which keeps the check free of false
//! rejections.

use pg_query::protobuf::{self, CreateFunctionStmt, KeywordKind, Token, node};

use super::DdlError;
use crate::coerce::{CoercionContext, can_coerce};
use crate::error::AnalyzeError;
use crate::oid::PgTypeOid;
use crate::pg_catalog::{ArgMode, PgCatalog, PgProc, TypCategory, TypType, oid};

/// Validate the body of `proc` as created by `stmt`.
pub(crate) fn validate_sql_function(
    interp: &PgCatalog,
    stmt: &CreateFunctionStmt,
    proc: &PgProc,
) -> Result<(), DdlError> {
    if !language_is_sql(stmt) {
        return Ok(());
    }
    // The statements of the body, as SQL text.
    let (statements, always_check) = match body_statements(stmt)? {
        Some(found) => found,
        None => return Ok(()),
    };
    if !always_check && !interp.check_function_bodies {
        return Ok(());
    }

    let params = input_params(proc);
    let polymorphic = params
        .iter()
        .map(|(_, t)| *t)
        .chain(std::iter::once(proc.prorettype))
        .any(|t| is_polymorphic(interp, t));
    if polymorphic {
        return Ok(());
    }

    let mut last: Option<LastStatement> = None;
    for sql in &statements {
        let Some(rewritten) = substitute_params(interp, sql, &params)? else {
            // A parameter name doubles as a column name: can't validate.
            return Ok(());
        };
        let parsed = pg_query::parse(&rewritten).map_err(|e| DdlError::Parse(e.to_string()))?;
        for raw in &parsed.protobuf.stmts {
            let Some(inner) = raw.stmt.as_ref().and_then(|n| n.node.as_ref()) else {
                continue;
            };
            if !is_analyzable(inner) {
                // A utility statement: PG runs it only at call time, and the
                // statements after it may depend on what it creates.
                return Ok(());
            }
            let columns = match crate::resolve::analyze_raw_node(interp, inner, &[]) {
                Ok((columns, _)) => Some(columns),
                // Report what PG's parse analysis certainly reports too — a
                // missing relation / schema / column; any other analyzer
                // complaint leaves the body unvalidated rather than risk
                // rejecting a function PG accepts.
                Err(
                    e @ (AnalyzeError::UndefinedTable(_)
                    | AnalyzeError::UndefinedSchema(_)
                    | AnalyzeError::UndefinedColumn(_)),
                ) => return Err(DdlError::UnsupportedDdl(format!("{e}"))),
                Err(_) => None,
            };
            last = Some(LastStatement {
                returns_rows: returns_rows(inner),
                column_types: columns.map(|cols| cols.iter().map(|c| c.type_oid).collect()),
            });
        }
    }
    check_return_type(interp, proc, last.as_ref())
}

struct LastStatement {
    /// A SELECT / VALUES, or DML with RETURNING.
    returns_rows: bool,
    /// The result column types, `None` when the analyzer couldn't tell.
    column_types: Option<Vec<PgTypeOid>>,
}

/// `check_sql_fn_retval` for scalar and void returns (composite, record and
/// set-returning functions are not checked).
fn check_return_type(
    interp: &PgCatalog,
    proc: &PgProc,
    last: Option<&LastStatement>,
) -> Result<(), DdlError> {
    let rettype = proc.prorettype;
    let is_void = interp
        .pg_type
        .get(&rettype)
        .is_some_and(|t| t.typname == "void" && t.typtype == TypType::Pseudo);
    if is_void || rettype == oid::UNKNOWN || proc.proretset {
        return Ok(());
    }
    let Some(t) = interp.pg_type.get(&interp.unwrap_domain(rettype)) else {
        return Ok(());
    };
    if matches!(t.typtype, TypType::Composite | TypType::Pseudo) {
        return Ok(());
    }
    let mismatch = || {
        format!(
            "return type mismatch in function declared to return {}",
            super::util::format_type_for_message(interp, rettype)
        )
    };
    let Some(last) = last else {
        return Ok(());
    };
    if !last.returns_rows {
        return Err(DdlError::Parse(format!(
            "{} (Function's final statement must be SELECT or \
             INSERT/UPDATE/DELETE/MERGE RETURNING.)",
            mismatch()
        )));
    }
    let Some(column_types) = last.column_types.as_deref() else {
        return Ok(());
    };
    match column_types {
        [single] => {
            if *single != oid::UNKNOWN
                && !can_coerce(*single, rettype, CoercionContext::Assignment, interp)
            {
                return Err(DdlError::Parse(format!(
                    "{} (Actual return type is {}.)",
                    mismatch(),
                    super::util::format_type_for_message(interp, *single)
                )));
            }
            Ok(())
        }
        _ => Err(DdlError::Parse(format!(
            "{} (Final statement must return exactly one column.)",
            mismatch()
        ))),
    }
}

fn language_is_sql(stmt: &CreateFunctionStmt) -> bool {
    // A `BEGIN ATOMIC` / `RETURN` body is always SQL.
    stmt.sql_body.is_some()
        || stmt.options.iter().any(|n| {
            matches!(n.node.as_ref(), Some(node::Node::DefElem(de))
                if de.defname == "language"
                    && matches!(de.arg.as_deref().and_then(|a| a.node.as_ref()),
                        Some(node::Node::String(s)) if s.sval.eq_ignore_ascii_case("sql")))
        })
}

/// The body's statements as SQL text, and whether PG always analyzes them
/// (`BEGIN ATOMIC` / `RETURN` bodies are parsed with the CREATE FUNCTION
/// itself; string bodies only under `check_function_bodies`).
fn body_statements(stmt: &CreateFunctionStmt) -> Result<Option<(Vec<String>, bool)>, DdlError> {
    if let Some(body) = stmt.sql_body.as_deref() {
        let mut out = Vec::new();
        collect_atomic(body, &mut out)?;
        return Ok(Some((out, true)));
    }
    let source = stmt.options.iter().find_map(|n| match n.node.as_ref()? {
        node::Node::DefElem(de) if de.defname == "as" => match de.arg.as_deref()?.node.as_ref()? {
            node::Node::List(l) => l.items.first().and_then(super::util::node_string),
            node::Node::String(s) => Some(s.sval.as_str()),
            _ => None,
        },
        _ => None,
    });
    Ok(source.map(|s| (vec![s.to_owned()], false)))
}

fn collect_atomic(node: &protobuf::Node, out: &mut Vec<String>) -> Result<(), DdlError> {
    match node.node.as_ref() {
        Some(node::Node::List(l)) => {
            for item in &l.items {
                collect_atomic(item, out)?;
            }
        }
        // `RETURN expr` is `SELECT expr`.
        Some(node::Node::ReturnStmt(r)) => {
            if let Some(val) = r.returnval.as_deref() {
                let select = protobuf::SelectStmt {
                    target_list: vec![protobuf::Node {
                        node: Some(node::Node::ResTarget(Box::new(protobuf::ResTarget {
                            val: Some(Box::new(val.clone())),
                            ..Default::default()
                        }))),
                    }],
                    limit_option: protobuf::LimitOption::Default as i32,
                    op: protobuf::SetOperation::SetopNone as i32,
                    ..Default::default()
                };
                out.push(deparse(node::Node::SelectStmt(Box::new(select)))?);
            }
        }
        Some(other) => out.push(deparse(other.clone())?),
        None => {}
    }
    Ok(())
}

fn deparse(node: node::Node) -> Result<String, DdlError> {
    node.deparse()
        .map_err(|e| DdlError::Internal(format!("deparse of SQL function body: {e}")))
}

/// Input parameters `(name, type)` in call order (IN, INOUT, VARIADIC).
fn input_params(proc: &PgProc) -> Vec<(String, PgTypeOid)> {
    if proc.proargmodes.is_empty() {
        return proc
            .proargtypes
            .iter()
            .enumerate()
            .map(|(i, &t)| (proc.proargnames.get(i).cloned().unwrap_or_default(), t))
            .collect();
    }
    proc.proargmodes
        .iter()
        .zip(&proc.proallargtypes)
        .enumerate()
        .filter(|(_, (m, _))| matches!(m, ArgMode::In | ArgMode::InOut | ArgMode::Variadic))
        .map(|(i, (_, &t))| (proc.proargnames.get(i).cloned().unwrap_or_default(), t))
        .collect()
}

fn is_polymorphic(interp: &PgCatalog, t: PgTypeOid) -> bool {
    interp
        .pg_type
        .get(&t)
        .is_some_and(|ty| ty.typtype == TypType::Pseudo && ty.typcategory == TypCategory::Pseudo)
        && interp.pg_type.get(&t).is_some_and(|ty| {
            matches!(
                ty.typname.as_str(),
                "anyelement"
                    | "anyarray"
                    | "anynonarray"
                    | "anyenum"
                    | "anyrange"
                    | "anymultirange"
                    | "anycompatible"
                    | "anycompatiblearray"
                    | "anycompatiblenonarray"
                    | "anycompatiblerange"
                    | "anycompatiblemultirange"
            )
        })
}

fn is_analyzable(stmt: &node::Node) -> bool {
    matches!(
        stmt,
        node::Node::SelectStmt(_)
            | node::Node::InsertStmt(_)
            | node::Node::UpdateStmt(_)
            | node::Node::DeleteStmt(_)
            | node::Node::MergeStmt(_)
    )
}

fn returns_rows(stmt: &node::Node) -> bool {
    match stmt {
        node::Node::SelectStmt(_) => true,
        node::Node::InsertStmt(s) => !s.returning_list.is_empty(),
        node::Node::UpdateStmt(s) => !s.returning_list.is_empty(),
        node::Node::DeleteStmt(s) => !s.returning_list.is_empty(),
        node::Node::MergeStmt(s) => !s.returning_list.is_empty(),
        _ => false,
    }
}

/// Rewrite every parameter reference (`$n`, or a parameter's name used as a
/// bare identifier) to `($n::type)`. Returns `None` when a parameter name is
/// also a column of a relation the SQL reads.
fn substitute_params(
    interp: &PgCatalog,
    sql: &str,
    params: &[(String, PgTypeOid)],
) -> Result<Option<String>, DdlError> {
    let scan = pg_query::scan(sql).map_err(|e| DdlError::Parse(e.to_string()))?;
    let tokens = &scan.tokens;
    let text = |t: &protobuf::ScanToken| &sql[t.start as usize..t.end as usize];
    let typed = |n: usize| -> Option<String> {
        let (_, t) = params.get(n.checked_sub(1)?)?;
        let ty = interp.pg_type.get(t)?;
        let schema = interp.namespace_name(ty.typnamespace)?;
        Some(format!(
            "(${n}::{})",
            crate::qualified_name::QualifiedName::new(schema, &ty.typname)
        ))
    };

    let named: Vec<(String, usize)> = params
        .iter()
        .enumerate()
        .filter(|(_, (name, _))| !name.is_empty())
        .map(|(i, (name, _))| (name.clone(), i + 1))
        .collect();
    if !named.is_empty() && names_collide_with_columns(interp, sql, &named) {
        return Ok(None);
    }

    // The analyzer numbers query parameters densely, so the referenced
    // parameters are renumbered in order of first use; the cast carries the
    // declared type either way.
    let mut renumbered: Vec<usize> = Vec::new();
    let mut typed = |n: usize| -> Option<String> {
        let cast = typed(n)?;
        let k = match renumbered.iter().position(|&m| m == n) {
            Some(k) => k + 1,
            None => {
                renumbered.push(n);
                renumbered.len()
            }
        };
        Some(cast.replacen(&format!("(${n}::"), &format!("(${k}::"), 1))
    };
    let mut out = String::with_capacity(sql.len() + 16);
    let mut pos = 0usize;
    for (i, tok) in tokens.iter().enumerate() {
        let kind = Token::try_from(tok.token).unwrap_or(Token::Nul);
        let keyword = KeywordKind::try_from(tok.keyword_kind).unwrap_or(KeywordKind::NoKeyword);
        let replacement = if kind == Token::Param {
            text(tok)
                .trim_start_matches('$')
                .parse::<usize>()
                .ok()
                .and_then(&mut typed)
        } else if kind == Token::Ident
            || (keyword != KeywordKind::NoKeyword && keyword != KeywordKind::ReservedKeyword)
        {
            let prev_dot = i > 0 && text(&tokens[i - 1]) == ".";
            let next = tokens.get(i + 1).map(text);
            let qualified_or_call = matches!(next, Some("." | "("));
            let ident = identifier_value(text(tok));
            match named.iter().find(|(n, _)| *n == ident) {
                Some((_, n)) if !prev_dot && !qualified_or_call => typed(*n),
                _ => None,
            }
        } else {
            None
        };
        if let Some(rep) = replacement {
            out.push_str(&sql[pos..tok.start as usize]);
            out.push_str(&rep);
            pos = tok.end as usize;
        }
    }
    out.push_str(&sql[pos..]);
    Ok(Some(out))
}

/// The identifier a token spells: `"Quoted"` as written, bare ones
/// downcased.
pub(crate) fn identifier_value(raw: &str) -> String {
    match raw.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
        Some(inner) => inner.replace("\"\"", "\""),
        None => raw.to_ascii_lowercase(),
    }
}

/// Whether one of the parameter names is also a column of a relation the
/// SQL refers to (PG resolves such a name to the column).
fn names_collide_with_columns(interp: &PgCatalog, sql: &str, named: &[(String, usize)]) -> bool {
    let Ok(parsed) = pg_query::parse(sql) else {
        return false;
    };
    parsed.protobuf.nodes().iter().any(|(n, ..)| {
        // Columns of subqueries, CTEs and FROM functions can shadow a
        // parameter too; don't try to tell those apart.
        if matches!(
            n,
            pg_query::NodeRef::RangeSubselect(_)
                | pg_query::NodeRef::CommonTableExpr(_)
                | pg_query::NodeRef::RangeFunction(_)
        ) {
            return true;
        }
        let pg_query::NodeRef::RangeVar(rv) = n else {
            return false;
        };
        let schema = (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str());
        interp
            .resolve_table(schema, &rv.relname)
            .is_some_and(|class| {
                interp
                    .attributes_of(class.oid)
                    .iter()
                    .any(|a| named.iter().any(|(name, _)| *name == a.attname))
            })
    })
}

/// The expression an inlinable `LANGUAGE sql` function stands for
/// (`inline_function`, optimizer/util/clauses.c): a non-set-returning
/// function whose body is a single `SELECT expr` — no FROM, WHERE,
/// grouping, ordering, limit, set operation or CTE — or `RETURN expr`.
pub(crate) fn inlinable_body(stmt: &CreateFunctionStmt, proc: &PgProc) -> Option<protobuf::Node> {
    if !language_is_sql(stmt) || proc.proretset {
        return None;
    }
    if let Some(body) = stmt.sql_body.as_deref() {
        return match body.node.as_ref()? {
            node::Node::ReturnStmt(r) => r.returnval.as_deref().cloned(),
            node::Node::List(l) if l.items.len() == 1 => match l.items[0].node.as_ref()? {
                node::Node::ReturnStmt(r) => r.returnval.as_deref().cloned(),
                _ => None,
            },
            _ => None,
        };
    }
    let (statements, _) = body_statements(stmt).ok()??;
    let [source] = statements.as_slice() else {
        return None;
    };
    let parsed = pg_query::parse(source).ok()?;
    let [raw] = parsed.protobuf.stmts.as_slice() else {
        return None;
    };
    let node::Node::SelectStmt(sel) = raw.stmt.as_ref()?.node.as_ref()? else {
        return None;
    };
    let simple = sel.from_clause.is_empty()
        && sel.where_clause.is_none()
        && sel.group_clause.is_empty()
        && sel.having_clause.is_none()
        && sel.sort_clause.is_empty()
        && sel.limit_count.is_none()
        && sel.limit_offset.is_none()
        && sel.distinct_clause.is_empty()
        && sel.window_clause.is_empty()
        && sel.with_clause.is_none()
        && sel.values_lists.is_empty()
        && sel.larg.is_none()
        && sel.target_list.len() == 1;
    if !simple {
        return None;
    }
    match sel.target_list[0].node.as_ref()? {
        node::Node::ResTarget(rt) => rt.val.as_deref().cloned(),
        _ => None,
    }
}

/// `plpgsql_validator` (pl_handler.c): with `check_function_bodies` on, a
/// `LANGUAGE plpgsql` body is compiled at CREATE FUNCTION — its syntax and
/// variable references checked by the PL/pgSQL grammar (the same one
/// libpg_query embeds), and the declared variables' types looked up.
pub(crate) fn validate_plpgsql_function(
    interp: &PgCatalog,
    stmt: &CreateFunctionStmt,
    proc: &PgProc,
) -> Result<(), DdlError> {
    let is_plpgsql = stmt.options.iter().any(|n| {
        matches!(n.node.as_ref(), Some(node::Node::DefElem(de))
            if de.defname == "language"
                && matches!(de.arg.as_deref().and_then(|a| a.node.as_ref()),
                    Some(node::Node::String(s)) if s.sval.eq_ignore_ascii_case("plpgsql")))
    });
    if !is_plpgsql || !interp.check_function_bodies {
        return Ok(());
    }
    let sql = deparse(node::Node::CreateFunctionStmt(Box::new(stmt.clone())))?;
    let void = interp
        .pg_type
        .get(&proc.prorettype)
        .is_some_and(|t| matches!(t.typname.as_str(), "void" | "event_trigger"));
    let returns = if proc.proretset {
        ReturnKind::Set
    } else if proc.prokind == crate::pg_catalog::ProKind::Procedure {
        ReturnKind::Procedure
    } else if void {
        ReturnKind::Void
    } else if proc
        .proargmodes
        .iter()
        .any(|m| matches!(m, ArgMode::Out | ArgMode::InOut | ArgMode::Table))
    {
        ReturnKind::OutParams
    } else {
        ReturnKind::Value
    };
    compile_plpgsql(interp, &sql, returns)
}

/// What a PL/pgSQL function returns, for make_return_stmt's checks.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReturnKind {
    Set,
    Procedure,
    Void,
    OutParams,
    Value,
}

/// make_return_stmt (pl_gram.y): whether `RETURN` takes an expression.
fn check_returns(json: &serde_json::Value, returns: ReturnKind) -> Result<(), DdlError> {
    match json {
        serde_json::Value::Object(map) => {
            if let Some(ret) = map.get("PLpgSQL_stmt_return") {
                let has_expr = ret.get("expr").is_some() || ret.get("retvarno").is_some();
                let msg = match (returns, has_expr) {
                    (ReturnKind::Set, true) => Some(
                        "RETURN cannot have a parameter in function returning set (Use RETURN \
                         NEXT or RETURN QUERY.)",
                    ),
                    (ReturnKind::Procedure, true) => {
                        Some("RETURN cannot have a parameter in a procedure")
                    }
                    (ReturnKind::Void, true) => {
                        Some("RETURN cannot have a parameter in function returning void")
                    }
                    (ReturnKind::OutParams, true) => {
                        Some("RETURN cannot have a parameter in function with OUT parameters")
                    }
                    (ReturnKind::Value, false) => Some("missing expression at or near \";\""),
                    _ => None,
                };
                if let Some(msg) = msg {
                    return Err(DdlError::Parse(msg.into()));
                }
            }
            for value in map.values() {
                check_returns(value, returns)?;
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                check_returns(item, returns)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// `DO [LANGUAGE lang] 'code'` (ExecuteDoStmt): the language must exist and
/// support inline code; a PL/pgSQL block is compiled (syntax and declared
/// types) before it runs. What the block does when it runs isn't modeled.
pub(crate) fn do_block(
    interp: &PgCatalog,
    stmt: &pg_query::protobuf::DoStmt,
) -> Result<(), DdlError> {
    let mut code = None;
    let mut language = "plpgsql".to_owned();
    for arg in &stmt.args {
        let Some(node::Node::DefElem(de)) = arg.node.as_ref() else {
            continue;
        };
        let Some(node::Node::String(s)) = de.arg.as_deref().and_then(|a| a.node.as_ref()) else {
            continue;
        };
        match de.defname.as_str() {
            "as" => code = Some(s.sval.clone()),
            "language" => language = s.sval.to_ascii_lowercase(),
            _ => {}
        }
    }
    super::languages::check(interp, &language)?;
    match language.as_str() {
        "plpgsql" => {}
        "sql" | "c" | "internal" => {
            return Err(DdlError::UnsupportedDdl(format!(
                "language \"{language}\" does not support inline code execution"
            )));
        }
        // A language created by CREATE LANGUAGE: its inline handler isn't
        // modeled.
        _ => return Ok(()),
    }
    let Some(code) = code else {
        return Ok(());
    };
    // Wrap the block as a function body, dollar-quoted with a tag the code
    // doesn't contain.
    let mut tag = String::from("do");
    while code.contains(&format!("${tag}$")) {
        tag.push('x');
    }
    let sql = format!(
        "CREATE FUNCTION inline_code_block() RETURNS void LANGUAGE plpgsql AS ${tag}${code}${tag}$"
    );
    compile_plpgsql(interp, &sql, ReturnKind::Void)
}

/// Compile a `CREATE FUNCTION ... LANGUAGE plpgsql` statement with the
/// PL/pgSQL grammar and resolve its declared variables' types.
fn compile_plpgsql(interp: &PgCatalog, sql: &str, returns: ReturnKind) -> Result<(), DdlError> {
    let parsed = pg_query::parse_plpgsql(sql).map_err(|e| match e {
        pg_query::Error::Parse(msg) => DdlError::Parse(msg),
        other => DdlError::Parse(other.to_string()),
    })?;
    // Declared variables: `datums[].PLpgSQL_var.datatype.PLpgSQL_type.typname`
    // as written (libpg_query doesn't look types up).
    let datums = parsed
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| f.get("PLpgSQL_function")?.get("datums")?.as_array())
        .flatten();
    for datum in datums {
        let Some(var) = datum.get("PLpgSQL_var") else {
            continue;
        };
        // The implicit FOUND variable has no line number.
        if var.get("lineno").is_none() {
            continue;
        }
        let Some(typname) = var
            .get("datatype")
            .and_then(|d| d.get("PLpgSQL_type"))
            .and_then(|t| t.get("typname"))
            .and_then(|t| t.as_str())
        else {
            continue;
        };
        check_declared_type(interp, typname)?;
    }
    check_returns(&parsed, returns)
}

/// Resolve a PL/pgSQL variable's declared type: `x%TYPE` names a column,
/// `r%ROWTYPE` a relation, anything else is an ordinary type name.
fn check_declared_type(interp: &PgCatalog, typname: &str) -> Result<(), DdlError> {
    let trimmed = typname.trim();
    let lower = trimmed.to_ascii_lowercase();
    if let Some(prefix) = lower
        .strip_suffix("%rowtype")
        .map(|_| &trimmed[..trimmed.len() - "%rowtype".len()])
    {
        let names = split_dotted(prefix);
        let (schema, name) = match names.as_slice() {
            [name] => (None, name.as_str()),
            [schema, name] => (Some(schema.as_str()), name.as_str()),
            _ => return Ok(()),
        };
        if interp.resolve_table(schema, name).is_none() {
            return Err(DdlError::TableNotFound(format!(
                "relation \"{}\" does not exist",
                names.join(".")
            )));
        }
        return Ok(());
    }
    if lower.ends_with("%type") {
        // A variable's own %TYPE (`x other_var%TYPE`) can't be told apart
        // from a column reference here; only check `rel.col%TYPE`.
        let names = split_dotted(&trimmed[..trimmed.len() - "%type".len()]);
        if names.len() < 2 {
            return Ok(());
        }
        let tn = pg_query::protobuf::TypeName {
            names: names
                .into_iter()
                .map(|n| pg_query::protobuf::Node {
                    node: Some(node::Node::String(pg_query::protobuf::String { sval: n })),
                })
                .collect(),
            pct_type: true,
            ..Default::default()
        };
        super::util::lookup_type_name(&tn, interp)?;
        return Ok(());
    }
    let Ok(parsed) = pg_query::parse(&format!("SELECT NULL::{trimmed}")) else {
        return Ok(());
    };
    let tn = parsed
        .protobuf
        .nodes()
        .into_iter()
        .find_map(|(n, ..)| match n {
            pg_query::NodeRef::TypeCast(tc) => tc.type_name.clone(),
            _ => None,
        });
    if let Some(tn) = tn {
        super::util::lookup_type_name(&tn, interp)?;
    }
    Ok(())
}

/// Split `a.b."C"` into identifiers (quoted ones as written, bare ones
/// downcased).
fn split_dotted(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = text.trim().chars().peekable();
    loop {
        let mut part = String::new();
        if chars.next_if_eq(&'"').is_some() {
            while let Some(c) = chars.next() {
                if c == '"' {
                    if chars.next_if_eq(&'"').is_some() {
                        part.push('"');
                    } else {
                        break;
                    }
                } else {
                    part.push(c);
                }
            }
        } else {
            while let Some(c) = chars.next_if(|&c| c != '.') {
                part.push(c.to_ascii_lowercase());
            }
        }
        out.push(part.trim().to_owned());
        if chars.next_if_eq(&'.').is_none() {
            break;
        }
    }
    out
}

/// The pseudo-types a function may take and return: ProcedureCreate's
/// `internal` rule for every language, then fmgr_sql_validator's and
/// plpgsql_validator's (checked whatever check_function_bodies says).
pub(crate) fn check_pseudo_types(
    interp: &PgCatalog,
    language: Option<&str>,
    proc: &PgProc,
) -> Result<(), DdlError> {
    let typname = |t: PgTypeOid| interp.pg_type.get(&t).map(|ty| ty.typname.clone());
    let is_pseudo = |t: PgTypeOid| {
        interp
            .pg_type
            .get(&t)
            .is_some_and(|ty| ty.typtype == TypType::Pseudo)
    };
    let internal = |t: PgTypeOid| typname(t).as_deref() == Some("internal");
    let inputs = input_params(proc);
    // A procedure's result is void or its OUT parameters' record.
    let is_procedure = proc.prokind == crate::pg_catalog::ProKind::Procedure;
    if internal(proc.prorettype) && !inputs.iter().any(|(_, t)| internal(*t)) {
        return Err(DdlError::Parse(
            "unsafe use of pseudo-type \"internal\" (A function returning \"internal\" must \
             have at least one \"internal\" argument.)"
                .into(),
        ));
    }
    let shown = |t: PgTypeOid| super::util::format_type_for_message(interp, t);
    match language {
        Some("sql") => {
            let rettype = proc.prorettype;
            if !is_procedure
                && is_pseudo(rettype)
                && !matches!(typname(rettype).as_deref(), Some("void" | "record"))
                && !is_polymorphic(interp, rettype)
            {
                return Err(DdlError::Parse(format!(
                    "SQL functions cannot return type {}",
                    shown(rettype)
                )));
            }
            for (_, t) in &inputs {
                if is_pseudo(*t) && !is_polymorphic(interp, *t) {
                    return Err(DdlError::Parse(format!(
                        "SQL functions cannot have arguments of type {}",
                        shown(*t)
                    )));
                }
            }
        }
        Some("plpgsql") => {
            let rettype = proc.prorettype;
            if !is_procedure
                && is_pseudo(rettype)
                && !matches!(
                    typname(rettype).as_deref(),
                    Some("void" | "record" | "trigger" | "event_trigger")
                )
                && !is_polymorphic(interp, rettype)
            {
                return Err(DdlError::UnsupportedDdl(format!(
                    "PL/pgSQL functions cannot return type {}",
                    shown(rettype)
                )));
            }
            for (_, t) in &inputs {
                if is_pseudo(*t)
                    && typname(*t).as_deref() != Some("record")
                    && !is_polymorphic(interp, *t)
                {
                    return Err(DdlError::UnsupportedDdl(format!(
                        "PL/pgSQL functions cannot accept type {}",
                        shown(*t)
                    )));
                }
            }
        }
        _ => {}
    }
    Ok(())
}
