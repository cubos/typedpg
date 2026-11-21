//! PL/pgSQL functions as PostgreSQL's PL/pgSQL compiler builds them.
//!
//! These types mirror the parse-time structs of PostgreSQL 19's
//! `src/pl/plpgsql/src/plpgsql.h` (the release named by
//! [`PG_VERSION`](crate::PG_VERSION)): a [`Function`] per compiled
//! `CREATE FUNCTION` / `DO`, its [`Datum`]s (variables, rows, records, record
//! fields, a trigger's promised `TG_*` values) and its statement tree
//! ([`Stmt`], one variant per `PLpgSQL_stmt_type`). Field names are PG's, in
//! snake case; C enums are Rust enums, nullable pointers `Option`s and
//! `List`s `Vec`s. What only the executor fills in (plans, values, caches)
//! is left out.
//!
//! The types are written by hand, as is their converter (`plpgsql/
//! convert.rs`), which reads the C structs through the bindgen bindings
//! `import/import.sh` generates from `plpgsql.h`. A PostgreSQL upgrade that
//! renames, retypes or removes a field the converter reads breaks the build
//! there; a new field or statement kind has to be added by hand (an unknown
//! `cmd_type` or `dtype` panics in the converter rather than being dropped).
//!
//! Embedded SQL stays text: an [`Expr`] carries its query as written in the
//! function body, with the [`RawParseMode`] PL/pgSQL parses it in.
//!
//! A variable a statement reads or writes (`INTO` targets, loop variables,
//! `RETURN` of a variable, …) is referenced by its `dno`: its index in
//! [`Function::datums`].
//!
//! Statements nest as deep as PL/pgSQL's grammar allows (3,330 blocks, when
//! its parser stack runs out), so each statement lives in a
//! [`NodeBox`](crate::NodeBox): converting, cloning and dropping the
//! deepest tree grow the stack as they go, as for the SQL AST.

pub(crate) mod convert;

use crate::NodeBox;

/// A compiled function (`PLpgSQL_function`).
#[derive(Clone, Debug, PartialEq)]
pub struct Function {
    /// The signature as `format_procedure` renders it; `inline_code_block`
    /// for a `DO` block.
    pub fn_signature: String,
    pub fn_is_trigger: Trigtype,
    pub fn_rettype: u32,
    pub fn_retset: bool,
    /// `pg_proc.prokind`: `f` (function) or `p` (procedure).
    pub fn_prokind: u8,
    /// The dnos of the arguments' variables (`fn_argvarnos[..fn_nargs]`).
    pub fn_argvarnos: Vec<i32>,
    /// The dno of the variable holding the OUT parameters, or `-1`.
    pub out_param_varno: i32,
    /// The dno of `FOUND`.
    pub found_varno: i32,
    /// The dnos of a trigger function's `NEW` and `OLD`.
    pub new_varno: i32,
    pub old_varno: i32,
    pub resolve_option: ResolveOption,
    pub print_strict_params: bool,
    /// `plpgsql.extra_warnings` / `extra_errors` (`PLPGSQL_XCHECK_*` bits).
    pub extra_warnings: i32,
    pub extra_errors: i32,
    /// The function's variables, by dno.
    pub datums: Vec<Datum>,
    /// The body, `None` only when the compiler left it unset.
    pub action: Option<StmtBlock>,
    /// How many statements the body holds (statement ids run 1..=this).
    pub nstatements: u32,
    pub requires_procedure_resowner: bool,
    pub has_exception_block: bool,
}

/// `PLpgSQL_trigtype`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Trigtype {
    DmlTrigger,
    EventTrigger,
    NotTrigger,
}

/// `PLpgSQL_resolve_option`: how a name that is both a variable and a
/// column resolves (`#variable_conflict`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ResolveOption {
    Error,
    Variable,
    Column,
}

