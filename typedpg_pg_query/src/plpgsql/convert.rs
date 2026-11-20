//! Conversion of PL/pgSQL's compiled C structs (`plpgsql.h`) into the
//! [`plpgsql`](crate::plpgsql) types.
//!
//! Every pointer is read as PostgreSQL's own dump functions (`pl_funcs.c`)
//! read it: a statement by its `cmd_type`, a datum by its `dtype`, a `List`
//! of statements, `PLpgSQL_expr`s or clause structs by its pointer cells.
//! Statements nest as deep as PL/pgSQL's grammar lets them (its parser
//! stack bounds the depth), so each statement list converts on a stack
//! `stacker` grows as needed.

// `c_char` and `c_long` differ by platform (signed or not, 32 or 64 bits): a
// cast that is a no-op on one is needed on another.
#![allow(clippy::unnecessary_cast)]

use std::ffi::{CStr, c_char, c_int};

use super::*;
use crate::NodeBox;
use crate::bindings as c;

/// A C string as a Rust string; NULL is `None`.
///
/// # Safety
/// `p` must be NULL or point to a NUL-terminated string.
unsafe fn opt_string(p: *const c_char) -> Option<String> {
    (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

/// A C string as a Rust string; NULL (which these fields never are) is
/// empty.
///
/// # Safety
/// As [`opt_string`].
unsafe fn string(p: *const c_char) -> String {
    unsafe { opt_string(p) }.unwrap_or_default()
}

/// The cells of a `List` of pointers, cast to `*const T`; NULL is empty.
///
/// # Safety
/// `list` must be NULL or a valid pointer `List`.
unsafe fn cells<T>(list: *const c::List) -> Vec<*const T> {
    if list.is_null() {
        return Vec::new();
    }
    let list = unsafe { &*list };
    (0..list.length.max(0) as usize)
        .map(|i| unsafe { (*list.elements.add(i)).ptr_value as *const T })
        .collect()
}

/// `n` C ints at `p`; NULL is empty.
///
/// # Safety
/// `p` must be NULL or point to at least `n` ints.
unsafe fn ints(p: *const c_int, n: c_int) -> Vec<i32> {
    if p.is_null() || n <= 0 {
        return Vec::new();
    }
    unsafe { std::slice::from_raw_parts(p, n as usize) }.to_vec()
}

/// Convert a compiled function.
///
/// # Safety
/// `p` must point to a live `PLpgSQL_function`, as `plpgsql_compile` or
/// `plpgsql_compile_inline` returns it.
pub(crate) unsafe fn function(p: *const c::PLpgSQL_function) -> Function {
    // The SQL converter's depth state, which `origtypname`s go through.
    crate::convert::begin();
    let f = unsafe { &*p };
    let nargs = f.fn_nargs.clamp(0, f.fn_argvarnos.len() as c_int) as usize;
    let datums = if f.datums.is_null() {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(f.datums, f.ndatums.max(0) as usize) }
            .iter()
            .map(|&d| unsafe { datum(d) })
            .collect()
    };
    Function {
        fn_signature: unsafe { string(f.fn_signature) },
        fn_is_trigger: match f.fn_is_trigger {
            c::PLpgSQL_trigtype_PLPGSQL_DML_TRIGGER => Trigtype::DmlTrigger,
            c::PLpgSQL_trigtype_PLPGSQL_EVENT_TRIGGER => Trigtype::EventTrigger,
            c::PLpgSQL_trigtype_PLPGSQL_NOT_TRIGGER => Trigtype::NotTrigger,
            other => unreachable!("{other} is not a PLpgSQL_trigtype"),
        },
        fn_rettype: f.fn_rettype,
        fn_retset: f.fn_retset,
        fn_prokind: f.fn_prokind as u8,
        fn_argvarnos: f.fn_argvarnos[..nargs].to_vec(),
        out_param_varno: f.out_param_varno,
        found_varno: f.found_varno,
        new_varno: f.new_varno,
        old_varno: f.old_varno,
        resolve_option: match f.resolve_option {
            c::PLpgSQL_resolve_option_PLPGSQL_RESOLVE_ERROR => ResolveOption::Error,
            c::PLpgSQL_resolve_option_PLPGSQL_RESOLVE_VARIABLE => ResolveOption::Variable,
            c::PLpgSQL_resolve_option_PLPGSQL_RESOLVE_COLUMN => ResolveOption::Column,
            other => unreachable!("{other} is not a PLpgSQL_resolve_option"),
        },
        print_strict_params: f.print_strict_params,
        extra_warnings: f.extra_warnings,
        extra_errors: f.extra_errors,
        datums,
        action: (!f.action.is_null()).then(|| unsafe { block(f.action) }),
        nstatements: f.nstatements,
        requires_procedure_resowner: f.requires_procedure_resowner,
        has_exception_block: f.has_exception_block,
    }
}

/// # Safety
/// `p` must point to a live datum.
unsafe fn datum(p: *const c::PLpgSQL_datum) -> Datum {
    match unsafe { (*p).dtype } {
        c::PLpgSQL_datum_type_PLPGSQL_DTYPE_VAR => {
            Datum::Var(Box::new(unsafe { var(p as *const c::PLpgSQL_var) }))
        }
        c::PLpgSQL_datum_type_PLPGSQL_DTYPE_PROMISE => {
            Datum::Promise(Box::new(unsafe { var(p as *const c::PLpgSQL_var) }))
        }
        c::PLpgSQL_datum_type_PLPGSQL_DTYPE_ROW => {
            let r = unsafe { &*(p as *const c::PLpgSQL_row) };
            let n = r.nfields.max(0) as usize;
            let fields = (0..n)
                .map(|i| RowField {
                    name: unsafe { opt_string(*r.fieldnames.add(i)) },
                    varno: unsafe { *r.varnos.add(i) },
                })
                .collect();
            Datum::Row(Box::new(Row {
                dno: r.dno,
                refname: unsafe { string(r.refname) },
                lineno: r.lineno,
                isconst: r.isconst,
                notnull: r.notnull,
                default_val: unsafe { expr(r.default_val) },
                fields,
            }))
        }
        c::PLpgSQL_datum_type_PLPGSQL_DTYPE_REC => {
            let r = unsafe { &*(p as *const c::PLpgSQL_rec) };
            Datum::Rec(Box::new(Rec {
                dno: r.dno,
                refname: unsafe { string(r.refname) },
                lineno: r.lineno,
                isconst: r.isconst,
                notnull: r.notnull,
                default_val: unsafe { expr(r.default_val) },
                datatype: unsafe { type_(r.datatype) },
                rectypeid: r.rectypeid,
                firstfield: r.firstfield,
            }))
        }
        c::PLpgSQL_datum_type_PLPGSQL_DTYPE_RECFIELD => {
            let f = unsafe { &*(p as *const c::PLpgSQL_recfield) };
            Datum::Recfield(Box::new(Recfield {
                dno: f.dno,
                fieldname: unsafe { string(f.fieldname) },
                recparentno: f.recparentno,
                nextfield: f.nextfield,
            }))
        }
        other => unreachable!("{other} is not a PLpgSQL_datum_type"),
    }
}

/// # Safety
/// `p` must point to a live `PLpgSQL_var` (a `VAR` or `PROMISE` datum).
unsafe fn var(p: *const c::PLpgSQL_var) -> Var {
    let v = unsafe { &*p };
    Var {
        dno: v.dno,
        refname: unsafe { string(v.refname) },
        lineno: v.lineno,
        isconst: v.isconst,
        notnull: v.notnull,
        default_val: unsafe { expr(v.default_val) },
        datatype: unsafe { type_(v.datatype) },
        cursor_explicit_expr: unsafe { expr(v.cursor_explicit_expr) },
        cursor_explicit_argrow: v.cursor_explicit_argrow,
        cursor_options: v.cursor_options,
        promise: match v.promise {
            c::PLpgSQL_promise_type_PLPGSQL_PROMISE_NONE => PromiseType::None,
            c::PLpgSQL_promise_type_PLPGSQL_PROMISE_TG_NAME => PromiseType::TgName,
            c::PLpgSQL_promise_type_PLPGSQL_PROMISE_TG_WHEN => PromiseType::TgWhen,
            c::PLpgSQL_promise_type_PLPGSQL_PROMISE_TG_LEVEL => PromiseType::TgLevel,
            c::PLpgSQL_promise_type_PLPGSQL_PROMISE_TG_OP => PromiseType::TgOp,
            c::PLpgSQL_promise_type_PLPGSQL_PROMISE_TG_RELID => PromiseType::TgRelid,
            c::PLpgSQL_promise_type_PLPGSQL_PROMISE_TG_TABLE_NAME => PromiseType::TgTableName,
            c::PLpgSQL_promise_type_PLPGSQL_PROMISE_TG_TABLE_SCHEMA => PromiseType::TgTableSchema,
            c::PLpgSQL_promise_type_PLPGSQL_PROMISE_TG_NARGS => PromiseType::TgNargs,
            c::PLpgSQL_promise_type_PLPGSQL_PROMISE_TG_ARGV => PromiseType::TgArgv,
            c::PLpgSQL_promise_type_PLPGSQL_PROMISE_TG_EVENT => PromiseType::TgEvent,
            c::PLpgSQL_promise_type_PLPGSQL_PROMISE_TG_TAG => PromiseType::TgTag,
            other => unreachable!("{other} is not a PLpgSQL_promise_type"),
        },
    }
}

/// # Safety
/// `p` must be NULL or point to a live `PLpgSQL_type`.
unsafe fn type_(p: *const c::PLpgSQL_type) -> Option<Type> {
    let t = unsafe { p.as_ref() }?;
    Some(Type {
        typname: unsafe { string(t.typname) },
        typoid: t.typoid,
        ttype: match t.ttype {
            c::PLpgSQL_type_type_PLPGSQL_TTYPE_SCALAR => TypeType::Scalar,
            c::PLpgSQL_type_type_PLPGSQL_TTYPE_REC => TypeType::Rec,
            c::PLpgSQL_type_type_PLPGSQL_TTYPE_PSEUDO => TypeType::Pseudo,
            other => unreachable!("{other} is not a PLpgSQL_type_type"),
        },
        typlen: t.typlen,
        typbyval: t.typbyval,
        typtype: t.typtype as u8,
        collation: t.collation,
        typisarray: t.typisarray,
        atttypmod: t.atttypmod,
        origtypname: (!t.origtypname.is_null())
            .then(|| unsafe { crate::convert::type_name(t.origtypname) }),
    })
}

/// # Safety
/// `p` must be NULL or point to a live `PLpgSQL_expr`.
unsafe fn expr(p: *const c::PLpgSQL_expr) -> Option<Expr> {
    let e = unsafe { p.as_ref() }?;
    Some(Expr {
        query: unsafe { string(e.query) },
        parse_mode: match e.parseMode {
            c::RawParseMode_RAW_PARSE_DEFAULT => RawParseMode::Default,
            c::RawParseMode_RAW_PARSE_TYPE_NAME => RawParseMode::TypeName,
            c::RawParseMode_RAW_PARSE_PLPGSQL_EXPR => RawParseMode::PlpgsqlExpr,
            c::RawParseMode_RAW_PARSE_PLPGSQL_ASSIGN1 => RawParseMode::PlpgsqlAssign1,
            c::RawParseMode_RAW_PARSE_PLPGSQL_ASSIGN2 => RawParseMode::PlpgsqlAssign2,
            c::RawParseMode_RAW_PARSE_PLPGSQL_ASSIGN3 => RawParseMode::PlpgsqlAssign3,
            other => unreachable!("{other} is not a RawParseMode"),
        },
        target_param: e.target_param,
        target_is_local: e.target_is_local,
    })
}

/// A `List` of `PLpgSQL_expr`.
///
/// # Safety
/// `list` must be NULL or a valid `List` of live `PLpgSQL_expr`s.
unsafe fn exprs(list: *const c::List) -> Vec<Expr> {
    unsafe { cells::<c::PLpgSQL_expr>(list) }
        .into_iter()
        .filter_map(|e| unsafe { expr(e) })
        .collect()
}

/// The dno of a `PLpgSQL_variable` (or `PLpgSQL_var`) pointer.
///
/// # Safety
/// `p` must be NULL or point to a live datum.
unsafe fn dno<T>(p: *const T) -> Option<i32> {
    let d = unsafe { (p as *const c::PLpgSQL_datum).as_ref() }?;
    Some(d.dno)
}

/// A `List` of statements, on a stack grown as the nesting needs.
///
/// # Safety
/// `list` must be NULL or a valid `List` of live statements.
unsafe fn stmts(list: *const c::List) -> Vec<Stmt> {
    stacker::maybe_grow(64 * 1024, 1024 * 1024, || {
        unsafe { cells::<c::PLpgSQL_stmt>(list) }
            .into_iter()
            .map(|s| unsafe { stmt(s) })
            .collect()
    })
}

/// # Safety
/// `p` must point to a live `PLpgSQL_stmt_block`.
unsafe fn block(p: *const c::PLpgSQL_stmt_block) -> StmtBlock {
    let b = unsafe { &*p };
    StmtBlock {
        lineno: b.lineno,
        stmtid: b.stmtid,
        label: unsafe { opt_string(b.label) },
        body: unsafe { stmts(b.body) },
        initvarnos: unsafe { ints(b.initvarnos, b.n_initvars) },
        exceptions: unsafe { b.exceptions.as_ref() }.map(|e| ExceptionBlock {
            sqlstate_varno: e.sqlstate_varno,
            sqlerrm_varno: e.sqlerrm_varno,
            exc_list: unsafe { cells::<c::PLpgSQL_exception>(e.exc_list) }
                .into_iter()
                .map(|x| {
                    let x = unsafe { &*x };
                    let mut conditions = Vec::new();
                    let mut cond = x.conditions as *const c::PLpgSQL_condition;
                    while let Some(c) = unsafe { cond.as_ref() } {
                        conditions.push(Condition {
                            sqlerrstate: c.sqlerrstate,
                            condname: unsafe { string(c.condname) },
                        });
                        cond = c.next;
                    }
                    Exception {
                        lineno: x.lineno,
                        conditions,
                        action: unsafe { stmts(x.action) },
                    }
                })
                .collect(),
        }),
    }
}

/// # Safety
/// `p` must point to a live statement.
unsafe fn stmt(p: *const c::PLpgSQL_stmt) -> Stmt {
    /// The statement struct `p` points to, by its type.
    macro_rules! node {
        ($ty:ident) => {
            unsafe { &*(p as *const c::$ty) }
        };
    }
    match unsafe { (*p).cmd_type } {
        c::PLpgSQL_stmt_type_PLPGSQL_STMT_BLOCK => Stmt::Block(NodeBox::new(unsafe {
            block(p as *const c::PLpgSQL_stmt_block)
        })),
        c::PLpgSQL_stmt_type_PLPGSQL_STMT_ASSIGN => {
            let s = node!(PLpgSQL_stmt_assign);
            Stmt::Assign(NodeBox::new(StmtAssign {
                lineno: s.lineno,
                stmtid: s.stmtid,
                varno: s.varno,
                expr: unsafe { expr(s.expr) },
            }))
        }
        c::PLpgSQL_stmt_type_PLPGSQL_STMT_IF => {
            let s = node!(PLpgSQL_stmt_if);
            Stmt::If(NodeBox::new(StmtIf {
                lineno: s.lineno,
                stmtid: s.stmtid,
                cond: unsafe { expr(s.cond) },
                then_body: unsafe { stmts(s.then_body) },
                elsif_list: unsafe { cells::<c::PLpgSQL_if_elsif>(s.elsif_list) }
                    .into_iter()
                    .map(|e| {
                        let e = unsafe { &*e };
                        IfElsif {
                            lineno: e.lineno,
                            cond: unsafe { expr(e.cond) },
                            stmts: unsafe { stmts(e.stmts) },
                        }
                    })
                    .collect(),
                else_body: unsafe { stmts(s.else_body) },
            }))
        }
        c::PLpgSQL_stmt_type_PLPGSQL_STMT_CASE => {
            let s = node!(PLpgSQL_stmt_case);
            Stmt::Case(NodeBox::new(StmtCase {
                lineno: s.lineno,
                stmtid: s.stmtid,
                t_expr: unsafe { expr(s.t_expr) },
                t_varno: s.t_varno,
                case_when_list: unsafe { cells::<c::PLpgSQL_case_when>(s.case_when_list) }
                    .into_iter()
                    .map(|w| {
                        let w = unsafe { &*w };
                        CaseWhen {
                            lineno: w.lineno,
                            expr: unsafe { expr(w.expr) },
                            stmts: unsafe { stmts(w.stmts) },
                        }
                    })
                    .collect(),
                have_else: s.have_else,
                else_stmts: unsafe { stmts(s.else_stmts) },
            }))
        }
        c::PLpgSQL_stmt_type_PLPGSQL_STMT_LOOP => {
            let s = node!(PLpgSQL_stmt_loop);
            Stmt::Loop(NodeBox::new(StmtLoop {
                lineno: s.lineno,
                stmtid: s.stmtid,
                label: unsafe { opt_string(s.label) },
                body: unsafe { stmts(s.body) },
            }))
        }
        c::PLpgSQL_stmt_type_PLPGSQL_STMT_WHILE => {
            let s = node!(PLpgSQL_stmt_while);
            Stmt::While(NodeBox::new(StmtWhile {
                lineno: s.lineno,
                stmtid: s.stmtid,
                label: unsafe { opt_string(s.label) },
                cond: unsafe { expr(s.cond) },
                body: unsafe { stmts(s.body) },
            }))
        }
        c::PLpgSQL_stmt_type_PLPGSQL_STMT_FORI => {
            let s = node!(PLpgSQL_stmt_fori);
            Stmt::Fori(NodeBox::new(StmtFori {
                lineno: s.lineno,
                stmtid: s.stmtid,
                label: unsafe { opt_string(s.label) },
                var: unsafe { dno(s.var) },
                lower: unsafe { expr(s.lower) },
                upper: unsafe { expr(s.upper) },
                step: unsafe { expr(s.step) },
                reverse: s.reverse != 0,
                body: unsafe { stmts(s.body) },
            }))
        }
        c::PLpgSQL_stmt_type_PLPGSQL_STMT_FORS => {
            let s = node!(PLpgSQL_stmt_fors);
            Stmt::Fors(NodeBox::new(StmtFors {
                lineno: s.lineno,
                stmtid: s.stmtid,
                label: unsafe { opt_string(s.label) },
                var: unsafe { dno(s.var) },
                body: unsafe { stmts(s.body) },
                query: unsafe { expr(s.query) },
            }))
        }
        c::PLpgSQL_stmt_type_PLPGSQL_STMT_FORC => {
            let s = node!(PLpgSQL_stmt_forc);
            Stmt::Forc(NodeBox::new(StmtForc {
                lineno: s.lineno,
                stmtid: s.stmtid,
                label: unsafe { opt_string(s.label) },
                var: unsafe { dno(s.var) },
                body: unsafe { stmts(s.body) },
                curvar: s.curvar,
                argquery: unsafe { expr(s.argquery) },
            }))
        }
        c::PLpgSQL_stmt_type_PLPGSQL_STMT_FOREACH_A => {
            let s = node!(PLpgSQL_stmt_foreach_a);
            Stmt::ForeachA(NodeBox::new(StmtForeachA {
                lineno: s.lineno,
                stmtid: s.stmtid,
                label: unsafe { opt_string(s.label) },
                varno: s.varno,
                slice: s.slice,
                expr: unsafe { expr(s.expr) },
                body: unsafe { stmts(s.body) },
            }))
        }
        c::PLpgSQL_stmt_type_PLPGSQL_STMT_EXIT => {
            let s = node!(PLpgSQL_stmt_exit);
            Stmt::Exit(NodeBox::new(StmtExit {
                lineno: s.lineno,
                stmtid: s.stmtid,
                is_exit: s.is_exit,
                label: unsafe { opt_string(s.label) },
                cond: unsafe { expr(s.cond) },
            }))
        }
        c::PLpgSQL_stmt_type_PLPGSQL_STMT_RETURN => {
            let s = node!(PLpgSQL_stmt_return);
            Stmt::Return(NodeBox::new(StmtReturn {
                lineno: s.lineno,
                stmtid: s.stmtid,
                expr: unsafe { expr(s.expr) },
                retvarno: s.retvarno,
            }))
        }
        c::PLpgSQL_stmt_type_PLPGSQL_STMT_RETURN_NEXT => {
            let s = node!(PLpgSQL_stmt_return_next);
            Stmt::ReturnNext(NodeBox::new(StmtReturnNext {
                lineno: s.lineno,
                stmtid: s.stmtid,
                expr: unsafe { expr(s.expr) },
                retvarno: s.retvarno,
            }))
        }
        c::PLpgSQL_stmt_type_PLPGSQL_STMT_RETURN_QUERY => {
            let s = node!(PLpgSQL_stmt_return_query);
            Stmt::ReturnQuery(NodeBox::new(StmtReturnQuery {
                lineno: s.lineno,
                stmtid: s.stmtid,
                query: unsafe { expr(s.query) },
                dynquery: unsafe { expr(s.dynquery) },
                params: unsafe { exprs(s.params) },
            }))
        }
        c::PLpgSQL_stmt_type_PLPGSQL_STMT_RAISE => {
            let s = node!(PLpgSQL_stmt_raise);
            Stmt::Raise(NodeBox::new(StmtRaise {
                lineno: s.lineno,
                stmtid: s.stmtid,
                elog_level: s.elog_level,
                condname: unsafe { opt_string(s.condname) },
                message: unsafe { opt_string(s.message) },
                params: unsafe { exprs(s.params) },
                options: unsafe { cells::<c::PLpgSQL_raise_option>(s.options) }
                    .into_iter()
                    .map(|o| {
                        let o = unsafe { &*o };
                        RaiseOption {
                            opt_type: raise_option_type(o.opt_type),
                            expr: unsafe { expr(o.expr) },
                        }
                    })
                    .collect(),
            }))
        }
        c::PLpgSQL_stmt_type_PLPGSQL_STMT_ASSERT => {
            let s = node!(PLpgSQL_stmt_assert);
            Stmt::Assert(NodeBox::new(StmtAssert {
                lineno: s.lineno,
                stmtid: s.stmtid,
                cond: unsafe { expr(s.cond) },
                message: unsafe { expr(s.message) },
            }))
        }
        c::PLpgSQL_stmt_type_PLPGSQL_STMT_EXECSQL => {
            let s = node!(PLpgSQL_stmt_execsql);
            Stmt::Execsql(NodeBox::new(StmtExecsql {
                lineno: s.lineno,
                stmtid: s.stmtid,
                sqlstmt: unsafe { expr(s.sqlstmt) },
                into: s.into,
                strict: s.strict,
                target: unsafe { dno(s.target) },
            }))
        }
        c::PLpgSQL_stmt_type_PLPGSQL_STMT_DYNEXECUTE => {
            let s = node!(PLpgSQL_stmt_dynexecute);
            Stmt::Dynexecute(NodeBox::new(StmtDynexecute {
                lineno: s.lineno,
                stmtid: s.stmtid,
                query: unsafe { expr(s.query) },
                into: s.into,
                strict: s.strict,
                target: unsafe { dno(s.target) },
                params: unsafe { exprs(s.params) },
            }))
        }
        c::PLpgSQL_stmt_type_PLPGSQL_STMT_DYNFORS => {
            let s = node!(PLpgSQL_stmt_dynfors);
            Stmt::Dynfors(NodeBox::new(StmtDynfors {
                lineno: s.lineno,
                stmtid: s.stmtid,
                label: unsafe { opt_string(s.label) },
                var: unsafe { dno(s.var) },
                body: unsafe { stmts(s.body) },
                query: unsafe { expr(s.query) },
                params: unsafe { exprs(s.params) },
            }))
        }
        c::PLpgSQL_stmt_type_PLPGSQL_STMT_GETDIAG => {
            let s = node!(PLpgSQL_stmt_getdiag);
            Stmt::Getdiag(NodeBox::new(StmtGetdiag {
                lineno: s.lineno,
                stmtid: s.stmtid,
                is_stacked: s.is_stacked,
                diag_items: unsafe { cells::<c::PLpgSQL_diag_item>(s.diag_items) }
                    .into_iter()
                    .map(|d| {
                        let d = unsafe { &*d };
                        DiagItem {
                            kind: getdiag_kind(d.kind),
                            target: d.target,
                        }
                    })
                    .collect(),
            }))
        }
        c::PLpgSQL_stmt_type_PLPGSQL_STMT_OPEN => {
            let s = node!(PLpgSQL_stmt_open);
            Stmt::Open(NodeBox::new(StmtOpen {
                lineno: s.lineno,
                stmtid: s.stmtid,
                curvar: s.curvar,
                cursor_options: s.cursor_options,
                argquery: unsafe { expr(s.argquery) },
                query: unsafe { expr(s.query) },
                dynquery: unsafe { expr(s.dynquery) },
                params: unsafe { exprs(s.params) },
            }))
        }
        c::PLpgSQL_stmt_type_PLPGSQL_STMT_FETCH => {
            let s = node!(PLpgSQL_stmt_fetch);
            Stmt::Fetch(NodeBox::new(StmtFetch {
                lineno: s.lineno,
                stmtid: s.stmtid,
                target: unsafe { dno(s.target) },
                curvar: s.curvar,
                direction: match s.direction {
                    c::FetchDirection_FETCH_FORWARD => crate::ast::FetchDirection::FetchForward,
                    c::FetchDirection_FETCH_BACKWARD => crate::ast::FetchDirection::FetchBackward,
                    c::FetchDirection_FETCH_ABSOLUTE => crate::ast::FetchDirection::FetchAbsolute,
                    c::FetchDirection_FETCH_RELATIVE => crate::ast::FetchDirection::FetchRelative,
                    other => unreachable!("{other} is not a FetchDirection"),
                },
                how_many: s.how_many as i64,
                expr: unsafe { expr(s.expr) },
                is_move: s.is_move,
                returns_multiple_rows: s.returns_multiple_rows,
            }))
        }
        c::PLpgSQL_stmt_type_PLPGSQL_STMT_CLOSE => {
            let s = node!(PLpgSQL_stmt_close);
            Stmt::Close(NodeBox::new(StmtClose {
                lineno: s.lineno,
                stmtid: s.stmtid,
                curvar: s.curvar,
            }))
        }
        c::PLpgSQL_stmt_type_PLPGSQL_STMT_PERFORM => {
            let s = node!(PLpgSQL_stmt_perform);
            Stmt::Perform(NodeBox::new(StmtPerform {
                lineno: s.lineno,
                stmtid: s.stmtid,
                expr: unsafe { expr(s.expr) },
            }))
        }
        c::PLpgSQL_stmt_type_PLPGSQL_STMT_CALL => {
            let s = node!(PLpgSQL_stmt_call);
            Stmt::Call(NodeBox::new(StmtCall {
                lineno: s.lineno,
                stmtid: s.stmtid,
                expr: unsafe { expr(s.expr) },
                is_call: s.is_call,
                target: unsafe { dno(s.target) },
            }))
        }
        c::PLpgSQL_stmt_type_PLPGSQL_STMT_COMMIT => {
            let s = node!(PLpgSQL_stmt_commit);
            Stmt::Commit(NodeBox::new(StmtCommit {
                lineno: s.lineno,
                stmtid: s.stmtid,
                chain: s.chain,
            }))
        }
        c::PLpgSQL_stmt_type_PLPGSQL_STMT_ROLLBACK => {
            let s = node!(PLpgSQL_stmt_rollback);
            Stmt::Rollback(NodeBox::new(StmtRollback {
                lineno: s.lineno,
                stmtid: s.stmtid,
                chain: s.chain,
            }))
        }
        other => unreachable!("{other} is not a PLpgSQL_stmt_type"),
    }
}

fn raise_option_type(value: c::PLpgSQL_raise_option_type) -> RaiseOptionType {
    match value {
        c::PLpgSQL_raise_option_type_PLPGSQL_RAISEOPTION_ERRCODE => RaiseOptionType::Errcode,
        c::PLpgSQL_raise_option_type_PLPGSQL_RAISEOPTION_MESSAGE => RaiseOptionType::Message,
        c::PLpgSQL_raise_option_type_PLPGSQL_RAISEOPTION_DETAIL => RaiseOptionType::Detail,
        c::PLpgSQL_raise_option_type_PLPGSQL_RAISEOPTION_HINT => RaiseOptionType::Hint,
        c::PLpgSQL_raise_option_type_PLPGSQL_RAISEOPTION_COLUMN => RaiseOptionType::Column,
        c::PLpgSQL_raise_option_type_PLPGSQL_RAISEOPTION_CONSTRAINT => RaiseOptionType::Constraint,
        c::PLpgSQL_raise_option_type_PLPGSQL_RAISEOPTION_DATATYPE => RaiseOptionType::Datatype,
        c::PLpgSQL_raise_option_type_PLPGSQL_RAISEOPTION_TABLE => RaiseOptionType::Table,
        c::PLpgSQL_raise_option_type_PLPGSQL_RAISEOPTION_SCHEMA => RaiseOptionType::Schema,
        other => unreachable!("{other} is not a PLpgSQL_raise_option_type"),
    }
}

fn getdiag_kind(value: c::PLpgSQL_getdiag_kind) -> GetdiagKind {
    match value {
        c::PLpgSQL_getdiag_kind_PLPGSQL_GETDIAG_ROW_COUNT => GetdiagKind::RowCount,
        c::PLpgSQL_getdiag_kind_PLPGSQL_GETDIAG_ROUTINE_OID => GetdiagKind::RoutineOid,
        c::PLpgSQL_getdiag_kind_PLPGSQL_GETDIAG_CONTEXT => GetdiagKind::Context,
        c::PLpgSQL_getdiag_kind_PLPGSQL_GETDIAG_ERROR_CONTEXT => GetdiagKind::ErrorContext,
        c::PLpgSQL_getdiag_kind_PLPGSQL_GETDIAG_ERROR_DETAIL => GetdiagKind::ErrorDetail,
        c::PLpgSQL_getdiag_kind_PLPGSQL_GETDIAG_ERROR_HINT => GetdiagKind::ErrorHint,
        c::PLpgSQL_getdiag_kind_PLPGSQL_GETDIAG_RETURNED_SQLSTATE => GetdiagKind::ReturnedSqlstate,
        c::PLpgSQL_getdiag_kind_PLPGSQL_GETDIAG_COLUMN_NAME => GetdiagKind::ColumnName,
        c::PLpgSQL_getdiag_kind_PLPGSQL_GETDIAG_CONSTRAINT_NAME => GetdiagKind::ConstraintName,
        c::PLpgSQL_getdiag_kind_PLPGSQL_GETDIAG_DATATYPE_NAME => GetdiagKind::DatatypeName,
        c::PLpgSQL_getdiag_kind_PLPGSQL_GETDIAG_MESSAGE_TEXT => GetdiagKind::MessageText,
        c::PLpgSQL_getdiag_kind_PLPGSQL_GETDIAG_TABLE_NAME => GetdiagKind::TableName,
        c::PLpgSQL_getdiag_kind_PLPGSQL_GETDIAG_SCHEMA_NAME => GetdiagKind::SchemaName,
        other => unreachable!("{other} is not a PLpgSQL_getdiag_kind"),
    }
}
