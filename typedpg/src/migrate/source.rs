use std::collections::HashMap;
use std::path::Path;

/// A single migration read from disk.
///
/// Each migration corresponds to a `NNNN_description.sql` file, with an optional
/// `NNNN_description.down.sql` companion for rollbacks.
///
/// # Example
///
/// `no_transaction` is not set by the caller — it is derived from the SQL, so
/// a migration that must run outside a transaction says so in its own text:
///
/// ```rust
/// use typedpg::migrate::MigrationSource;
///
/// let source = MigrationSource::from_embedded([
///     (
///         "0001_add_enum_value",
///         "-- no-transaction\nALTER TYPE status ADD VALUE 'archived';",
///         None,
///     ),
/// ])?;
///
/// assert!(source.migrations()[0].no_transaction);
/// # Ok::<(), typedpg::Error>(())
/// ```
#[derive(Debug, Clone)]
pub struct Migration {
    /// Numeric prefix extracted from the filename, e.g. `"0001"`.
    /// Used for ordering migrations.
    pub version: String,
    /// Full name without extension, e.g. `"0001_create_users"`.
    /// This is the key used in the tracking table.
    pub name: String,
    /// SQL content of the up migration file.
    pub sql: String,
    /// SQL content of the down migration file, if `NNNN_description.down.sql` exists.
    pub down_sql: Option<String>,
    /// If `true`, this migration should run outside a transaction.
    /// Set automatically when the first line of the SQL file is `-- no-transaction`.
    pub no_transaction: bool,
}

/// A collection of migrations read from a directory on disk.
///
/// Construct with [`from_dir`](MigrationSource::from_dir), then pass to
/// [`run`](super::run), [`status`](super::status), or [`revert`](super::revert).
///
/// # Expected directory layout
///
/// ```text
/// migrations/
///   0001_create_users.sql
///   0001_create_users.down.sql   # optional rollback
///   0002_add_email.sql
///   0003_create_index.sql
/// ```
///
/// - Up files: `NNNN_description.sql` (numeric prefix + underscore + description).
/// - Down files: `NNNN_description.down.sql` (same base name, `.down.sql` suffix).
/// - Non-`.sql` files and subdirectories are ignored.
/// - Migrations are sorted by numeric prefix.
#[derive(Debug)]
pub struct MigrationSource {
    migrations: Vec<Migration>,
}

impl MigrationSource {
    /// Reads and sorts all migrations from the given directory.
    ///
    /// Scans for `*.sql` files matching the `NNNN_description.sql` naming convention,
    /// pairs them with optional `.down.sql` files, and returns them sorted by version.
    ///
    /// If the directory does not exist, returns an empty migration source
    /// (zero migrations).
    ///
    /// # Errors
    ///
    /// - [`Error::Migration`](crate::Error::Migration) if a file does not match
    ///   the expected naming format.
    /// - [`Error::Io`](crate::Error::Io) if reading a file fails.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use typedpg::migrate::MigrationSource;
    /// use std::path::Path;
    ///
    /// let source = MigrationSource::from_dir(Path::new("./migrations"))
    ///     .expect("failed to read migrations");
    /// println!("Found {} migrations", source.migrations().len());
    /// ```
    pub fn from_dir(path: &Path) -> Result<Self, crate::Error> {
        if !path.is_dir() {
            return Ok(Self {
                migrations: Vec::new(),
            });
        }

        let mut entries: Vec<_> = std::fs::read_dir(path)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|e| e.file_name());

        // Collect down files first: "0001_create_users.down.sql" -> key "0001_create_users"
        let mut down_files: HashMap<String, String> = HashMap::new();
        let mut up_entries = Vec::new();

        for entry in &entries {
            let entry_path = entry.path();
            if entry_path.is_dir() {
                continue;
            }

            let file_name = entry_path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("");

            if let Some(base_name) = file_name.strip_suffix(".down.sql") {
                let sql = std::fs::read_to_string(&entry_path)?;
                down_files.insert(base_name.to_string(), sql);
            } else if file_name.ends_with(".sql") {
                up_entries.push(entry);
            }
        }