/// A function's variable (`PLpgSQL_datum`, by `PLpgSQL_datum_type`).
#[derive(Clone, Debug, PartialEq)]
pub enum Datum {
    /// A scalar variable (`PLPGSQL_DTYPE_VAR`).
    Var(Box<Var>),
    /// A row of variables: an `INTO` target list, OUT parameters
    /// (`PLPGSQL_DTYPE_ROW`).
    Row(Box<Row>),
    /// A composite or `record` variable (`PLPGSQL_DTYPE_REC`).
    Rec(Box<Rec>),
    /// A field of a record variable (`PLPGSQL_DTYPE_RECFIELD`).
    Recfield(Box<Recfield>),
    /// A trigger function's `TG_*` variable, computed on first use
    /// (`PLPGSQL_DTYPE_PROMISE`: a `PLpgSQL_var` whose `promise` says
    /// which).
    Promise(Box<Var>),
}

impl Datum {
    /// The datum's number, its index in [`Function::datums`].
    pub fn dno(&self) -> i32 {
        match self {
            Datum::Var(v) | Datum::Promise(v) => v.dno,
            Datum::Row(r) => r.dno,
            Datum::Rec(r) => r.dno,
            Datum::Recfield(f) => f.dno,
        }
    }

    /// The variable's name (`refname`); `None` for a record field.
    pub fn refname(&self) -> Option<&str> {
        match self {
            Datum::Var(v) | Datum::Promise(v) => Some(&v.refname),
            Datum::Row(r) => Some(&r.refname),
            Datum::Rec(r) => Some(&r.refname),
            Datum::Recfield(_) => None,
        }
    }
}

/// `PLpgSQL_var`.
#[derive(Clone, Debug, PartialEq)]
pub struct Var {
    pub dno: i32,
    pub refname: String,
    pub lineno: i32,
    /// `CONSTANT`.
    pub isconst: bool,
    /// `NOT NULL`.
    pub notnull: bool,
    /// The `DEFAULT` / `:=` expression.
    pub default_val: Option<Expr>,
    pub datatype: Option<Type>,
    /// A bound cursor's query (`CURSOR FOR …`).
    pub cursor_explicit_expr: Option<Expr>,
    /// The dno of a bound cursor's argument row, or `-1`.
    pub cursor_explicit_argrow: i32,
    pub cursor_options: i32,
    /// Which `TG_*` value a [`Datum::Promise`] stands for.
    pub promise: PromiseType,
}

/// `PLpgSQL_row`.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub dno: i32,
    pub refname: String,
    pub lineno: i32,
    pub isconst: bool,
    pub notnull: bool,
    pub default_val: Option<Expr>,
    /// The row's fields (`fieldnames[i]`, `varnos[i]` for `i <
    /// nfields`).
    pub fields: Vec<RowField>,
}

/// One field of a [`Row`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowField {
    /// `None` for a dropped column of the row's type.
    pub name: Option<String>,
    /// The dno of the variable the field is stored in.
    pub varno: i32,
}

/// `PLpgSQL_rec`.
#[derive(Clone, Debug, PartialEq)]
pub struct Rec {
    pub dno: i32,
    pub refname: String,
    pub lineno: i32,
    pub isconst: bool,
    pub notnull: bool,
    pub default_val: Option<Expr>,
    /// The declared type; `None` for `record`.
    pub datatype: Option<Type>,
    pub rectypeid: u32,
    /// The dno of the record's first [`Recfield`], or `-1`.
    pub firstfield: i32,
}

/// `PLpgSQL_recfield`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recfield {
    pub dno: i32,
    pub fieldname: String,
    /// The dno of the record.
    pub recparentno: i32,
    /// The dno of the record's next field, or `-1`.
    pub nextfield: i32,
}

/// `PLpgSQL_promise_type`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PromiseType {
    None,
    TgName,
    TgWhen,
    TgLevel,
    TgOp,
    TgRelid,
    TgTableName,
    TgTableSchema,
    TgNargs,
    TgArgv,
    TgEvent,
    TgTag,
}

