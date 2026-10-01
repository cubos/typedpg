//! Running a `DO` block (ExecuteDoStmt → plpgsql_inline_handler) as far
//! as it can be known statically.
//!
//! The analyzer has no PL/pgSQL interpreter, so it runs the block's
//! straight-line statements, in order — SQL statements that don't read the
//! block's variables, `EXECUTE` of a constant string, `PERFORM`, `RAISE`,
//! assignments, nested `BEGIN ... END` blocks, `RETURN` — for as long as
//! what they do is certain. A statement whose effect depends on run-time
//! values (`IF`, loops, `CASE`, SQL reading a variable, `EXECUTE` of a
//! computed string, `GET DIAGNOSTICS`, ...) makes the rest of the block
//! unknowable, and with it the block's final effect: what ran before may
//! be undone later (a `search_path` set and restored, a table dropped
//! again). Such a block leaves the catalog as it was.
//!
//! Errors are certain wherever they happen before that point: a failing
//! statement, `RAISE EXCEPTION`, a constant the variable's type rejects —
//! they fail the migration as in PG. An error inside a block with an
//! EXCEPTION clause rolls the block back; `WHEN OTHERS` then runs its
//! handler, while a specific condition makes the block unknowable (the
//! analyzer's errors don't carry the SQLSTATE the condition is matched
//! against).

use serde_json::Value;
use typedpg_pg_query::protobuf::{KeywordKind, Token, a_const, node};

use super::DdlError;
use crate::pg_catalog::PgCatalog;

/// A block variable (`PLpgSQL_var`, `_rec`, `_row` datum).
struct Variable {
    name: String,
    /// The declared type, as a type name the analyzer resolves (`None` for
    /// records and rows).
    type_name: Option<String>,
    /// The DEFAULT expression (`default_val`).
    default: Option<String>,
    /// The current value, as its type's output text — `Some(None)` for
    /// NULL — when it is a known constant.
    value: Option<Option<String>>,
}

/// How running a statement list ended.
enum Flow {
    /// Ran to its end.
    Done,
    /// `RETURN`: the block is over.
    Return,
    /// A statement whose effect can't be known statically: nothing after
    /// it runs.
    Unknown,
}

struct Run {
    variables: Vec<Variable>,
}

/// Run the PL/pgSQL `DO` block `code` against the catalog.
pub(crate) fn execute(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::protobuf::DoStmt,
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
    let (Some(code), "plpgsql") = (code, language.as_str()) else {
        return Ok(());
    };
    // The block as the body of a function, as function_body::do_block
    // compiles it.
    let mut tag = String::from("do");
    while code.contains(&format!("${tag}$")) {
        tag.push('x');
    }
    let sql = format!(
        "CREATE FUNCTION inline_code_block() RETURNS void LANGUAGE plpgsql AS ${tag}${code}${tag}$"
    );
    let Ok(parsed) = typedpg_pg_query::parse_plpgsql_with_catalog(&sql, &*interp) else {
        return Ok(());
    };
    let Some(function) = parsed.get(0).and_then(|f| f.get("PLpgSQL_function")) else {
        return Ok(());
    };
    let mut run = Run {
        variables: variables(function),
    };
    let Some(action) = function.get("action") else {
        return Ok(());
    };
    let start = interp.snapshot();
    if let Flow::Unknown = run.statement(interp, action)? {
        interp.roll_back_to(&start);
    }
    Ok(())
}

/// The function's datums, by varno.
fn variables(function: &Value) -> Vec<Variable> {
    let Some(datums) = function.get("datums").and_then(Value::as_array) else {
        return Vec::new();
    };
    datums
        .iter()
        .map(|datum| {
            let (kind, body) = datum
                .as_object()
                .and_then(|o| o.iter().next())
                .map(|(k, v)| (k.as_str(), v))
                .unwrap_or(("", &Value::Null));
            let name = body
                .get("refname")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let type_name = (kind == "PLpgSQL_var")
                .then(|| body.pointer("/datatype/PLpgSQL_type"))
                .flatten()
                .and_then(type_name);
            Variable {
                name,
                type_name,
                default: body
                    .get("default_val")
                    .and_then(expr_query)
                    .map(str::to_owned),
                // A variable starts out NULL; its DEFAULT is evaluated at
                // block entry.
                value: Some(None),
            }
        })
        .collect()
}

