//! Row streams: query results delivered as the server sends them, instead
//! of collected into a `Vec` first.
//!
//! [`RowStream`] yields raw [`Row`]s (what [`Executor::query_stream`]
//! returns); [`QueryStream<T>`] decodes each one into `T` — the type the
//! `sql!` macro's `fetch_stream` / `fetch_stream_as` return. Both are
//! `Send` and `Unpin`, so they can be stored and polled anywhere. Consume
//! them with [`StreamExt`] / [`TryStreamExt`]:
//!
//! ```rust,ignore
//! use typedpg::stream::TryStreamExt;
//!
//! let mut users = sql!(pool, "SELECT id, name FROM users").fetch_stream().await?;
//! while let Some(user) = users.try_next().await? {
//!     println!("{} {}", user.id, user.name);
//! }
//! ```
//!
//! [`Executor::query_stream`]: crate::Executor::query_stream

use std::pin::Pin;
use std::task::{Context, Poll};

pub use futures_core::Stream;
pub use futures_util::{StreamExt, TryStreamExt};
use tokio_postgres::Row;

/// The rows of a query, as the server sends them.
pub struct RowStream {
    // Field order matters: the stream is dropped before the connection it
    // reads from goes back to its pool.
    rows: Rows,
    _connection: Option<Box<dyn Send>>,
}

enum Rows {
    Live(Pin<Box<tokio_postgres::RowStream>>),
    Buffered(std::vec::IntoIter<Row>),
}

impl RowStream {
    /// A stream over a `tokio_postgres` row stream.
    pub fn new(rows: tokio_postgres::RowStream) -> Self {
        RowStream {
            rows: Rows::Live(Box::pin(rows)),
            _connection: None,
        }
    }

    /// A stream that also keeps `connection` (e.g. a pooled connection)
    /// alive until the stream is dropped.
    pub fn with_connection(
        rows: tokio_postgres::RowStream,
        connection: impl Send + 'static,
    ) -> Self {
        RowStream {
            rows: Rows::Live(Box::pin(rows)),
            _connection: Some(Box::new(connection)),
        }
    }

    /// A stream over rows already fetched — what an [`Executor`] without a
    /// streaming implementation returns.
    ///
    /// [`Executor`]: crate::Executor
    pub fn from_rows(rows: Vec<Row>) -> Self {
        RowStream {
            rows: Rows::Buffered(rows.into_iter()),
            _connection: None,
        }
    }

    /// A stream with no rows.
    pub fn empty() -> Self {
        Self::from_rows(Vec::new())
    }
}

impl Stream for RowStream {
    type Item = Result<Row, crate::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match &mut self.rows {
            Rows::Live(rows) => rows
                .as_mut()
                .poll_next(cx)
                .map(|next| next.map(|row| row.map_err(crate::Error::from))),
            Rows::Buffered(rows) => Poll::Ready(rows.next().map(Ok)),
        }
    }
}

/// The rows of a query decoded into `T`, as the server sends them.
pub struct QueryStream<T> {
    rows: RowStream,
    decode: fn(&Row) -> Result<T, crate::Error>,
}

impl<T> QueryStream<T> {
    /// Decode each row of `rows` with `decode`.
    pub fn new(rows: RowStream, decode: fn(&Row) -> Result<T, crate::Error>) -> Self {
        QueryStream { rows, decode }
    }
}

impl<T> Stream for QueryStream<T> {
    type Item = Result<T, crate::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let decode = self.decode;
        Pin::new(&mut self.rows)
            .poll_next(cx)
            .map(|next| next.map(|row| row.and_then(|row| decode(&row))))
    }
}
