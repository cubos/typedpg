//! Error types for the SQL analyzer.
//!
//! Variant names mirror PostgreSQL error categories (SQLSTATE class 42):
//! `UndefinedTable` = 42P01, `UndefinedColumn` = 42703, `UndefinedFunction` /
//! `UndefinedOperator` = 42883, and so on. [`AnalyzeError::sqlstate`] is the
//! canonical variant → code mapping; the `pg_sanity` oracle cross-checks it
//! against the code the live server attaches, which keeps the taxonomy
//! honest about what each variant actually represents.

use thiserror::Error;

use crate::lexer::LexError;

/// Errors that can occur during static SQL analysis.
#[derive(Debug, Error)]
pub enum AnalyzeError {
    /// The SQL could not be lexed (unclosed string, comment, etc.). The
    /// payload already includes the PG-verbatim wording (`unclosed string
    /// literal at byte N` etc.).
    #[error("{0}")]
    Lex(String),

    /// The SQL could not be parsed.
    #[error("{0}")]
    Parse(String),

    /// A table or view referenced in the query was not found in the schema
    /// snapshot. Equivalent to PG `undefined_table` (SQLSTATE 42P01).
    #[error("{0}")]
    UndefinedTable(String),

    /// A column referenced in the query was not found in scope. Equivalent to
    /// PG `undefined_column` (SQLSTATE 42703).
    #[error("{0}")]
    UndefinedColumn(String),

    /// Several columns in scope match an unqualified reference. Equivalent
    /// to PG `ambiguous_column` (SQLSTATE 42702).
    #[error("{0}")]
    AmbiguousColumn(String),

    /// Several function or operator overloads survived every resolution
    /// tiebreak (`operator is not unique`, `function … is not unique`).
    /// Equivalent to PG `ambiguous_function` (SQLSTATE 42725).
    #[error("{0}")]
    AmbiguousFunction(String),

    /// A schema object other than a relation/column/function/type is
    /// missing — a named window, the array type of a type, etc. Equivalent
    /// to PG `undefined_object` (SQLSTATE 42704).
    #[error("{0}")]
    UndefinedObject(String),

    /// A schema qualifier names no existing schema (`nosuchschema.f(1)`).
    /// Equivalent to PG `invalid_schema_name` (SQLSTATE 3F000).
    #[error("{0}")]
    UndefinedSchema(String),

    /// A construct PostgreSQL parses but deliberately doesn't implement
    /// (`DISTINCT is not implemented for window functions`, named
    /// arguments to an aggregate). Equivalent to PG `feature_not_supported`
    /// (SQLSTATE 0A000) — unlike [`Self::Unsupported`], which is the
    /// *analyzer* lacking support.
    #[error("{0}")]
    FeatureNotSupported(String),

    /// An object of the wrong kind was used: a procedure called in an
    /// expression, `OVER` on a plain function, a window function without
    /// `OVER`. Equivalent to PG `wrong_object_type` (SQLSTATE 42809).
    #[error("{0}")]
    WrongObjectType(String),

    /// A GROUP BY / ORDER BY ordinal out of range, the SELECT DISTINCT
    /// ORDER BY rule, or a FROM column-alias list longer than the relation.
    /// Equivalent to PG `invalid_column_reference` (SQLSTATE 42P10).
    #[error("{0}")]
    InvalidColumnReference(String),

    /// A FROM-clause alias used more than once. Equivalent to PG
    /// `duplicate_alias` (SQLSTATE 42712).
    #[error("{0}")]
    DuplicateAlias(String),

    /// Construct-level type reconciliation failed: the `… types X and Y
    /// cannot be matched` family (CASE/COALESCE/UNION/ARRAY/JOIN USING),
    /// recursive-CTE column types, clause arguments of the wrong type
    /// (`argument of WHERE must be type boolean`). Equivalent to PG
    /// `datatype_mismatch` (SQLSTATE 42804).
    #[error("{0}")]
    DatatypeMismatch(String),

    /// Aggregate placement / grouping rules: aggregates not allowed in a
    /// clause, ungrouped column references, nested aggregates. Equivalent
    /// to PG `grouping_error` (SQLSTATE 42803).
    #[error("{0}")]
    GroupingError(String),

    /// Window-function placement rules (`window functions are not allowed
    /// in WHERE`). Equivalent to PG `windowing_error` (SQLSTATE 42P20).
    #[error("{0}")]
    WindowingError(String),

    /// Semantic-analysis errors PostgreSQL classifies as `syntax_error`
    /// (SQLSTATE 42601) even though they aren't grammar failures: VALUES
    /// list arity, set-operation column counts.
    #[error("{0}")]
    SyntaxError(String),

    /// A column name defined twice where names must be unique (a column
    /// definition list, an INSERT target list). Equivalent to PG
    /// `duplicate_column` (SQLSTATE 42701).
    #[error("{0}")]
    DuplicateColumn(String),

