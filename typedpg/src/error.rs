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
    #[error("query returned no rows")]
    NoRows,

    /// A `fetch_one()` / `fetch_value()` call returned more than one row.
    #[error("query returned more than one row")]
    TooManyRows,

    /// Failed to deserialize a domain/enum column value from a query result.
    #[error("deserialization error: {0}")]
    Deserialize(String),

    /// Failed to serialize a domain/enum value for a query parameter.
    #[error("serialization error: {0}")]
    Serialize(String),
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