/// A declared type as a type name: its name as written (`origtypname`),
/// each part quoted, or the resolved name.
fn type_name(datatype: &Value) -> Option<String> {
    let quote = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    if let Some(parts) = datatype.get("origtypname").and_then(Value::as_array) {
        let parts: Vec<String> = parts.iter().filter_map(Value::as_str).map(quote).collect();
        if !parts.is_empty() {
            return Some(parts.join("."));
        }
    }
    datatype.get("typname").and_then(Value::as_str).map(quote)
}

/// The SQL text of a `PLpgSQL_expr`.
fn expr_query(expr: &Value) -> Option<&str> {
    expr.pointer("/PLpgSQL_expr/query").and_then(Value::as_str)
}

/// The value of a constant expression: `Some(Some(text))` for a literal
/// (as its type's output renders it), `Some(None)` for NULL, `None` when
/// `expr` isn't a constant.
fn constant(expr: &str) -> Option<Option<String>> {
    let parsed = typedpg_pg_query::parse(&format!("SELECT {expr}")).ok()?;
    let [raw] = parsed.protobuf.stmts.as_slice() else {
        return None;
    };
    let Some(node::Node::SelectStmt(select)) = raw.stmt.as_ref()?.node.as_ref() else {
        return None;
    };
    let [target] = select.target_list.as_slice() else {
        return None;
    };
    let Some(node::Node::ResTarget(rt)) = target.node.as_ref() else {
        return None;
    };
    let Some(node::Node::AConst(c)) = rt.val.as_deref()?.node.as_ref() else {
        return None;
    };
    if c.isnull {
        return Some(None);
    }
    Some(Some(match c.val.as_ref()? {
        a_const::Val::Ival(i) => i.ival.to_string(),
        a_const::Val::Fval(f) => f.fval.clone(),
        a_const::Val::Sval(s) => s.sval.clone(),
        a_const::Val::Boolval(b) => if b.boolval { "t" } else { "f" }.to_owned(),
        a_const::Val::Bsval(b) => b.bsval.clone(),
    }))
}

impl Run {
    /// Whether SQL `query` reads one of the block's variables (any
    /// identifier naming one — a column of the same name counts too, which
    /// only makes the run stop earlier).
    fn reads_variables(&self, query: &str) -> bool {
        let Ok(scan) = typedpg_pg_query::scan(query) else {
            return true;
        };
        scan.tokens.iter().any(|t| {
            let identifier = t.token == Token::Ident as i32
                || matches!(
                    KeywordKind::try_from(t.keyword_kind),
                    Ok(KeywordKind::UnreservedKeyword
                        | KeywordKind::ColNameKeyword
                        | KeywordKind::TypeFuncNameKeyword)
                );
            if !identifier {
                return false;
            }
            let (Ok(start), Ok(end)) = (usize::try_from(t.start), usize::try_from(t.end)) else {
                return true;
            };
            let Some(text) = query.get(start..end) else {
                return true;
            };
            let name = match text.strip_prefix('"').and_then(|t| t.strip_suffix('"')) {
                Some(quoted) => quoted.replace("\"\"", "\""),
                None => text.to_ascii_lowercase(),
            };
            self.variables.iter().any(|v| v.name == name)
        })
    }

