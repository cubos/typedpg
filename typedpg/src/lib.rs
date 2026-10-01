//! Typed PostgreSQL access with compile-time query verification.
//!
//! `typedpg` checks your SQL queries against a real PostgreSQL schema at compile time,
//! generating type-safe Rust code with zero runtime overhead. Write plain SQL, get full
//! type safety.
//!
//! # Philosophy
//!
//! - **Postgres-only** -- no abstraction over multiple databases. Embraces PostgreSQL
//!   features like `JSONB`, advisory locks, and `CREATE DOMAIN`.
//! - **SQL-native** -- write real SQL, not a Rust DSL. The `sql!` macro takes a SQL
//!   string and verifies it at compile time.
//! - **Compile-time checked** -- the proc macro reads your migrations, reconstructs the
//!   PostgreSQL schema in memory, and statically type-checks every query against it.
//!   No external server, no live database connection — just the migrations on disk.
//! - **Human-friendly syntax** -- use `$name` for parameters instead of `$1`. Use
//!   `$..spread` for bulk inserts. Get named fields on output structs, not positional
//!   indices.
//!
//! # Quick start
//!
//! Add to your `Cargo.toml`:
//!
//! ```toml
//! [dependencies]
//! typedpg = "0.1"
//! deadpool-postgres = "0.14"
//! tokio-postgres = "0.7"
//! tokio = { version = "1", features = ["full"] }
//!
//! [package.metadata.typedpg.database]
//! migrations = "./migrations"
//! ```
//!
//! Add a `build.rs` next to your `Cargo.toml` so the `sql!` macro is
//! re-checked whenever a migration file changes (see the [`build`] module):
//!
//! ```ignore
//! // build.rs
//! fn main() {
//!     typedpg::build::track_migrations();
//! }
//! ```
//!
//! ```toml
//! [build-dependencies]
//! typedpg = "0.1"
//! ```
//!
//! Create `migrations/0001_create_users.sql`:
//!
//! ```sql
//! CREATE TABLE users (
//!     id    SERIAL PRIMARY KEY,
//!     name  TEXT NOT NULL,
//!     email TEXT NOT NULL UNIQUE
//! );
//! ```
//!
//! Use it:
//!
//! ```rust,ignore
//! use typedpg::sql;
//!
//! let users = sql!(pool, "SELECT id, name, email FROM users")
//!     .fetch_all()
//!     .await?;
//!
//! for user in &users {
//!     println!("{}: {} ({})", user.id, user.name, user.email);
//! }
//! ```
//!
//! That's it. The macro statically analyses every query against the schema built from
//! your migrations — purely at compile time, with no database connection required.
//!
//! # Configuration
//!
//! The only required configuration is the migrations path. All other settings
//! have sensible defaults:
//!
//! ```toml
//! [package.metadata.typedpg.database]
//! migrations = "./migrations"             # required
//! extra_migrations = ["../shared/migrations"]  # optional, compile-time only
//!
//! [package.metadata.typedpg.migrations]
//! table = "public._migrations"            # optional, tracking table name
//! lock_id = 713705                        # optional, advisory lock ID
//! use_transaction = true                  # optional, wrap each migration in a tx
//! fail_on_drift = true                    # optional, abort if applied migration changed
//!
//! [package.metadata.typedpg.types]          # optional, PG type → Rust type
//! user_preferences = "crate::UserPrefs"   # a JSONB domain (serde)
//! user_role = "crate::UserRole"           # an enum (Display + FromStr)
//! "public.address" = "crate::Address"     # a composite (rebuilt field by field)
//! "extensions.ltree" = "String"           # any other type (ToSql + FromSql)
//! ```
//!
//! A key typedpg does not know is a compile error, not silently ignored.
//!
//! Crates that talk to more than one database declare each additional one
//! under `[package.metadata.typedpg.databases.<name>]` (same shape: `database`,
//! `migrations`, `types`) and select it at the call site with
//! `sql!(db = <name>, ...)`. See the [Multiple databases](#multiple-databases)
//! section below.
//!
//! # Rebuilding when migrations change
//!
//! The `sql!` macro reads your migration files at compile time, but a proc
//! macro cannot, on stable Rust, declare those files as build inputs. Without
//! help, editing or adding a migration would not trigger a rebuild and `sql!`
//! would keep producing types from a stale schema.
//!
//! The fix is a one-line `build.rs` calling [`build::track_migrations`]. It
//! tells Cargo to watch every migration directory (recursively, so future
//! files count too) and to recompile — re-running every `sql!` — when their
//! contents change. See the [`build`] module for details.
//!
//! # The `sql!` macro
//!
//! The macro verifies your SQL at compile time and generates an anonymous struct
//! for the result columns. It supports four terminal methods:
//!
//! ```rust,ignore
//! use typedpg::sql;
//!
//! # async fn example(pool: &deadpool_postgres::Pool) -> Result<(), typedpg::Error> {
//! // fetch_all -- returns Vec<Row>. Use for SELECT queries expecting multiple rows.
//! let users = sql!(pool, "SELECT id, name FROM users")
//!     .fetch_all()
//!     .await?;
//!
//! // fetch_one -- returns a single Row. Returns Error::NoRows if empty.
//! let user = sql!(pool, "SELECT id, name FROM users WHERE id = $id", id = 1)
//!     .fetch_one()
//!     .await?;
//!
//! // fetch_optional -- returns Option<Row>. Use when the row might not exist.
//! let maybe_user = sql!(pool, "SELECT id, name FROM users WHERE id = $id", id = 42)
//!     .fetch_optional()
//!     .await?;
//!
//! // execute -- returns u64 (number of affected rows). Use for INSERT/UPDATE/DELETE.
//! let rows_affected = sql!(pool, "DELETE FROM users WHERE id = $id", id = 1)
//!     .execute()
//!     .await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Named parameters
//!
//! Parameters use `$name` syntax in SQL. You can provide values with explicit
//! assignment or rely on scope capture (like closures):
//!
//! ```rust,ignore
//! use typedpg::sql;
//!
//! # async fn example(pool: &deadpool_postgres::Pool) -> Result<(), typedpg::Error> {
//! // Explicit assignment
//! let user = sql!(
//!     pool,
//!     "SELECT id, name FROM users WHERE email = $email",
//!     email = "alice@example.com"
//! )
//!     .fetch_one()
//!     .await?;
//!
//! // Scope capture -- if a variable `email` is in scope, just use $email
//! let email = "alice@example.com".to_string();
//! let user = sql!(
//!     pool,
//!     "SELECT id, name FROM users WHERE email = $email"
//! )
//!     .fetch_one()
//!     .await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Bulk insert with `$..spread`
//!
//! Insert multiple rows in a single statement using the spread syntax. The macro
//! expands `$..items { field1, field2 }` into a multi-row `VALUES` clause:
//!
//! ```rust,ignore
//! use typedpg::sql;
//!
//! # struct NewUser { name: String, email: String }
//! # async fn example(pool: &deadpool_postgres::Pool) -> Result<(), typedpg::Error> {
//! let new_users = vec![
//!     NewUser { name: "Alice".into(), email: "alice@example.com".into() },
//!     NewUser { name: "Bob".into(),   email: "bob@example.com".into() },
//! ];
//!
//! let rows_affected = sql!(
//!     pool,
//!     "INSERT INTO users (name, email) VALUES $..new_users { name, email }"
//! )
//!     .execute()
//!     .await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Domain types (JSONB)
//!
//! Map PostgreSQL `CREATE DOMAIN ... AS JSONB` types to Rust structs that
//! implement `serde::Serialize` and `serde::Deserialize`. Configure the mapping
//! in `Cargo.toml`:
//!
//! ```toml
//! [package.metadata.typedpg.types]
//! user_preferences = "crate::domains::UserPreferences"
//! ```
//!
//! Then the `sql!` macro automatically serializes and deserializes through the
//! mapped Rust type instead of raw `serde_json::Value`.
//!
//! # Transactions
//!
//! The [`Executor`] trait is implemented for `tokio_postgres::Transaction`, so you
//! can pass a transaction directly to `sql!`:
//!
//! ```rust,ignore
//! use typedpg::sql;
//!
//! # async fn example(pool: &deadpool_postgres::Pool) -> Result<(), typedpg::Error> {
//! let mut client = pool.get().await?;
//! let tx = client.transaction().await?;
//!
//! sql!(&tx, "INSERT INTO users (name, email) VALUES ($name, $email)",
//!     name = "Charlie", email = "charlie@example.com")
//!     .execute()
//!     .await?;
//!
//! sql!(&tx, "UPDATE users SET name = $name WHERE email = $email",
//!     name = "Charles", email = "charlie@example.com")
//!     .execute()
//!     .await?;
//!
//! tx.commit().await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Mapping rows to your own structs
//!
//! `sql!` synthesises an anonymous result struct for every query. When you
//! want to return a struct you already own — typically a domain type shared
//! across several queries — derive [`FromRow`] and use the `fetch_*_as::<T>`
//! methods generated alongside the default ones:
//!
//! ```rust,ignore
//! #[derive(typedpg::FromRow)]
//! struct User {
//!     id: i32,
//!     name: String,
//!     email: Option<String>,
//! }
//!
//! let user: User = sql!(pool, "SELECT id, name, email FROM users WHERE id = $id", id = 1)
//!     .fetch_one_as::<User>()
//!     .await?;
//! ```
//!
//! Field names must match the query's output column names; `Option<T>` fields
//! receive nullable columns. The compile-time type check still runs against
//! the SQL itself, not against `T`.
//!
//! # Multiple databases
//!
//! Declare a named database under `[package.metadata.typedpg.databases.<name>]`
//! in your `Cargo.toml` (each with its own `migrations`, `[migrations]`
//! runner settings, and `[types]` map), then select it at the call site with
//! a `db = <name>` prefix:
//!
//! ```rust,ignore
//! // Default database (uses the top-level [typedpg.database] config)
//! let users = sql!(app_pool, "SELECT id, name FROM users")
//!     .fetch_all().await?;
//!
//! // Named database (uses [typedpg.databases.analytics.*])
//! let metrics = sql!(db = analytics, warehouse_pool,
//!     "SELECT kind, total FROM daily_metrics")
//!     .fetch_all().await?;
//! ```
//!
//! `typedpg` does not multiplex pools — the executor (`app_pool` /
//! `warehouse_pool`) is still your responsibility. The `db = ...` prefix only
//! tells the compile-time analyzer which schema to type-check against.
//!
//! # Migrations (programmatic API)
//!
//! Use the [`migrate`] module to run migrations at application startup:
//!
//! ```rust,no_run
//! use typedpg::migrate::{MigrationSource, MigrationsConfig, run};
//! use std::path::Path;
//!
//! # async fn example() -> Result<(), typedpg::Error> {
//! // Connect directly with tokio-postgres for migrations
//! let (mut client, connection) =
//!     tokio_postgres::connect("host=localhost dbname=mydb user=postgres", tokio_postgres::NoTls).await?;
//! tokio::spawn(connection);
//!
//! let source = MigrationSource::from_dir(Path::new("./migrations"))?;
//! let config = MigrationsConfig::default();
//! let applied = run(&mut client, &source, &config).await?;
//! println!("Applied {} migrations", applied.len());
//! # Ok(())
//! # }
//! ```
//!
//! For binaries that should ship without their migration files on disk, the
//! [`macro@embed_migrations`] macro bakes the SQL into the binary at compile
//! time:
//!
//! ```rust,ignore
//! use typedpg::migrate::{MigrationsConfig, run};
//!
//! let source = typedpg::embed_migrations!("./migrations");
//! let applied = run(&mut client, &source, &MigrationsConfig::default()).await?;
//! ```
//!
//! See the [`migrate`] module for details on [`migrate::status`] and [`migrate::revert`].
//!
//! # Pool setup
//!
//! `typedpg` does **not** create or manage connection pools — that is the
//! application's responsibility. This crate implements [`Executor`] for common
//! pool types so you can pass them directly to `sql!`.
//!
//! ## With `deadpool-postgres` (default feature: `deadpool`)
//!
//! ```rust,no_run
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! use deadpool_postgres::{Config, Runtime};
//! use tokio_postgres::NoTls;
//!
//! let mut cfg = Config::new();
//! cfg.url = Some(std::env::var("DATABASE_URL")?);
//! let pool = cfg.create_pool(Some(Runtime::Tokio1), NoTls)?;
//!
//! // Pass `&pool` directly to sql! -- acquires a connection automatically
//! // Or get a dedicated connection: pool.get().await?
//! # Ok(())
//! # }
//! ```
//!
//! ## With `bb8-postgres` (feature: `bb8`)
//!
//! ```toml
//! typedpg = { version = "0.1", default-features = false, features = ["bb8"] }
//! bb8-postgres = "0.9"
//! ```
//!
//! # The `Executor` trait
//!
//! The [`Executor`] trait abstracts over connection types. It is implemented for:
//!
//! | Type | Feature | Behavior |
//! |------|---------|----------|
//! | `deadpool_postgres::Pool` | `deadpool` (default) | Acquires a connection per query |
//! | `deadpool_postgres::Object` | `deadpool` (default) | Uses the pooled connection |
//! | `bb8::Pool<PostgresConnectionManager<Tls>>` | `bb8` | Acquires a connection per query |
//! | `tokio_postgres::Client` | always | Uses the raw client directly |
//! | `tokio_postgres::Transaction<'_>` | always | Executes within the transaction |
//!
//! You do not call `Executor` methods directly -- the `sql!` macro generates code
//! that is generic over any `Executor` implementation.
//!
//! # Type mapping
//!
//! The following PostgreSQL types are supported and mapped to Rust types automatically:
//!
//! | PostgreSQL type | Rust type |
//! |-----------------|-----------|
//! | `bool` | `bool` |
//! | `int2` / `smallint` | `i16` |
//! | `int4` / `integer` | `i32` |
//! | `int8` / `bigint` | `i64` |
//! | `float4` / `real` | `f32` |
//! | `float8` / `double precision` | `f64` |
//! | `numeric` / `decimal` | `rust_decimal::Decimal` |
//! | `text` | `String` |
//! | `varchar` / `char(n)` | `String` |
//! | `bytea` | `Vec<u8>` |
//! | `uuid` | `uuid::Uuid` |
//! | `date` | `chrono::NaiveDate` |
//! | `time` | `chrono::NaiveTime` |
//! | `timestamp` | `chrono::NaiveDateTime` |
//! | `timestamptz` | `chrono::DateTime<chrono::Utc>` |
//! | `json` | `serde_json::Value` |
//! | `jsonb` | `serde_json::Value` |
//! | `oid` | `u32` |
//! | `bool[]` | `Vec<bool>` |
//! | `int2[]` | `Vec<i16>` |
//! | `int4[]` | `Vec<i32>` |
//! | `int8[]` | `Vec<i64>` |
//! | `float4[]` | `Vec<f32>` |
//! | `float8[]` | `Vec<f64>` |
//! | `text[]` | `Vec<String>` |
//! | `uuid[]` | `Vec<uuid::Uuid>` |
//! | `jsonb[]` | `Vec<serde_json::Value>` |
//!
//! Types declared in your own schema — JSONB domains, enums, and types from
//! extensions like `pgvector` — are mapped through the
//! `[package.metadata.typedpg.{domains, enums, types}]` sections shown above.
//! See `typedpg_macros::pg_type_map` for the full set of recognized built-ins.
//!
//! Composite types and anonymous `ROW(...)` / subquery records are decoded
//! into a Rust struct synthesized by the `sql!` macro, one typed field per
//! composite attribute (nested composites nest). Pointing a composite at a
//! pre-existing struct via `[package.metadata.typedpg.types]` makes the
//! macro rebuild *that* struct field-by-field instead — its field names must
//! match the composite's attributes.
//!
//! Nullable columns (no `NOT NULL` constraint) are wrapped in `Option<T>`.

