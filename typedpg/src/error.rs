/// Errors returned by `typedpg` operations.
///
/// This enum covers all failure modes you may encounter when using the library:
/// database communication errors, migration problems, connection pool issues,
/// I/O failures when reading migration files, and empty query results.
///
/// All variants implement [`std::fmt::Display`] and [`std::error::Error`], so they
/// integrate naturally with `?` and error-reporting crates like `anyhow` or `eyre`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A PostgreSQL protocol or query execution error.
    ///
    /// Displayed with the server's message and SQLSTATE (plus its DETAIL and
    /// HINT), or for a client-side failure with the chain of causes —
    /// `tokio_postgres::Error`'s own `Display` says only "db error".
    #[error("database error: {}", DatabaseErrorDisplay(.0))]
    Database(#[from] tokio_postgres::Error),

    /// The executor can't do what was asked (e.g. `copy_in!` on an executor
    /// without a COPY implementation).
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// A migration-specific error.
    #[error("migration error: {0}")]
    Migration(String),

    /// An I/O error, typically from reading migration files from disk.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// Failed to acquire a connection from the connection pool.
    #[error("pool error: {0}")]
    Pool(String),

    /// A `fetch_one()` / `fetch_value()` call returned zero rows.
    ///
    /// Match it with `Error::NoRows { .. }`.
    #[error("query returned no rows: {query}")]
    #[non_exhaustive]
    NoRows {
        /// The query, and where it was written.
        query: QueryContext,
    },

    /// A `fetch_one()` / `fetch_optional()` / `fetch_value()` call returned
    /// more than one row.
    ///
    /// Match it with `Error::TooManyRows { .. }`.
    #[error("query returned more than one row: {query}")]
    #[non_exhaustive]
    TooManyRows {
        /// The query, and where it was written.
        query: QueryContext,
    },

    /// Failed to deserialize a domain/enum column value from a query result.
    #[error("deserialization error: {0}")]
    Deserialize(String),

    /// Failed to serialize a domain/enum value for a query parameter.
    #[error("serialization error: {0}")]
    Serialize(String),
}

impl Error {
    #[doc(hidden)]
    pub fn no_rows(query: QueryContext) -> Self {
        Error::NoRows { query }
    }

    #[doc(hidden)]
    pub fn too_many_rows(query: QueryContext) -> Self {
        Error::TooManyRows { query }
    }
}

/// Which query an error is about: its SQL, and the `sql!` invocation that
/// wrote it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryContext {
    sql: &'static str,
    file: &'static str,
    line: u32,
    column: u32,
}

impl QueryContext {
    #[doc(hidden)]
    pub const fn new(sql: &'static str, file: &'static str, line: u32, column: u32) -> Self {
        QueryContext {
            sql,
            file,
            line,
            column,
        }
    }

    /// The query's SQL, with its parameters numbered (`$1`, `$2`, …) and,
    /// for a `$..spread`, before the spread's rows are expanded.
    pub fn sql(&self) -> &'static str {
        self.sql
    }

    /// The source file of the `sql!` invocation.
    pub fn file(&self) -> &'static str {
        self.file
    }

    /// The line of the `sql!` invocation (1-based).
    pub fn line(&self) -> u32 {
        self.line
    }

    /// The column of the `sql!` invocation (1-based).
    pub fn column(&self) -> u32 {
        self.column
    }
}

impl std::fmt::Display for QueryContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "`{}` (sql! at {}:{}:{})",
            self.sql, self.file, self.line, self.column
        )
    }
}

#[cfg(feature = "deadpool")]
impl From<deadpool_postgres::PoolError> for Error {
    fn from(e: deadpool_postgres::PoolError) -> Self {
        Error::Pool(e.to_string())
    }
}

#[cfg(feature = "bb8")]
impl From<bb8::RunError<tokio_postgres::Error>> for Error {
    fn from(e: bb8::RunError<tokio_postgres::Error>) -> Self {
        Error::Pool(e.to_string())
    }
}

/// Renders a `tokio_postgres::Error` with what its own `Display` leaves to
/// `source()`: the server's message, or the underlying causes.
struct DatabaseErrorDisplay<'a>(&'a tokio_postgres::Error);

impl std::fmt::Display for DatabaseErrorDisplay<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(db) = self.0.as_db_error() {
            write!(f, "{} (SQLSTATE {})", db.message(), db.code().code())?;
            if let Some(detail) = db.detail() {
                write!(f, " DETAIL: {detail}")?;
            }
            if let Some(hint) = db.hint() {
                write!(f, " HINT: {hint}")?;
            }
            return Ok(());
        }
        write!(f, "{}", self.0)?;
        let mut source = std::error::Error::source(self.0);
        while let Some(cause) = source {
            write!(f, ": {cause}")?;
            source = cause.source();
        }
        Ok(())
    }
}