    /// Run one statement.
    fn statement(&mut self, interp: &mut PgCatalog, stmt: &Value) -> Result<Flow, DdlError> {
        let Some((kind, body)) = stmt.as_object().and_then(|o| o.iter().next()) else {
            return Ok(Flow::Unknown);
        };
        match kind.as_str() {
            "PLpgSQL_stmt_block" => self.block(interp, body),
            "PLpgSQL_stmt_execsql" => {
                let Some(query) = body.get("sqlstmt").and_then(expr_query) else {
                    return Ok(Flow::Unknown);
                };
                if self.reads_variables(query) {
                    return Ok(Flow::Unknown);
                }
                if !runs_in_a_function(query) {
                    return Ok(Flow::Unknown);
                }
                super::apply_sql_to(interp, query)?;
                if body.get("into").and_then(Value::as_bool) == Some(true) {
                    // The targets get the row's values; STRICT fails
                    // unless exactly one row comes back.
                    self.forget_targets(body.get("target"));
                    if body.get("strict").and_then(Value::as_bool) == Some(true) {
                        return Ok(Flow::Unknown);
                    }
                }
                Ok(Flow::Done)
            }
            "PLpgSQL_stmt_dynexecute" => {
                if body.get("into").and_then(Value::as_bool) == Some(true)
                    || body.get("params").is_some()
                {
                    return Ok(Flow::Unknown);
                }
                let Some(Some(sql)) = body
                    .get("query")
                    .and_then(expr_query)
                    .and_then(|q| (!self.reads_variables(q)).then(|| constant(q)).flatten())
                else {
                    return Ok(Flow::Unknown);
                };
                if !runs_in_a_function(&sql) {
                    return Ok(Flow::Unknown);
                }
                super::apply_sql_to(interp, &sql)?;
                Ok(Flow::Done)
            }
            "PLpgSQL_stmt_perform" => {
                let Some(query) = body.get("expr").and_then(expr_query) else {
                    return Ok(Flow::Unknown);
                };
                if self.reads_variables(query) {
                    return Ok(Flow::Unknown);
                }
                super::apply_sql_to(interp, query)?;
                Ok(Flow::Done)
            }
            "PLpgSQL_stmt_assign" => self.assign(interp, body),
            "PLpgSQL_stmt_raise" => self.raise(body),
            "PLpgSQL_stmt_return" => Ok(Flow::Return),
            _ => Ok(Flow::Unknown),
        }
    }

    /// Run a statement list.
    fn statements(&mut self, interp: &mut PgCatalog, list: &Value) -> Result<Flow, DdlError> {
        for stmt in list.as_array().into_iter().flatten() {
            match self.statement(interp, stmt)? {
                Flow::Done => {}
                other => return Ok(other),
            }
        }
        Ok(Flow::Done)
    }

    /// `[DECLARE ...] BEGIN ... [EXCEPTION ...] END` (exec_stmt_block): the
    /// declared variables get their defaults, then the body runs — inside a
    /// subtransaction when there are exception handlers.
    fn block(&mut self, interp: &mut PgCatalog, block: &Value) -> Result<Flow, DdlError> {
        let handlers = block.pointer("/exceptions/PLpgSQL_exception_block/exc_list");
        let Some(handlers) = handlers else {
            self.initialize(interp, block)?;
            return self.statements(interp, block.get("body").unwrap_or(&Value::Null));
        };
        let start = interp.snapshot();
        let result = self
            .initialize(interp, block)
            .and_then(|()| self.statements(interp, block.get("body").unwrap_or(&Value::Null)));
        let Err(_) = result else {
            return result;
        };
        // exec_stmt_block rolls the subtransaction back, then looks for a
        // handler of the error's condition.
        interp.roll_back_to(&start);
        let others = handlers.as_array().into_iter().flatten().find(|handler| {
            handler
                .pointer("/PLpgSQL_exception/conditions")
                .and_then(Value::as_array)
                .is_some_and(|conditions| {
                    conditions.iter().any(|c| {
                        c.pointer("/PLpgSQL_condition/condname")
                            .and_then(Value::as_str)
                            == Some("others")
                    })
                })
        });
        match others {
            Some(handler) => self.statements(
                interp,
                handler
                    .pointer("/PLpgSQL_exception/action")
                    .unwrap_or(&Value::Null),
            ),
            None => Ok(Flow::Unknown),
        }
    }