mod copy;
mod decode;
pub mod error;
pub mod executor;
pub mod from_row;
pub mod migrate;
mod pool; // Executor impls for pool types (deadpool, bb8)
pub mod stream;

pub use error::Error;
pub use executor::Executor;
pub use from_row::FromRow;
pub use stream::{QueryStream, RowStream};

/// Build-script helpers — re-exported from `typedpg_core`.
///
/// Call [`build::track_migrations`] from your crate's `build.rs` so the `sql!`
/// macro is re-checked whenever a migration file changes.
pub use typedpg_core::build;

pub use typedpg_macros::FromRow;
/// Re-export the `embed_migrations!` macro from `typedpg_macros`.
///
/// See the [`macro@embed_migrations`] documentation for usage details.
pub use typedpg_macros::embed_migrations;
/// Re-export the `sql!` macro from `typedpg_macros`.
///
/// See the [`macro@sql`] documentation for full syntax, examples, and
/// configuration details.
pub use typedpg_macros::sql;

/// Re-export the `copy_in!` macro from `typedpg_macros`: bulk-load rows with
/// a binary `COPY ... FROM STDIN`. See [`macro@copy_in`].
pub use typedpg_macros::copy_in;

// Re-export rust_decimal so generated code can reference it.
pub use rust_decimal;

