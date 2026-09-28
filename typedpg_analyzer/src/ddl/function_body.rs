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
fn identifier_value(raw: &str) -> String {
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