/// A variable's resolved type (`PLpgSQL_type`).
#[derive(Clone, Debug, PartialEq)]
pub struct Type {
    /// The type's (unqualified) `pg_type.typname`.
    pub typname: String,
    pub typoid: u32,
    pub ttype: TypeType,
    pub typlen: i16,
    pub typbyval: bool,
    /// `pg_type.typtype`: `b`, `c`, `d`, `e`, `p`, `r` or `m`.
    pub typtype: u8,
    pub collation: u32,
    /// A true array, or a domain over one.
    pub typisarray: bool,
    pub atttypmod: i32,
    /// The type name as written, kept for a named composite type (the
    /// compiler re-looks it up by this name).
    pub origtypname: Option<crate::ast::TypeName>,
}

/// `PLpgSQL_type_type`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TypeType {
    /// Scalar types and domains.
    Scalar,
    /// Composite types, `record` included.
    Rec,
    /// Pseudo-types.
    Pseudo,
}

/// An embedded SQL expression or statement (`PLpgSQL_expr`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expr {
    /// The SQL, verbatim from the function body.
    pub query: String,
    /// How PL/pgSQL parses it (`parseMode`).
    pub parse_mode: RawParseMode,
    /// The dno of the variable this expression is assigned to, or `-1`.
    pub target_param: i32,
    /// Whether that variable is declared inside the closest exception
    /// block around the assignment.
    pub target_is_local: bool,
}

/// PostgreSQL's `RawParseMode` (parser/parser.h).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RawParseMode {
    /// A list of SQL statements.
    Default,
    TypeName,
    /// A PL/pgSQL expression (`SELECT` and the list after it).
    PlpgsqlExpr,
    /// An assignment target with 1, 2 or 3 name parts.
    PlpgsqlAssign1,
    PlpgsqlAssign2,
    PlpgsqlAssign3,
}

/// A statement (`PLpgSQL_stmt`, by `PLpgSQL_stmt_type`).
#[derive(Clone, Debug, PartialEq)]
pub enum Stmt {
    Block(NodeBox<StmtBlock>),
    Assign(NodeBox<StmtAssign>),
    If(NodeBox<StmtIf>),
    Case(NodeBox<StmtCase>),
    Loop(NodeBox<StmtLoop>),
    While(NodeBox<StmtWhile>),
    Fori(NodeBox<StmtFori>),
    Fors(NodeBox<StmtFors>),
    Forc(NodeBox<StmtForc>),
    ForeachA(NodeBox<StmtForeachA>),
    Exit(NodeBox<StmtExit>),
    Return(NodeBox<StmtReturn>),
    ReturnNext(NodeBox<StmtReturnNext>),
    ReturnQuery(NodeBox<StmtReturnQuery>),
    Raise(NodeBox<StmtRaise>),
    Assert(NodeBox<StmtAssert>),
    Execsql(NodeBox<StmtExecsql>),
    Dynexecute(NodeBox<StmtDynexecute>),
    Dynfors(NodeBox<StmtDynfors>),
    Getdiag(NodeBox<StmtGetdiag>),
    Open(NodeBox<StmtOpen>),
    Fetch(NodeBox<StmtFetch>),
    Close(NodeBox<StmtClose>),
    Perform(NodeBox<StmtPerform>),
    Call(NodeBox<StmtCall>),
    Commit(NodeBox<StmtCommit>),
    Rollback(NodeBox<StmtRollback>),
}

impl Stmt {
    /// The statement's line in the function body.
    pub fn lineno(&self) -> i32 {
        self.header().0
    }

    /// The statement's id, unique in the function (1-based).
    pub fn stmtid(&self) -> u32 {
        self.header().1
    }

    fn header(&self) -> (i32, u32) {
        macro_rules! header {
            ($($variant:ident),*) => {
                match self {
                    $(Stmt::$variant(s) => (s.lineno, s.stmtid),)*
                }
            };
        }
        header!(
            Block,
            Assign,
            If,
            Case,
            Loop,
            While,
            Fori,
            Fors,
            Forc,
            ForeachA,
            Exit,
            Return,
            ReturnNext,
            ReturnQuery,
            Raise,
            Assert,
            Execsql,
            Dynexecute,
            Dynfors,
            Getdiag,
            Open,
            Fetch,
            Close,
            Perform,
            Call,
            Commit,
            Rollback
        )
    }
}