    /// A recursive CTE that violates PG's structural rules for recursion
    /// (`recursive reference to query "r" must not appear within an outer
    /// join`, …). Equivalent to PG `invalid_recursion` (SQLSTATE 42P19).
    #[error("{0}")]
    InvalidRecursion(String),

    /// A type referenced in the query is not in the catalog. Equivalent to
    /// PG `undefined_object` (SQLSTATE 42704) when the lookup was by name,
    /// or surfaces an internal OID mismatch when the lookup was by OID.
    /// The payload carries the PG-verbatim wording (`type "x" does not
    /// exist`) plus any extra context appended by the caller.
    #[error("{0}")]
    UndefinedType(String),

    /// A function does not exist for the given argument types. Equivalent to
    /// PG `undefined_function` (SQLSTATE 42883). In PG the same SQLSTATE covers
    /// missing operators; here we keep operators in their own variant for
    /// clarity.
    #[error("{0}")]
    UndefinedFunction(String),

    /// An operator does not exist for the given operand types. Shares PG
    /// SQLSTATE 42883 with `UndefinedFunction`.
    #[error("{0}")]
    UndefinedOperator(String),

    /// The type of an expression could not be determined — a bare parameter
    /// no use of the query could type (`SELECT $1 IS NULL`, `ROW($1)`,
    /// `concat(name, $1)`). Equivalent to PG `indeterminate_datatype`
    /// (SQLSTATE 42P18). PG reports the *same wording* with a different
    /// code when the parameter's uses deduced conflicting types — see
    /// [`Self::AmbiguousParameter`]; both verified on PG 18.
    #[error("{0}")]
    IndeterminateType(String),

    /// A parameter with one use that locks it as untypable *and* another
    /// that deduces a concrete type — the deductions conflict
    /// (`SELECT $1 IS NULL, $1 = 1`). Same `could not determine data type
    /// of parameter $N` wording as [`Self::IndeterminateType`], but PG
    /// attaches `ambiguous_parameter` (SQLSTATE 42P08) on this path.
    #[error("{0}")]
    AmbiguousParameter(String),

    /// A type mismatch: an expression's type cannot be coerced to the expected
    /// type. Equivalent to PG `datatype_mismatch` (SQLSTATE 42804) or
    /// `cannot_coerce` (42846) depending on context.
    ///
    /// `context` carries the PG-verbatim message (and, after rendering, the
    /// full multi-line diagnostic); `actual`/`expected` are kept around so
    /// the proc macro can still introspect what was being coerced.
    #[error("{context}")]
    TypeMismatch {
        actual: String,
        expected: String,
        context: String,
    },

    /// The analyzer encountered an AST node or SQL feature it does not yet support.
    #[error("{0}")]
    Unsupported(String),

    /// The query violates PostgreSQL's placement rules for a construct
    /// (aggregate in WHERE, window function in WHERE, nested aggregates,
    /// INSERT/SELECT arity mismatch, etc.). Maps to a mix of PG SQLSTATEs —
    /// primarily `grouping_error` (42803) and `syntax_error` (42601) — that we
    /// don't yet split further.
    ///
    /// Display emits the payload verbatim (no `"invalid SQL: "` prefix) so
    /// the `pglite_sanity` mirror can match it against PG's wording.
    #[error("{0}")]
    Invalid(String),

    /// An untyped string literal's *content* is not valid input for the
    /// concrete type the context coerces it to (`'x'::int`,
    /// `WHERE int_col = 'x'`, …). PG runs the type's input function on such
    /// constants at parse-analysis time; this mirrors `invalid_text_
    /// representation` (22P02) and friends with PG's verbatim wording.
    ///
    /// Kept as its own variant (rather than folded into [`Self::Invalid`])
    /// because inference sites that *speculatively* re-walk an expression
    /// under a type goal — operator/CASE/COALESCE back-fills, whose other
    /// failures are deliberately swallowed — must still propagate this one.
    #[error("{0}")]
    InvalidLiteral(String),

    /// The parser reported a JOIN kind the analyzer does not recognize.
    /// Returned instead of silently falling back to INNER JOIN semantics,
    /// which would produce incorrect nullability.
    #[error("unsupported join type: {0}")]
    UnsupportedJoinType(i32),

    /// An analyzer invariant was violated — typically because a placeholder
    /// survived lexing but was not walked during type inference (e.g. it sat
    /// inside an AST node the analyzer does not yet traverse). Surfaced as an
    /// error instead of a panic so callers can report the offending SQL
    /// without crashing the macro host process.
    #[error("internal analyzer error: {0}")]
    Internal(String),

