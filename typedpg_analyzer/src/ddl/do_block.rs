//! Running a `DO` block (ExecuteDoStmt → plpgsql_inline_handler) as far
//! as it can be known statically.
//!
//! The analyzer has no PL/pgSQL interpreter. It runs the block's statements
//! in order for as long as what they do is certain: SQL statements that
//! don't read the block's variables, `EXECUTE` of a string whose value it
//! knows (a constant, a `format()` or concatenation of known values),
//! `PERFORM`, `RAISE`, assignments, nested `BEGIN ... END` blocks, `RETURN`,
//! an `IF` or `EXIT WHEN` whose condition it can decide, and loops over
//! constants (`FOREACH x IN ARRAY ARRAY[...]`, `FOR r IN SELECT * FROM
//! (VALUES ...)`, `FOR i IN 1..3`), iteration by iteration. A value is
//! tracked symbolically ([`super::symbolic`]); a condition is decided from
//! the variables' values, or from the catalog for the few catalog views
//! whose rows it holds (`EXISTS (SELECT FROM pg_roles WHERE rolname = ...)`,
//! `pg_publication`).
//!
//! A statement whose effect depends on what the analyzer can't know — a
//! loop over a table's rows, an `EXECUTE` of a computed string, a condition
//! it can't decide — is not run: what it *may* change is recorded instead,
//! in the catalog's [`Uncertain`] state, from the statements it may run
//! (an `EXECUTE format('GRANT %s ON %s TO %I', ...)` may grant privileges to
//! the role the last argument names; a fully unknown string may change
//! anything). The block then goes on: what follows certainly runs. Only a
//! statement that may leave the block (`RETURN`, an `EXIT` out of an
//! enclosing loop) makes the rest of it uncertain the same way.
//!
//! Errors are certain wherever the analyzer is certain of what ran before:
//! a failing statement, `RAISE EXCEPTION`, a constant the variable's type
//! rejects — they fail the migration as in PG. Once the catalog itself is
//! uncertain, a failing statement only becomes uncertain too. An error
//! inside a block with an EXCEPTION clause rolls the block back; `WHEN
//! OTHERS` then runs its handler, while a specific condition may or may not
//! match (the analyzer's errors don't carry the SQLSTATE the condition is
//! matched against): the handlers may run. A block with handlers whose body
//! ran something uncertain may have caught an error from it: the body's
//! effects may have been rolled back, and its handlers may have run.
//!
//! `CALL` runs a procedure's body the same way ([`call`]): a PL/pgSQL body
//! through this simulation, its arguments values the analyzer doesn't know;
//! a `LANGUAGE sql` body statement by statement, through the migration's
//! own interpreter, a query reading the arguments analyzed with them as
//! parameters. Bodies in other languages, or run under `SECURITY DEFINER`
//! or `SET` clauses, are not followed.
//!
//! A migration's `SELECT f(...)` (or a body's `PERFORM f(...)`) of a
//! PL/pgSQL function runs its body too ([`select_call`]), its arguments the
//! values the call passes as far as they are known, under its `SET
//! search_path` (the one `SET` clause followed); a call of one inside an
//! expression the analyzer evaluates gives the value its `RETURN` certainly
//! returns, the body run on a copy of the catalog ([`function_value`]).

use std::collections::BTreeMap;

use typedpg_pg_query::ast::{self, KeywordKind, Token};
use typedpg_pg_query::plpgsql::{self, Datum, Stmt};

use super::DdlError;
use super::functions::{AtomicBody, RoutineSource};
use super::symbolic::{self, Val};
use super::uncertain::Uncertain;
use crate::error::AnalyzeError;
use crate::oid::{PgProcOid, PgTypeOid};
use crate::pg_catalog::{PgCatalog, PgProc, ProKind, TypType};

/// How many iterations of a loop over constants the analyzer runs; a
/// longer loop is uncertain.
const MAX_ITERATIONS: usize = 1000;

/// A block variable (`PLpgSQL_var`, `_rec`, `_row` datum).
#[derive(Clone)]
struct Variable {
    name: String,
    /// The declared type, as a type name the analyzer resolves (`None` for
    /// records and rows).
    type_name: Option<String>,
    /// The DEFAULT expression (`default_val`).
    default: Option<String>,
    /// The current value (see [`Val`]); NULL to start with.
    value: Val,
    /// For a row (an `INTO` target list), the varnos of its fields'
    /// variables.
    fields: Option<Vec<i32>>,
    /// A record or a row: read by field.
    is_record: bool,
    /// A record's fields' values, by name, when it holds a row the
    /// analyzer follows.
    record: Option<BTreeMap<String, Val>>,
}

/// How running a statement list ended.
enum Flow {
    /// Ran to its end.
    Done,
    /// `RETURN`: the block is over.
    Return,
    /// `EXIT [label]`.
    Exit(Option<String>),
    /// `CONTINUE [label]`.
    Continue(Option<String>),
    /// Control may have left by a way the analyzer can't tell: nothing after
    /// it runs, and what would have run is recorded as uncertain.
    Unknown,
}

#[derive(Clone)]
struct Run {
    variables: Vec<Variable>,
    /// The block runs in an atomic context: transaction control fails
    /// (`_SPI_commit` / `_SPI_rollback`: "invalid transaction
    /// termination").
    atomic: bool,
    /// A procedure's body: `$n` reads an argument.
    arguments: bool,
    /// Something ran whose outcome the analyzer doesn't know, and may have
    /// raised an error an enclosing block's handler caught.
    may_raise: bool,
    /// The varnos in scope, innermost last: the blocks' declarations and
    /// the loops' targets entered.
    scope: Vec<i32>,
    /// The value a function's `RETURN expr` certainly returned.
    returned: Option<Val>,
}

thread_local! {
    /// What the bodies running may have changed (see [`Uncertain`]),
    /// innermost top-level run last: kept apart from the catalog, which a
    /// subtransaction's rollback restores.
    static PENDING: std::cell::RefCell<Vec<Uncertain>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Collects what a top-level `DO` or `CALL` may change while it runs.
struct Pending;

impl Pending {
    fn enter() -> Pending {
        PENDING.with_borrow_mut(|stack| stack.push(Uncertain::default()));
        Pending
    }

    /// What was recorded while it ran.
    fn finish(self) -> Uncertain {
        let found = PENDING
            .with_borrow_mut(|stack| stack.pop())
            .unwrap_or_default();
        std::mem::forget(self);
        found
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        PENDING.with_borrow_mut(|stack| stack.pop());
    }
}

/// Record that the running body may have changed `found`.
fn note(interp: &mut PgCatalog, found: &Uncertain) {
    if found.is_empty() {
        return;
    }
    PENDING.with_borrow_mut(|stack| {
        if let Some(top) = stack.last_mut() {
            top.merge(found);
        }
    });
    record(interp, found);
}

/// Record `found` in the catalog.
pub(super) fn record(interp: &mut PgCatalog, found: &Uncertain) {
    if found.is_empty() {
        return;
    }
    interp.roles.uncertain.merge(found);
    if found.routines {
        interp.forget_user_routine_bodies();
    }
}

/// After a rollback inside the running body: what it recorded still holds.
fn restore_pending(interp: &mut PgCatalog) {
    let found = PENDING.with_borrow(|stack| stack.last().cloned().unwrap_or_default());
    record(interp, &found);
}

/// Run `f`, the statements of `sql`, which the body certainly runs: an
/// error is certain — unless it may come from what is uncertain (see
/// [`super::uncertain::Uncertain::may_fail_on`]), when the statements'
/// effect only becomes uncertain (`None`).
fn certainly<R>(
    interp: &mut PgCatalog,
    sql: &str,
    f: impl FnOnce(&mut PgCatalog) -> Result<R, DdlError>,
) -> Result<Option<R>, DdlError> {
    let uncertain = &interp.roles.uncertain;
    let may_fail_on = !uncertain.is_empty()
        && typedpg_pg_query::parse(sql).map_or(true, |parsed| {
            parsed
                .tree
                .stmts
                .iter()
                .filter_map(|raw| raw.stmt.as_ref())
                .any(|stmt| uncertain.may_fail_on(stmt))
        });
    uncertain_unless(interp, may_fail_on, f)
}

/// [`certainly`], once whether an error may come from what is uncertain
/// (`may_fail_on`) is known.
fn uncertain_unless<R>(
    interp: &mut PgCatalog,
    may_fail_on: bool,
    f: impl FnOnce(&mut PgCatalog) -> Result<R, DdlError>,
) -> Result<Option<R>, DdlError> {
    if !may_fail_on {
        return f(interp).map(Some);
    }
    let start = interp.snapshot();
    match f(interp) {
        Ok(r) => Ok(Some(r)),
        Err(_) => {
            interp.roll_back_to(&start);
            restore_pending(interp);
            Ok(None)
        }
    }
}

/// Run the `DO` statement `stmt` of a migration: ExecuteDoStmt runs the
/// block non-atomically only as a top-level statement outside a
/// transaction block (standard_ProcessUtility's isAtomicContext; the
/// implicit block of a multi-statement query counts). What the block may
/// have changed stays recorded in the catalog.
pub(crate) fn execute(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::ast::DoStmt,
) -> Result<(), DdlError> {
    let atomic = interp.in_transaction_block;
    let pending = Pending::enter();
    run_do_block(interp, stmt, atomic)?;
    let found = pending.finish();
    record(interp, &found);
    Ok(())
}

/// A `DO` statement a running body executes — a PL/pgSQL `DO`
/// (exec_stmt_call with `is_call` false), an `EXECUTE`d one, a SQL
/// procedure's — as standard_ProcessUtility runs it: compiled
/// (function_body::do_block), then run, atomically when `atomic`.
fn run_nested_do(interp: &mut PgCatalog, stmt: &ast::Node, atomic: bool) -> Result<Flow, DdlError> {
    let ast::Node::DoStmt(do_stmt) = stmt else {
        return Ok(Flow::Unknown);
    };
    super::fk_bypass::note(interp, stmt);
    super::function_body::do_block(interp, do_stmt)?;
    super::note_user_code(interp);
    run_do_block(interp, do_stmt, atomic)
}

/// Nothing of a body is known: it may have changed anything.
fn unknowable(interp: &mut PgCatalog) -> Result<Flow, DdlError> {
    note(
        interp,
        &Uncertain {
            catalog: true,
            ..Uncertain::default()
        },
    );
    Ok(Flow::Unknown)
}

/// Run the PL/pgSQL block of `DO` statement `stmt`
/// (plpgsql_inline_handler), atomically when `atomic`, unless that would
/// nest too deep (see [`enter_body`]). A block in another language, or
/// one that doesn't compile, may change anything.
fn run_do_block(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::ast::DoStmt,
    atomic: bool,
) -> Result<Flow, DdlError> {
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
        return unknowable(interp);
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
        return unknowable(interp);
    };
    let Some(function) = parsed.first() else {
        return unknowable(interp);
    };
    let Some(action) = &function.action else {
        return unknowable(interp);
    };
    let Some(_leave) = enter_body(None) else {
        return unknowable(interp);
    };
    let mut run = Run {
        variables: variables(function),
        atomic,
        arguments: false,
        may_raise: false,
        scope: Vec::new(),
        returned: None,
    };
    // The block's RETURN ends the block, not its caller.
    Ok(match run.block(interp, action)? {
        Flow::Unknown => Flow::Unknown,
        _ if run.may_raise => Flow::Unknown,
        _ => Flow::Done,
    })
}