        let mut migrations = Vec::new();

        for entry in up_entries {
            let path = entry.path();
            let file_stem = path.file_stem().and_then(|s| s.to_str()).ok_or_else(|| {
                crate::Error::Migration(format!("invalid file name: {}", path.display()))
            })?;

            let (version, _description) = parse_migration_name(file_stem, &path)?;

            let sql = std::fs::read_to_string(&path)?;
            let no_transaction = sql
                .lines()
                .next()
                .is_some_and(|line| line.trim() == "-- no-transaction");

            let down_sql = down_files.remove(file_stem);

            migrations.push(Migration {
                version,
                name: file_stem.to_string(),
                sql,
                down_sql,
                no_transaction,
            });
        }

        migrations.sort_by(by_version);

        Ok(Self { migrations })
    }

    /// Build a `MigrationSource` from migration contents already in memory.
    /// Lets a binary embed migrations via `include_str!` (or any other
    /// source), without round-tripping through the filesystem at runtime.
    ///
    /// Each entry is `(file_stem, up_sql, down_sql)` where `file_stem`
    /// matches `NNNN_description` — same naming rules as
    /// [`from_dir`](Self::from_dir). Migrations are sorted by their
    /// numeric prefix.
    ///
    /// For the typical case of embedding `*.sql` files from a directory at
    /// compile-time, prefer [`embed_migrations!`](crate::embed_migrations);
    /// this method is the lower-level entry point when migration sources
    /// come from arbitrary in-memory strings.
    ///
    /// # Errors
    ///
    /// - [`Error::Migration`](crate::Error::Migration) if a `file_stem`
    ///   doesn't follow the `NNNN_description` format.
    ///
    /// # Example
    ///
    /// ```rust
    /// use typedpg::migrate::MigrationSource;
    ///
    /// // Given out of order on purpose: entries come back sorted by version.
    /// let source = MigrationSource::from_embedded([
    ///     (
    ///         "0002_add_email",
    ///         "ALTER TABLE users ADD COLUMN email TEXT NOT NULL;",
    ///         Some("ALTER TABLE users DROP COLUMN email;"),
    ///     ),
    ///     (
    ///         "0001_create_users",
    ///         "CREATE TABLE users (id SERIAL PRIMARY KEY);",
    ///         None,
    ///     ),
    /// ])?;
    ///
    /// let first = &source.migrations()[0];
    /// assert_eq!(first.version, "0001");
    /// assert_eq!(first.name, "0001_create_users");
    /// assert_eq!(first.sql, "CREATE TABLE users (id SERIAL PRIMARY KEY);");
    /// assert!(first.down_sql.is_none(), "0001 ships no rollback");
    /// assert!(!first.no_transaction);
    ///
    /// let second = &source.migrations()[1];
    /// assert_eq!(second.version, "0002");
    /// assert_eq!(
    ///     second.down_sql.as_deref(),
    ///     Some("ALTER TABLE users DROP COLUMN email;"),
    /// );
    /// # Ok::<(), typedpg::Error>(())
    /// ```
    pub fn from_embedded<'a, I>(entries: I) -> Result<Self, crate::Error>
    where
        I: IntoIterator<Item = (&'a str, &'a str, Option<&'a str>)>,
    {
        let mut migrations = Vec::new();
        for (stem, up, down) in entries {
            // Reuse from_dir's parser so the format error wording stays
            // identical — pass the stem as the file path for nice errors.
            let synthetic_path = std::path::PathBuf::from(format!("{stem}.sql"));
            let (version, _description) = parse_migration_name(stem, &synthetic_path)?;
            let no_transaction = up
                .lines()
                .next()
                .is_some_and(|line| line.trim() == "-- no-transaction");
            migrations.push(Migration {
                version,
                name: stem.to_owned(),
                sql: up.to_owned(),
                down_sql: down.map(str::to_owned),
                no_transaction,
            });
        }
        migrations.sort_by(by_version);
        Ok(Self { migrations })
    }

    /// Returns all migrations in version order.
    pub fn migrations(&self) -> &[Migration] {
        &self.migrations
    }

    /// Finds a migration by its full name (e.g. `"0001_create_users"`).
    ///
    /// Returns `None` if no migration with that name exists in this source.
    /// The name is the same key [`revert`](super::revert) takes and the one
    /// recorded in the tracking table.
    ///
    /// # Example
    ///
    /// ```rust
    /// use typedpg::migrate::MigrationSource;
    ///
    /// let source = MigrationSource::from_embedded([
    ///     ("0001_create_users", "CREATE TABLE users (id SERIAL PRIMARY KEY);", None),
    /// ])?;
    ///
    /// let found = source.find("0001_create_users").expect("just added");
    /// assert_eq!(found.version, "0001");
    ///
    /// // Lookup is by full name, not by version prefix.
    /// assert!(source.find("0001").is_none());
    /// # Ok::<(), typedpg::Error>(())
    /// ```
    pub fn find(&self, name: &str) -> Option<&Migration> {
        self.migrations.iter().find(|m| m.name == name)
    }
}