    /// JSON serialization/deserialization error.
    #[error("serde error: {0}")]
    Serde(#[from] serde_json::Error),

    /// IO error (reading/writing snapshot files).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl AnalyzeError {
    /// The SQLSTATE PostgreSQL attaches to the error class this variant
    /// represents — a pure variant → code mapping, with no inspection of
    /// the message payload.
    ///
    /// Returns `None` for variants that span several PG codes
    /// ([`Self::Invalid`], [`Self::InvalidLiteral`] — 22P02/22003/0A000
    /// depending on the input function, [`Self::TypeMismatch`] —
    /// 42804/42846 depending on context) and for purely internal failures.
    /// The `pg_sanity` oracle compares this against the live server's
    /// `DbError::code()` whenever it is `Some`.
    ///
    /// Only that oracle and the `pgmsg` unit tests consult it, so a plain
    /// build legitimately has no caller.
    #[cfg_attr(not(any(test, feature = "pg_sanity")), allow(dead_code))]
    pub(crate) fn sqlstate(&self) -> Option<&'static str> {
        use AnalyzeError::*;
        match self {
            Lex(_) | Parse(_) | SyntaxError(_) => Some("42601"),
            UndefinedTable(_) => Some("42P01"),
            UndefinedColumn(_) => Some("42703"),
            AmbiguousColumn(_) => Some("42702"),
            UndefinedType(_) | UndefinedObject(_) => Some("42704"),
            UndefinedSchema(_) => Some("3F000"),
            FeatureNotSupported(_) => Some("0A000"),
            DuplicateColumn(_) => Some("42701"),
            InvalidRecursion(_) => Some("42P19"),
            UndefinedFunction(_) | UndefinedOperator(_) => Some("42883"),
            AmbiguousFunction(_) => Some("42725"),
            WrongObjectType(_) => Some("42809"),
            IndeterminateType(_) => Some("42P18"),
            AmbiguousParameter(_) => Some("42P08"),
            InvalidColumnReference(_) => Some("42P10"),
            DuplicateAlias(_) => Some("42712"),
            DatatypeMismatch(_) => Some("42804"),
            GroupingError(_) => Some("42803"),
            WindowingError(_) => Some("42P20"),
            TypeMismatch { .. }
            | Invalid(_)
            | InvalidLiteral(_)
            | Unsupported(_)
            | UnsupportedJoinType(_)
            | Internal(_)
            | Serde(_)
            | Io(_) => None,
        }
    }
}

impl From<LexError> for AnalyzeError {
    fn from(err: LexError) -> Self {
        AnalyzeError::Lex(err.to_string())
    }
}

// ─── Internal error type with span information ─────────────────────────────
//
// `AnalyzeError` (public) carries the final rendered string per variant —
// what `Display` produces and what users (and `pg_sanity`) see. During
// analysis we use `RawError` instead, which keeps the structured location
// info (`primary_span`, `labels`, `hint`) alongside the original message.
//
// The public boundary in `PgCatalog::analyze` is the single place that
// resolves a `RawError` into a fully-rendered `AnalyzeError`, given the
// original SQL and the lexer's offset map.

/// A byte range in the **post-lex** SQL. Translated to the original SQL via
/// [`crate::param::LexOutput::original_span`] when rendering diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SourceSpan {
    pub start: usize,
    pub end: usize,
}

/// Bundle of references threaded through analyzer functions on the migrated
/// hot path so that `RawError`s built mid-analysis can be rendered into the
/// final multi-line `AnalyzeError` without reaching back up to the barrier.
///
/// `sql_original` is the SQL exactly as the user wrote it (what shows up in
/// the snippet); `lex_output` maps post-lex byte offsets (what
/// `pg_query`/AST `location` fields refer to) back to the original.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DiagContext<'a> {
    pub sql_original: &'a str,
    pub lex_output: &'a crate::param::LexOutput,
}

impl<'a> DiagContext<'a> {
    /// Render the given raw error into an `AnalyzeError` using this context.
    fn render(&self, raw: RawError) -> AnalyzeError {
        raw.into_analyze(self.sql_original, self.lex_output)
    }
}

// ─── Thread-local diagnostic context ───────────────────────────────────────
//
// The public boundary in `PgCatalog::analyze` installs a `DiagContext` via
// `DiagContextGuard::install` before running the static analyzer. Sites
// deep in the analyzer that want to render a rich diagnostic call
// `RawError::finalize_implicit()`, which reads the context from TLS.
//
// The guard is RAII: dropping it clears the slot. Reentrant installs stack
// (saved/restored on drop). Threading explicit context through the dozens
// of analyzer functions would touch far more code for the same effect;
// pinning the lifetimes via raw pointers + a guard keeps the API surface
// small and the cost paid only at the boundary.

use std::cell::RefCell;

struct DiagSlot {
    sql_ptr: *const str,
    lex_ptr: *const crate::param::LexOutput,
}