/// `[<<label>>] [DECLARE …] BEGIN … [EXCEPTION …] END`
/// (`PLpgSQL_stmt_block`).
#[derive(Clone, Debug, PartialEq)]
pub struct StmtBlock {
    pub lineno: i32,
    pub stmtid: u32,
    pub label: Option<String>,
    pub body: Vec<Stmt>,
    /// The dnos of the variables the block declares, initialized on
    /// entry.
    pub initvarnos: Vec<i32>,
    pub exceptions: Option<ExceptionBlock>,
}

/// A block's `EXCEPTION` clause (`PLpgSQL_exception_block`).
#[derive(Clone, Debug, PartialEq)]
pub struct ExceptionBlock {
    /// The dnos of the handlers' `SQLSTATE` and `SQLERRM` variables.
    pub sqlstate_varno: i32,
    pub sqlerrm_varno: i32,
    /// The `WHEN` clauses.
    pub exc_list: Vec<Exception>,
}

/// One `WHEN … THEN …` handler (`PLpgSQL_exception`).
#[derive(Clone, Debug, PartialEq)]
pub struct Exception {
    pub lineno: i32,
    /// The conditions, `OR`ed.
    pub conditions: Vec<Condition>,
    pub action: Vec<Stmt>,
}

/// A handler's condition (`PLpgSQL_condition`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Condition {
    /// The SQLSTATE, packed as `MAKE_SQLSTATE` does, or
    /// [`Condition::OTHERS`].
    pub sqlerrstate: i32,
    /// The condition's name (`others`, `division_by_zero`, …) or SQLSTATE
    /// as written.
    pub condname: String,
}

impl Condition {
    /// `PLPGSQL_OTHERS`: `WHEN OTHERS`.
    pub const OTHERS: i32 = -1;
}

/// `target := expr` (`PLpgSQL_stmt_assign`).
#[derive(Clone, Debug, PartialEq)]
pub struct StmtAssign {
    pub lineno: i32,
    pub stmtid: u32,
    /// The dno of the variable assigned to.
    pub varno: i32,
    /// The whole assignment (`target := expr`), parsed in a
    /// `PlpgsqlAssign*` mode.
    pub expr: Option<Expr>,
}

/// `IF … THEN … [ELSIF …] [ELSE …] END IF` (`PLpgSQL_stmt_if`).
#[derive(Clone, Debug, PartialEq)]
pub struct StmtIf {
    pub lineno: i32,
    pub stmtid: u32,
    pub cond: Option<Expr>,
    pub then_body: Vec<Stmt>,
    pub elsif_list: Vec<IfElsif>,
    pub else_body: Vec<Stmt>,
}

/// `ELSIF cond THEN …` (`PLpgSQL_if_elsif`).
#[derive(Clone, Debug, PartialEq)]
pub struct IfElsif {
    pub lineno: i32,
    pub cond: Option<Expr>,
    pub stmts: Vec<Stmt>,
}

/// `CASE [expr] WHEN … END CASE` (`PLpgSQL_stmt_case`).
#[derive(Clone, Debug, PartialEq)]
pub struct StmtCase {
    pub lineno: i32,
    pub stmtid: u32,
    /// The tested expression of a simple `CASE`.
    pub t_expr: Option<Expr>,
    /// The dno of the variable the tested value is stored in.
    pub t_varno: i32,
    pub case_when_list: Vec<CaseWhen>,
    /// Whether there is an `ELSE` (the list may be empty).
    pub have_else: bool,
    pub else_stmts: Vec<Stmt>,
}

/// `WHEN expr THEN …` (`PLpgSQL_case_when`).
#[derive(Clone, Debug, PartialEq)]
pub struct CaseWhen {
    pub lineno: i32,
    pub expr: Option<Expr>,
    pub stmts: Vec<Stmt>,
}

/// `LOOP … END LOOP` (`PLpgSQL_stmt_loop`).
#[derive(Clone, Debug, PartialEq)]
pub struct StmtLoop {
    pub lineno: i32,
    pub stmtid: u32,
    pub label: Option<String>,
    pub body: Vec<Stmt>,
}