    /// Block entry (exec_stmt_block): each variable the block declares
    /// gets its DEFAULT — a constant coerced to the variable's type, which
    /// fails on a value the type rejects — or NULL.
    fn initialize(&mut self, interp: &PgCatalog, block: &Value) -> Result<(), DdlError> {
        let Some(varnos) = block.get("initvarnos").and_then(Value::as_array) else {
            return Ok(());
        };
        for varno in varnos.iter().filter_map(Value::as_u64) {
            let Some(variable) = usize::try_from(varno)
                .ok()
                .and_then(|i| self.variables.get_mut(i))
            else {
                continue;
            };
            variable.value = match variable.default.as_deref() {
                None => Some(None),
                Some(default) => {
                    let value = constant(default);
                    if let (Some(Some(_)), Some(type_name)) = (&value, &variable.type_name) {
                        check_constant(interp, default, type_name)?;
                    }
                    value
                }
            };
        }
        Ok(())
    }

    /// `var := expr` (exec_stmt_assign): a constant is coerced to the
    /// variable's type — a value the type's input function rejects fails.
    fn assign(&mut self, interp: &PgCatalog, body: &Value) -> Result<Flow, DdlError> {
        let Some(varno) = body
            .get("varno")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
        else {
            return Ok(Flow::Unknown);
        };
        let Some(query) = body.get("expr").and_then(expr_query) else {
            return Ok(Flow::Unknown);
        };
        let Some(variable) = self.variables.get(varno) else {
            return Ok(Flow::Unknown);
        };
        // `name := expr` / `name = expr` onto the whole variable.
        let rest = query.trim_start();
        let Some(rest) = rest
            .get(..variable.name.len())
            .filter(|head| head.eq_ignore_ascii_case(&variable.name))
            .map(|_| rest[variable.name.len()..].trim_start())
        else {
            return Ok(Flow::Unknown);
        };
        let Some(rhs) = rest.strip_prefix(":=").or_else(|| rest.strip_prefix('=')) else {
            return Ok(Flow::Unknown);
        };
        let value = constant(rhs);
        if let (Some(Some(_)), Some(type_name)) = (&value, &variable.type_name) {
            check_constant(interp, rhs, type_name)?;
        }
        if let Some(v) = self.variables.get_mut(varno) {
            v.value = value;
        }
        Ok(Flow::Done)
    }

    /// The targets of `INTO` get values the analyzer doesn't know.
    fn forget_targets(&mut self, target: Option<&Value>) {
        let Some(target) = target else {
            return;
        };
        let fields = target
            .pointer("/PLpgSQL_row/fields")
            .and_then(Value::as_array);
        let varnos: Vec<u64> = match fields {
            Some(fields) => fields
                .iter()
                .filter_map(|f| f.get("varno").and_then(Value::as_u64))
                .collect(),
            None => target
                .as_object()
                .and_then(|o| o.values().next())
                .and_then(|b| b.get("dno"))
                .and_then(Value::as_u64)
                .into_iter()
                .collect(),
        };
        for varno in varnos {
            if let Some(v) = usize::try_from(varno)
                .ok()
                .and_then(|i| self.variables.get_mut(i))
            {
                v.value = None;
            }
        }
    }