thread_local! {
    static DIAG_TLS: RefCell<Vec<DiagSlot>> = const { RefCell::new(Vec::new()) };
}

/// RAII guard that installs a [`DiagContext`] into thread-local storage and
/// pops it on drop. The guard borrows `sql` and `lex_output` for its
/// lifetime; the analyzer's `?` chains must complete before the guard is
/// dropped (which they do, because `analyze_static` runs synchronously).
pub(crate) struct DiagContextGuard<'a> {
    _marker: std::marker::PhantomData<(&'a str, &'a crate::param::LexOutput)>,
}

impl<'a> DiagContextGuard<'a> {
    pub(crate) fn install(sql: &'a str, lex_output: &'a crate::param::LexOutput) -> Self {
        DIAG_TLS.with(|tls| {
            tls.borrow_mut().push(DiagSlot {
                sql_ptr: sql as *const str,
                lex_ptr: lex_output as *const crate::param::LexOutput,
            });
        });
        Self {
            _marker: std::marker::PhantomData,
        }
    }
}

impl Drop for DiagContextGuard<'_> {
    fn drop(&mut self) {
        DIAG_TLS.with(|tls| {
            tls.borrow_mut().pop();
        });
    }
}

/// Run `f` with the topmost `DiagContext` from TLS, or `None` if none is
/// installed. The borrow guarantees the pointers in TLS are still valid.
fn with_diag_ctx<R>(f: impl FnOnce(Option<DiagContext>) -> R) -> R {
    DIAG_TLS.with(|tls| {
        let slots = tls.borrow();
        match slots.last() {
            Some(slot) => {
                // SAFETY: the pointers are valid for the lifetime of the
                // `DiagContextGuard` that installed them; the guard is on a
                // stack frame strictly outer to this call (the analyzer is
                // synchronous), so the borrow here is sound.
                let ctx = unsafe {
                    DiagContext {
                        sql_original: &*slot.sql_ptr,
                        lex_output: &*slot.lex_ptr,
                    }
                };
                f(Some(ctx))
            }
            None => f(None),
        }
    })
}

impl SourceSpan {
    pub(crate) fn new(start: usize, end: usize) -> Self {
        debug_assert!(end >= start, "SourceSpan: end < start");
        Self { start, end }
    }

    /// Build a span starting at `start` and covering a qualified name
    /// (`schema.relation`, with optional double-quoted identifiers).
    ///
    /// `sql` is the post-lex SQL (the same coordinate space `RangeVar.location`
    /// uses). Returns `None` if `start` is past end-of-input or doesn't begin
    /// with an identifier character.
    fn at_qualified_name(sql: &str, start: usize) -> Option<Self> {
        let end = scan_qualified_name(sql.as_bytes(), start)?;
        Some(Self::new(start, end))
    }

    /// Build a span at `start` covering a single SQL identifier (no schema
    /// qualifier — unlike [`Self::at_qualified_name`]). Used when the AST
    /// node's `location` points at a bare token.
    #[allow(dead_code)] // infra reserved for variants not yet migrated
    pub(crate) fn at_token(sql: &str, start: usize) -> Option<Self> {
        let end = scan_identifier(sql.as_bytes(), start)?;
        Some(Self::new(start, end))
    }

    /// Build a span of exactly `len` bytes starting at `start`. Useful when
    /// the offending token has a known length (e.g. an operator symbol).
    pub(crate) fn at_length(start: usize, len: usize) -> Self {
        Self::new(start, start + len)
    }

    /// Build a span at the offset of a 1-character anchor — used as a
    /// last-resort fallback when the analyzer only has a `location` and
    /// can't determine the real token boundary.
    pub(crate) fn one_char_at(start: usize) -> Self {
        Self::new(start, start + 1)
    }

    /// Convert a `pg_query` AST `location` (i32; -1 means absent) into a
    /// caret-only span. Returns `None` when the location is unset.
    pub(crate) fn from_location(location: i32) -> Option<Self> {
        if location < 0 {
            None
        } else {
            Some(Self::one_char_at(location as usize))
        }
    }

    /// Build a span at `location` covering the qualified identifier that
    /// starts there, by reading the post-lex SQL from the thread-local
    /// `DiagContext`. Returns `None` if the location is unset or no
    /// diagnostic context is installed.
    ///
    /// Used as the canonical way to turn an AST node's `location` into a
    /// caret span of the right width — every site that builds an error
    /// from an AST node should funnel through here so callers stay
    /// short and the spans stay consistent.
    pub(crate) fn from_node_qname(location: i32) -> Option<Self> {
        if location < 0 {
            return None;
        }
        with_diag_ctx(|ctx| {
            ctx.and_then(|c| Self::at_qualified_name(&c.lex_output.sql, location as usize))
        })
    }