/// `WHILE cond LOOP … END LOOP` (`PLpgSQL_stmt_while`).
#[derive(Clone, Debug, PartialEq)]
pub struct StmtWhile {
    pub lineno: i32,
    pub stmtid: u32,
    pub label: Option<String>,
    pub cond: Option<Expr>,
    pub body: Vec<Stmt>,
}

/// `FOR i IN [REVERSE] lower .. upper [BY step] LOOP` (`PLpgSQL_stmt_fori`).
#[derive(Clone, Debug, PartialEq)]
pub struct StmtFori {
    pub lineno: i32,
    pub stmtid: u32,
    pub label: Option<String>,
    /// The dno of the loop variable.
    pub var: Option<i32>,
    pub lower: Option<Expr>,
    pub upper: Option<Expr>,
    /// `None` is `BY 1`.
    pub step: Option<Expr>,
    pub reverse: bool,
    pub body: Vec<Stmt>,
}

/// `FOR target IN query LOOP` (`PLpgSQL_stmt_fors`).
#[derive(Clone, Debug, PartialEq)]
pub struct StmtFors {
    pub lineno: i32,
    pub stmtid: u32,
    pub label: Option<String>,
    /// The dno of the loop's target (a record or row).
    pub var: Option<i32>,
    pub body: Vec<Stmt>,
    pub query: Option<Expr>,
}

/// `FOR target IN cursor [(args)] LOOP` (`PLpgSQL_stmt_forc`).
#[derive(Clone, Debug, PartialEq)]
pub struct StmtForc {
    pub lineno: i32,
    pub stmtid: u32,
    pub label: Option<String>,
    /// The dno of the loop's target (a record).
    pub var: Option<i32>,
    pub body: Vec<Stmt>,
    /// The dno of the cursor variable.
    pub curvar: i32,
    /// The cursor's arguments.
    pub argquery: Option<Expr>,
}

/// `FOREACH target [SLICE n] IN ARRAY expr LOOP`
/// (`PLpgSQL_stmt_foreach_a`).
#[derive(Clone, Debug, PartialEq)]
pub struct StmtForeachA {
    pub lineno: i32,
    pub stmtid: u32,
    pub label: Option<String>,
    /// The dno of the loop's target.
    pub varno: i32,
    /// The slice dimension, or 0.
    pub slice: i32,
    pub expr: Option<Expr>,
    pub body: Vec<Stmt>,
}

/// `EXIT` / `CONTINUE [label] [WHEN cond]` (`PLpgSQL_stmt_exit`).
#[derive(Clone, Debug, PartialEq)]
pub struct StmtExit {
    pub lineno: i32,
    pub stmtid: u32,
    /// `EXIT` rather than `CONTINUE`.
    pub is_exit: bool,
    pub label: Option<String>,
    pub cond: Option<Expr>,
}

/// `RETURN [expr]` (`PLpgSQL_stmt_return`).
#[derive(Clone, Debug, PartialEq)]
pub struct StmtReturn {
    pub lineno: i32,
    pub stmtid: u32,
    pub expr: Option<Expr>,
    /// The dno of the variable returned, when the function returns a row
    /// or a set and `expr` is `None`; `-1` otherwise.
    pub retvarno: i32,
}

/// `RETURN NEXT [expr]` (`PLpgSQL_stmt_return_next`).
#[derive(Clone, Debug, PartialEq)]
pub struct StmtReturnNext {
    pub lineno: i32,
    pub stmtid: u32,
    pub expr: Option<Expr>,
    /// As [`StmtReturn::retvarno`].
    pub retvarno: i32,
}

/// `RETURN QUERY query` / `RETURN QUERY EXECUTE dynquery [USING params]`
/// (`PLpgSQL_stmt_return_query`).
#[derive(Clone, Debug, PartialEq)]
pub struct StmtReturnQuery {
    pub lineno: i32,
    pub stmtid: u32,
    pub query: Option<Expr>,
    pub dynquery: Option<Expr>,
    pub params: Vec<Expr>,
}