/// Migrations are ordered by their numeric prefix: `2` before `10`. The
/// prefix is a number, not text — `10_x` is a later migration than `2_x`.
///
/// Two migrations with the same prefix keep the order they were read in.
fn by_version(a: &Migration, b: &Migration) -> std::cmp::Ordering {
    match (a.version.parse::<u128>(), b.version.parse::<u128>()) {
        (Ok(x), Ok(y)) => x.cmp(&y),
        // The prefix is a number (`parse_migration_name` checks), so this
        // never happens: keep the order anyway.
        (Err(_), Ok(_)) => std::cmp::Ordering::Less,
        (Ok(_), Err(_)) => std::cmp::Ordering::Greater,
        (Err(_), Err(_)) => std::cmp::Ordering::Equal,
    }
}

/// Parses a migration file stem like "0001_create_users" into ("0001", "create_users").
/// Returns an error if the format is invalid.
fn parse_migration_name(stem: &str, file_path: &Path) -> Result<(String, String), crate::Error> {
    let underscore_pos = stem.find('_').ok_or_else(|| {
        crate::Error::Migration(format!(
            "migration file does not follow NNNN_description.sql format: {}",
            file_path.display()
        ))
    })?;

    let prefix = &stem[..underscore_pos];
    let description = &stem[underscore_pos + 1..];

    if prefix.is_empty() || !prefix.chars().all(|c| c.is_ascii_digit()) {
        return Err(crate::Error::Migration(format!(
            "migration file does not have a numeric prefix (NNNN_...): {}",
            file_path.display()
        )));
    }

    if description.is_empty() {
        return Err(crate::Error::Migration(format!(
            "migration file has no description after prefix: {}",
            file_path.display()
        )));
    }

    Ok((prefix.to_string(), description.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn create_temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("failed to create temp dir")
    }

    /// The prefix is a number, not text: `10_x` is a later migration than
    /// `2_x`.
    #[test]
    fn migrations_are_ordered_by_their_numeric_prefix() {
        let dir = create_temp_dir();
        for name in ["1_a.sql", "2_b.sql", "10_c.sql", "20_d.sql"] {
            fs::write(dir.path().join(name), "SELECT 1;").unwrap();
        }
        let source = MigrationSource::from_dir(dir.path()).unwrap();
        let versions: Vec<&str> = source
            .migrations()
            .iter()
            .map(|m| m.version.as_str())
            .collect();
        assert_eq!(versions, ["1", "2", "10", "20"]);

        // The embedded source orders them the same way.
        let source = MigrationSource::from_embedded([
            ("10_c", "SELECT 1;", None),
            ("2_b", "SELECT 1;", None),
            ("1_a", "SELECT 1;", None),
        ])
        .unwrap();
        let versions: Vec<&str> = source
            .migrations()
            .iter()
            .map(|m| m.version.as_str())
            .collect();
        assert_eq!(versions, ["1", "2", "10"]);
    }

    #[test]
    fn reads_and_orders_migrations() {
        let dir = create_temp_dir();

        fs::write(
            dir.path().join("0002_add_email.sql"),
            "ALTER TABLE users ADD email TEXT;",
        )
        .unwrap();
        fs::write(
            dir.path().join("0001_create_users.sql"),
            "CREATE TABLE users (id SERIAL PRIMARY KEY);",
        )
        .unwrap();
        fs::write(
            dir.path().join("0003_add_index.sql"),
            "CREATE INDEX idx_email ON users(email);",
        )
        .unwrap();

        let source = MigrationSource::from_dir(dir.path()).unwrap();
        let migrations = source.migrations();

        assert_eq!(migrations.len(), 3);
        assert_eq!(migrations[0].version, "0001");
        assert_eq!(migrations[0].name, "0001_create_users");
        assert_eq!(
            migrations[0].sql,
            "CREATE TABLE users (id SERIAL PRIMARY KEY);"
        );
        assert_eq!(migrations[1].version, "0002");
        assert_eq!(migrations[2].version, "0003");
    }

    #[test]
    fn reads_down_migrations() {
        let dir = create_temp_dir();

        fs::write(
            dir.path().join("0001_create_users.sql"),
            "CREATE TABLE users (id SERIAL PRIMARY KEY);",
        )
        .unwrap();
        fs::write(
            dir.path().join("0001_create_users.down.sql"),
            "DROP TABLE users;",
        )
        .unwrap();

        let source = MigrationSource::from_dir(dir.path()).unwrap();
        let m = &source.migrations()[0];
        assert_eq!(m.down_sql.as_deref(), Some("DROP TABLE users;"));
    }

    #[test]
    fn migration_without_down_file() {
        let dir = create_temp_dir();

        fs::write(
            dir.path().join("0001_create_users.sql"),
            "CREATE TABLE users (id SERIAL PRIMARY KEY);",
        )
        .unwrap();

        let source = MigrationSource::from_dir(dir.path()).unwrap();
        assert!(source.migrations()[0].down_sql.is_none());
    }

    #[test]
    fn down_file_not_counted_as_migration() {
        let dir = create_temp_dir();

        fs::write(dir.path().join("0001_init.sql"), "CREATE TABLE t();").unwrap();
        fs::write(dir.path().join("0001_init.down.sql"), "DROP TABLE t;").unwrap();
        fs::write(dir.path().join("0002_more.sql"), "CREATE TABLE t2();").unwrap();

        let source = MigrationSource::from_dir(dir.path()).unwrap();
        assert_eq!(source.migrations().len(), 2);
    }

    #[test]
    fn find_by_name() {
        let dir = create_temp_dir();

        fs::write(dir.path().join("0001_init.sql"), "SELECT 1;").unwrap();
        fs::write(dir.path().join("0002_more.sql"), "SELECT 2;").unwrap();

        let source = MigrationSource::from_dir(dir.path()).unwrap();
        assert!(source.find("0001_init").is_some());
        assert!(source.find("0002_more").is_some());
        assert!(source.find("0003_nope").is_none());
    }

    #[test]
    fn ignores_non_sql_files() {
        let dir = create_temp_dir();

        fs::write(
            dir.path().join("0001_create_users.sql"),
            "CREATE TABLE users();",
        )
        .unwrap();
        fs::write(dir.path().join("README.md"), "# Migrations").unwrap();

        let source = MigrationSource::from_dir(dir.path()).unwrap();
        assert_eq!(source.migrations().len(), 1);
    }

    #[test]
    fn missing_directory_returns_empty() {
        let source = MigrationSource::from_dir(Path::new("/nonexistent/path/migrations")).unwrap();
        assert!(source.migrations().is_empty());
    }

    #[test]
    fn errors_on_invalid_format_no_underscore() {
        let dir = create_temp_dir();
        fs::write(dir.path().join("createusers.sql"), "SELECT 1;").unwrap();

        let result = MigrationSource::from_dir(dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn errors_on_non_numeric_prefix() {
        let dir = create_temp_dir();
        fs::write(dir.path().join("abc_create_users.sql"), "SELECT 1;").unwrap();

        let result = MigrationSource::from_dir(dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn errors_on_empty_description() {
        let dir = create_temp_dir();
        fs::write(dir.path().join("0001_.sql"), "SELECT 1;").unwrap();

        let result = MigrationSource::from_dir(dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn empty_directory_returns_empty_list() {
        let dir = create_temp_dir();
        let source = MigrationSource::from_dir(dir.path()).unwrap();
        assert!(source.migrations().is_empty());
    }

    #[test]
    fn ignores_subdirectories() {
        let dir = create_temp_dir();
        fs::create_dir(dir.path().join("subdir")).unwrap();
        fs::write(dir.path().join("0001_init.sql"), "SELECT 1;").unwrap();

        let source = MigrationSource::from_dir(dir.path()).unwrap();
        assert_eq!(source.migrations().len(), 1);
    }

    #[test]
    fn detects_no_transaction_comment() {
        let dir = create_temp_dir();
        fs::write(
            dir.path().join("0001_create_index.sql"),
            "-- no-transaction\nCREATE INDEX CONCURRENTLY idx ON t(col);",
        )
        .unwrap();

        let source = MigrationSource::from_dir(dir.path()).unwrap();
        assert!(source.migrations()[0].no_transaction);
    }

    #[test]
    fn no_transaction_with_whitespace() {
        let dir = create_temp_dir();
        fs::write(
            dir.path().join("0001_create_index.sql"),
            "  -- no-transaction  \nCREATE INDEX CONCURRENTLY idx ON t(col);",
        )
        .unwrap();

        let source = MigrationSource::from_dir(dir.path()).unwrap();
        assert!(source.migrations()[0].no_transaction);
    }

    #[test]
    fn from_embedded_orders_by_version() {
        let source = MigrationSource::from_embedded([
            ("0002_b", "SELECT 2;", None),
            ("0001_a", "SELECT 1;", Some("DROP TABLE a;")),
            ("0003_c", "SELECT 3;", None),
        ])
        .unwrap();
        let m = source.migrations();
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].name, "0001_a");
        assert_eq!(m[0].down_sql.as_deref(), Some("DROP TABLE a;"));
        assert_eq!(m[1].name, "0002_b");
        assert_eq!(m[2].name, "0003_c");
    }

    #[test]
    fn from_embedded_detects_no_transaction_directive() {
        let source = MigrationSource::from_embedded([(
            "0001_idx",
            "-- no-transaction\nCREATE INDEX CONCURRENTLY idx ON t(col);",
            None,
        )])
        .unwrap();
        assert!(source.migrations()[0].no_transaction);
    }

    #[test]
    fn from_embedded_rejects_invalid_stem() {
        let err = MigrationSource::from_embedded([("not_numeric_prefix", "SELECT 1;", None)])
            .unwrap_err();
        assert!(format!("{err}").contains("numeric prefix"));
    }

    #[test]
    fn normal_migration_is_transactional() {
        let dir = create_temp_dir();
        fs::write(
            dir.path().join("0001_create_users.sql"),
            "CREATE TABLE users (id SERIAL PRIMARY KEY);",
        )
        .unwrap();

        let source = MigrationSource::from_dir(dir.path()).unwrap();
        assert!(!source.migrations()[0].no_transaction);
    }
}
