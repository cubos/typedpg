#[cfg(feature = "deadpool")]
use std::ops::Deref;

#[cfg(any(feature = "deadpool", feature = "bb8"))]
use tokio_postgres::Row;
#[cfg(any(feature = "deadpool", feature = "bb8"))]
use tokio_postgres::types::ToSql;

#[cfg(any(feature = "deadpool", feature = "bb8"))]
use crate::copy::{CopyRow, column_types, write_rows};
#[cfg(any(feature = "deadpool", feature = "bb8"))]
use crate::executor::{Executor, slice_iter};
#[cfg(any(feature = "deadpool", feature = "bb8"))]
use crate::stream::RowStream;

// ── deadpool-postgres ────────────────────────────────────────────────────────

/// [`Executor`] implementation for `deadpool_postgres::Pool`.
///
/// Each method call acquires a connection from the pool, executes the query,
/// and returns the connection to the pool when done. This is the most convenient
/// way to run queries -- just pass `&pool` to `sql!` and connection management
/// is handled automatically.
///
/// If the pool is exhausted (no available connections), methods return
/// [`Error::Pool`](crate::Error::Pool).
#[cfg(feature = "deadpool")]
impl Executor for deadpool_postgres::Pool {
    async fn query<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [&'a (dyn ToSql + Sync)],
    ) -> Result<Vec<Row>, crate::Error> {
        let client = self.get().await?;
        Ok(client.deref().query(sql, params).await?)
    }

    async fn execute<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [&'a (dyn ToSql + Sync)],
    ) -> Result<u64, crate::Error> {
        let client = self.get().await?;
        Ok(client.deref().execute(sql, params).await?)
    }

    async fn query_stream<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [&'a (dyn ToSql + Sync)],
    ) -> Result<RowStream, crate::Error> {
        // The stream keeps the connection until it is dropped.
        let client = self.get().await?;
        let rows = client.deref().query_raw(sql, slice_iter(params)).await?;
        Ok(RowStream::with_connection(rows, client))
    }

    async fn copy_in<'a, I>(
        &'a self,
        describe_sql: &'a str,
        copy_sql: &'a str,
        rows: I,
    ) -> Result<u64, crate::Error>
    where
        I: Iterator<Item = Result<CopyRow, crate::Error>> + Send + 'a,
    {
        let client = self.get().await?;
        let types = column_types(&client.deref().prepare(describe_sql).await?);
        let sink = client.deref().copy_in(copy_sql).await?;
        write_rows(sink, &types, rows).await
    }
}

/// [`Executor`] implementation for `deadpool_postgres::Object` (the pooled connection).
///
/// Delegates directly to the inner `tokio_postgres::Client` via `Deref`. Use this
/// when you need to hold a connection across multiple queries (e.g., to run them on
/// the same connection) without using a transaction.
#[cfg(feature = "deadpool")]
impl Executor for deadpool_postgres::Object {
    async fn query<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [&'a (dyn ToSql + Sync)],
    ) -> Result<Vec<Row>, crate::Error> {
        Ok(self.deref().query(sql, params).await?)
    }

    async fn execute<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [&'a (dyn ToSql + Sync)],
    ) -> Result<u64, crate::Error> {
        Ok(self.deref().execute(sql, params).await?)
    }

    async fn query_stream<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [&'a (dyn ToSql + Sync)],
    ) -> Result<RowStream, crate::Error> {
        let rows = self.deref().query_raw(sql, slice_iter(params)).await?;
        Ok(RowStream::new(rows))
    }

    async fn copy_in<'a, I>(
        &'a self,
        describe_sql: &'a str,
        copy_sql: &'a str,
        rows: I,
    ) -> Result<u64, crate::Error>
    where
        I: Iterator<Item = Result<CopyRow, crate::Error>> + Send + 'a,
    {
        let types = column_types(&self.deref().prepare(describe_sql).await?);
        let sink = self.deref().copy_in(copy_sql).await?;
        write_rows(sink, &types, rows).await
    }
}