/// Re-exports used by the `sql!` macro generated code.
/// Not part of the public API — do not rely on these directly.
#[doc(hidden)]
pub mod __private {
    pub use crate::copy::CopyRow;
    pub use crate::decode::{read_column, read_named_column};
    pub use bytes;
    pub use tokio_postgres;

    use bytes::{Buf, BytesMut};
    use tokio_postgres::types::{FromSql, IsNull, Kind, ToSql, Type, to_sql_checked};

    /// Reads a column as `T`, with an array of a domain read as an array of
    /// the domain's base type.
    ///
    /// PostgreSQL describes a domain-typed column by its base type, but an
    /// array of a domain has its own array type, whose element is the
    /// domain — and `FromSql` impls (`Vec<i32>`, `Vec<serde_json::Value>`)
    /// accept only their base element types. The binary format is the
    /// base type's either way.
    #[doc(hidden)]
    pub struct BaseTyped<T>(pub T);

    /// `ty` with its array element's domains replaced by their base type,
    /// if it is such an array.
    fn base_typed(ty: &Type) -> Option<Type> {
        let Kind::Array(member) = ty.kind() else {
            return None;
        };
        let mut base = member;
        while let Kind::Domain(inner) = base.kind() {
            base = inner;
        }
        if std::ptr::eq(base, member) {
            return None;
        }
        Some(Type::new(
            ty.name().to_owned(),
            ty.oid(),
            Kind::Array(base.clone()),
            ty.schema().to_owned(),
        ))
    }