/// The function's datums, by varno.
fn variables(function: &plpgsql::Function) -> Vec<Variable> {
    function
        .datums
        .iter()
        .map(|datum| {
            let (type_name, default, fields, is_record) = match datum {
                Datum::Var(v) | Datum::Promise(v) => (
                    v.datatype.as_ref().map(type_name),
                    v.default_val.as_ref(),
                    None,
                    false,
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
                    true,
                ),
                Datum::Rec(r) => (None, r.default_val.as_ref(), None, true),
                Datum::Recfield(_) => (None, None, None, false),
            };
            Variable {
                name: datum.refname().unwrap_or_default().to_owned(),
                type_name,
                default: default.map(|e| e.query.clone()),
                // A variable starts out NULL; its DEFAULT is evaluated at
                // block entry.
                value: Val::known(None),
                fields,
                is_record,
                record: None,
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

/// The right-hand side of assignment `query` (`name := expr` or `name =
/// expr`) onto the whole variable `name`.
fn assignment_rhs<'q>(query: &'q str, name: &str) -> Option<&'q str> {
    let rest = query.trim_start();
    let rest = rest
        .get(..name.len())
        .filter(|head| head.eq_ignore_ascii_case(name))
        .map(|_| rest[name.len()..].trim_start())?;
    rest.strip_prefix(":=").or_else(|| rest.strip_prefix('='))
}

/// Whether running `stmt` may leave the statement list it is in: a
/// `RETURN`, or an `EXIT` / `CONTINUE` aimed at a loop or block around it.
fn escapes(stmt: &Stmt) -> bool {
    fn walk(stmt: &Stmt, labels: &mut Vec<String>, in_loop: bool) -> bool {
        let list = |list: &[Stmt], labels: &mut Vec<String>, in_loop: bool| {
            list.iter().any(|s| walk(s, labels, in_loop))
        };
        let looped = |label: &Option<String>, body: &[Stmt], labels: &mut Vec<String>| {
            labels.extend(label.clone());
            let out = list(body, labels, true);
            if label.is_some() {
                labels.pop();
            }
            out
        };
        match stmt {
            Stmt::Return(_) => true,
            Stmt::Exit(e) => match &e.label {
                None => !in_loop,
                Some(label) => !labels.contains(label),
            },
            Stmt::Block(b) => {
                labels.extend(b.label.clone());
                let out = list(&b.body, labels, in_loop)
                    || b.exceptions
                        .iter()
                        .flat_map(|e| &e.exc_list)
                        .any(|h| list(&h.action, labels, in_loop));
                if b.label.is_some() {
                    labels.pop();
                }
                out
            }
            Stmt::If(s) => {
                list(&s.then_body, labels, in_loop)
                    || s.elsif_list.iter().any(|e| list(&e.stmts, labels, in_loop))
                    || list(&s.else_body, labels, in_loop)
            }
            Stmt::Case(s) => {
                s.case_when_list
                    .iter()
                    .any(|w| list(&w.stmts, labels, in_loop))
                    || list(&s.else_stmts, labels, in_loop)
            }
            Stmt::Loop(s) => looped(&s.label, &s.body, labels),
            Stmt::While(s) => looped(&s.label, &s.body, labels),
            Stmt::Fori(s) => looped(&s.label, &s.body, labels),
            Stmt::Fors(s) => looped(&s.label, &s.body, labels),
            Stmt::Forc(s) => looped(&s.label, &s.body, labels),
            Stmt::ForeachA(s) => looped(&s.label, &s.body, labels),
            Stmt::Dynfors(s) => looped(&s.label, &s.body, labels),
            _ => false,
        }
    }
    walk(stmt, &mut Vec::new(), false)
}

impl Run {
    /// The variable `name` reads: the innermost one in scope (a block's
    /// declarations, a loop's target), or the only variable of that name.
    /// `Err(())` when several are and none is in scope: which one is meant
    /// isn't followed.
    fn lookup(&self, name: &str, record: bool) -> Option<Result<&Variable, ()>> {
        let matches = |v: &Variable| v.name == name && (!record || v.is_record);
        if let Some(v) = self
            .scope
            .iter()
            .rev()
            .filter_map(|&varno| self.variables.get(usize::try_from(varno).ok()?))
            .find(|v| matches(v))
        {
            return Some(Ok(v));
        }
        let mut found = self.variables.iter().filter(|v| matches(v));
        let first = found.next()?;
        Some(if found.next().is_some() {
            Err(())
        } else {
            Ok(first)
        })
    }
}

impl symbolic::Scope for Run {
    fn variable(&self, name: &str) -> Option<Val> {
        Some(match self.lookup(name, false)? {
            Ok(v) if !v.is_record => v.value.clone(),
            _ => Val::any(),
        })
    }

    fn field(&self, record: &str, field: &str) -> Option<Val> {
        Some(match self.lookup(record, true)? {
            Ok(v) => v
                .record
                .as_ref()
                .and_then(|fields| fields.get(field).cloned())
                .unwrap_or_else(Val::any),
            Err(()) => Val::any(),
        })
    }
}

/// The block's variables, as a statement that certainly runs reads them:
/// with the catalog as it is now (`current_setting('search_path')`,
/// `EXISTS (SELECT FROM pg_roles ...)`).
struct Live<'a>(&'a Run, &'a PgCatalog);

impl symbolic::Scope for Live<'_> {
    fn variable(&self, name: &str) -> Option<Val> {
        self.0.variable(name)
    }

    fn field(&self, record: &str, field: &str) -> Option<Val> {
        self.0.field(record, field)
    }

    fn catalog(&self) -> Option<&PgCatalog> {
        Some(self.1)
    }
}

/// One iteration's values for a loop's target.
#[derive(Clone)]
enum Binding {
    /// A scalar variable's value.
    Value(Val),
    /// A record's (or row's) fields, by name, in order.
    Fields(Vec<(String, Val)>),
}

/// A binding standing for any of `bindings`: each value, or each field,
/// joined (a field some lack is unknown).
fn join_bindings(bindings: impl IntoIterator<Item = Binding>) -> Binding {
    let mut joined: Option<Binding> = None;
    for binding in bindings {
        joined = Some(match (joined, binding) {
            (None, b) => b,
            (Some(Binding::Value(a)), Binding::Value(b)) => Binding::Value(a.join(&b)),
            (Some(Binding::Fields(a)), Binding::Fields(b)) => Binding::Fields(
                a.into_iter()
                    .map(|(name, value)| {
                        let joined = match b.iter().find(|(n, _)| *n == name) {
                            Some((_, other)) => value.join(other),
                            None => Val::any(),
                        };
                        (name, joined)
                    })
                    .collect(),
            ),
            _ => Binding::Value(Val::any()),
        });
    }
    // A loop over nothing runs no iteration: its target's value doesn't
    // matter.
    joined.unwrap_or(Binding::Value(Val::known(None)))
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
            // `$n`: an argument, by position.
            if t.token == Token::Param && self.arguments {
                return true;
            }
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

    /// `query`, a `SELECT` of expressions only (no `FROM`), with every
    /// variable it reads replaced by its value — as PL/pgSQL passes it, a
    /// value of the variable's type — when the analyzer knows them all.
    /// In such a query every name that isn't a function's, a type's or an
    /// alias is a variable or an error.
    fn substitute(&self, interp: &PgCatalog, query: &str) -> Option<String> {
        let parsed = typedpg_pg_query::parse(query).ok()?;
        let [raw] = parsed.tree.stmts.as_slice() else {
            return None;
        };
        let Some(ast::Node::SelectStmt(select)) = raw.stmt.as_ref() else {
            return None;
        };
        if !select.from_clause.is_empty()
            || select.with_clause.is_some()
            || select.larg.is_some()
            || select.into_clause.is_some()
            || !select.values_lists.is_empty()
        {
            return None;
        }
        let scan = typedpg_pg_query::scan(query).ok()?;
        let live = Live(self, interp);
        let mut out = String::new();
        let mut copied = 0;
        let tokens = &scan.tokens;
        for (i, t) in tokens.iter().enumerate() {
            if t.token == Token::Param {
                return None;
            }
            let identifier = t.token == Token::Ident
                || matches!(
                    t.keyword_kind,
                    KeywordKind::UnreservedKeyword
                        | KeywordKind::ColNameKeyword
                        | KeywordKind::TypeFuncNameKeyword
                );
            if !identifier {
                continue;
            }
            let start = usize::try_from(t.start).ok()?;
            let end = usize::try_from(t.end).ok()?;
            let text = query.get(start..end)?;
            let name = match text.strip_prefix('"').and_then(|t| t.strip_suffix('"')) {
                Some(quoted) => quoted.replace("\"\"", "\""),
                None => text.to_ascii_lowercase(),
            };
            let Some(variable) = self.variables.iter().find(|v| v.name == name) else {
                continue;
            };
            let text_of = |token: Option<&ast::ScanToken>| {
                token
                    .and_then(|t| {
                        query.get(usize::try_from(t.start).ok()?..usize::try_from(t.end).ok()?)
                    })
                    .unwrap_or_default()
            };
            let before = text_of(i.checked_sub(1).and_then(|j| tokens.get(j)));
            let after = text_of(tokens.get(i + 1));
            // A function's name, a type's, an alias, a qualified name.
            if after == "(" || after == "." || before == "." || before == "::" {
                continue;
            }
            if before.eq_ignore_ascii_case("as") {
                continue;
            }
            if variable.is_record {
                return None;
            }
            let value = symbolic::Scope::variable(&live, &name)?;
            let value = value.as_known()?;
            let literal = match value {
                Some(text) => format!("'{}'", text.replace('\'', "''")),
                None => "NULL".to_owned(),
            };
            out.push_str(query.get(copied..start)?);
            // A string type's value as a plain literal, which resolves to
            // the parameter's string type as the variable would (and which
            // `set_config` reads as the constant it is).
            let string_type = variable.type_name.as_deref().is_some_and(|t| {
                let t = t.to_ascii_lowercase().replace('"', "");
                ["text", "varchar", "character varying", "name"]
                    .iter()
                    .any(|s| t == *s || t == format!("pg_catalog.{s}"))
            });
            match &variable.type_name {
                Some(type_name) if !string_type || value.is_none() => {
                    out.push_str(&format!("CAST({literal} AS {type_name})"));
                }
                _ => out.push_str(&literal),
            }
            copied = end;
        }
        out.push_str(query.get(copied..)?);
        Some(out)
    }

    fn variable_mut(&mut self, varno: i32) -> Option<&mut Variable> {
        usize::try_from(varno)
            .ok()
            .and_then(|i| self.variables.get_mut(i))
    }

    /// Bind a loop's target `varno` for one iteration.
    fn bind(&mut self, varno: i32, binding: &Binding) {
        let row_fields = self.variable_mut(varno).and_then(|v| v.fields.clone());
        match (binding, row_fields) {
            (Binding::Value(value), _) => {
                if let Some(v) = self.variable_mut(varno) {
                    v.value = value.clone();
                    v.record = None;
                }
            }
            // A row: its variables, by position.
            (Binding::Fields(fields), Some(varnos)) => {
                for (i, &field) in varnos.iter().enumerate() {
                    let value = fields
                        .get(i)
                        .map_or_else(Val::any, |(_, value)| value.clone());
                    if let Some(v) = self.variable_mut(field) {
                        v.value = value;
                    }
                }
            }
            (Binding::Fields(fields), None) => {
                if let Some(v) = self.variable_mut(varno) {
                    v.value = Val::any();
                    v.record = Some(fields.iter().cloned().collect());
                }
            }
        }
    }

    /// A loop's body may run any number of times, its target bound to
    /// `target` (whatever row it reads) at each iteration: what it may
    /// change goes to `found`. Each pass starts from what the loop started
    /// with merged with what the previous pass left, until the values
    /// settle; a value still changing after a few passes is unknown. After
    /// the loop, the variables hold what they held before it or what the
    /// body left.
    fn taint_loop(
        &mut self,
        interp: &PgCatalog,
        body: &[Stmt],
        target: Option<(i32, Binding)>,
        found: &mut Uncertain,
    ) {
        const PASSES: usize = 3;
        let before = self.clone();
        let mut start = before.clone();
        for pass in 0..=PASSES {
            let mut walk = start.clone();
            if let Some((varno, binding)) = &target {
                walk.scope.push(*varno);
                walk.bind(*varno, binding);
            }
            for stmt in body {
                walk.taint(interp, stmt, found);
            }
            walk.scope.truncate(before.scope.len());
            let mut next = before.clone();
            next.join_from(&walk);
            let settled = next.same_values(&start);
            start = next;
            if settled {
                break;
            }
            if pass + 1 == PASSES {
                // Still changing: what changes is unknown.
                for (v, b) in start.variables.iter_mut().zip(&before.variables) {
                    if v.value != b.value {
                        v.value = Val::any();
                    }
                    if v.record != b.record {
                        v.record = None;
                    }
                }
            }
        }
        *self = start;
    }

    /// Whether every variable holds the same values as in `other`.
    fn same_values(&self, other: &Run) -> bool {
        self.variables
            .iter()
            .zip(&other.variables)
            .all(|(a, b)| a.value == b.value && a.record == b.record)
    }

    /// Every variable may hold what it holds in `other` too.
    fn join_from(&mut self, other: &Run) {
        for (mine, theirs) in self.variables.iter_mut().zip(&other.variables) {
            mine.value = mine.value.join(&theirs.value);
            mine.record = match (mine.record.take(), &theirs.record) {
                (Some(a), Some(b)) => Some(
                    a.into_iter()
                        .map(|(name, value)| {
                            let joined = match b.get(&name) {
                                Some(other) => value.join(other),
                                None => Val::any(),
                            };
                            (name, joined)
                        })
                        .collect(),
                ),
                _ => None,
            };
        }
    }

    /// Run one statement.
    fn statement(&mut self, interp: &mut PgCatalog, stmt: &Stmt) -> Result<Flow, DdlError> {
        match stmt {
            Stmt::Block(block) => self.block(interp, block),
            Stmt::Execsql(s) => {
                let Some(query) = expr_query(s.sqlstmt.as_ref()) else {
                    return self.unknown(interp, stmt);
                };
                if let Ok(parsed) = typedpg_pg_query::parse(query) {
                    for raw in &parsed.tree.stmts {
                        refuse_copy_to_or_from_client(raw.stmt.as_ref())?;
                    }
                }
                if !runs_in_a_function(query) {
                    return self.unknown(interp, stmt);
                }
                let query = match self.reads_variables(query) {
                    false => query.to_owned(),
                    true => match self.substitute(interp, query) {
                        Some(query) => query,
                        None => return self.unknown(interp, stmt),
                    },
                };
                let ran = certainly(interp, &query, |interp| super::apply_sql_to(interp, &query))?;
                if ran.is_none() {
                    return self.unknown(interp, stmt);
                }
                if s.into {
                    // The targets get the row's values; STRICT fails
                    // unless exactly one row comes back.
                    self.forget_targets(s.target);
                    if s.strict {
                        self.may_raise = true;
                    }
                }
                Ok(Flow::Done)
            }
            Stmt::Dynexecute(s) => {
                if s.into || !s.params.is_empty() {
                    return self.unknown(interp, stmt);
                }
                let value =
                    expr_query(s.query.as_ref()).map(|q| symbolic::eval(&Live(self, interp), q));
                let Some(Some(sql)) = value.as_ref().and_then(Val::as_known).cloned() else {
                    return self.unknown(interp, stmt);
                };
                if !runs_in_a_function(&sql) {
                    return self.unknown(interp, stmt);
                }
                // exec_stmt_dynexecute runs it atomically (SPI_execute_extended
                // without allow_nonatomic).
                match certainly(interp, &sql, |interp| run_sql(interp, &sql, true))? {
                    None => self.unknown(interp, stmt),
                    Some(flow) => Ok(self.ran_body(flow)),
                }
            }
            Stmt::Perform(s) => {
                let Some(query) = expr_query(s.expr.as_ref()) else {
                    return self.unknown(interp, stmt);
                };
                let query = match self.reads_variables(query) {
                    false => query.to_owned(),
                    true => match self.substitute(interp, query) {
                        Some(query) => query,
                        None => return self.unknown(interp, stmt),
                    },
                };
                let ran = certainly(interp, &query, |interp| super::apply_sql_to(interp, &query))?;
                if ran.is_none() {
                    return self.unknown(interp, stmt);
                }
                Ok(Flow::Done)
            }
            Stmt::Assign(s) => self.assign(interp, stmt, s),
            Stmt::Raise(s) => self.raise(s),
            Stmt::Return(s) => {
                if let Some(query) = expr_query(s.expr.as_ref()) {
                    self.returned = Some(symbolic::eval(&Live(self, interp), query));
                }
                Ok(Flow::Return)
            }
            Stmt::If(s) => self.if_statement(interp, stmt, s),
            Stmt::Exit(s) => {
                let flow = if s.is_exit {
                    Flow::Exit(s.label.clone())
                } else {
                    Flow::Continue(s.label.clone())
                };
                match expr_query(s.cond.as_ref()) {
                    None => Ok(flow),
                    Some(cond) => match symbolic::truth(&Live(self, interp), cond) {
                        Some(true) => Ok(flow),
                        Some(false) => Ok(Flow::Done),
                        None => Ok(Flow::Unknown),
                    },
                }
            }
            Stmt::ForeachA(s) => {
                let elements = (s.slice == 0)
                    .then(|| expr_query(s.expr.as_ref()))
                    .flatten()
                    .and_then(|q| symbolic::array_elements(&Live(self, interp), q));
                match elements {
                    Some(elements) if elements.len() <= MAX_ITERATIONS => {
                        let bindings = elements.into_iter().map(Binding::Value).collect();
                        self.unroll(interp, s.label.as_ref(), s.varno, &s.body, bindings)
                    }
                    _ => self.unknown(interp, stmt),
                }
            }
            Stmt::Fors(s) => {
                let rows = expr_query(s.query.as_ref())
                    .and_then(|q| symbolic::constant_values_rows(&Live(self, interp), q));
                match (rows, s.var) {
                    (Some(rows), Some(var)) if rows.len() <= MAX_ITERATIONS => {
                        let bindings = rows.into_iter().map(Binding::Fields).collect();
                        self.unroll(interp, s.label.as_ref(), var, &s.body, bindings)
                    }
                    _ => self.unknown(interp, stmt),
                }
            }
            Stmt::Fori(s) => match (self.fori_values(interp, s), s.var) {
                (Some(values), Some(var)) => {
                    let bindings = values
                        .into_iter()
                        .map(|i| Binding::Value(Val::known(Some(i.to_string()))))
                        .collect();
                    self.unroll(interp, s.label.as_ref(), var, &s.body, bindings)
                }
                _ => self.unknown(interp, stmt),
            },
            Stmt::Call(s) => self.call(interp, stmt, s),
            // _SPI_commit / _SPI_rollback: an atomic context can't end its
            // transaction.
            Stmt::Commit(_) | Stmt::Rollback(_) if self.atomic => Err(DdlError::UnsupportedDdl(
                "invalid transaction termination".into(),
            )),
            _ => self.unknown(interp, stmt),
        }
    }

    /// A procedure or `DO` block the body ran: an uncertain one recorded
    /// what it may have changed, its own continuation included — the body
    /// goes on, with its outcome uncertain.
    fn ran_body(&mut self, flow: Flow) -> Flow {
        if let Flow::Unknown = flow {
            self.may_raise = true;
        }
        Flow::Done
    }

    /// Statement `stmt` can't be run: record what it may change, and the
    /// values it may leave in the variables. What follows runs, unless
    /// `stmt` may leave the statement list.
    fn unknown(&mut self, interp: &mut PgCatalog, stmt: &Stmt) -> Result<Flow, DdlError> {
        let mut found = Uncertain::default();
        let mut walk = self.clone();
        walk.taint(interp, stmt, &mut found);
        note(interp, &found);
        self.variables = walk.variables;
        self.may_raise = true;
        Ok(if escapes(stmt) {
            Flow::Unknown
        } else {
            Flow::Done
        })
    }

    /// The statements of `rest` may run (control may have left before):
    /// record what they may change.
    fn uncertain_rest(&mut self, interp: &mut PgCatalog, rest: &[Stmt]) {
        let mut found = Uncertain::default();
        let mut walk = self.clone();
        for stmt in rest {
            walk.taint(interp, stmt, &mut found);
        }
        note(interp, &found);
        self.may_raise = true;
    }

    /// The `FOR i IN lower .. upper [BY step]` values, when its bounds are
    /// constants.
    fn fori_values(&self, interp: &PgCatalog, s: &plpgsql::StmtFori) -> Option<Vec<i64>> {
        let int = |expr: Option<&plpgsql::Expr>| -> Option<i64> {
            symbolic::eval(&Live(self, interp), expr_query(expr)?)
                .as_known()?
                .as_deref()?
                .parse()
                .ok()
        };
        let lower = int(s.lower.as_ref())?;
        let upper = int(s.upper.as_ref())?;
        let step = match s.step.as_ref() {
            Some(step) => int(Some(step))?,
            None => 1,
        };
        if step <= 0 {
            return None;
        }
        let mut values = Vec::new();
        let mut i = lower;
        loop {
            let more = if s.reverse { i >= upper } else { i <= upper };
            if !more {
                break;
            }
            if values.len() >= MAX_ITERATIONS {
                return None;
            }
            values.push(i);
            i = if s.reverse {
                i.checked_sub(step)?
            } else {
                i.checked_add(step)?
            };
        }
        Some(values)
    }

    /// Run a loop over values the analyzer knows, iteration by iteration
    /// (exec_for_query / exec_stmt_foreach_a / exec_stmt_fori).
    fn unroll(
        &mut self,
        interp: &mut PgCatalog,
        label: Option<&String>,
        varno: i32,
        body: &[Stmt],
        bindings: Vec<Binding>,
    ) -> Result<Flow, DdlError> {
        let ours = |l: &Option<String>| l.is_none() || l.as_ref() == label;
        let mark = self.scope.len();
        self.scope.push(varno);
        let flow = self.iterate(interp, varno, body, &bindings, ours);
        self.scope.truncate(mark);
        flow
    }

    /// The iterations of [`Self::unroll`].
    fn iterate(
        &mut self,
        interp: &mut PgCatalog,
        varno: i32,
        body: &[Stmt],
        bindings: &[Binding],
        ours: impl Fn(&Option<String>) -> bool,
    ) -> Result<Flow, DdlError> {
        for (i, binding) in bindings.iter().enumerate() {
            self.bind(varno, binding);
            match self.statements(interp, body)? {
                Flow::Done => {}
                Flow::Continue(l) if ours(&l) => {}
                Flow::Exit(l) if ours(&l) => break,
                Flow::Unknown => {
                    // The remaining iterations may run.
                    let mut found = Uncertain::default();
                    let mut walk = self.clone();
                    for binding in &bindings[i + 1..] {
                        walk.bind(varno, binding);
                        for stmt in body {
                            walk.taint(interp, stmt, &mut found);
                        }
                    }
                    note(interp, &found);
                    return Ok(Flow::Unknown);
                }
                other => return Ok(other),
            }
        }
        Ok(Flow::Done)
    }

    /// `CALL proc(...)` or `DO ...` (exec_stmt_call): the procedure or the
    /// block runs ([`run_sql`]), non-atomically only when this block is;
    /// a CALL's INOUT and OUT arguments' values go to the target
    /// variables. Arguments reading the block's variables make the call
    /// uncertain (a DO's code is a string constant, which reads none).
    fn call(
        &mut self,
        interp: &mut PgCatalog,
        stmt: &Stmt,
        s: &plpgsql::StmtCall,
    ) -> Result<Flow, DdlError> {
        let Some(query) = expr_query(s.expr.as_ref()) else {
            return self.unknown(interp, stmt);
        };
        if self.reads_variables(query) {
            return self.unknown(interp, stmt);
        }
        let atomic = self.atomic;
        let Some(flow) = certainly(interp, query, |interp| run_sql(interp, query, atomic))? else {
            return self.unknown(interp, stmt);
        };
        self.forget_targets(s.target);
        Ok(self.ran_body(flow))
    }

    /// `IF condition THEN ... [ELSIF ...] [ELSE ...] END IF`
    /// (exec_stmt_if): the first condition that holds selects its branch,
    /// as long as the analyzer can decide each condition in turn (see
    /// [`symbolic::truth`]); otherwise the statement is uncertain.
    fn if_statement(
        &mut self,
        interp: &mut PgCatalog,
        stmt: &Stmt,
        s: &plpgsql::StmtIf,
    ) -> Result<Flow, DdlError> {
        let branches = std::iter::once((s.cond.as_ref(), &s.then_body))
            .chain(s.elsif_list.iter().map(|e| (e.cond.as_ref(), &e.stmts)));
        for (cond, body) in branches {
            let truth =
                expr_query(cond).and_then(|cond| symbolic::truth(&Live(self, interp), cond));
            match truth {
                Some(true) => return self.statements(interp, body),
                Some(false) => {}
                None => return self.unknown(interp, stmt),
            }
        }
        self.statements(interp, &s.else_body)
    }

    /// Run a statement list.
    fn statements(&mut self, interp: &mut PgCatalog, list: &[Stmt]) -> Result<Flow, DdlError> {
        for (i, stmt) in list.iter().enumerate() {
            match self.statement(interp, stmt)? {
                Flow::Done => {}
                Flow::Unknown => {
                    self.uncertain_rest(interp, &list[i + 1..]);
                    return Ok(Flow::Unknown);
                }
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
        let mark = self.scope.len();
        self.scope.extend(&block.initvarnos);
        let flow = match handlers {
            None => self
                .initialize(interp, block)
                .and_then(|()| self.statements(interp, &block.body)),
            Some(handlers) => self.guarded_block(interp, block, handlers),
        };
        self.scope.truncate(mark);
        let flow = flow?;
        Ok(match flow {
            // `EXIT label` of this block ends it.
            Flow::Exit(Some(label)) if block.label.as_ref() == Some(&label) => Flow::Done,
            flow => flow,
        })
    }

    /// A block with exception handlers.
    fn guarded_block(
        &mut self,
        interp: &mut PgCatalog,
        block: &plpgsql::StmtBlock,
        handlers: &[plpgsql::Exception],
    ) -> Result<Flow, DdlError> {
        let start = interp.snapshot();
        let entry = self.clone();
        let raised_before = std::mem::replace(&mut self.may_raise, false);
        let result = self
            .initialize(interp, block)
            .and_then(|()| self.statements(interp, &block.body));
        let may_raise = std::mem::replace(&mut self.may_raise, raised_before);
        match result {
            Ok(flow) if !may_raise && !matches!(flow, Flow::Unknown) => Ok(flow),
            Ok(flow) => {
                // An error the analyzer can't rule out may have been
                // caught: what the body did may have been rolled back, and a
                // handler may have run instead.
                let mut found = Uncertain::default();
                let mut walk = entry.clone();
                for stmt in &block.body {
                    walk.taint(interp, stmt, &mut found);
                }
                found.make_symmetric();
                let escaping = self.uncertain_handlers(interp, &walk, handlers, &mut found);
                note(interp, &found);
                self.may_raise = true;
                Ok(if escaping { Flow::Unknown } else { flow })
            }
            Err(_) => {
                // exec_stmt_block rolls the subtransaction back, then looks
                // for a handler of the error's condition.
                interp.roll_back_to(&start);
                restore_pending(interp);
                *self = Run {
                    may_raise: self.may_raise,
                    ..entry
                };
                let others = handlers.iter().find(|handler| {
                    handler
                        .conditions
                        .iter()
                        .any(|c| c.sqlerrstate == plpgsql::Condition::OTHERS)
                });
                if let Some(handler) = others {
                    return self.statements(interp, &handler.action);
                }
                // Whether a specific condition matches the error isn't
                // known: one of the handlers may run, or the error fails the
                // migration.
                let mut found = Uncertain::default();
                let walk = self.clone();
                let escaping = self.uncertain_handlers(interp, &walk, handlers, &mut found);
                note(interp, &found);
                self.may_raise = true;
                Ok(if escaping { Flow::Unknown } else { Flow::Done })
            }
        }
    }

    /// The handlers may run, from the variables `from`: what they may
    /// change goes to `found`, the values they may leave to the variables.
    /// Whether one may leave the block's statement list.
    fn uncertain_handlers(
        &mut self,
        interp: &PgCatalog,
        from: &Run,
        handlers: &[plpgsql::Exception],
        found: &mut Uncertain,
    ) -> bool {
        let mut escaping = false;
        for handler in handlers {
            let mut walk = from.clone();
            for stmt in &handler.action {
                walk.taint(interp, stmt, found);
                escaping |= escapes(stmt);
            }
            self.join_from(&walk);
        }
        self.join_from(from);
        escaping
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
                .and_then(|i| self.variables.get(i))
            else {
                continue;
            };
            let value = match variable.default.as_deref() {
                None => Val::known(None),
                Some(default) => {
                    if let (Some(Some(_)), Some(type_name)) =
                        (constant(default), &variable.type_name)
                    {
                        check_constant(interp, default, type_name)?;
                    }
                    symbolic::eval(&Live(self, interp), default)
                }
            };
            if let Some(v) = self.variable_mut(varno) {
                v.value = value;
                v.record = None;
            }
        }
        Ok(())
    }

    /// `var := expr` (exec_stmt_assign): a constant is coerced to the
    /// variable's type — a value the type's input function rejects fails.
    /// An assignment to a record's field or an array element leaves the
    /// variable unknown.
    fn assign(
        &mut self,
        interp: &mut PgCatalog,
        stmt: &Stmt,
        s: &plpgsql::StmtAssign,
    ) -> Result<Flow, DdlError> {
        let (Some(query), Some(variable)) = (
            expr_query(s.expr.as_ref()),
            usize::try_from(s.varno)
                .ok()
                .and_then(|i| self.variables.get(i)),
        ) else {
            return self.unknown(interp, stmt);
        };
        let Some(rhs) = assignment_rhs(query, &variable.name) else {
            return self.unknown(interp, stmt);
        };
        if let (Some(Some(_)), Some(type_name)) = (constant(rhs), &variable.type_name) {
            check_constant(interp, rhs, type_name)?;
        }
        let value = symbolic::eval(&Live(self, interp), rhs);
        if let Some(v) = self.variable_mut(s.varno) {
            v.value = value;
            v.record = None;
        }
        Ok(Flow::Done)
    }

    /// The targets of `INTO` get values the analyzer doesn't know: a row's
    /// fields, or the record or variable itself.
    fn forget_targets(&mut self, target: Option<i32>) {
        let Some(target) = target else {
            return;
        };
        let varnos: Vec<i32> = match self.variable_mut(target) {
            Some(Variable {
                fields: Some(fields),
                ..
            }) => fields.clone(),
            Some(_) => vec![target],
            None => return,
        };
        for varno in varnos {
            if let Some(v) = self.variable_mut(varno) {
                v.value = Val::any();
                v.record = None;
            }
        }
    }

    /// What `stmt` may change, were it to run, from the variables' current
    /// values (which it updates to every value it may leave): into
    /// `found`. Nothing is run. Values merge where paths do: after a
    /// branch, and around a loop's body (see [`Self::taint_loop`]).
    fn taint(&mut self, interp: &PgCatalog, stmt: &Stmt, found: &mut Uncertain) {
        match stmt {
            Stmt::Block(b) => {
                let mark = self.scope.len();
                self.scope.extend(&b.initvarnos);
                for &varno in &b.initvarnos {
                    let value = self
                        .variable_mut(varno)
                        .and_then(|v| v.default.clone())
                        .map_or(Val::known(None), |d| symbolic::eval(self, &d));
                    if let Some(v) = self.variable_mut(varno) {
                        v.value = value;
                        v.record = None;
                    }
                }
                let entry = self.clone();
                for stmt in &b.body {
                    self.taint(interp, stmt, found);
                }
                if let Some(exceptions) = &b.exceptions {
                    for varno in [exceptions.sqlstate_varno, exceptions.sqlerrm_varno] {
                        self.forget_targets(Some(varno));
                    }
                    // A handler may run after any part of the body.
                    self.join_from(&entry);
                    let from = self.clone();
                    for handler in &exceptions.exc_list {
                        let mut walk = from.clone();
                        for stmt in &handler.action {
                            walk.taint(interp, stmt, found);
                        }
                        self.join_from(&walk);
                    }
                }
                self.scope.truncate(mark);
            }
            Stmt::Assign(s) => {
                let name = self
                    .variable_mut(s.varno)
                    .map(|v| v.name.clone())
                    .unwrap_or_default();
                let value = expr_query(s.expr.as_ref())
                    .and_then(|q| assignment_rhs(q, &name))
                    .map(|rhs| symbolic::eval(self, rhs));
                match (value, self.variable_mut(s.varno)) {
                    (Some(value), Some(v)) => {
                        v.value = value;
                        v.record = None;
                    }
                    _ => self.forget_targets(Some(s.varno)),
                }
            }
            Stmt::If(s) => {
                // Decided on the variables only: what the catalog holds may
                // differ by now.
                let before = self.clone();
                let mut after = Vec::new();
                let branches = std::iter::once((s.cond.as_ref(), &s.then_body))
                    .chain(s.elsif_list.iter().map(|e| (e.cond.as_ref(), &e.stmts)));
                let mut decided = false;
                for (cond, body) in branches {
                    let truth = expr_query(cond).and_then(|c| symbolic::truth(&before, c));
                    if truth == Some(false) {
                        continue;
                    }
                    let mut walk = before.clone();
                    for stmt in body {
                        walk.taint(interp, stmt, found);
                    }
                    after.push(walk);
                    if truth == Some(true) {
                        decided = true;
                        break;
                    }
                }
                if !decided {
                    let mut walk = before.clone();
                    for stmt in &s.else_body {
                        walk.taint(interp, stmt, found);
                    }
                    after.push(walk);
                }
                if let Some((first, rest)) = after.split_first() {
                    *self = first.clone();
                    for walk in rest {
                        self.join_from(walk);
                    }
                }
            }
            Stmt::Case(s) => {
                let value = expr_query(s.t_expr.as_ref())
                    .map_or_else(Val::any, |q| symbolic::eval(self, q));
                if let Some(v) = self.variable_mut(s.t_varno) {
                    v.value = value;
                }
                let before = self.clone();
                let mut first = true;
                for body in s
                    .case_when_list
                    .iter()
                    .map(|w| &w.stmts)
                    .chain(std::iter::once(&s.else_stmts))
                {
                    let mut walk = before.clone();
                    for stmt in body {
                        walk.taint(interp, stmt, found);
                    }
                    if first {
                        *self = walk;
                        first = false;
                    } else {
                        self.join_from(&walk);
                    }
                }
            }
            Stmt::Loop(s) => self.taint_loop(interp, &s.body, None, found),
            Stmt::While(s) => self.taint_loop(interp, &s.body, None, found),
            Stmt::Fori(s) => {
                let target = s.var.map(|var| (var, Binding::Value(Val::any())));
                self.taint_loop(interp, &s.body, target, found);
            }
            Stmt::Fors(s) => {
                let target = s.var.map(|var| {
                    let query = expr_query(s.query.as_ref());
                    let rows = query.and_then(|q| symbolic::constant_values_rows(self, q));
                    let binding = match rows {
                        Some(rows) => join_bindings(rows.into_iter().map(Binding::Fields)),
                        None => query
                            .and_then(|q| symbolic::query_record(self, q))
                            .map_or(Binding::Value(Val::any()), Binding::Fields),
                    };
                    (var, binding)
                });
                self.taint_loop(interp, &s.body, target, found);
            }
            Stmt::Forc(s) => {
                let target = s.var.map(|var| (var, Binding::Value(Val::any())));
                self.taint_loop(interp, &s.body, target, found);
            }
            Stmt::Dynfors(s) => {
                let target = s.var.map(|var| (var, Binding::Value(Val::any())));
                self.taint_loop(interp, &s.body, target, found);
            }
            Stmt::ForeachA(s) => {
                let elements = (s.slice == 0)
                    .then(|| expr_query(s.expr.as_ref()))
                    .flatten()
                    .and_then(|q| symbolic::array_elements(self, q));
                let binding = match elements {
                    Some(elements) => join_bindings(elements.into_iter().map(Binding::Value)),
                    None => Binding::Value(Val::any()),
                };
                self.taint_loop(interp, &s.body, Some((s.varno, binding)), found);
            }
            Stmt::Execsql(s) => {
                if let Some(query) = expr_query(s.sqlstmt.as_ref()) {
                    symbolic::classify_sql(query, found);
                }
                if s.into {
                    self.forget_targets(s.target);
                }
            }
            Stmt::Dynexecute(s) => {
                let value =
                    expr_query(s.query.as_ref()).map_or_else(Val::any, |q| symbolic::eval(self, q));
                symbolic::classify_dynamic(&value, found);
                if s.into {
                    self.forget_targets(s.target);
                }
            }
            Stmt::Getdiag(s) => {
                for item in &s.diag_items {
                    self.forget_targets(Some(item.target));
                }
            }
            Stmt::Fetch(s) => self.forget_targets(s.target),
            Stmt::Call(s) => {
                // A procedure's body or a nested block isn't followed here.
                found.catalog = true;
                self.forget_targets(s.target);
            }
            Stmt::Perform(s) => {
                if let Some(query) = expr_query(s.expr.as_ref()) {
                    symbolic::classify_sql(query, found);
                }
            }
            Stmt::Exit(_)
            | Stmt::Return(_)
            | Stmt::ReturnNext(_)
            | Stmt::ReturnQuery(_)
            | Stmt::Raise(_)
            | Stmt::Assert(_)
            | Stmt::Open(_)
            | Stmt::Close(_)
            | Stmt::Commit(_)
            | Stmt::Rollback(_) => {}
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
            let value = symbolic::eval(self, query).as_known()?.clone();
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
                            // A value only running the block would give:
                            // the block fails, with a message the analyzer
                            // can't tell.
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

// ─── CALL ───────────────────────────────────────────────────────────────────

/// How deep `CALL`s and `DO` blocks nest before the analyzer stops
/// following them: a deeper one is unknowable (a server runs out of
/// `max_stack_depth` at a depth its configuration decides).
const MAX_CALL_DEPTH: usize = 16;

/// How many procedure bodies and `DO` blocks one top-level statement may
/// run before the rest of them become unknowable — a bound on bodies
/// running several others each, whatever the depth.
const MAX_CALLS: u32 = 1000;

thread_local! {
    /// The bodies running, outermost first: a procedure's, or a `DO`
    /// block's (`None`).
    static CALL_STACK: std::cell::RefCell<Vec<Option<PgProcOid>>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// The bodies run since the outermost one began.
    static CALLS_RUN: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Leaves the body [`enter_body`] entered.
struct Leave;

impl Drop for Leave {
    fn drop(&mut self) {
        CALL_STACK.with_borrow_mut(|stack| stack.pop());
    }
}

/// Enter a body — procedure `proc`'s, or a `DO` block's for `None` —
/// unless that would recurse into a procedure already running, nest too
/// deep or run too many: such a body is unknowable.
fn enter_body(proc: Option<PgProcOid>) -> Option<Leave> {
    CALL_STACK.with_borrow_mut(|stack| {
        if stack.is_empty() {
            CALLS_RUN.set(0);
        }
        if stack.len() >= MAX_CALL_DEPTH
            || (proc.is_some() && stack.contains(&proc))
            || CALLS_RUN.get() >= MAX_CALLS
        {
            return None;
        }
        CALLS_RUN.set(CALLS_RUN.get() + 1);
        stack.push(proc);
        Some(Leave)
    })
}

/// Whether a `DO` block or a procedure's body is running: a statement runs
/// from inside user code, under the executor state of the code running it.
pub(crate) fn running_body() -> bool {
    CALL_STACK.with_borrow(|stack| !stack.is_empty())
}

/// A procedure body the analyzer can run.
enum Body {
    /// `LANGUAGE sql AS '...'`.
    SqlText(String),
    /// `LANGUAGE sql BEGIN ATOMIC ... END`.
    Atomic(std::sync::Arc<AtomicBody>),
    /// `LANGUAGE plpgsql`: the `CREATE PROCEDURE` statement it was compiled
    /// from.
    Plpgsql(String),
}

/// `CALL proc(...)` in a migration (ExecuteCallStmt): the call is checked,
/// then the procedure's body runs as far as it can be known — non-atomically
/// only as a top-level statement outside a transaction block
/// (standard_ProcessUtility's isAtomicContext). What a body the analyzer
/// can't follow throughout may have changed stays recorded in the catalog,
/// as for a `DO` block.
pub(crate) fn call(interp: &mut PgCatalog, stmt: &ast::Node) -> Result<(), DdlError> {
    super::dml::check_executed(interp, stmt)?;
    let Some((proc, body)) = procedure_body(interp, stmt) else {
        return Ok(());
    };
    let atomic = interp.in_transaction_block;
    let pending = Pending::enter();
    run_body(interp, &proc, &body, atomic)?;
    let found = pending.finish();
    record(interp, &found);
    Ok(())
}

/// A `CALL` inside a running body (a PL/pgSQL `CALL`, a SQL body's
/// statement): checked, then run. A procedure the analyzer can't run is
/// taken to change nothing, as a top-level [`call`] of it is.
fn run_call(interp: &mut PgCatalog, stmt: &ast::Node, atomic: bool) -> Result<Flow, DdlError> {
    super::dml::check_executed(interp, stmt)?;
    match procedure_body(interp, stmt) {
        Some((proc, body)) => run_body(interp, &proc, &body, atomic),
        None => Ok(Flow::Done),
    }
}

/// The procedure `stmt` calls and its body, when the analyzer can run it: a
/// `LANGUAGE sql` or `plpgsql` procedure the migrations created, without
/// `SECURITY DEFINER` or `SET` clauses (fmgr_security_definer runs those
/// under another identity or settings, atomically).
fn procedure_body(interp: &PgCatalog, stmt: &ast::Node) -> Option<(PgProc, Body)> {
    let ast::Node::CallStmt(call) = stmt else {
        return None;
    };
    let oid = crate::resolve::call_target(call, interp)?;
    let proc = interp.pg_proc.get(&oid)?;
    if proc.prokind != ProKind::Procedure || interp.procs_with_config.contains(&oid) {
        return None;
    }
    let routine = interp.routine_bodies.get(&oid)?;
    if routine.security_definer {
        return None;
    }
    let body = match (routine.language.as_str(), &routine.source) {
        ("sql", RoutineSource::SqlText(text)) => Body::SqlText(text.clone()),
        ("sql", RoutineSource::SqlStandard(_)) => Body::Atomic(routine.atomic_body.clone()?),
        ("plpgsql", RoutineSource::Plpgsql { create_sql }) => Body::Plpgsql(create_sql.clone()),
        _ => return None,
    };
    Some((proc.clone(), body))
}

/// Run a procedure's body, unless that would recurse into a procedure
/// already running or nest too deep: such a call may change anything.
/// [`Flow::Unknown`] when what the body did is uncertain (and recorded so).
fn run_body(
    interp: &mut PgCatalog,
    proc: &PgProc,
    body: &Body,
    atomic: bool,
) -> Result<Flow, DdlError> {
    let Some(_leave) = enter_body(Some(proc.oid)) else {
        return unknowable(interp);
    };
    // The body's statements are parsed from its own text, not the
    // migration.
    let _barrier = crate::error::DiagContextGuard::barrier();
    let flow = match body {
        Body::SqlText(text) => run_sql_body(interp, proc, text)?,
        Body::Atomic(atomic_body) => run_atomic_body(interp, proc, atomic_body)?,
        Body::Plpgsql(create_sql) => run_plpgsql_body(interp, create_sql, atomic)?,
    };
    // The body's RETURN ends the procedure, not its caller.
    Ok(match flow {
        Flow::Unknown => Flow::Unknown,
        _ => Flow::Done,
    })
}

/// A PL/pgSQL procedure's body, as a `DO` block runs (plpgsql_exec_function):
/// its arguments — OUT ones included, which a procedure takes too — are
/// values the analyzer doesn't know, so what reads them is uncertain.
fn run_plpgsql_body(
    interp: &mut PgCatalog,
    create_sql: &str,
    atomic: bool,
) -> Result<Flow, DdlError> {
    run_plpgsql_routine(interp, create_sql, atomic, &[]).map(|(flow, _)| flow)
}

/// [`run_plpgsql_body`] of a routine whose first arguments have the values
/// `args` (the rest unknown), with the value its `RETURN` certainly gave.
fn run_plpgsql_routine(
    interp: &mut PgCatalog,
    create_sql: &str,
    atomic: bool,
    args: &[Val],
) -> Result<(Flow, Option<Val>), DdlError> {
    let Ok(parsed) = typedpg_pg_query::parse_plpgsql_with_catalog(create_sql, &*interp) else {
        return unknowable(interp).map(|flow| (flow, None));
    };
    let Some(function) = parsed.first() else {
        return unknowable(interp).map(|flow| (flow, None));
    };
    let Some(action) = &function.action else {
        return unknowable(interp).map(|flow| (flow, None));
    };
    let mut run = Run {
        variables: variables(function),
        atomic,
        arguments: true,
        may_raise: false,
        scope: Vec::new(),
        returned: None,
    };
    for (i, &varno) in function.fn_argvarnos.iter().enumerate() {
        if let Some(v) = usize::try_from(varno)
            .ok()
            .and_then(|i| run.variables.get_mut(i))
        {
            v.value = args.get(i).cloned().unwrap_or_else(Val::any);
        }
    }
    let flow = run.block(interp, action)?;
    let flow = if run.may_raise { Flow::Unknown } else { flow };
    Ok((flow, run.returned))
}

/// A PL/pgSQL function the migrations created that `call` names, which the
/// analyzer can run: one candidate of its name and argument count, without
/// `SECURITY DEFINER` and with no `SET` clause but `search_path`'s. With
/// its `CREATE FUNCTION` statement and the path its `SET` clause gives.
fn plpgsql_function(
    interp: &PgCatalog,
    call: &ast::FuncCall,
) -> Option<(PgProc, String, Option<Vec<String>>)> {
    if call.agg_star
        || call.agg_distinct
        || call.over.is_some()
        || call.func_variadic
        || call
            .args
            .iter()
            .any(|a| matches!(a, Some(ast::Node::NamedArgExpr(_))))
    {
        return None;
    }
    let names: Vec<&str> = call
        .funcname
        .iter()
        .filter_map(|n| super::util::node_string(n.as_ref()))
        .collect();
    let (schema, name) = match names.as_slice() {
        [schema, name] => (Some(*schema), *name),
        [name] => (None, *name),
        _ => return None,
    };
    let mut candidates = interp
        .find_functions(schema, name)
        .into_iter()
        .filter(|p| p.prokind == ProKind::Function && p.proargtypes.len() == call.args.len());
    let proc = candidates.next()?;
    if candidates.next().is_some() {
        return None;
    }
    let routine = interp.routine_bodies.get(&proc.oid)?;
    if routine.security_definer {
        return None;
    }
    let RoutineSource::Plpgsql { create_sql } = &routine.source else {
        return None;
    };
    let search_path = if interp.procs_with_config.contains(&proc.oid) {
        Some(search_path_clause(create_sql)?)
    } else {
        None
    };
    Some((proc.clone(), create_sql.clone(), search_path))
}

/// The path a routine's only `SET` clause, `SET search_path = ...`, gives;
/// `None` for any other clause.
fn search_path_clause(create_sql: &str) -> Option<Vec<String>> {
    let parsed = typedpg_pg_query::parse(create_sql).ok()?;
    let Some(ast::Node::CreateFunctionStmt(create)) = parsed.tree.stmts.first()?.stmt.as_ref()
    else {
        return None;
    };
    let mut path = None;
    for option in &create.options {
        let Some(ast::Node::DefElem(de)) = option.as_ref() else {
            continue;
        };
        if de.defname != "set" {
            continue;
        }
        let Some(ast::Node::VariableSetStmt(set)) = de.arg.as_ref() else {
            return None;
        };
        if !set.name.eq_ignore_ascii_case("search_path")
            || set.kind != ast::VariableSetKind::VarSetValue
        {
            return None;
        }
        path = Some(super::session::search_path_args(set)?);
    }
    path
}

/// The arguments of a call, as far as the analyzer knows them.
fn call_arguments(interp: &PgCatalog, call: &ast::FuncCall) -> Vec<Val> {
    struct Constants<'a>(&'a PgCatalog);
    impl symbolic::Scope for Constants<'_> {
        fn variable(&self, _: &str) -> Option<Val> {
            None
        }
        fn field(&self, _: &str, _: &str) -> Option<Val> {
            None
        }
        fn catalog(&self) -> Option<&PgCatalog> {
            Some(self.0)
        }
    }
    call.args
        .iter()
        .map(|a| {
            a.as_ref()
                .map_or_else(Val::any, |a| symbolic::eval_node(&Constants(interp), a))
        })
        .collect()
}

/// A migration's `SELECT f(...)` — or a body's `PERFORM f(...)` — of a
/// PL/pgSQL function the migrations created (see [`plpgsql_function`]):
/// its body runs as a procedure's does ([`call`]), atomically, under its
/// `SET search_path`, its arguments the values the call passes, as far as
/// the analyzer knows them (pgmq's `SELECT pgmq.create('orders')` creates
/// the queue's tables).
pub(crate) fn select_call(interp: &mut PgCatalog, call: &ast::FuncCall) -> Result<(), DdlError> {
    let Some((proc, create_sql, search_path)) = plpgsql_function(interp, call) else {
        return Ok(());
    };
    let args = call_arguments(interp, call);
    let pending = (!running_body()).then(Pending::enter);
    let saved = search_path.map(|path| interp.replace_search_path(path));
    let result = match enter_body(Some(proc.oid)) {
        Some(_leave) => {
            let _barrier = crate::error::DiagContextGuard::barrier();
            run_plpgsql_routine(interp, &create_sql, true, &args).map(drop)
        }
        None => unknowable(interp).map(drop),
    };
    if let Some(saved) = saved {
        interp.restore_scoped_search_path(saved);
    }
    if let Some(pending) = pending {
        let found = pending.finish();
        record(interp, &found);
    }
    result
}

/// The value `call` of a PL/pgSQL function the migrations created returns
/// (see [`plpgsql_function`]), its arguments `args`, when running its body
/// on a copy of the catalog certainly returns one (pgmq's
/// `format_table_name`). What the body does to the catalog is not kept.
pub(crate) fn function_value(
    interp: &PgCatalog,
    call: &ast::FuncCall,
    args: &[Val],
) -> Option<Val> {
    let (proc, create_sql, search_path) = plpgsql_function(interp, call)?;
    let mut scratch = interp.clone();
    let pending = Pending::enter();
    if let Some(path) = search_path {
        scratch.replace_search_path(path);
    }
    let returned = match enter_body(Some(proc.oid)) {
        Some(_leave) => {
            let _barrier = crate::error::DiagContextGuard::barrier();
            run_plpgsql_routine(&mut scratch, &create_sql, true, args)
                .ok()
                .and_then(|(_, returned)| returned)
        }
        None => None,
    };
    drop(pending.finish());
    returned
}

/// What may fail in trigger function `function`'s body (a `LANGUAGE
/// plpgsql` one; `None` for another), for a trigger firing it: the error
/// it raises every time it runs, when running it as far as it can be known
/// statically reaches one (a `RAISE EXCEPTION` before any statement whose
/// effect depends on the row, a statement that fails) — the row's values
/// and the trigger's variables are unknown — and the statements that may
/// raise one depending on the data, as their text: `RAISE` at `EXCEPTION`
/// level, `ASSERT`, `INTO STRICT` and dynamic `EXECUTE`.
pub(crate) fn trigger_body_outcome(
    interp: &PgCatalog,
    function: PgProcOid,
) -> Option<(Option<String>, std::collections::BTreeSet<String>)> {
    /// `ERROR` (elog.h): RAISE EXCEPTION's level.
    const ERROR: i32 = 21;
    let routine = interp.routine_bodies.get(&function)?;
    let ("plpgsql", RoutineSource::Plpgsql { create_sql }) =
        (routine.language.as_str(), &routine.source)
    else {
        return None;
    };
    let parsed = typedpg_pg_query::parse_plpgsql_with_catalog(create_sql, interp).ok()?;
    let compiled = parsed.first()?;
    let action = compiled.action.as_ref()?;
    let mut sites = std::collections::BTreeSet::new();
    let text = |expr: Option<&plpgsql::Expr>| expr_query(expr).unwrap_or_default().to_owned();
    crate::privileges::walk_plpgsql_block(action, &mut |stmt| match stmt {
        Stmt::Raise(s) if s.elog_level >= ERROR => {
            let params: Vec<String> = s.params.iter().map(|p| text(Some(p))).collect();
            sites.insert(format!(
                "RAISE {} {} {}",
                s.condname.as_deref().unwrap_or_default(),
                s.message.as_deref().unwrap_or_default(),
                params.join(", ")
            ));
        }
        Stmt::Assert(s) => {
            sites.insert(format!("ASSERT {}", text(s.cond.as_ref())));
        }
        Stmt::Execsql(s) if s.strict => {
            sites.insert(format!("STRICT {}", text(s.sqlstmt.as_ref())));
        }
        Stmt::Dynexecute(s) => {
            sites.insert(format!("EXECUTE {}", text(s.query.as_ref())));
        }
        _ => {}
    });
    // Run on a copy: what the body's statements do to the catalog is no
    // migration's.
    let mut scratch = interp.clone();
    let always = {
        let _barrier = crate::error::DiagContextGuard::barrier();
        let mut run = Run {
            variables: variables(compiled),
            atomic: true,
            arguments: true,
            may_raise: false,
            scope: Vec::new(),
            returned: None,
        };
        for variable in &mut run.variables {
            variable.value = Val::any();
        }
        match enter_body(Some(function)) {
            Some(_leave) => run.block(&mut scratch, action).err(),
            None => None,
        }
    };
    let always = always.map(|e| {
        let message = e.to_string();
        message.lines().next().unwrap_or_default().trim().to_owned()
    });
    Some((always, sites))
}

/// SQL `sql` a running PL/pgSQL body executes through SPI (a constant
/// `EXECUTE`, a `CALL` or `DO`): its statements apply as a migration's
/// do, a `CALL` running its procedure and a `DO` its block, atomically
/// when `atomic`. [`Flow::Unknown`] when one of those was uncertain.
fn run_sql(interp: &mut PgCatalog, sql: &str, atomic: bool) -> Result<Flow, DdlError> {
    // The statements' locations are offsets into `sql`, not the migration.
    let _barrier = crate::error::DiagContextGuard::barrier();
    let parsed = super::parse(sql)?;
    let mut uncertain = false;
    for raw in &parsed.tree.stmts {
        let Some(stmt) = raw.stmt.as_ref() else {
            continue;
        };
        refuse_copy_to_or_from_client(Some(stmt))?;
        let mut flow = Flow::Done;
        super::with_statement_sql(interp, sql, raw, |interp| {
            match stmt {
                ast::Node::CallStmt(_) => flow = run_call(interp, stmt, atomic)?,
                ast::Node::DoStmt(_) => flow = run_nested_do(interp, stmt, atomic)?,
                _ => super::apply_statement(interp, stmt)?,
            }
            Ok(())
        })?;
        if let Flow::Unknown = flow {
            uncertain = true;
        }
    }
    Ok(if uncertain { Flow::Unknown } else { Flow::Done })
}

/// A `LANGUAGE sql` procedure's string body: its statements run in order,
/// each parsed and analyzed once the ones before it ran
/// (prepare_next_query), so one sees what they created. A statement the
/// analyzer can't run records what it may change, and the rest run.
fn run_sql_body(interp: &mut PgCatalog, proc: &PgProc, text: &str) -> Result<Flow, DdlError> {
    let parsed = super::parse(text)?;
    let params = Params::of(interp, proc);
    let mut uncertain = false;
    for raw in &parsed.tree.stmts {
        let Some(stmt) = raw.stmt.as_ref() else {
            continue;
        };
        let mut flow = Flow::Done;
        let may_fail_on = interp.roles.uncertain.may_fail_on(stmt);
        let ran = uncertain_unless(interp, may_fail_on, |interp| {
            super::with_statement_sql(interp, text, raw, |interp| {
                flow = sql_body_statement(interp, stmt, &params)?;
                Ok(())
            })
        })?;
        if ran.is_none() {
            let mut found = Uncertain::default();
            symbolic::classify_statement(stmt, &mut found);
            note(interp, &found);
            uncertain = true;
        }
        if let Flow::Unknown = flow {
            uncertain = true;
        }
    }
    Ok(if uncertain { Flow::Unknown } else { Flow::Done })
}

/// One statement of a SQL procedure's string body.
fn sql_body_statement(
    interp: &mut PgCatalog,
    stmt: &ast::Node,
    params: &Params,
) -> Result<Flow, DdlError> {
    // init_execution_state's checks of each statement.
    match stmt {
        ast::Node::TransactionStmt(_) => {
            return Err(DdlError::UnsupportedDdl(format!(
                "{} is not allowed in an SQL function",
                super::cmdtag::command_tag(stmt)
            )));
        }
        ast::Node::CopyStmt(copy) if copy.filename.is_empty() => {
            return Err(DdlError::UnsupportedDdl(
                "cannot COPY to/from client in an SQL function".into(),
            ));
        }
        _ => {}
    }
    if params.read_by(stmt) {
        // A query reading the arguments is analyzed with them as values of
        // their types; anything else reading them may change what it
        // would with any arguments.
        if !is_query(stmt) || params.polymorphic {
            let mut found = Uncertain::default();
            symbolic::classify_statement(stmt, &mut found);
            note(interp, &found);
            return Ok(Flow::Unknown);
        }
        super::dml::verdict(params.check_executed(interp, stmt))?;
        return Ok(Flow::Done);
    }
    match stmt {
        // A SQL function's statements run atomically.
        ast::Node::CallStmt(_) => run_call(interp, stmt, true),
        ast::Node::DoStmt(_) => run_nested_do(interp, stmt, true),
        _ if !statement_runs_in_a_function(stmt) => Ok(Flow::Unknown),
        _ => {
            super::apply_statement(interp, stmt)?;
            Ok(Flow::Done)
        }
    }
}

/// A procedure's `BEGIN ATOMIC` body: queries PG stored resolved at
/// CREATE PROCEDURE (interpret_AS_clause takes no other statement),
/// analyzed again along the search path they were resolved with. A name
/// that no longer resolves, or resolves differently, says nothing about
/// the stored statement (it names its objects by OID): only an error
/// outside the name-resolution classes (SQLSTATE classes 42 and 3F) is
/// certain; the others make the body's outcome uncertain (its queries
/// change no catalog).
fn run_atomic_body(
    interp: &mut PgCatalog,
    proc: &PgProc,
    body: &AtomicBody,
) -> Result<Flow, DdlError> {
    let params = Params::of(interp, proc);
    for stmt in &body.statements {
        if !is_query(stmt) || (params.polymorphic && params.read_by(stmt)) {
            let mut found = Uncertain::default();
            symbolic::classify_statement(stmt, &mut found);
            note(interp, &found);
            return Ok(Flow::Unknown);
        }
        let session = std::mem::replace(&mut interp.search_path, body.search_path.clone());
        let analyzed = params.check_executed(interp, stmt);
        interp.search_path = session;
        if let Err(e) = &analyzed
            && e.sqlstate()
                .is_none_or(|code| code.starts_with("42") || code.starts_with("3F"))
        {
            return Ok(Flow::Unknown);
        }
        super::dml::verdict(analyzed)?;
    }
    Ok(Flow::Done)
}

/// A query: what a SQL function's statements are analyzed as.
fn is_query(stmt: &ast::Node) -> bool {
    matches!(
        stmt,
        ast::Node::SelectStmt(_)
            | ast::Node::InsertStmt(_)
            | ast::Node::UpdateStmt(_)
            | ast::Node::DeleteStmt(_)
            | ast::Node::MergeStmt(_)
    )
}

/// A SQL procedure's input parameters, as its body's statements see them
/// (sql_fn_parser_setup): `$n`, or the name, also qualified by the
/// procedure's.
struct Params {
    procedure: String,
    inputs: Vec<(String, PgTypeOid)>,
    /// A parameter of a polymorphic type: its type is the call's.
    polymorphic: bool,
}

impl Params {
    fn of(interp: &PgCatalog, proc: &PgProc) -> Self {
        let inputs = super::function_body::input_params(proc);
        let polymorphic = inputs.iter().any(|(_, t)| {
            interp
                .pg_type
                .get(t)
                .is_some_and(|ty| ty.typtype == TypType::Pseudo)
        });
        Params {
            procedure: proc.proname.clone(),
            inputs,
            polymorphic,
        }
    }

    /// Whether `stmt` may read a parameter: a `$n`, or a column reference
    /// spelling a parameter's (or the procedure's) name.
    fn read_by(&self, stmt: &ast::Node) -> bool {
        stmt.nodes().any(|(node, _)| match node {
            typedpg_pg_query::NodeRef::ParamRef(_) => true,
            typedpg_pg_query::NodeRef::ColumnRef(c) => c.fields.iter().any(|f| {
                matches!(f, Some(ast::Node::String(s))
                    if s.sval == self.procedure
                        || self.inputs.iter().any(|(name, _)| *name == s.sval))
            }),
            _ => false,
        })
    }

    /// [`super::dml::check_executed`] with the parameters bound as values of
    /// their types the analyzer doesn't know.
    fn check_executed(
        &self,
        interp: &PgCatalog,
        stmt: &ast::Node,
    ) -> Result<
        (
            Vec<crate::resolve::RawColumn>,
            Vec<crate::resolve::RawParam>,
        ),
        AnalyzeError,
    > {
        let types: Vec<PgTypeOid> = self.inputs.iter().map(|(_, t)| *t).collect();
        let namespace = crate::expr::SqlFunctionParams {
            name: self.procedure.clone(),
            params: self.inputs.clone(),
            locals: Vec::new(),
        };
        crate::resolve::analyze_executed(interp, stmt, || {
            crate::expr::with_sql_function_params(namespace, || {
                crate::resolve::analyze_raw_node_with_param_types(interp, stmt, &types)
            })
        })
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
    parsed
        .tree
        .stmts
        .iter()
        .all(|raw| raw.stmt.as_ref().is_none_or(statement_runs_in_a_function))
}

/// _SPI_execute_plan: a COPY to or from the client (no file name) is
/// SPI_ERROR_COPY, which PL/pgSQL reports before the COPY runs.
fn refuse_copy_to_or_from_client(stmt: Option<&ast::Node>) -> Result<(), DdlError> {
    match stmt {
        Some(ast::Node::CopyStmt(copy)) if copy.filename.is_empty() => Err(
            DdlError::UnsupportedDdl("cannot COPY to/from client in PL/pgSQL".into()),
        ),
        _ => Ok(()),
    }
}

/// [`runs_in_a_function`] for one parsed statement.
fn statement_runs_in_a_function(stmt: &ast::Node) -> bool {
    !(matches!(
        stmt,
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
    ) || matches!(stmt, ast::Node::IndexStmt(i) if i.concurrent))
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