    /// Like [`Self::from_node_qname`], but accepts numeric literals and
    /// quoted strings in addition to identifiers. Used for expression-level
    /// markers (TypeMismatch, IndeterminateType, …) where the AST node may
    /// be an `AConst` (`42`, `'hi'`, `true`), not necessarily an identifier.
    pub(crate) fn from_node_token(location: i32) -> Option<Self> {
        if location < 0 {
            return None;
        }
        with_diag_ctx(|ctx| {
            ctx.and_then(|c| {
                let bytes = c.lex_output.sql.as_bytes();
                let start = location as usize;
                let end = scan_value_token(bytes, start)?;
                Some(Self::new(start, end))
            })
        })
    }
}

/// Extract the `location` byte offset (post-lex SQL) from any AST node
/// variant that carries one. Returns `None` for nodes without location
/// info (e.g. `BoolExpr` — `pg_query` doesn't track its position).
pub(crate) fn node_location(node: &pg_query::protobuf::Node) -> Option<i32> {
    use pg_query::protobuf::node::Node;
    let inner = node.node.as_ref()?;
    let loc = match inner {
        Node::ColumnRef(n) => n.location,
        Node::AConst(n) => n.location,
        Node::AExpr(n) => n.location,
        Node::FuncCall(n) => n.location,
        Node::TypeCast(n) => n.location,
        Node::ParamRef(n) => n.location,
        Node::AArrayExpr(n) => n.location,
        Node::ResTarget(n) => n.location,
        Node::RangeVar(n) => n.location,
        Node::CaseExpr(n) => n.location,
        Node::SubLink(n) => n.location,
        Node::TypeName(n) => n.location,
        Node::NamedArgExpr(n) => n.location,
        _ => return None,
    };
    if loc < 0 { None } else { Some(loc) }
}

/// Scan whatever token starts at `start` — qualified identifier, numeric
/// literal, single-quoted string, or boolean/keyword identifier. Returns
/// the exclusive end byte offset, or `None` if `start` doesn't begin a
/// recognizable token.
///
/// Used by [`SourceSpan::from_node_token`] to size carets on expression
/// nodes where the underlying token type isn't known statically (e.g.
/// `AConst`, which carries `42`, `'hi'`, or `true` indistinguishably from
/// the analyzer's point of view).
fn scan_value_token(bytes: &[u8], start: usize) -> Option<usize> {
    if start >= bytes.len() {
        return None;
    }
    let b = bytes[start];
    // Single-quoted string literal, with `''` escape.
    if b == b'\'' {
        let mut i = start + 1;
        while i < bytes.len() {
            if bytes[i] == b'\'' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    i += 2;
                } else {
                    return Some(i + 1);
                }
            } else {
                i += 1;
            }
        }
        // Unterminated — fall back to end-of-input.
        return Some(bytes.len());
    }
    // Numeric literal — optionally signed, decimal point, scientific notation.
    let is_sign_with_digit =
        (b == b'-' || b == b'+') && start + 1 < bytes.len() && bytes[start + 1].is_ascii_digit();
    if b.is_ascii_digit() || is_sign_with_digit {
        let mut i = start + 1;
        while i < bytes.len() {
            let c = bytes[i];
            if c.is_ascii_digit() || c == b'.' || c == b'_' {
                i += 1;
            } else if c == b'e' || c == b'E' {
                i += 1;
                // Optional sign after exponent.
                if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
                    i += 1;
                }
            } else {
                break;
            }
        }
        return Some(i);
    }
    // Identifier (possibly qualified).
    scan_qualified_name(bytes, start)
}

/// Scan a (possibly schema-qualified) identifier starting at `start`.
/// Returns the exclusive end byte offset.
fn scan_qualified_name(bytes: &[u8], start: usize) -> Option<usize> {
    let after_first = scan_identifier(bytes, start)?;
    // Optional `.<identifier>` after the first identifier.
    if after_first < bytes.len()
        && bytes[after_first] == b'.'
        && let Some(after_second) = scan_identifier(bytes, after_first + 1)
    {
        return Some(after_second);
    }
    Some(after_first)
}