    impl<'a, T: FromSql<'a>> FromSql<'a> for BaseTyped<T> {
        fn from_sql(
            ty: &Type,
            raw: &'a [u8],
        ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
            Self::from_sql_nullable(ty, Some(raw))
        }

        fn from_sql_null(ty: &Type) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
            Self::from_sql_nullable(ty, None)
        }

        fn from_sql_nullable(
            ty: &Type,
            raw: Option<&'a [u8]>,
        ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
            match base_typed(ty) {
                Some(base) => T::from_sql_nullable(&base, raw).map(BaseTyped),
                None => T::from_sql_nullable(ty, raw).map(BaseTyped),
            }
        }

        fn accepts(ty: &Type) -> bool {
            match base_typed(ty) {
                Some(base) => T::accepts(&base),
                None => T::accepts(ty),
            }
        }
    }

    /// Bridge type for PostgreSQL enums.
    ///
    /// `tokio_postgres` will not decode/encode a PG enum as `String` directly:
    /// `FromSql`/`ToSql` for `String` only accept `TEXT`/`VARCHAR`/etc. Enum
    /// values travel on the wire as plain UTF-8 label bytes, so we expose a
    /// thin wrapper whose `accepts` matches any `Kind::Enum` type.
    #[derive(Debug)]
    pub struct EnumString(pub String);

    impl<'a> FromSql<'a> for EnumString {
        fn from_sql(
            _ty: &Type,
            raw: &'a [u8],
        ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
            Ok(EnumString(std::str::from_utf8(raw)?.to_owned()))
        }

        fn accepts(ty: &Type) -> bool {
            matches!(ty.kind(), Kind::Enum(_))
        }
    }

    impl ToSql for EnumString {
        fn to_sql(
            &self,
            _ty: &Type,
            out: &mut BytesMut,
        ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
            out.extend_from_slice(self.0.as_bytes());
            Ok(IsNull::No)
        }

        fn accepts(ty: &Type) -> bool {
            matches!(ty.kind(), Kind::Enum(_))
        }

        to_sql_checked!();
    }

    /// Converts a bare string-like value or an `Option<T: Into<String>>` into
    /// `Option<String>` so callers of `sql!` can pass either at a site that
    /// expects a nullable string parameter.
    ///
    /// Impls are concrete for common string types to avoid coherence
    /// conflicts with a hypothetical future `Option<T>: Into<String>`.
    pub trait IntoOptionString {
        fn into_option_string(self) -> Option<String>;
    }

    impl IntoOptionString for &str {
        fn into_option_string(self) -> Option<String> {
            Some(self.to_owned())
        }
    }

    impl IntoOptionString for String {
        fn into_option_string(self) -> Option<String> {
            Some(self)
        }
    }

    impl IntoOptionString for &String {
        fn into_option_string(self) -> Option<String> {
            Some(self.clone())
        }
    }

    impl<T: Into<String>> IntoOptionString for Option<T> {
        fn into_option_string(self) -> Option<String> {
            self.map(Into::into)
        }
    }

    /// Collect any `IntoIterator` whose items convert into `T` into a
    /// `Vec<T>`. Used by generated code for `Vec<T>` parameters so callers
    /// can pass `Vec<&str>`, `[&str; N]`, `[T; N]`, etc., in addition to an
    /// exact `Vec<T>`.
    pub fn into_flex_vec<T, I>(iter: I) -> Vec<T>
    where
        I: IntoIterator,
        I::Item: Into<T>,
    {
        iter.into_iter().map(Into::into).collect()
    }

    // -----------------------------------------------------------------------
    // Composite / record (de)serialization
    //
    // PostgreSQL's binary wire format for a `record` value (and, identically,
    // for any named composite type) is *self-describing*: an `i32` field
    // count, then for each field a `u32` type OID, an `i32` byte length
    // (`-1` for SQL NULL), and the field body. Because every field carries
    // its own OID inline, a value can be decoded with no catalog round-trip.
    //
    // The `sql!` macro synthesizes a Rust struct for every composite column
    // and anonymous `ROW(...)` / subquery record it sees, and emits a manual
    // `FromSql` impl that drives [`RecordReader`].
    // -----------------------------------------------------------------------

    /// Boxed error used across the record (de)serialization helpers.
    type BoxError = Box<dyn std::error::Error + Sync + Send>;

    /// Returns `true` when `ty` is a composite / record type — i.e. a value
    /// the synthesized record structs know how to decode. Domains are
    /// unwrapped to their base type.
    ///
    /// Used by the `accepts` method of generated `FromSql` / `ToSql` impls.
    pub fn record_accepts(ty: &Type) -> bool {
        match ty.kind() {
            Kind::Composite(_) => true,
            Kind::Domain(inner) => record_accepts(inner),
            _ => *ty == Type::RECORD,
        }
    }

    /// Streaming reader over the binary wire format of a `record` / composite.
    ///
    /// Generated `FromSql` impls construct one with [`RecordReader::new`],
    /// call [`RecordReader::read_field`] once per struct field, and close with
    /// [`RecordReader::finish`].
    pub struct RecordReader<'a> {
        buf: &'a [u8],
        declared: usize,
        consumed: usize,
    }

    impl<'a> RecordReader<'a> {
        /// Parse the leading `i32` field count. Fails on a truncated buffer.
        pub fn new(mut raw: &'a [u8]) -> Result<Self, BoxError> {
            if raw.remaining() < 4 {
                return Err("record wire format: truncated field count".into());
            }
            let declared = raw.get_i32();
            if declared < 0 {
                return Err("record wire format: negative field count".into());
            }
            Ok(RecordReader {
                buf: raw,
                declared: declared as usize,
                consumed: 0,
            })
        }

        /// Decode the next field as `T`, reconstructing its PG type from the
        /// OID carried inline. A user-defined OID with no built-in mapping
        /// falls back to [`Type::RECORD`] — harmless, since the synthesized
        /// nested structs ignore the passed type and re-read the inline OIDs.
        pub fn read_field<T: FromSql<'a>>(&mut self) -> Result<T, BoxError> {
            self.read_field_inner(None)
        }

        /// Decode the next field as `T`, decoding it as `expected` rather than
        /// the type reconstructed from the inline OID.
        ///
        /// Needed for types whose `FromSql` impl is type-sensitive (notably
        /// `serde_json::Value`, which strips a version byte only for `JSONB`)
        /// when the inline OID is a user type — e.g. a domain over `jsonb` —
        /// that does not resolve back to a built-in.
        pub fn read_field_with<T: FromSql<'a>>(&mut self, expected: &Type) -> Result<T, BoxError> {
            self.read_field_inner(Some(expected))
        }

        fn read_field_inner<T: FromSql<'a>>(
            &mut self,
            expected: Option<&Type>,
        ) -> Result<T, BoxError> {
            if self.consumed >= self.declared {
                return Err(format!(
                    "record wire format: tried to read field {} but only {} present",
                    self.consumed + 1,
                    self.declared,
                )
                .into());
            }
            if self.buf.remaining() < 8 {
                return Err("record wire format: truncated field header".into());
            }
            let oid = self.buf.get_u32();
            let len = self.buf.get_i32();
            let field_ty = match expected {
                Some(ty) => ty.clone(),
                None => Type::from_oid(oid).unwrap_or(Type::RECORD),
            };
            let body = if len < 0 {
                None
            } else {
                let len = len as usize;
                if self.buf.remaining() < len {
                    return Err("record wire format: truncated field body".into());
                }
                let (head, tail) = self.buf.split_at(len);
                self.buf = tail;
                Some(head)
            };
            self.consumed += 1;
            T::from_sql_nullable(&field_ty, body)
        }

        /// Assert the record carried exactly `expected` fields. Generated
        /// code passes its statically known field count.
        pub fn finish(self, expected: usize) -> Result<(), BoxError> {
            if self.declared != expected {
                return Err(format!(
                    "record wire format: expected {expected} fields, got {}",
                    self.declared,
                )
                .into());
            }
            Ok(())
        }
    }

    #[cfg(test)]
    mod record_tests {
        use super::*;

        /// Build a record wire payload from `(oid, body)` field tuples;
        /// `body = None` encodes a SQL NULL field.
        fn encode(fields: &[(u32, Option<&[u8]>)]) -> Vec<u8> {
            let mut buf = Vec::new();
            buf.extend_from_slice(&(fields.len() as i32).to_be_bytes());
            for (oid, body) in fields {
                buf.extend_from_slice(&oid.to_be_bytes());
                match body {
                    Some(b) => {
                        buf.extend_from_slice(&(b.len() as i32).to_be_bytes());
                        buf.extend_from_slice(b);
                    }
                    None => buf.extend_from_slice(&(-1i32).to_be_bytes()),
                }
            }
            buf
        }

        #[test]
        fn reads_scalar_fields() {
            // (int4 = 7, text = "hi")
            let wire = encode(&[
                (Type::INT4.oid(), Some(&7i32.to_be_bytes())),
                (Type::TEXT.oid(), Some(b"hi")),
            ]);
            let mut r = RecordReader::new(&wire).unwrap();
            let a: i32 = r.read_field().unwrap();
            let b: String = r.read_field().unwrap();
            r.finish(2).unwrap();
            assert_eq!(a, 7);
            assert_eq!(b, "hi");
        }

        #[test]
        fn reads_null_field_into_option() {
            let wire = encode(&[(Type::INT4.oid(), None)]);
            let mut r = RecordReader::new(&wire).unwrap();
            let v: Option<i32> = r.read_field().unwrap();
            r.finish(1).unwrap();
            assert!(v.is_none());
        }

        #[test]
        fn null_field_into_non_option_errors() {
            let wire = encode(&[(Type::INT4.oid(), None)]);
            let mut r = RecordReader::new(&wire).unwrap();
            assert!(r.read_field::<i32>().is_err());
        }

        #[test]
        fn finish_rejects_field_count_mismatch() {
            let wire = encode(&[(Type::INT4.oid(), Some(&1i32.to_be_bytes()))]);
            let mut r = RecordReader::new(&wire).unwrap();
            let _: i32 = r.read_field().unwrap();
            assert!(r.finish(2).is_err());
        }

        #[test]
        fn reading_past_declared_count_errors() {
            let wire = encode(&[(Type::INT4.oid(), Some(&1i32.to_be_bytes()))]);
            let mut r = RecordReader::new(&wire).unwrap();
            let _: i32 = r.read_field().unwrap();
            assert!(r.read_field::<i32>().is_err());
        }

        #[test]
        fn truncated_count_errors() {
            assert!(RecordReader::new(&[0u8, 0]).is_err());
        }

        #[test]
        fn truncated_body_errors() {
            // declares one int4 field but the body claims 8 bytes, supplies 2
            let mut wire = Vec::new();
            wire.extend_from_slice(&1i32.to_be_bytes());
            wire.extend_from_slice(&Type::INT4.oid().to_be_bytes());
            wire.extend_from_slice(&8i32.to_be_bytes());
            wire.extend_from_slice(&[0u8, 0]);
            let mut r = RecordReader::new(&wire).unwrap();
            assert!(r.read_field::<i32>().is_err());
        }

        #[test]
        fn record_accepts_matches_record_and_composite() {
            assert!(record_accepts(&Type::RECORD));
            assert!(!record_accepts(&Type::INT4));
        }
    }
}
