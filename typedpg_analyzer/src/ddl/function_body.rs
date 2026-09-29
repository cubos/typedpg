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
//! regular query analyzer after rewriting each `$n` to `($n::type)`, so the
//! analyzer sees the declared parameter types; names are resolved by the
//! analyzer's column lookup through the SQL-function parameter namespace
//! ([`crate::expr::with_sql_function_params`]), after columns and relations,
//! as PG's `sql_fn_post_column_ref` does.

use pg_query::protobuf::{self, CreateFunctionStmt, Token, node};

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

    let namespace = || crate::expr::SqlFunctionParams {
        name: proc.proname.clone(),
        params: params.clone(),
    };
    let mut last: Option<LastStatement> = None;
    for sql in &statements {
        let rewritten = substitute_params(interp, sql, &params)?;
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
            let analyzed = crate::expr::with_sql_function_params(namespace(), || {
                crate::resolve::analyze_raw_node(interp, inner, &[])
            });
            let columns = match analyzed {
                Ok((columns, _)) => Some(columns),
                // A construct the analyzer doesn't model says nothing about
                // the body; every other error is one PG's parse analysis
                // raises too.
                Err(
                    AnalyzeError::Unsupported(_)
                    | AnalyzeError::UnsupportedJoinType(_)
                    | AnalyzeError::Internal(_),
                ) => None,
                Err(e) => return Err(DdlError::UnsupportedDdl(format!("{e}"))),
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

/// `check_sql_fn_retval`: the final statement must return rows, and its
/// columns must fit the declared result under assignment coercion — one
/// column for a scalar (also per row of a `SETOF`), the attributes of a
/// named composite, or the OUT / TABLE parameters, in order. A lone column of
/// the composite type itself is accepted too; a bare `record` result is not
/// checked.
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
    if is_void || rettype == oid::UNKNOWN {
        return Ok(());
    }
    let Some(t) = interp.pg_type.get(&interp.unwrap_domain(rettype)) else {
        return Ok(());
    };
    // The row the function returns, when it returns one.
    let row: Option<Vec<PgTypeOid>> = if t.typtype == TypType::Composite {
        let relid = t.typrelid;
        Some(
            relid
                .map(|r| {
                    interp
                        .attributes_of(r)
                        .iter()
                        .filter(|a| a.attnum > 0)
                        .map(|a| a.atttypid)
                        .collect()
                })
                .unwrap_or_default(),
        )
    } else if rettype == oid::RECORD {
        let out: Vec<PgTypeOid> = proc
            .proargmodes
            .iter()
            .zip(&proc.proallargtypes)
            .filter(|(m, _)| matches!(m, ArgMode::Out | ArgMode::InOut | ArgMode::Table))
            .map(|(_, &t)| t)
            .collect();
        if out.is_empty() {
            return Ok(());
        }
        Some(out)
    } else if t.typtype == TypType::Pseudo {
        return Ok(());
    } else {
        None
    };
    let mismatch = |detail: String| {
        DdlError::Parse(format!(
            "return type mismatch in function declared to return {} ({detail})",
            super::util::format_type_for_message(interp, rettype)
        ))
    };
    let Some(last) = last else {
        return Ok(());
    };
    if !last.returns_rows {
        return Err(mismatch(
            "Function's final statement must be SELECT or INSERT/UPDATE/DELETE/MERGE RETURNING."
                .into(),
        ));
    }
    let Some(column_types) = last.column_types.as_deref() else {
        return Ok(());
    };
    let fits = |actual: PgTypeOid, declared: PgTypeOid| {
        actual == oid::UNKNOWN || can_coerce(actual, declared, CoercionContext::Assignment, interp)
    };
    let name = |t: PgTypeOid| super::util::format_type_for_message(interp, t);
    let Some(row) = row else {
        return match column_types {
            [single] if fits(*single, rettype) => Ok(()),
            [single] => Err(mismatch(format!(
                "Actual return type is {}.",
                name(*single)
            ))),
            _ => Err(mismatch(
                "Final statement must return exactly one column.".into(),
            )),
        };
    };
    if let [single] = column_types
        && fits(*single, rettype)
        && interp.unwrap_domain(*single) == interp.unwrap_domain(rettype)
    {
        return Ok(());
    }
    for (i, &actual) in column_types.iter().enumerate() {
        let Some(&declared) = row.get(i) else {
            return Err(mismatch("Final statement returns too many columns.".into()));
        };
        if !fits(actual, declared) {
            return Err(mismatch(format!(
                "Final statement returns {} instead of {} at column {}.",
                name(actual),
                name(declared),
                i + 1
            )));
        }
    }
    if column_types.len() < row.len() {
        return Err(mismatch("Final statement returns too few columns.".into()));
    }
    Ok(())
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
        node::Node::InsertStmt(s) => {
            !crate::resolve::returning_exprs(&s.returning_clause).is_empty()
        }
        node::Node::UpdateStmt(s) => {
            !crate::resolve::returning_exprs(&s.returning_clause).is_empty()
        }
        node::Node::DeleteStmt(s) => {
            !crate::resolve::returning_exprs(&s.returning_clause).is_empty()
        }
        node::Node::MergeStmt(s) => {
            !crate::resolve::returning_exprs(&s.returning_clause).is_empty()
        }
        _ => false,
    }
}