/// `RAISE [level] [format, params…] [USING option = expr, …]`
/// (`PLpgSQL_stmt_raise`).
#[derive(Clone, Debug, PartialEq)]
pub struct StmtRaise {
    pub lineno: i32,
    pub stmtid: u32,
    /// The `elog.h` level: `DEBUG1` (14) … `EXCEPTION` (`ERROR`, 21).
    pub elog_level: i32,
    /// The condition name or SQLSTATE given instead of a format.
    pub condname: Option<String>,
    /// The format string.
    pub message: Option<String>,
    /// The format's arguments.
    pub params: Vec<Expr>,
    pub options: Vec<RaiseOption>,
}

/// One `USING option = expr` of a `RAISE` (`PLpgSQL_raise_option`).
#[derive(Clone, Debug, PartialEq)]
pub struct RaiseOption {
    pub opt_type: RaiseOptionType,
    pub expr: Option<Expr>,
}

/// `PLpgSQL_raise_option_type`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RaiseOptionType {
    Errcode,
    Message,
    Detail,
    Hint,
    Column,
    Constraint,
    Datatype,
    Table,
    Schema,
}

/// `ASSERT cond [, message]` (`PLpgSQL_stmt_assert`).
#[derive(Clone, Debug, PartialEq)]
pub struct StmtAssert {
    pub lineno: i32,
    pub stmtid: u32,
    pub cond: Option<Expr>,
    pub message: Option<Expr>,
}

/// A SQL statement (`PLpgSQL_stmt_execsql`).
#[derive(Clone, Debug, PartialEq)]
pub struct StmtExecsql {
    pub lineno: i32,
    pub stmtid: u32,
    pub sqlstmt: Option<Expr>,
    /// Whether there is an `INTO`.
    pub into: bool,
    /// `INTO STRICT`.
    pub strict: bool,
    /// The dno of the `INTO` target (a record or row).
    pub target: Option<i32>,
}

/// `EXECUTE query [INTO [STRICT] target] [USING params]`
/// (`PLpgSQL_stmt_dynexecute`).
#[derive(Clone, Debug, PartialEq)]
pub struct StmtDynexecute {
    pub lineno: i32,
    pub stmtid: u32,
    /// The expression computing the SQL string.
    pub query: Option<Expr>,
    pub into: bool,
    pub strict: bool,
    /// The dno of the `INTO` target (a record or row).
    pub target: Option<i32>,
    pub params: Vec<Expr>,
}

/// `FOR target IN EXECUTE query [USING params] LOOP`
/// (`PLpgSQL_stmt_dynfors`).
#[derive(Clone, Debug, PartialEq)]
pub struct StmtDynfors {
    pub lineno: i32,
    pub stmtid: u32,
    pub label: Option<String>,
    /// The dno of the loop's target (a record or row).
    pub var: Option<i32>,
    pub body: Vec<Stmt>,
    pub query: Option<Expr>,
    pub params: Vec<Expr>,
}

/// `GET [CURRENT | STACKED] DIAGNOSTICS target = item, …`
/// (`PLpgSQL_stmt_getdiag`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StmtGetdiag {
    pub lineno: i32,
    pub stmtid: u32,
    /// `STACKED` rather than `CURRENT`.
    pub is_stacked: bool,
    pub diag_items: Vec<DiagItem>,
}

/// One `target = item` of a `GET DIAGNOSTICS` (`PLpgSQL_diag_item`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiagItem {
    pub kind: GetdiagKind,
    /// The dno of the target variable.
    pub target: i32,
}

/// `PLpgSQL_getdiag_kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GetdiagKind {
    RowCount,
    RoutineOid,
    Context,
    ErrorContext,
    ErrorDetail,
    ErrorHint,
    ReturnedSqlstate,
    ColumnName,
    ConstraintName,
    DatatypeName,
    MessageText,
    TableName,
    SchemaName,
}

