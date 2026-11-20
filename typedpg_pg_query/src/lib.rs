//! PostgreSQL parser bindings for typedpg, built on PostgreSQL's own,
//! unmodified sources: the parser, the grammar and the scanner of the release
//! named by [`PG_VERSION`].
//!
//! - [`parse`] / [`parse_type_name`] / [`scan`] call into the C parser;
//! - [`ast`] holds the AST types, generated from PostgreSQL's headers: a
//!   struct per node type and [`ast::Node`], the enum over all of them, each
//!   kind in a [`NodeBox`];
//! - [`NodeRef`] / [`NodeMut`] view a node by kind, and `nodes()` /
//!   `nodes_mut()` list a tree's nodes.
//!
//! The generated modules are produced by `typedpg_pg_query_codegen`; see the
//! crate README for how the sources are imported and regenerated.
//!
//! [`parse_plpgsql`] compiles `DO` blocks and `CREATE FUNCTION … LANGUAGE
//! plpgsql` statements with PostgreSQL's own PL/pgSQL compiler and returns
//! the compiled functions as [`plpgsql::Function`]s, converted straight from
//! the compiler's structs; [`parse_plpgsql_with_catalog`] resolves their
//! types, schemas and relations against a [`Catalog`] instead of only the
//! built-in `pg_catalog` rows.

#[allow(clippy::all, missing_docs)]
pub mod ast;
mod bindings;
mod catalog;
mod convert;
mod equal;
mod node;
mod node_box;
pub mod plpgsql;
mod stack;

use std::collections::VecDeque;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::sync::Mutex;

use catalog::CatalogFns;
pub use catalog::{Catalog, CatalogAttribute, CatalogType};
pub use equal::Equal;
pub use node::{NodeMut, NodeRef};
pub use node_box::NodeBox;

include!(concat!(env!("OUT_DIR"), "/version.rs"));

/// Errors from the parser and from moving trees across the C boundary.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    /// The input contains a NUL byte, which C strings can't carry.
    #[error("Invalid statement format: {0}")]
    Conversion(#[from] std::ffi::NulError),
    /// PostgreSQL's parser rejected the input; the message is the server's.
    #[error("Invalid statement: {message}")]
    Parse {
        /// The server's error message (`syntax error at or near "x"`).
        message: String,
        /// Where the error is, as a byte offset into the input — what PG
        /// reports as the error's `POSITION` (`cursorpos`). `None` when the
        /// error has no position.
        position: Option<usize>,
        /// The PostgreSQL function that raised it (`core_yylex` for the
        /// scanner, `base_yyparse` for the grammar, …): the same message
        /// can carry a different SQLSTATE depending on where it comes
        /// from. `None` when unknown.
        funcname: Option<String>,
    },
    /// The scanner rejected the input.
    #[error("Error scanning: {0}")]
    Scan(String),
}

/// `Result` with this crate's [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

/// A parsed statement list.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseResult {
    /// The parse tree.
    pub tree: ast::ParseResult,
    /// Whatever the parser wrote to stderr (warnings).
    pub stderr: String,
}

/// The parser's globals are process-wide: one call at a time.
static LOCK: Mutex<()> = Mutex::new(());

/// `ShimError` in csrc/pg/shim.c.
#[repr(C)]
struct ShimError {
    message: *mut c_char,
    funcname: *mut c_char,
    filename: *mut c_char,
    lineno: c_int,
    cursorpos: c_int,
}

/// `ShimParseResult` in csrc/pg/shim.c.
#[repr(C)]
struct ShimParseResult {
    /// The `List *` of `RawStmt` (in `context`).
    tree: *mut c_void,
    /// WARNING/NOTICE text, malloc'd.
    notices: *mut c_char,
    error: *mut ShimError,
    /// The `MemoryContext` the tree lives in.
    context: *mut c_void,
}

/// `ShimToken` in csrc/pg/shim.c.
#[repr(C)]
struct ShimToken {
    start: i32,
    end: i32,
    token: i32,
    keyword_kind: i32,
}

/// `ShimScanResult` in csrc/pg/shim.c.
#[repr(C)]
struct ShimScanResult {
    tokens: *mut ShimToken,
    ntokens: c_int,
    error: *mut ShimError,
}

/// `ShimPlpgsqlResult` in csrc/plpgsql/parse_plpgsql.c.
#[repr(C)]
struct ShimPlpgsqlResult {
    /// WARNING/NOTICE text, malloc'd.
    notices: *mut c_char,
    error: *mut ShimError,
}

