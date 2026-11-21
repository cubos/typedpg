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

use typedpg_pg_query::ast::{self, KeywordKind, Token};
use typedpg_pg_query::plpgsql::{self, Datum, Stmt};

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
    /// For a row (an `INTO` target list), the varnos of its fields'
    /// variables.
    fields: Option<Vec<i32>>,
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
    stmt: &typedpg_pg_query::ast::DoStmt,
) -> Result<(), DdlError> {
    // The block's statements are parsed from its code, not the migration.
    let _barrier = crate::error::DiagContextGuard::barrier();
    let mut code = None;
    let mut language = "plpgsql".to_owned();
    for arg in &stmt.args {
        let Some(ast::Node::DefElem(de)) = arg.as_ref() else {
            continue;
        };
        let Some(ast::Node::String(s)) = de.arg.as_ref() else {
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
    let Some(function) = parsed.first() else {
        return Ok(());
    };
    let mut run = Run {
        variables: variables(function),
    };
    let Some(action) = &function.action else {
        return Ok(());
    };
    let start = interp.snapshot();
    if let Flow::Unknown = run.block(interp, action)? {
        interp.roll_back_to(&start);
    }
    Ok(())
}

/// The function's datums, by varno.
fn variables(function: &plpgsql::Function) -> Vec<Variable> {
    function
        .datums
        .iter()
        .map(|datum| {
            let (type_name, default, fields) = match datum {
                Datum::Var(v) | Datum::Promise(v) => (
                    v.datatype.as_ref().map(type_name),
                    v.default_val.as_ref(),
                    None,
                ),
                Datum::Row(r) => (
                    None,
                    r.default_val.as_ref(),
                    // A dropped column's field has no variable.
                    Some(
                        r.fields
                            .iter()
                            .filter(|f| f.name.is_some())
                            .map(|f| f.varno)
                            .collect(),
                    ),
                ),
                Datum::Rec(r) => (None, r.default_val.as_ref(), None),
                Datum::Recfield(_) => (None, None, None),
            };
            Variable {
                name: datum.refname().unwrap_or_default().to_owned(),
                type_name,
                default: default.map(|e| e.query.clone()),
                // A variable starts out NULL; its DEFAULT is evaluated at
                // block entry.
                value: Some(None),
                fields,
            }
        })
        .collect()
}

/// A declared type as a type name: its name as written (`origtypname`),
/// each part quoted, or the resolved name.
fn type_name(datatype: &plpgsql::Type) -> String {
    let quote = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    let written: Vec<String> = datatype
        .origtypname
        .iter()
        .flat_map(|t| &t.names)
        .filter_map(|n| match n {
            Some(ast::Node::String(s)) => Some(quote(&s.sval)),
            _ => None,
        })
        .collect();
    if written.is_empty() {
        quote(&datatype.typname)
    } else {
        written.join(".")
    }
}

/// The SQL text of a `PLpgSQL_expr`.
fn expr_query(expr: Option<&plpgsql::Expr>) -> Option<&str> {
    expr.map(|e| e.query.as_str())
}

/// The value of a constant expression: `Some(Some(text))` for a literal
/// (as its type's output renders it), `Some(None)` for NULL, `None` when
/// `expr` isn't a constant.
fn constant(expr: &str) -> Option<Option<String>> {
    let parsed = typedpg_pg_query::parse(&format!("SELECT {expr}")).ok()?;
    let [raw] = parsed.tree.stmts.as_slice() else {
        return None;
    };
    let Some(ast::Node::SelectStmt(select)) = raw.stmt.as_ref() else {
        return None;
    };
    let [target] = select.target_list.as_slice() else {
        return None;
    };
    let Some(ast::Node::ResTarget(rt)) = target.as_ref() else {
        return None;
    };
    let Some(ast::Node::AConst(c)) = rt.val.as_ref() else {
        return None;
    };
    if c.isnull {
        return Some(None);
    }
    Some(Some(match c.val.as_ref()? {
        ast::AConstValue::Integer(i) => i.ival.to_string(),
        ast::AConstValue::Float(f) => f.fval.clone(),
        ast::AConstValue::String(s) => s.sval.clone(),
        ast::AConstValue::Boolean(b) => if b.boolval { "t" } else { "f" }.to_owned(),
        ast::AConstValue::BitString(b) => b.bsval.clone(),
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
            let identifier = t.token == Token::Ident
                || matches!(
                    t.keyword_kind,
                    KeywordKind::UnreservedKeyword
                        | KeywordKind::ColNameKeyword
                        | KeywordKind::TypeFuncNameKeyword
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
    fn statement(&mut self, interp: &mut PgCatalog, stmt: &Stmt) -> Result<Flow, DdlError> {
        match stmt {
            Stmt::Block(block) => self.block(interp, block),
            Stmt::Execsql(s) => {
                let Some(query) = expr_query(s.sqlstmt.as_ref()) else {
                    return Ok(Flow::Unknown);
                };
                if self.reads_variables(query) {
                    return Ok(Flow::Unknown);
                }
                if !runs_in_a_function(query) {
                    return Ok(Flow::Unknown);
                }
                super::apply_sql_to(interp, query)?;
                if s.into {
                    // The targets get the row's values; STRICT fails
                    // unless exactly one row comes back.
                    self.forget_targets(s.target);
                    if s.strict {
                        return Ok(Flow::Unknown);
                    }
                }
                Ok(Flow::Done)
            }
            Stmt::Dynexecute(s) => {
                if s.into || !s.params.is_empty() {
                    return Ok(Flow::Unknown);
                }
                let Some(Some(sql)) = expr_query(s.query.as_ref())
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
            Stmt::Perform(s) => {
                let Some(query) = expr_query(s.expr.as_ref()) else {
                    return Ok(Flow::Unknown);
                };
                if self.reads_variables(query) {
                    return Ok(Flow::Unknown);
                }
                super::apply_sql_to(interp, query)?;
                Ok(Flow::Done)
            }
            Stmt::Assign(s) => self.assign(interp, s),
            Stmt::Raise(s) => self.raise(s),
            Stmt::Return(_) => Ok(Flow::Return),
            Stmt::If(s) => self.if_statement(interp, s),
            _ => Ok(Flow::Unknown),
        }
    }

    /// `IF condition THEN ... END IF` (exec_stmt_if). The common
    /// `CREATE ROLE IF NOT EXISTS` idiom is statically known: a
    /// `NOT EXISTS (SELECT ... FROM pg_roles WHERE rolname = 'name')`
    /// condition, whose outcome the catalog knows, selects the branch — the
    /// then-branch when the role doesn't exist, the else-branch (if any)
    /// when it does — and that branch runs through the normal interpreter.
    /// Any other condition, or an `ELSIF` chain that would have to be
    /// evaluated, is unknowable.
    fn if_statement(
        &mut self,
        interp: &mut PgCatalog,
        s: &plpgsql::StmtIf,
    ) -> Result<Flow, DdlError> {
        let Some(condition) = expr_query(s.cond.as_ref()) else {
            return Ok(Flow::Unknown);
        };
        let Some((role, view)) = role_not_exists_condition(condition) else {
            return Ok(Flow::Unknown);
        };
        // `pg_roles` has every role; `pg_user` (and `pg_shadow`) only those
        // with `rolcanlogin`.
        let exists = match view {
            RoleView::Roles => interp.roles.exists(&role),
            RoleView::User => interp.roles.exists_with_login(&role),
        };
        if exists {
            // The then-branch doesn't run. The else-branch does, unless an
            // ELSIF chain would have to decide first.
            if !s.elsif_list.is_empty() {
                return Ok(Flow::Unknown);
            }
            return self.statements(interp, &s.else_body);
        }
        // The condition is true: the then-branch runs and no ELSIF is
        // evaluated.
        self.statements(interp, &s.then_body)
    }

    /// Run a statement list.
    fn statements(&mut self, interp: &mut PgCatalog, list: &[Stmt]) -> Result<Flow, DdlError> {
        for stmt in list {
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
    fn block(
        &mut self,
        interp: &mut PgCatalog,
        block: &plpgsql::StmtBlock,
    ) -> Result<Flow, DdlError> {
        let handlers = block
            .exceptions
            .as_ref()
            .map(|e| &e.exc_list)
            .filter(|handlers| !handlers.is_empty());
        let Some(handlers) = handlers else {
            self.initialize(interp, block)?;
            return self.statements(interp, &block.body);
        };
        let start = interp.snapshot();
        let result = self
            .initialize(interp, block)
            .and_then(|()| self.statements(interp, &block.body));
        let Err(_) = result else {
            return result;
        };
        // exec_stmt_block rolls the subtransaction back, then looks for a
        // handler of the error's condition.
        interp.roll_back_to(&start);
        let others = handlers.iter().find(|handler| {
            handler
                .conditions
                .iter()
                .any(|c| c.sqlerrstate == plpgsql::Condition::OTHERS)
        });
        match others {
            Some(handler) => self.statements(interp, &handler.action),
            None => Ok(Flow::Unknown),
        }
    }

    /// Block entry (exec_stmt_block): each variable the block declares
    /// gets its DEFAULT — a constant coerced to the variable's type, which
    /// fails on a value the type rejects — or NULL.
    fn initialize(
        &mut self,
        interp: &PgCatalog,
        block: &plpgsql::StmtBlock,
    ) -> Result<(), DdlError> {
        for &varno in &block.initvarnos {
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
    fn assign(&mut self, interp: &PgCatalog, s: &plpgsql::StmtAssign) -> Result<Flow, DdlError> {
        let Ok(varno) = usize::try_from(s.varno) else {
            return Ok(Flow::Unknown);
        };
        let Some(query) = expr_query(s.expr.as_ref()) else {
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

    /// The targets of `INTO` get values the analyzer doesn't know: a row's
    /// fields, or the record or variable itself.
    fn forget_targets(&mut self, target: Option<i32>) {
        let Some(target) = target.and_then(|t| usize::try_from(t).ok()) else {
            return;
        };
        let varnos: Vec<usize> = match self.variables.get(target) {
            Some(Variable {
                fields: Some(fields),
                ..
            }) => fields
                .iter()
                .filter_map(|&f| usize::try_from(f).ok())
                .collect(),
            Some(_) => vec![target],
            None => return,
        };
        for varno in varnos {
            if let Some(v) = self.variables.get_mut(varno) {
                v.value = None;
            }
        }
    }

    /// `RAISE` (exec_stmt_raise): below EXCEPTION only a message; at
    /// EXCEPTION the block fails with the message — the format with each
    /// `%` replaced by the next parameter's text, `USING MESSAGE`, the
    /// condition's name or the SQLSTATE.
    fn raise(&self, s: &plpgsql::StmtRaise) -> Result<Flow, DdlError> {
        /// `ERROR` (elog.h): RAISE EXCEPTION's level.
        const ERROR: i32 = 21;
        if s.elog_level < ERROR {
            return Ok(Flow::Done);
        }
        // A re-raise (bare RAISE) happens only in a handler.
        let format = s.message.as_deref();
        let condname = s.condname.as_deref();
        if format.is_none() && condname.is_none() && s.options.is_empty() {
            return Ok(Flow::Unknown);
        }
        let text = |expr: Option<&plpgsql::Expr>| -> Option<String> {
            let query = expr_query(expr)?;
            let value = match self.variables.iter().find(|v| v.name == query.trim()) {
                Some(v) => v.value.clone()?,
                None => constant(query)?,
            };
            Some(value.unwrap_or_else(|| "<NULL>".to_owned()))
        };
        let mut message = match format {
            Some(format) => {
                let mut params = s.params.iter();
                let mut out = String::new();
                let mut chars = format.chars().peekable();
                while let Some(c) = chars.next() {
                    if c != '%' {
                        out.push(c);
                    } else if chars.next_if_eq(&'%').is_some() {
                        out.push('%');
                    } else {
                        match text(params.next()) {
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
        for option in &s.options {
            let value = text(option.expr.as_ref());
            match option.opt_type {
                plpgsql::RaiseOptionType::Errcode => match value {
                    Some(code) => errcode = Some(code),
                    None => return Ok(Flow::Unknown),
                },
                plpgsql::RaiseOptionType::Message => match value {
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
/// function, WAIT (top level only, PostgreSQL 19), and transaction control
/// (only a non-atomic DO may commit).
fn runs_in_a_function(sql: &str) -> bool {
    let Ok(parsed) = typedpg_pg_query::parse(sql) else {
        return true;
    };
    !parsed.tree.stmts.iter().any(|raw| {
        matches!(
            raw.stmt.as_ref(),
            Some(
                ast::Node::TransactionStmt(_)
                    | ast::Node::VacuumStmt(_)
                    | ast::Node::ReindexStmt(_)
                    | ast::Node::RepackStmt(_)
                    | ast::Node::WaitStmt(_)
                    | ast::Node::CreatedbStmt(_)
                    | ast::Node::DropdbStmt(_)
                    | ast::Node::CreateTableSpaceStmt(_)
                    | ast::Node::DropTableSpaceStmt(_)
                    | ast::Node::AlterSystemStmt(_)
                    | ast::Node::DiscardStmt(_)
                    | ast::Node::LockStmt(_)
                    | ast::Node::DeclareCursorStmt(_)
                    | ast::Node::DoStmt(_)
            )
        ) || matches!(
            raw.stmt.as_ref(),
            Some(ast::Node::IndexStmt(i)) if i.concurrent
        )
    })
}

/// The role a `NOT EXISTS (...)` condition checks for, when it is the common
/// Which catalog view a role-existence condition reads: `pg_roles` lists
/// every role, `pg_user` only the ones that can log in (it selects from
/// `pg_shadow`, which filters `rolcanlogin`).
#[derive(Clone, Copy)]
enum RoleView {
    Roles,
    User,
}

/// `IF NOT EXISTS (SELECT ... FROM pg_roles WHERE rolname = 'name')` idiom
/// (`pg_catalog.pg_roles`, `pg_user` with `usename`).
fn role_not_exists_condition(query: &str) -> Option<(String, RoleView)> {
    use typedpg_pg_query::ast::{AExprKind, BoolExprType, Node, SubLinkType};
    let parsed = typedpg_pg_query::parse(&format!("SELECT {query}")).ok()?;
    let [raw] = parsed.tree.stmts.as_slice() else {
        return None;
    };
    let Some(Node::SelectStmt(select)) = raw.stmt.as_ref() else {
        return None;
    };
    let [target] = select.target_list.as_slice() else {
        return None;
    };
    let Some(Node::ResTarget(res_target)) = target.as_ref() else {
        return None;
    };
    let Some(Node::BoolExpr(bool_expr)) = res_target.val.as_ref() else {
        return None;
    };
    if bool_expr.boolop != BoolExprType::NotExpr {
        return None;
    }
    let [argument] = bool_expr.args.as_slice() else {
        return None;
    };
    let Some(Node::SubLink(sub_link)) = argument.as_ref() else {
        return None;
    };
    if sub_link.sub_link_type != SubLinkType::ExistsSublink {
        return None;
    }
    let Some(Node::SelectStmt(subselect)) = sub_link.subselect.as_ref() else {
        return None;
    };
    let [from] = subselect.from_clause.as_slice() else {
        return None;
    };
    let Some(Node::RangeVar(range_var)) = from.as_ref() else {
        return None;
    };
    if !range_var.catalogname.is_empty()
        || !matches!(range_var.schemaname.as_str(), "" | "pg_catalog")
    {
        return None;
    }
    let (column, view) = match range_var.relname.as_str() {
        "pg_roles" => ("rolname", RoleView::Roles),
        "pg_user" => ("usename", RoleView::User),
        _ => return None,
    };
    if !subselect.group_clause.is_empty()
        || subselect.having_clause.is_some()
        || subselect.with_clause.is_some()
        || !subselect.sort_clause.is_empty()
        || subselect.limit_count.is_some()
        || subselect.limit_offset.is_some()
    {
        return None;
    }
    let Some(Node::AExpr(expr)) = subselect.where_clause.as_ref() else {
        return None;
    };
    if expr.kind != AExprKind::AexprOp
        || !expr
            .name
            .iter()
            .filter_map(|n| match n.as_ref()? {
                Node::String(s) => Some(s.sval.as_str()),
                _ => None,
            })
            .eq(["="])
    {
        return None;
    }
    let column_ref = |node: Option<&Node>| -> Option<String> {
        let Node::ColumnRef(column_ref) = node? else {
            return None;
        };
        let Node::String(name) = column_ref.fields.last()?.as_ref()? else {
            return None;
        };
        Some(name.sval.clone())
    };
    let literal = |node: Option<&Node>| -> Option<String> {
        let Node::AConst(constant) = node? else {
            return None;
        };
        if constant.isnull {
            return None;
        }
        match constant.val.as_ref()? {
            ast::AConstValue::String(s) => Some(s.sval.clone()),
            _ => None,
        }
    };
    let left = expr.lexpr.as_ref();
    let right = expr.rexpr.as_ref();
    if column_ref(left).as_deref() != Some(column) {
        return None;
    }
    Some((literal(right)?, view))
}

/// A constant coerced to `type_name`: what the type's input function
/// rejects fails, with its message.
fn check_constant(interp: &PgCatalog, constant: &str, type_name: &str) -> Result<(), DdlError> {
    let sql = format!("SELECT CAST({constant} AS {type_name})");
    let Ok(parsed) = typedpg_pg_query::parse(&sql) else {
        return Ok(());
    };
    let Some(stmt) = parsed.tree.stmts.first().and_then(|s| s.stmt.as_ref()) else {
        return Ok(());
    };
    super::dml::check_statement(interp, stmt)
}

#[cfg(test)]
mod tests {
    use crate::pg_catalog::PgCatalog;

    #[test]
    fn deeply_nested_blocks_run() {
        // 500 nested blocks: the innermost statement runs. (PL/pgSQL's
        // grammar takes up to 3,330; a server running them hits
        // max_stack_depth well before that, at a depth its configuration
        // decides.)
        let n = 500;
        let sql = format!(
            "DO $$ BEGIN {}CREATE TABLE t (a int);{} END $$",
            "BEGIN ".repeat(n),
            " END;".repeat(n)
        );
        let mut db = PgCatalog::new().unwrap();
        db.apply_sql(&sql).unwrap();
        let err = db.apply_sql("CREATE TABLE t (a int)").unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
    }
}