    /// `RAISE` (exec_stmt_raise): below EXCEPTION only a message; at
    /// EXCEPTION the block fails with the message — the format with each
    /// `%` replaced by the next parameter's text, `USING MESSAGE`, the
    /// condition's name or the SQLSTATE.
    fn raise(&self, body: &Value) -> Result<Flow, DdlError> {
        /// `ERROR` (elog.h): RAISE EXCEPTION's level.
        const ERROR: u64 = 21;
        /// PLPGSQL_RAISEOPTION_ERRCODE / _MESSAGE.
        const OPTION_ERRCODE: u64 = 0;
        const OPTION_MESSAGE: u64 = 1;
        if body
            .get("elog_level")
            .and_then(Value::as_u64)
            .unwrap_or(ERROR)
            < ERROR
        {
            return Ok(Flow::Done);
        }
        // A re-raise (bare RAISE) happens only in a handler.
        let format = body.get("message").and_then(Value::as_str);
        let condname = body.get("condname").and_then(Value::as_str);
        if format.is_none() && condname.is_none() && body.get("options").is_none() {
            return Ok(Flow::Unknown);
        }
        let text = |expr: &Value| -> Option<String> {
            let query = expr_query(expr)?;
            let value = match self.variables.iter().find(|v| v.name == query.trim()) {
                Some(v) => v.value.clone()?,
                None => constant(query)?,
            };
            Some(value.unwrap_or_else(|| "<NULL>".to_owned()))
        };
        let mut message = match format {
            Some(format) => {
                let mut params = body
                    .get("params")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten();
                let mut out = String::new();
                let mut chars = format.chars().peekable();
                while let Some(c) = chars.next() {
                    if c != '%' {
                        out.push(c);
                    } else if chars.next_if_eq(&'%').is_some() {
                        out.push('%');
                    } else {
                        match params.next().and_then(text) {
                            Some(value) => out.push_str(&value),
                            // A value only running the block would give.
                            None => return Ok(Flow::Unknown),
                        }
                    }
                }
                Some(out)
            }
            None => None,
        };
        let mut errcode = None;
        for option in body
            .get("options")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let option = option.get("PLpgSQL_raise_option").unwrap_or(&Value::Null);
            let value = option.get("expr").and_then(text);
            match option.get("opt_type").and_then(Value::as_u64).unwrap_or(0) {
                OPTION_ERRCODE => match value {
                    Some(code) => errcode = Some(code),
                    None => return Ok(Flow::Unknown),
                },
                OPTION_MESSAGE => match value {
                    Some(text) => message = Some(text),
                    None => return Ok(Flow::Unknown),
                },
                _ => {}
            }
        }
        let message = message
            .or_else(|| condname.map(str::to_owned))
            .or(errcode)
            .unwrap_or_else(|| "P0001".to_owned());
        Err(DdlError::UnsupportedDdl(message))
    }
}

/// Statements a PL/pgSQL block can't run the way a migration does: those
/// PreventInTransactionBlock / RequireTransactionBlock refuse from a
/// function, and transaction control (only a non-atomic DO may commit).
fn runs_in_a_function(sql: &str) -> bool {
    let Ok(parsed) = typedpg_pg_query::parse(sql) else {
        return true;
    };
    !parsed.protobuf.stmts.iter().any(|raw| {
        matches!(
            raw.stmt.as_ref().and_then(|n| n.node.as_ref()),
            Some(
                node::Node::TransactionStmt(_)
                    | node::Node::VacuumStmt(_)
                    | node::Node::ReindexStmt(_)
                    | node::Node::ClusterStmt(_)
                    | node::Node::CreatedbStmt(_)
                    | node::Node::DropdbStmt(_)
                    | node::Node::CreateTableSpaceStmt(_)
                    | node::Node::DropTableSpaceStmt(_)
                    | node::Node::AlterSystemStmt(_)
                    | node::Node::DiscardStmt(_)
                    | node::Node::LockStmt(_)
                    | node::Node::DeclareCursorStmt(_)
                    | node::Node::DoStmt(_)
            )
        ) || matches!(
            raw.stmt.as_ref().and_then(|n| n.node.as_ref()),
            Some(node::Node::IndexStmt(i)) if i.concurrent
        )
    })
}

/// A constant coerced to `type_name`: what the type's input function
/// rejects fails, with its message.
fn check_constant(interp: &PgCatalog, constant: &str, type_name: &str) -> Result<(), DdlError> {
    let sql = format!("SELECT CAST({constant} AS {type_name})");
    let Ok(parsed) = typedpg_pg_query::parse(&sql) else {
        return Ok(());
    };
    let Some(stmt) = parsed
        .protobuf
        .stmts
        .first()
        .and_then(|s| s.stmt.as_ref())
        .and_then(|n| n.node.as_ref())
    else {
        return Ok(());
    };
    super::dml::check_statement(interp, stmt)
}