unsafe extern "C" {
    fn shim_parse(sql: *const c_char, mode: c_int) -> ShimParseResult;
    fn shim_finish(result: ShimParseResult);
    fn shim_scan(sql: *const c_char) -> ShimScanResult;
    fn shim_scan_free(result: ShimScanResult);
    fn shim_plpgsql(
        sql: *const c_char,
        catalog: *const CatalogFns,
        state: *mut c_void,
    ) -> ShimPlpgsqlResult;
    fn shim_plpgsql_free(result: ShimPlpgsqlResult);
}

/// The error a tree too deep for the AST is refused with: the same message
/// (and SQLSTATE, 54001, which the analyzer maps it to) PostgreSQL's
/// `check_stack_depth` raises.
fn stack_depth_error() -> Error {
    Error::Parse {
        message: "stack depth limit exceeded".into(),
        position: None,
        funcname: Some("check_stack_depth".into()),
    }
}

/// A malloc'd C string as a Rust string; NULL is empty.
///
/// # Safety
/// `p` must be NULL or point to a NUL-terminated string.
unsafe fn take_string(p: *mut c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

/// A shim error as an [`Error::Parse`], its position converted to a byte
/// offset into `input`.
///
/// PG's `cursorpos` is its `errposition`: a 1-based *character* index
/// (`pg_mbstrlen_with_len(scanbuf, location) + 1` in `scanner_errposition`),
/// 0 when the error has none.
///
/// # Safety
/// `error` must be a live `ShimError`.
unsafe fn parse_error(error: *const ShimError, input: &str) -> Error {
    let error = unsafe { &*error };
    let message = unsafe { take_string(error.message) };
    let position = usize::try_from(error.cursorpos)
        .ok()
        .and_then(|pos| pos.checked_sub(1))
        .map(|chars| {
            input
                .char_indices()
                .nth(chars)
                .map_or(input.len(), |(byte, _)| byte)
        });
    let funcname = (!error.funcname.is_null()).then(|| unsafe { take_string(error.funcname) });
    Error::Parse {
        message,
        position,
        funcname,
    }
}

/// Parse `sql` with PostgreSQL's grammar.
pub fn parse(sql: &str) -> Result<ParseResult> {
    parse_with_options(sql, 0)
}

/// PostgreSQL's `RAW_PARSE_TYPE_NAME` mode.
const PARSE_TYPE_NAME: c_int = 1;

/// Parse `text` as a lone type name — PG's `raw_parser` in
/// `RAW_PARSE_TYPE_NAME` mode, what `parseTypeString` (`'…'::regtype`,
/// `to_regtype`) uses. The result holds one statement whose node is a
/// `TypeName`.
pub fn parse_type_name(text: &str) -> Result<ParseResult> {
    parse_with_options(text, PARSE_TYPE_NAME)
}

fn parse_with_options(sql: &str, mode: c_int) -> Result<ParseResult> {
    let input = CString::new(sql)?;
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    stack::with_stack(|| {
        let result = unsafe { shim_parse(input.as_ptr(), mode) };
        let parsed = if result.error.is_null() {
            // The tree lives in the call's context: convert it before
            // shim_finish deletes that.
            let stmts = unsafe { convert::raw_stmts(result.tree as *const bindings::List) };
            if convert::too_deep() {
                Err(stack_depth_error())
            } else {
                Ok(ParseResult {
                    tree: ast::ParseResult {
                        version: PG_VERSION_NUM,
                        stmts,
                    },
                    stderr: unsafe { take_string(result.notices) },
                })
            }
        } else {
            Err(unsafe { parse_error(result.error, sql) })
        };
        unsafe { shim_finish(result) };
        parsed
    })
}

/// Split `sql` into PostgreSQL's lexical tokens.
pub fn scan(sql: &str) -> Result<ast::ScanResult> {
    let input = CString::new(sql)?;
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    stack::with_stack(|| {
        let result = unsafe { shim_scan(input.as_ptr()) };
        let scanned = if result.error.is_null() {
            let raw = unsafe {
                std::slice::from_raw_parts(result.tokens, result.ntokens.max(0) as usize)
            };
            raw.iter()
                .map(|t| {
                    Ok(ast::ScanToken {
                        start: t.start,
                        end: t.end,
                        token: convert::token(t.token).ok_or_else(|| {
                            Error::Scan(format!("unknown token number {}", t.token))
                        })?,
                        keyword_kind: convert::keyword_kind(t.keyword_kind).ok_or_else(|| {
                            Error::Scan(format!("unknown keyword kind {}", t.keyword_kind))
                        })?,
                    })
                })
                .collect::<Result<Vec<_>>>()
                .map(|tokens| ast::ScanResult {
                    version: PG_VERSION_NUM,
                    tokens: with_comments(sql, tokens),
                })
        } else {
            Err(Error::Scan(unsafe { take_string((*result.error).message) }))
        };
        unsafe { shim_scan_free(result) };
        scanned
    })
}

/// PostgresQL's scanner drops comments; libpg_query's returned them as
/// `SQL_COMMENT` / `C_COMMENT` tokens, and the analyzer relies on that (a
/// comment hides a `--` that would otherwise run to the end of the line, and
/// splitting a query must know where comments are). Rebuild them from the
/// text between the tokens: `--` up to but excluding the newline, and
/// `/* … */` with its nesting.
fn with_comments(sql: &str, tokens: Vec<ast::ScanToken>) -> Vec<ast::ScanToken> {
    fn comment(token: ast::Token, start: usize, end: usize) -> ast::ScanToken {
        ast::ScanToken {
            start: i32::try_from(start).unwrap_or(i32::MAX),
            end: i32::try_from(end).unwrap_or(i32::MAX),
            token,
            keyword_kind: ast::KeywordKind::NoKeyword,
        }
    }

    /// Push the comments of `text` (which starts at `base` in the SQL).
    fn push_comments(text: &str, base: usize, out: &mut Vec<ast::ScanToken>) {
        let bytes = text.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'-' && bytes.get(i + 1) == Some(&b'-') {
                let start = i;
                while i < bytes.len() && bytes[i] != b'\n' && bytes[i] != b'\r' {
                    i += 1;
                }
                out.push(comment(ast::Token::SqlComment, base + start, base + i));
            } else if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                let start = i;
                i += 2;
                let mut depth = 1usize;
                while i < bytes.len() && depth > 0 {
                    if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        i += 2;
                    } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                out.push(comment(ast::Token::CComment, base + start, base + i));
            } else {
                i += 1;
            }
        }
    }

    let mut out = Vec::with_capacity(tokens.len() + 4);
    let mut cursor = 0usize;
    for token in tokens {
        let start = usize::try_from(token.start)
            .unwrap_or(0)
            .min(sql.len())
            .max(cursor);
        if let Some(gap) = sql.get(cursor..start) {
            push_comments(gap, cursor, &mut out);
        }
        cursor = usize::try_from(token.end)
            .unwrap_or(start)
            .max(cursor)
            .min(sql.len());
        out.push(token);
    }
    if let Some(gap) = sql.get(cursor..) {
        push_comments(gap, cursor, &mut out);
    }
    out
}