/// Rewrite every positional parameter reference `$n` to `($n::type)`.
fn substitute_params(
    interp: &PgCatalog,
    sql: &str,
    params: &[(String, PgTypeOid)],
) -> Result<String, DdlError> {
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
    for tok in tokens {
        let kind = Token::try_from(tok.token).unwrap_or(Token::Nul);
        let replacement = if kind == Token::Param {
            text(tok)
                .trim_start_matches('$')
                .parse::<usize>()
                .ok()
                .and_then(&mut typed)
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
    Ok(out)
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
    compile_plpgsql(interp, &sql)
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
    compile_plpgsql(interp, &sql)
}

/// Compile a `CREATE FUNCTION ... LANGUAGE plpgsql` statement with the
/// PL/pgSQL grammar — which, compiling against the declared signature, also
/// applies make_return_stmt's rules — and resolve its declared variables'
/// types.
fn compile_plpgsql(interp: &PgCatalog, sql: &str) -> Result<(), DdlError> {
    let parsed = pg_query::parse_plpgsql(sql).map_err(|e| match e {
        pg_query::Error::Parse(msg) => DdlError::Parse(msg),
        other => DdlError::Parse(other.to_string()),
    })?;
    let datums = parsed
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| f.get("PLpgSQL_function")?.get("datums")?.as_array())
        .flatten();
    for datum in datums {
        let Some(var) = datum
            .get("PLpgSQL_var")
            .or_else(|| datum.get("PLpgSQL_rec"))
        else {
            continue;
        };
        // Parameters and implicit variables (FOUND, TG_*) have no line
        // number; only DECLAREd variables are checked.
        if var.get("lineno").is_none() {
            continue;
        }
        let Some(ty) = var.get("datatype").and_then(|d| d.get("PLpgSQL_type")) else {
            continue;
        };
        // The name as written: libpg_query has no catalog, so a type it
        // couldn't resolve (any user-defined one) is compiled as a record
        // and only its written name tells what it was.
        if let Some(names) = ty.get("origtypname").and_then(|n| n.as_array()) {
            let tn = pg_query::protobuf::TypeName {
                names: names
                    .iter()
                    .filter_map(|n| n.as_str())
                    .map(|n| pg_query::protobuf::Node {
                        node: Some(node::Node::String(pg_query::protobuf::String {
                            sval: n.to_owned(),
                        })),
                    })
                    .collect(),
                array_bounds: (0..ty
                    .get("origtypname_array_bounds")
                    .and_then(|b| b.as_u64())
                    .unwrap_or(0))
                    .map(|_| pg_query::protobuf::Node {
                        node: Some(node::Node::Integer(pg_query::protobuf::Integer {
                            ival: -1,
                        })),
                    })
                    .collect(),
                typemod: -1,
                ..Default::default()
            };
            super::util::lookup_type_name(&tn, interp)?;
            continue;
        }
        if let Some(typname) = ty.get("typname").and_then(|t| t.as_str()) {
            check_declared_type(interp, typname)?;
        }
    }
    Ok(())
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