impl GetdiagKind {
    /// The item's name in SQL, as PL/pgSQL's `plpgsql_getdiag_kindname`
    /// spells it (`ROW_COUNT`, `PG_CONTEXT`, …).
    pub fn name(self) -> &'static str {
        match self {
            GetdiagKind::RowCount => "ROW_COUNT",
            GetdiagKind::RoutineOid => "PG_ROUTINE_OID",
            GetdiagKind::Context => "PG_CONTEXT",
            GetdiagKind::ErrorContext => "PG_EXCEPTION_CONTEXT",
            GetdiagKind::ErrorDetail => "PG_EXCEPTION_DETAIL",
            GetdiagKind::ErrorHint => "PG_EXCEPTION_HINT",
            GetdiagKind::ReturnedSqlstate => "RETURNED_SQLSTATE",
            GetdiagKind::ColumnName => "COLUMN_NAME",
            GetdiagKind::ConstraintName => "CONSTRAINT_NAME",
            GetdiagKind::DatatypeName => "PG_DATATYPE_NAME",
            GetdiagKind::MessageText => "MESSAGE_TEXT",
            GetdiagKind::TableName => "TABLE_NAME",
            GetdiagKind::SchemaName => "SCHEMA_NAME",
        }
    }
}

/// `OPEN cursor [[NO] SCROLL] [FOR query | FOR EXECUTE dynquery [USING
/// params] | (args)]` (`PLpgSQL_stmt_open`).
#[derive(Clone, Debug, PartialEq)]
pub struct StmtOpen {
    pub lineno: i32,
    pub stmtid: u32,
    /// The dno of the cursor variable.
    pub curvar: i32,
    /// `CURSOR_OPT_*` bits.
    pub cursor_options: i32,
    /// A bound cursor's arguments.
    pub argquery: Option<Expr>,
    pub query: Option<Expr>,
    pub dynquery: Option<Expr>,
    pub params: Vec<Expr>,
}

/// `FETCH` / `MOVE [direction] cursor [INTO target]`
/// (`PLpgSQL_stmt_fetch`).
#[derive(Clone, Debug, PartialEq)]
pub struct StmtFetch {
    pub lineno: i32,
    pub stmtid: u32,
    /// The dno of the `INTO` target (a record or row); `None` for `MOVE`.
    pub target: Option<i32>,
    /// The dno of the cursor variable.
    pub curvar: i32,
    pub direction: crate::ast::FetchDirection,
    /// The count, when constant (`expr` is `None`).
    pub how_many: i64,
    /// The count, when an expression.
    pub expr: Option<Expr>,
    pub is_move: bool,
    /// Whether it can move over more than one row (`MOVE` only).
    pub returns_multiple_rows: bool,
}

/// `CLOSE cursor` (`PLpgSQL_stmt_close`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StmtClose {
    pub lineno: i32,
    pub stmtid: u32,
    /// The dno of the cursor variable.
    pub curvar: i32,
}

/// `PERFORM expr` (`PLpgSQL_stmt_perform`).
#[derive(Clone, Debug, PartialEq)]
pub struct StmtPerform {
    pub lineno: i32,
    pub stmtid: u32,
    /// The query, as `SELECT …` (PERFORM's keyword replaced).
    pub expr: Option<Expr>,
}

/// `CALL proc(…)` / `DO …` (`PLpgSQL_stmt_call`).
#[derive(Clone, Debug, PartialEq)]
pub struct StmtCall {
    pub lineno: i32,
    pub stmtid: u32,
    pub expr: Option<Expr>,
    /// `CALL` rather than `DO`.
    pub is_call: bool,
    /// The dno of the row the procedure's output arguments go to.
    pub target: Option<i32>,
}

/// `COMMIT [AND [NO] CHAIN]` (`PLpgSQL_stmt_commit`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StmtCommit {
    pub lineno: i32,
    pub stmtid: u32,
    pub chain: bool,
}

/// `ROLLBACK [AND [NO] CHAIN]` (`PLpgSQL_stmt_rollback`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StmtRollback {
    pub lineno: i32,
    pub stmtid: u32,
    pub chain: bool,
}