/// Compile the `CREATE FUNCTION … LANGUAGE plpgsql` statements and `DO`
/// blocks of `sql` with PL/pgSQL's compiler, returning the compiled
/// functions in statement order (statements of other languages yield none).
/// Without a catalog, the compiler only knows the built-in `pg_catalog`
/// types and schemas — an empty database, where anything else is an error.
pub fn parse_plpgsql(sql: &str) -> Result<Vec<plpgsql::Function>> {
    let input = CString::new(sql)?;
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    stack::with_stack(|| plpgsql_result(&input, std::ptr::null(), sql))
}

/// [`parse_plpgsql`], with the compiler resolving types, schemas, relations
/// and columns (`%TYPE`, `%ROWTYPE`) against `catalog` — so it reports what
/// PostgreSQL would for that catalog (`type "x" does not exist`, `relation
/// "t" does not exist`, …).
pub fn parse_plpgsql_with_catalog(
    sql: &str,
    catalog: &dyn Catalog,
) -> Result<Vec<plpgsql::Function>> {
    let input = CString::new(sql)?;
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    stack::with_stack(|| catalog::with_catalog(catalog, |hooks| plpgsql_result(&input, hooks, sql)))
}

/// What one `shim_plpgsql` call collects: the converted functions, or the
/// panic a conversion raised (resumed once the call is back in Rust).
#[derive(Default)]
struct PlpgsqlState {
    functions: Vec<plpgsql::Function>,
    panic: Option<Box<dyn std::any::Any + Send>>,
}

/// Called by csrc/plpgsql/parse_plpgsql.c with each compiled function,
/// before it is freed.
#[unsafe(no_mangle)]
extern "C" fn typedpg_plpgsql_convert(state: *mut c_void, func: *const bindings::PLpgSQL_function) {
    // SAFETY: `state` is the `PlpgsqlState` plpgsql_result passed to
    // shim_plpgsql, which is still running.
    let state = unsafe { &mut *(state as *mut PlpgsqlState) };
    if state.panic.is_some() {
        return;
    }
    // A panic must not unwind into C: keep it for plpgsql_result.
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        plpgsql::convert::function(func)
    })) {
        Ok(function) => state.functions.push(function),
        Err(payload) => state.panic = Some(payload),
    }
}