/// Scan a single SQL identifier (quoted or unquoted) starting at `start`.
fn scan_identifier(bytes: &[u8], start: usize) -> Option<usize> {
    if start >= bytes.len() {
        return None;
    }
    if bytes[start] == b'"' {
        // Quoted: read until matching `"`, accepting `""` as escape.
        let mut i = start + 1;
        while i < bytes.len() {
            if bytes[i] == b'"' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                    i += 2;
                } else {
                    return Some(i + 1);
                }
            } else {
                i += 1;
            }
        }
        // Unterminated quoted identifier — fall back to whole tail.
        Some(bytes.len())
    } else if is_ident_start(bytes[start]) {
        let mut i = start + 1;
        while i < bytes.len() && is_ident_cont(bytes[i]) {
            i += 1;
        }
        Some(i)
    } else {
        None
    }
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_cont(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// A span attached to an error, with an optional short label rendered next
/// to its caret. Used both for the primary location of an error and for
/// secondary spans that point at related sites (e.g. the two sides of a
/// type mismatch).
///
/// The `message` may be empty: a caret with no label still gives the
/// renderer enough info to underline the token.
#[derive(Debug, Clone)]
pub(crate) struct DiagnosticLabel {
    pub span: SourceSpan,
    pub message: String,
}

impl DiagnosticLabel {
    pub(crate) fn new(span: SourceSpan, message: impl Into<String>) -> Self {
        Self {
            span,
            message: message.into(),
        }
    }
}

/// Internal counterpart of [`AnalyzeError`] carrying structured location info.
///
/// Funções privadas do analyzer retornam `Result<_, RawError>` (no caminho
/// migrado). Cada site constrói o `kind` (com a mensagem PG-verbatim na
/// string da variante), opcionalmente anexa um `primary` apontando a
/// localização do erro, `secondaries` para contexto adicional, e `hint`.
/// A barreira pública chama [`RawError::into_analyze`] para renderizar a
/// string final.
#[derive(Debug)]
pub(crate) struct RawError {
    pub kind: AnalyzeError,
    /// The primary location of the error. Its `message` is the short label
    /// drawn beneath the caret (e.g. `relation does not exist`); may be empty.
    pub primary: Option<DiagnosticLabel>,
    /// Additional locations relevant to the diagnostic (e.g. the other side
    /// of a type mismatch).
    pub secondaries: Vec<DiagnosticLabel>,
    /// One-line hint rendered as `= help: ...`.
    pub hint: Option<String>,
}

impl RawError {
    /// Build a `RawError` from any existing `AnalyzeError`-convertible source,
    /// with no span information attached. Used by `?` for `LexError`,
    /// `std::io::Error`, `serde_json::Error`, and direct constructions of
    /// variants we have not migrated yet.
    fn passthrough(e: AnalyzeError) -> Self {
        Self {
            kind: e,
            primary: None,
            secondaries: Vec::new(),
            hint: None,
        }
    }

    /// Build a raw error around an already-constructed [`AnalyzeError`] —
    /// used by the `pgmsg` constructors, which pick the variant carrying
    /// the right SQLSTATE for each wording. The caret label starts empty;
    /// chain [`Self::with_primary_label`] to set one.
    pub(crate) fn new(kind: AnalyzeError, span: Option<SourceSpan>, hint: Option<String>) -> Self {
        let primary = span.map(|s| DiagnosticLabel::new(s, ""));
        Self {
            kind,
            primary,
            secondaries: Vec::new(),
            hint,
        }
    }

    /// Construct an `UndefinedTable` raw error with optional span and hint.
    /// `qualified` is the relation name as the user wrote it
    /// (`schema.name` when qualified, bare `name` otherwise) — PG's error
    /// message keeps the schema prefix, so we must too for the prefix check.
    pub(crate) fn undefined_table(
        qualified: &str,
        span: Option<SourceSpan>,
        hint: Option<String>,
    ) -> Self {
        let primary = span.map(|s| DiagnosticLabel::new(s, "relation does not exist"));
        Self {
            kind: AnalyzeError::UndefinedTable(format!("relation \"{qualified}\" does not exist")),
            primary,
            secondaries: Vec::new(),
            hint,
        }
    }

    // ── Constructors for the other AnalyzeError variants ───────────────────
    //
    // All follow the same pattern: the PG-verbatim message (which the
    // `pg_sanity` prefix check sees) lives in the variant's payload string,
    // and `primary` carries the caret label rendered beneath the snippet.
    // Hints are optional and free-form, rendered as `= help: ...` below the
    // snippet.

    /// Build an `UndefinedColumn` raw error. `message` is the PG-verbatim
    /// first line (e.g. `column "x" does not exist` or
    /// `column "x" of relation "t" does not exist`).
    pub(crate) fn undefined_column(
        message: String,
        span: Option<SourceSpan>,
        hint: Option<String>,
    ) -> Self {
        let primary = span.map(|s| DiagnosticLabel::new(s, "column does not exist"));
        Self {
            kind: AnalyzeError::UndefinedColumn(message),
            primary,
            secondaries: Vec::new(),
            hint,
        }
    }

    /// Build an `UndefinedFunction` raw error. `message` is the PG-verbatim
    /// first line (e.g. `function foo(text) does not exist`).
    pub(crate) fn undefined_function(
        message: String,
        span: Option<SourceSpan>,
        hint: Option<String>,
    ) -> Self {
        let primary = span.map(|s| DiagnosticLabel::new(s, "function does not exist"));
        Self {
            kind: AnalyzeError::UndefinedFunction(message),
            primary,
            secondaries: Vec::new(),
            hint,
        }
    }

    /// Build an `UndefinedOperator` raw error.
    pub(crate) fn undefined_operator(
        message: String,
        span: Option<SourceSpan>,
        hint: Option<String>,
    ) -> Self {
        let primary = span.map(|s| DiagnosticLabel::new(s, "operator does not exist"));
        Self {
            kind: AnalyzeError::UndefinedOperator(message),
            primary,
            secondaries: Vec::new(),
            hint,
        }
    }

    /// Build a `TypeMismatch` raw error. Two labels point at both sides
    /// of the mismatch: `primary` on the offending expression, `secondary`
    /// on whatever sets the expectation (column being assigned, expression
    /// on the other side of a comparison, etc.).
    #[allow(clippy::too_many_arguments)] // builder for a structured diagnostic
    pub(crate) fn type_mismatch(
        actual: String,
        expected: String,
        actual_pg: &str,
        expected_pg: &str,
        context: String,
        primary_span: Option<SourceSpan>,
        secondary: Option<DiagnosticLabel>,
        hint: Option<String>,
    ) -> Self {
        let primary = primary_span
            .map(|s| DiagnosticLabel::new(s, format!("expected {expected_pg}, found {actual_pg}")));
        let secondaries = secondary.into_iter().collect();
        Self {
            kind: AnalyzeError::TypeMismatch {
                actual,
                expected,
                context,
            },
            primary,
            secondaries,
            hint,
        }
    }

    /// Build an `Invalid` raw error (placement rules, arity mismatches,
    /// constraint references that don't exist, etc.).
    pub(crate) fn invalid(message: String, span: Option<SourceSpan>, hint: Option<String>) -> Self {
        let primary = span.map(|s| DiagnosticLabel::new(s, ""));
        Self {
            kind: AnalyzeError::Invalid(message),
            primary,
            secondaries: Vec::new(),
            hint,
        }
    }

    /// Build an `InvalidLiteral` raw error (a string literal whose content
    /// fails the target type's input function at parse time).
    pub(crate) fn invalid_literal(message: String, span: Option<SourceSpan>) -> Self {
        let primary = span.map(|s| DiagnosticLabel::new(s, "this literal"));
        Self {
            kind: AnalyzeError::InvalidLiteral(message),
            primary,
            secondaries: Vec::new(),
            hint: None,
        }
    }

    /// Build an `Unsupported` raw error.
    #[allow(dead_code)] // infra reserved for variants not yet migrated
    pub(crate) fn unsupported(
        message: String,
        span: Option<SourceSpan>,
        hint: Option<String>,
    ) -> Self {
        let primary = span.map(|s| DiagnosticLabel::new(s, "unsupported construct"));
        Self {
            kind: AnalyzeError::Unsupported(message),
            primary,
            secondaries: Vec::new(),
            hint,
        }
    }

    /// Build a `Lex` raw error. The lexer's own errors already carry a
    /// `position`; this wraps the text + position into a label.
    pub(crate) fn lex(message: String, span: Option<SourceSpan>) -> Self {
        let primary = span.map(|s| DiagnosticLabel::new(s, ""));
        Self {
            kind: AnalyzeError::Lex(message),
            primary,
            secondaries: Vec::new(),
            hint: None,
        }
    }

    /// Build a `Parse` raw error (from `pg_query`).
    #[allow(dead_code)] // infra reserved for variants not yet migrated
    pub(crate) fn parse(message: String, span: Option<SourceSpan>) -> Self {
        let primary = span.map(|s| DiagnosticLabel::new(s, ""));
        Self {
            kind: AnalyzeError::Parse(message),
            primary,
            secondaries: Vec::new(),
            hint: None,
        }
    }

    /// Set the text rendered next to the primary caret (`╰─ <message>`).
    /// No-op when the error has no primary span (e.g. no source location was
    /// available). Builder-style.
    pub(crate) fn with_primary_label(mut self, message: impl Into<String>) -> Self {
        if let Some(p) = self.primary.as_mut() {
            p.message = message.into();
        }
        self
    }

    /// Attach a secondary label to the error. Returns `self` builder-style.
    #[allow(dead_code)] // infra reserved for diagnostics that need multiple labels
    pub(crate) fn with_secondary(mut self, label: DiagnosticLabel) -> Self {
        self.secondaries.push(label);
        self
    }

    /// The PG-verbatim first line of the diagnostic. Always identical to what
    /// PostgreSQL would emit for the same error.
    pub(crate) fn pg_message(&self) -> String {
        self.kind.to_string()
    }

    /// Render using an optional context. When `ctx` is `None`, no snippet or
    /// hint is rendered and the variant is returned as-is.
    fn finalize(self, ctx: Option<DiagContext>) -> AnalyzeError {
        match ctx {
            Some(c) => c.render(self),
            None => self.kind,
        }
    }

    /// Render using the `DiagContext` installed in thread-local storage by
    /// the public boundary, or fall back to the raw variant when no context
    /// is installed (e.g. analysis triggered from DDL view handling).
    pub(crate) fn finalize_implicit(self) -> AnalyzeError {
        with_diag_ctx(|ctx| self.finalize(ctx))
    }

    /// Render the diagnostic and convert into the public [`AnalyzeError`].
    ///
    /// `sql_original` is the SQL exactly as the user wrote it; `lex_output`
    /// provides the post-lex → original offset translation.
    fn into_analyze(
        self,
        sql_original: &str,
        lex_output: &crate::param::LexOutput,
    ) -> AnalyzeError {
        let has_context =
            self.primary.is_some() || !self.secondaries.is_empty() || self.hint.is_some();

        if !has_context {
            return self.kind;
        }

        let pg_message = self.kind.to_string();
        let rendered = crate::diagnostic::render(&pg_message, &self, sql_original, lex_output);
        replace_message(self.kind, rendered)
    }
}

/// Replace the message inside a string-shaped variant with `rendered`,
/// preserving the variant. For struct-shaped variants (not yet migrated to
/// carry span info) the original is returned unchanged.
fn replace_message(e: AnalyzeError, rendered: String) -> AnalyzeError {
    match e {
        AnalyzeError::Lex(_) => AnalyzeError::Lex(rendered),
        AnalyzeError::Parse(_) => AnalyzeError::Parse(rendered),
        AnalyzeError::UndefinedTable(_) => AnalyzeError::UndefinedTable(rendered),
        AnalyzeError::UndefinedColumn(_) => AnalyzeError::UndefinedColumn(rendered),
        AnalyzeError::UndefinedFunction(_) => AnalyzeError::UndefinedFunction(rendered),
        AnalyzeError::UndefinedOperator(_) => AnalyzeError::UndefinedOperator(rendered),
        AnalyzeError::IndeterminateType(_) => AnalyzeError::IndeterminateType(rendered),
        AnalyzeError::AmbiguousParameter(_) => AnalyzeError::AmbiguousParameter(rendered),
        AnalyzeError::Unsupported(_) => AnalyzeError::Unsupported(rendered),
        AnalyzeError::Invalid(_) => AnalyzeError::Invalid(rendered),
        AnalyzeError::InvalidLiteral(_) => AnalyzeError::InvalidLiteral(rendered),
        AnalyzeError::UndefinedType(_) => AnalyzeError::UndefinedType(rendered),
        AnalyzeError::AmbiguousColumn(_) => AnalyzeError::AmbiguousColumn(rendered),
        AnalyzeError::AmbiguousFunction(_) => AnalyzeError::AmbiguousFunction(rendered),
        AnalyzeError::UndefinedObject(_) => AnalyzeError::UndefinedObject(rendered),
        AnalyzeError::WrongObjectType(_) => AnalyzeError::WrongObjectType(rendered),
        AnalyzeError::InvalidColumnReference(_) => AnalyzeError::InvalidColumnReference(rendered),
        AnalyzeError::DuplicateAlias(_) => AnalyzeError::DuplicateAlias(rendered),
        AnalyzeError::DatatypeMismatch(_) => AnalyzeError::DatatypeMismatch(rendered),
        AnalyzeError::GroupingError(_) => AnalyzeError::GroupingError(rendered),
        AnalyzeError::WindowingError(_) => AnalyzeError::WindowingError(rendered),
        AnalyzeError::SyntaxError(_) => AnalyzeError::SyntaxError(rendered),
        AnalyzeError::UndefinedSchema(_) => AnalyzeError::UndefinedSchema(rendered),
        AnalyzeError::FeatureNotSupported(_) => AnalyzeError::FeatureNotSupported(rendered),
        AnalyzeError::DuplicateColumn(_) => AnalyzeError::DuplicateColumn(rendered),
        AnalyzeError::InvalidRecursion(_) => AnalyzeError::InvalidRecursion(rendered),
        AnalyzeError::TypeMismatch {
            actual, expected, ..
        } => AnalyzeError::TypeMismatch {
            actual,
            expected,
            context: rendered,
        },
        other => other,
    }
}

impl<E> From<E> for RawError
where
    E: Into<AnalyzeError>,
{
    fn from(e: E) -> Self {
        Self::passthrough(e.into())
    }
}

impl std::fmt::Display for RawError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Internal Display — used by `format!("{e}")` in nested error
        // wrapping. Renders the PG-verbatim first line only; the multi-line
        // diagnostic is produced by `into_analyze` at the public boundary.
        write!(f, "{}", self.pg_message())
    }
}

impl std::error::Error for RawError {}