/// [`Executor`] implementation for `deadpool_postgres::Transaction`.
///
/// Delegates to the inner `tokio_postgres::Transaction` via `Deref`. This allows
/// using `sql!` directly with a deadpool transaction:
///
/// ```rust,ignore
/// let mut client = pool.get().await?;
/// let tx = client.transaction().await?;
/// sql!(&tx, "UPDATE users SET name = $name WHERE id = $id", name = "foo", id = 1)
///     .execute().await?;
/// tx.commit().await?;
/// ```
#[cfg(feature = "deadpool")]
impl Executor for deadpool_postgres::Transaction<'_> {
    async fn query<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [&'a (dyn ToSql + Sync)],
    ) -> Result<Vec<Row>, crate::Error> {
        Ok(self.deref().query(sql, params).await?)
    }

    async fn execute<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [&'a (dyn ToSql + Sync)],
    ) -> Result<u64, crate::Error> {
        Ok(self.deref().execute(sql, params).await?)
    }

    async fn query_stream<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [&'a (dyn ToSql + Sync)],
    ) -> Result<RowStream, crate::Error> {
        let rows = self.deref().query_raw(sql, slice_iter(params)).await?;
        Ok(RowStream::new(rows))
    }

    async fn copy_in<'a, I>(
        &'a self,
        describe_sql: &'a str,
        copy_sql: &'a str,
        rows: I,
    ) -> Result<u64, crate::Error>
    where
        I: Iterator<Item = Result<CopyRow, crate::Error>> + Send + 'a,
    {
        let types = column_types(&self.deref().prepare(describe_sql).await?);
        let sink = self.deref().copy_in(copy_sql).await?;
        write_rows(sink, &types, rows).await
    }
}

// ── bb8-postgres ─────────────────────────────────────────────────────────────

/// [`Executor`] implementation for `bb8::Pool` with `bb8_postgres::PostgresConnectionManager`.
///
/// Works identically to the deadpool implementation: acquires a connection per
/// method call and returns it when done. Enable the `bb8` feature to use this.
#[cfg(feature = "bb8")]
impl<Tls> Executor for bb8::Pool<bb8_postgres::PostgresConnectionManager<Tls>>
where
    Tls:
        tokio_postgres::tls::MakeTlsConnect<tokio_postgres::Socket> + Clone + Send + Sync + 'static,
    <Tls as tokio_postgres::tls::MakeTlsConnect<tokio_postgres::Socket>>::Stream: Send + Sync,
    <Tls as tokio_postgres::tls::MakeTlsConnect<tokio_postgres::Socket>>::TlsConnect: Send,
    <<Tls as tokio_postgres::tls::MakeTlsConnect<tokio_postgres::Socket>>::TlsConnect as
        tokio_postgres::tls::TlsConnect<tokio_postgres::Socket>>::Future: Send,
{
    async fn query<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [&'a (dyn ToSql + Sync)],
    ) -> Result<Vec<Row>, crate::Error> {
        let client = self.get().await?;
        Ok(client.query(sql, params).await?)
    }

    async fn execute<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [&'a (dyn ToSql + Sync)],
    ) -> Result<u64, crate::Error> {
        let client = self.get().await?;
        Ok(client.execute(sql, params).await?)
    }

    async fn query_stream<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [&'a (dyn ToSql + Sync)],
    ) -> Result<RowStream, crate::Error> {
        // An owned connection, kept by the stream until it is dropped.
        let client = self.get_owned().await?;
        let rows = client.query_raw(sql, slice_iter(params)).await?;
        Ok(RowStream::with_connection(rows, client))
    }

    async fn copy_in<'a, I>(
        &'a self,
        describe_sql: &'a str,
        copy_sql: &'a str,
        rows: I,
    ) -> Result<u64, crate::Error>
    where
        I: Iterator<Item = Result<CopyRow, crate::Error>> + Send + 'a,
    {
        let client = self.get().await?;
        let types = column_types(&client.prepare(describe_sql).await?);
        let sink = client.copy_in(copy_sql).await?;
        write_rows(sink, &types, rows).await
    }
}