/// One `shim_plpgsql` call: compile and convert, or take the error.
fn plpgsql_result(
    input: &CString,
    catalog: *const CatalogFns,
    sql: &str,
) -> Result<Vec<plpgsql::Function>> {
    let mut state = PlpgsqlState::default();
    let result = unsafe {
        shim_plpgsql(
            input.as_ptr(),
            catalog,
            &mut state as *mut PlpgsqlState as *mut c_void,
        )
    };
    let compiled = if result.error.is_null() {
        Ok(std::mem::take(&mut state.functions))
    } else {
        Err(unsafe { parse_error(result.error, sql) })
    };
    unsafe { shim_plpgsql_free(result) };
    if let Some(payload) = state.panic {
        std::panic::resume_unwind(payload);
    }
    compiled
}

/// Every node reachable from `roots`, breadth-first, each with its depth
/// (the roots at 0).
fn walk(roots: Vec<NodeRef<'_>>) -> Vec<(NodeRef<'_>, usize)> {
    let mut queue: VecDeque<(NodeRef<'_>, usize)> = roots.into_iter().map(|n| (n, 0)).collect();
    let mut out = Vec::new();
    while let Some((n, depth)) = queue.pop_front() {
        out.push((n, depth));
        node::children(n, &mut |child| queue.push_back((child, depth + 1)));
    }
    out
}

/// [`walk`] over raw pointers.
///
/// # Safety
/// See [`ast::ParseResult::nodes_mut`].
unsafe fn walk_mut(roots: Vec<NodeMut>) -> Vec<(NodeMut, usize)> {
    let mut queue: VecDeque<(NodeMut, usize)> = roots.into_iter().map(|n| (n, 0)).collect();
    let mut out = Vec::new();
    while let Some((n, depth)) = queue.pop_front() {
        out.push((n, depth));
        unsafe { node::children_mut(n, &mut |child| queue.push_back((child, depth + 1))) };
    }
    out
}

impl ast::ParseResult {
    /// Every node of every statement, breadth-first, with its depth.
    pub fn nodes(&self) -> Vec<(NodeRef<'_>, usize)> {
        walk(
            self.stmts
                .iter()
                .filter_map(|s| s.stmt.as_ref())
                .map(ast::Node::to_ref)
                .collect(),
        )
    }

    /// [`Self::nodes`] as raw mutable pointers.
    ///
    /// # Safety
    /// The pointers are valid only while the tree is neither moved nor
    /// dropped, and a pointer into a subtree dangles once a mutation through
    /// another pointer replaces that subtree. The caller must not create
    /// overlapping references through them.
    pub unsafe fn nodes_mut(&mut self) -> Vec<(NodeMut, usize)> {
        let roots = self
            .stmts
            .iter_mut()
            .filter_map(|s| s.stmt.as_mut())
            .map(ast::Node::to_mut)
            .collect();
        unsafe { walk_mut(roots) }
    }
}

impl<'a> NodeRef<'a> {
    /// The node's direct children, in field order — the building block for
    /// walkers that, like PostgreSQL's `raw_expression_tree_walker`
    /// callers, need to decide per node whether and how to descend.
    pub fn children(self) -> Vec<NodeRef<'a>> {
        let mut out = Vec::new();
        node::children(self, &mut |c| out.push(c));
        out
    }
}

impl ast::Node {
    /// This node and every node below it, breadth-first, with its depth
    /// (this node at 0).
    pub fn nodes(&self) -> Vec<(NodeRef<'_>, usize)> {
        walk(vec![self.to_ref()])
    }

    /// Rewrite this tree bottom-up: `f` sees each node once the nodes below
    /// it are rewritten, and may replace it with a node of any kind. The
    /// subtree below a node `descend` refuses is left as it is.
    pub fn rewrite(
        &mut self,
        descend: &mut dyn FnMut(&ast::Node) -> bool,
        f: &mut dyn FnMut(&mut ast::Node),
    ) {
        if descend(self) {
            // SAFETY: `self` is borrowed mutably for the whole walk, and the
            // slots handed out are disjoint fields below it.
            unsafe { node::child_slots_mut(self.to_mut(), &mut |slot| slot.rewrite(descend, f)) };
        }
        f(self);
    }
}
