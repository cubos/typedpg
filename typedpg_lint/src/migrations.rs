//! The migration and schema rules: what a migration does to a table that
//! already has rows, and what the schema it leaves behind costs at runtime.
//!
//! Migration rules run on the catalog state before each statement, so a
//! table created earlier in the same file is empty and has no hazard. Schema
//! rules run once, on the catalog after every migration, and are placed at
//! the statement that created the object.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use typedpg_analyzer::{
    ConType, DdlError, MigrationObserver, MigrationStatement, PgAttribute, PgCatalog, PgClass,
    PgClassOid, PgConstraintOid, PgIndex, PgProcOid, PgTypeOid, ProKind, RelKind, TypCategory,
};
use typedpg_core::{QualifiedName, quote_identifier};
use typedpg_pg_query::ast::{
    AConstValue, AlterTableType, BoolExprType, ConstrType, Node, Node as N, NullTestType,
    ObjectType, SortByDir, SortByNulls, TransactionStmtKind, VariableSetKind,
};

use crate::{AllowComment, Finding, LintOptions, RawFinding, Rule, Severity, allow_comment};

/// What [`lint_migrations`] reports for a migration file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationLint {
    /// Every rule runs: the statement rules, the schema rules and the
    /// file's allow comments are validated.
    All,
    /// The statement rules are skipped, but the schema rules still run over
    /// the final catalog and place their findings on the statements that
    /// created the objects — as do the file's own allow comments, which are
    /// still validated. Files unchanged from `--previous-deployment` and
    /// files matched by `lint.ignore` use this: their history cannot be
    /// changed, but the schema they leave behind is still linted.
    SchemaOnly,
    /// Nothing is reported: the file is somebody else's schema
    /// (`extra_migrations`), applied only so the catalog can be built.
    None,
}

/// One migration file given to [`lint_migrations`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationFile {
    /// The path diagnostics show.
    pub path: String,
    /// The file's text.
    pub sql: String,
    /// What the linter reports for this file.
    pub lint: MigrationLint,
}

/// A finding in a migration file: an index into the input slice and the
/// finding, whose span is a byte range in that file's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationFinding {
    pub file: usize,
    pub finding: Finding,
}

/// The findings of [`lint_migrations`], and the catalog the migrations
/// built — the schema their queries are analyzed against.
#[derive(Clone)]
pub struct LintedMigrations {
    pub findings: Vec<MigrationFinding>,
    pub catalog: PgCatalog,
}

/// Lint the migrations, applying them in order to build the schema they
/// leave behind.
///
/// `use_transaction` is the database runner's setting: it decides which
/// statements can run at all (a `CREATE INDEX CONCURRENTLY` in a wrapped
/// migration is an error, as it is at runtime).
pub fn lint_migrations(
    files: &[MigrationFile],
    use_transaction: bool,
    options: &LintOptions,
) -> Result<LintedMigrations, DdlError> {
    let mut catalog = PgCatalog::new().map_err(|error| {
        DdlError::Internal(format!("failed to load the PG catalog seed: {error}"))
    })?;
    catalog.set_migrations_use_transaction(use_transaction);
    let mut linter = MigrationLinter {
        files,
        options,
        current: 0,
        pre_existing: HashSet::new(),
        findings: Vec::new(),
        created_indexes: HashMap::new(),
        created_constraints: HashMap::new(),
        column_creators: HashMap::new(),
        before_classes: HashSet::new(),
        before_indexes: HashSet::new(),
        before_constraints: HashSet::new(),
        before_relation: None,
        before_columns: HashSet::new(),
        orderings: HashMap::new(),
        comments: Vec::new(),
        used_comments: HashSet::new(),
        reported_comments: HashSet::new(),
        current_suppressed: HashSet::new(),
        current_span: 0..0,
        last_statement_end: 0,
        use_transaction,
        lock_timeout_set: false,
        lock_timeout_local: None,
        transaction_open: false,
        transaction_snapshot: None,
        savepoints: Vec::new(),
        lock_timeout_reported: false,
        created_relations: HashMap::new(),
        created_policies: HashMap::new(),
        rls_enablers: HashMap::new(),
        created_routines: HashMap::new(),
        before_routines: HashSet::new(),
        not_valid_in_transaction: HashSet::new(),
    };
    for (file, migration) in files.iter().enumerate() {
        linter.begin_file(file, &catalog);
        catalog.apply_migration_observed(&migration.path, &migration.sql, &mut linter)?;
        linter.end_file();
    }
    linter.finish(&catalog);
    let mut findings = linter.findings;
    findings.sort_by(|a, b| {
        (
            a.file,
            a.finding.span.start,
            a.finding.span.end,
            a.finding.rule,
        )
            .cmp(&(
                b.file,
                b.finding.span.start,
                b.finding.span.end,
                b.finding.rule,
            ))
    });
    Ok(LintedMigrations { findings, catalog })
}

/// A `typedpg-lint` comment in a migration file.
struct Comment {
    start: usize,
    end: usize,
    kind: CommentKind,
}

enum CommentKind {
    Allow(Vec<String>),
    Malformed,
}

struct Creator {
    file: usize,
    span: Range<usize>,
    suppressed: HashSet<Rule>,
}

/// What a schema rule's finding is about, to find the statement that created
/// it.
enum CreatedObject {
    Constraint(PgConstraintOid),
    Index(PgClassOid),
    Column(PgClassOid, i16),
    /// A table: the statement that created it.
    Relation(PgClassOid),
    /// A policy on a table: the `CREATE POLICY` that created it, else the
    /// table's creator.
    Policy(PgClassOid, String),
    /// A table's row-level security: the `ALTER TABLE … ENABLE ROW LEVEL
    /// SECURITY` that turned it on, else the table's creator.
    RowSecurity(PgClassOid),
    /// A function or procedure: the `CREATE [OR REPLACE]` that defined it.
    Routine(PgProcOid),
}

/// A schema rule's finding, before it is placed on its creator's statement.
struct SchemaFinding {
    object: CreatedObject,
    raw: RawFinding,
    /// The help when the creator is in a [`MigrationLint::SchemaOnly`] file:
    /// its statement cannot change, so the fix is a new migration.
    help_in_new_migration: Option<String>,
}

struct MigrationLinter<'a> {
    files: &'a [MigrationFile],
    options: &'a LintOptions,
    current: usize,
    /// The relations that existed before the current file began.
    pre_existing: HashSet<PgClassOid>,
    findings: Vec<MigrationFinding>,
    created_indexes: HashMap<PgClassOid, Creator>,
    created_constraints: HashMap<PgConstraintOid, Creator>,
    /// For each column of a table, the last statement that gave it its
    /// current type: `CREATE TABLE`, `ADD COLUMN` or `ALTER COLUMN TYPE`.
    column_creators: HashMap<(PgClassOid, i16), Creator>,
    /// The relations that existed before the current statement: a
    /// `CREATE TABLE IF NOT EXISTS` that found its table already there
    /// created nothing, so it defines no column's type.
    before_classes: HashSet<PgClassOid>,
    before_indexes: HashSet<PgClassOid>,
    before_constraints: HashSet<PgConstraintOid>,
    /// The columns of the relation an `ALTER TABLE` targets, before that
    /// statement: an `ADD COLUMN IF NOT EXISTS` that skipped an existing one
    /// defines no new type.
    before_relation: Option<PgClassOid>,
    before_columns: HashSet<i16>,
    /// Each index's key columns' order, `(descending, nulls first)`, from
    /// the `CREATE INDEX` that made it (the catalog keeps no `indoption`).
    /// An index a constraint or `CREATE TABLE` made has none: ascending,
    /// nulls last.
    orderings: HashMap<PgClassOid, Vec<(bool, bool)>>,
    comments: Vec<Comment>,
    used_comments: HashSet<usize>,
    reported_comments: HashSet<usize>,
    current_suppressed: HashSet<Rule>,
    current_span: Range<usize>,
    /// Where the statement before this one ended: the comments between the
    /// two lead into this one even when the parser's `stmt_location` starts
    /// at its first token.
    last_statement_end: usize,
    /// The database runner's setting, which decides whether a file is
    /// wrapped in a transaction (`used_transaction` also checks the file's
    /// `-- no-transaction` marker).
    use_transaction: bool,
    /// The session's `lock_timeout` is set to a non-zero value.
    lock_timeout_set: bool,
    /// A `SET LOCAL` / `set_config(…, true)` override in the open
    /// transaction: `Some(true)` for a proven non-zero timeout, `Some(false)`
    /// for zero or a value the lint cannot evaluate. `None` when the
    /// transaction has no local override.
    lock_timeout_local: Option<bool>,
    /// Whether a transaction is open: the file's runner transaction, or an
    /// explicit `BEGIN` / `START TRANSACTION`.
    transaction_open: bool,
    /// The `(session, local)` timeout state at `BEGIN`. PostgreSQL rolls GUC
    /// changes back with the transaction, so a full `ROLLBACK` restores it.
    transaction_snapshot: Option<(bool, Option<bool>)>,
    /// The `(session, local)` state at each open `SAVEPOINT`, innermost last:
    /// `ROLLBACK TO` restores it and `RELEASE` drops it.
    savepoints: Vec<(String, (bool, Option<bool>))>,
    /// Whether `missing_lock_timeout` already reported a statement of the
    /// current file: at most one finding per file.
    lock_timeout_reported: bool,
    /// The statement that created each table (`CREATE TABLE`, `CREATE
    /// TABLE AS`).
    created_relations: HashMap<PgClassOid, Creator>,
    /// The `CREATE POLICY` of each policy, by table and policy name.
    created_policies: HashMap<(PgClassOid, String), Creator>,
    /// The last `ALTER TABLE … ENABLE ROW LEVEL SECURITY` of each table.
    rls_enablers: HashMap<PgClassOid, Creator>,
    /// The last `CREATE [OR REPLACE] FUNCTION / PROCEDURE` of each routine.
    created_routines: HashMap<PgProcOid, Creator>,
    /// The migrations' routines before a `CREATE FUNCTION`, to find the one
    /// it creates.
    before_routines: HashSet<PgProcOid>,
    /// The constraints the open transaction added `NOT VALID` to a table
    /// that existed before the file: their `ADD CONSTRAINT` still holds its
    /// lock until the transaction ends.
    not_valid_in_transaction: HashSet<PgConstraintOid>,
}

impl MigrationLinter<'_> {
    fn begin_file(&mut self, file: usize, catalog: &PgCatalog) {
        self.current = file;
        // Every relation, not just the tables: the lock rule also looks at
        // indexes (`DROP INDEX`) and materialized views (`REFRESH`).
        self.pre_existing = catalog.iter_classes().map(|class| class.oid).collect();
        self.comments = comments(&self.files[file].sql);
        self.used_comments.clear();
        self.reported_comments.clear();
        self.last_statement_end = 0;
        self.lock_timeout_set = false;
        self.lock_timeout_local = None;
        self.transaction_open = self.file_runs_in_transaction();
        self.transaction_snapshot = self.transaction_open.then_some((false, None));
        self.savepoints.clear();
        self.lock_timeout_reported = false;
        self.not_valid_in_transaction.clear();
    }

    /// Report the invalid allow comments of a file whose statements never
    /// reached them (an empty file, or one that failed to parse).
    fn end_file(&mut self) {
        if self.files[self.current].lint == MigrationLint::None {
            return;
        }
        let comments = std::mem::take(&mut self.comments);
        for (i, comment) in comments.iter().enumerate() {
            if !self.used_comments.contains(&i) {
                self.report_comment_error(i, comment);
            }
        }
        self.comments = comments;
    }

    fn report_comment_error(&mut self, i: usize, comment: &Comment) {
        if !self.reported_comments.insert(i) {
            return;
        }
        let message = match &comment.kind {
            CommentKind::Malformed => "malformed typedpg-lint comment: expected \
                                       `typedpg-lint: allow(rule, ...)` and nothing after it"
                .to_owned(),
            CommentKind::Allow(names) => {
                let Some(name) = names.iter().find(|name| Rule::from_name(name).is_none()) else {
                    return;
                };
                let valid = Rule::ALL
                    .iter()
                    .map(|rule| rule.name())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "unknown lint rule `{name}` in a typedpg-lint allow comment: \
                     expected one of {valid}"
                )
            }
        };
        self.findings.push(MigrationFinding {
            file: self.current,
            finding: Finding {
                rule: Rule::InvalidAllow,
                severity: Severity::Error,
                span: comment.start..comment.end,
                message,
                help: None,
                fix: Vec::new(),
            },
        });
    }

    /// The rules the comments inside `span` suppress, reporting the invalid
    /// ones.
    fn suppressions(&mut self, span: &Range<usize>) -> HashSet<Rule> {
        let mut suppressed = HashSet::new();
        for i in 0..self.comments.len() {
            let comment = &self.comments[i];
            let inside = comment.start >= span.start && comment.start < span.end;
            // A leading comment may sit just before the parser's
            // `stmt_location`, which starts at the first token.
            let leading = comment.end <= span.start && comment.start >= self.last_statement_end;
            if !inside && !leading {
                continue;
            }
            self.used_comments.insert(i);
            match &comment.kind {
                CommentKind::Malformed => {}
                CommentKind::Allow(names) => {
                    for name in names {
                        if let Some(rule) = Rule::from_name(name) {
                            suppressed.insert(rule);
                        }
                    }
                }
            }
            let kind = match &comment.kind {
                CommentKind::Malformed => CommentKind::Malformed,
                CommentKind::Allow(names) => CommentKind::Allow(names.clone()),
            };
            let comment = Comment {
                start: comment.start,
                end: comment.end,
                kind,
            };
            self.report_comment_error(i, &comment);
        }
        suppressed
    }

    /// `span` without the whitespace and comments that lead into it.
    fn trimmed(&self, span: &Range<usize>) -> Range<usize> {
        let sql = &self.files[self.current].sql;
        let Some(text) = sql.get(span.start..span.end) else {
            return span.clone();
        };
        let Ok(scanned) = typedpg_pg_query::scan(text) else {
            return span.clone();
        };
        let tokens: Vec<_> = scanned
            .tokens
            .iter()
            .filter_map(|token| {
                if matches!(
                    token.token,
                    typedpg_pg_query::ast::Token::SqlComment
                        | typedpg_pg_query::ast::Token::CComment
                ) {
                    return None;
                }
                Some((
                    usize::try_from(token.start).ok()?,
                    usize::try_from(token.end).ok()?,
                ))
            })
            .collect();
        let (Some((first, _)), Some((_, last))) = (tokens.first(), tokens.last()) else {
            return span.clone();
        };
        span.start + first..span.start + last
    }

    fn report(&mut self, rule: Rule, span: Range<usize>, raw: RawFinding) {
        let Some(severity) = self.options.level(rule) else {
            return;
        };
        if self.current_suppressed.contains(&rule) {
            return;
        }
        self.findings.push(MigrationFinding {
            file: self.current,
            finding: Finding {
                rule,
                severity,
                span,
                message: raw.message,
                help: raw.help,
                fix: Vec::new(),
            },
        });
    }

    fn table(
        &self,
        catalog: &PgCatalog,
        relation: &typedpg_pg_query::ast::RangeVar,
    ) -> Option<(PgClassOid, String)> {
        let schema = (!relation.schemaname.is_empty()).then_some(relation.schemaname.as_str());
        let class = catalog.resolve_table(schema, &relation.relname)?;
        if !self.pre_existing.contains(&class.oid)
            || !matches!(
                class.relkind,
                RelKind::Table | RelKind::Partitioned | RelKind::ForeignTable
            )
        {
            return None;
        }
        Some((class.oid, qualified_name(catalog, class)))
    }

    fn statement_rules(&mut self, catalog: &PgCatalog, statement: &MigrationStatement<'_>) {
        if self.files[self.current].lint != MigrationLint::All {
            return;
        }
        // A `SET`/`RESET`/`set_config` of `lock_timeout` changes it for the
        // statements that follow, and `BEGIN`/`COMMIT` decide how long a
        // LOCAL setting — and an `ADD CONSTRAINT`'s lock — lasts, whichever
        // rules are enabled.
        self.track_lock_timeout(catalog, statement.node);
        for rule in [
            Rule::CreateIndexBlocking,
            Rule::ConstraintValidating,
            Rule::SetNotNullScan,
            Rule::TableRewrite,
            Rule::MissingLockTimeout,
            Rule::DropIndexBlocking,
            Rule::BlockingRebuild,
            Rule::DetachPartitionBlocking,
            Rule::ValidateInSameTransaction,
            Rule::AttachPartitionScan,
        ] {
            if self.options.level(rule).is_none() || self.current_suppressed.contains(&rule) {
                continue;
            }
            let findings = match (rule, statement.node) {
                (Rule::CreateIndexBlocking, N::IndexStmt(index)) => {
                    self.create_index_blocking(catalog, index)
                }
                (Rule::CreateIndexBlocking, N::AlterTableStmt(alter)) => {
                    self.constraint_index_blocking(catalog, alter)
                }
                (Rule::ConstraintValidating, N::AlterTableStmt(alter)) => {
                    self.constraint_validating(catalog, alter)
                }
                (Rule::SetNotNullScan, N::AlterTableStmt(alter)) => {
                    self.set_not_null_scan(catalog, alter)
                }
                (Rule::TableRewrite, N::AlterTableStmt(alter)) => {
                    self.table_rewrite(catalog, alter)
                }
                (Rule::MissingLockTimeout, _) => self.missing_lock_timeout(catalog, statement.node),
                (Rule::DropIndexBlocking, N::DropStmt(drop)) => {
                    self.drop_index_blocking(catalog, drop)
                }
                (Rule::BlockingRebuild, _) => self.blocking_rebuild(catalog, statement.node),
                (Rule::DetachPartitionBlocking, N::AlterTableStmt(alter)) => {
                    self.detach_partition_blocking(catalog, alter)
                }
                (Rule::ValidateInSameTransaction, N::AlterTableStmt(alter)) => {
                    self.validate_in_same_transaction(catalog, alter)
                }
                (Rule::AttachPartitionScan, N::AlterTableStmt(alter)) => {
                    self.attach_partition_scan(catalog, alter)
                }
                _ => Vec::new(),
            };
            for raw in findings {
                self.report(rule, self.current_span.clone(), raw);
            }
        }
    }

    fn create_index_blocking(
        &self,
        catalog: &PgCatalog,
        index: &typedpg_pg_query::ast::IndexStmt,
    ) -> Vec<RawFinding> {
        if index.concurrent {
            return Vec::new();
        }
        let Some(relation) = index.relation.as_ref() else {
            return Vec::new();
        };
        let Some((_, table)) = self.table(catalog, relation) else {
            return Vec::new();
        };
        vec![RawFinding {
            span: 0..0,
            message: format!(
                "CREATE INDEX without CONCURRENTLY blocks writes on {table} for the whole build"
            ),
            help: Some(
                "use CREATE INDEX CONCURRENTLY, in a migration whose first line is \
                 -- no-transaction"
                    .to_owned(),
            ),
            fix: Vec::new(),
        }]
    }

    /// `ADD PRIMARY KEY` / `UNIQUE` / `EXCLUDE` (or a column with one) builds
    /// the constraint's index as `CREATE INDEX` does, unless it adopts one
    /// with `USING INDEX`.
    fn constraint_index_blocking(
        &self,
        catalog: &PgCatalog,
        alter: &typedpg_pg_query::ast::AlterTableStmt,
    ) -> Vec<RawFinding> {
        let Some(relation) = alter.relation.as_ref() else {
            return Vec::new();
        };
        let Some((_, table)) = self.table(catalog, relation) else {
            return Vec::new();
        };
        // The constraints that build an index: a UNIQUE / PRIMARY KEY one
        // (`true`), which can adopt an index built concurrently instead, or
        // an EXCLUDE one (`false`), which can't.
        let builds_index = |constraint: &typedpg_pg_query::ast::Constraint| match constraint.contype
        {
            _ if !constraint.indexname.is_empty() => None,
            ConstrType::ConstrPrimary | ConstrType::ConstrUnique => Some(true),
            ConstrType::ConstrExclusion => Some(false),
            _ => None,
        };
        let mut built = Vec::new();
        for command in &alter.cmds {
            let Some(command) = alter_command(command.as_ref()) else {
                continue;
            };
            match (command.subtype, command.def.as_ref()) {
                (AlterTableType::AtAddConstraint, Some(N::Constraint(constraint))) => {
                    built.extend(builds_index(constraint));
                }
                (AlterTableType::AtAddColumn, Some(N::ColumnDef(column))) => {
                    built.extend(column.constraints.iter().filter_map(
                        |node| match node.as_ref() {
                            Some(N::Constraint(constraint)) => builds_index(constraint),
                            _ => None,
                        },
                    ));
                }
                _ => {}
            }
        }
        if built.is_empty() {
            return Vec::new();
        }
        // Unlike CREATE INDEX (SHARE), ALTER TABLE builds a constraint's
        // index under ACCESS EXCLUSIVE (`AlterTableGetLockLevel`: PRIMARY
        // KEY, UNIQUE and EXCLUDE constraints, and ADD COLUMN), so reads
        // wait too.
        let help = if built.contains(&true) {
            "CREATE UNIQUE INDEX CONCURRENTLY first (in a migration whose first line is \
             -- no-transaction), then ADD CONSTRAINT … UNIQUE / PRIMARY KEY USING INDEX, which \
             only takes the lock briefly"
        } else {
            "an EXCLUDE constraint can't adopt an index built CONCURRENTLY: add it while the \
             table is small, or when the table can be locked for the whole build"
        };
        vec![RawFinding {
            span: 0..0,
            message: format!(
                "adding the constraint builds its index under an ACCESS EXCLUSIVE lock, blocking \
                 reads and writes on {table} for the whole build"
            ),
            help: Some(help.to_owned()),
            fix: Vec::new(),
        }]
    }

    fn constraint_validating(
        &self,
        catalog: &PgCatalog,
        alter: &typedpg_pg_query::ast::AlterTableStmt,
    ) -> Vec<RawFinding> {
        let Some(relation) = alter.relation.as_ref() else {
            return Vec::new();
        };
        let Some((relid, table)) = self.table(catalog, relation) else {
            return Vec::new();
        };
        // PostgreSQL decides `skipValidation` once for the whole statement
        // (`transformAlterTableStmt` / `transformFKConstraints`,
        // src/backend/parser/parse_utilcmd.c, PG 18): it starts true and
        // becomes false when any `ADD COLUMN` leaves a `raw_default` on the
        // column — after `transformColumnDefinition`, which turns
        // `serial` into a `nextval` DEFAULT and a GENERATED expression into
        // `raw_default`, while identity does not — or when any
        // `ADD CONSTRAINT` is a foreign key. Only when it stays true does
        // `transformFKConstraints` mark every FK of the statement
        // `skip_validation`, so one defaulted column makes all of them scan.
        let mut skip_validation = true;
        for command in &alter.cmds {
            let Some(command) = alter_command(command.as_ref()) else {
                continue;
            };
            match (command.subtype, command.def.as_ref()) {
                (AlterTableType::AtAddColumn, Some(N::ColumnDef(column))) => {
                    skip_validation &= !column_has_raw_default(column);
                }
                (AlterTableType::AtAddConstraint, Some(N::Constraint(constraint))) => {
                    skip_validation &= !constraint_is_foreign_key(constraint);
                }
                _ => {}
            }
        }
        let mut found = false;
        for command in &alter.cmds {
            let Some(command) = alter_command(command.as_ref()) else {
                continue;
            };
            match command.subtype {
                AlterTableType::AtAddConstraint => {
                    let Some(N::Constraint(constraint)) = command.def.as_ref() else {
                        continue;
                    };
                    if validates(constraint) {
                        found = true;
                    }
                }
                AlterTableType::AtAddColumn => {
                    let Some(N::ColumnDef(column)) = command.def.as_ref() else {
                        continue;
                    };
                    // `ADD COLUMN IF NOT EXISTS` on an existing column adds
                    // nothing (constraints included).
                    if catalog
                        .attributes_of(relid)
                        .iter()
                        .any(|attribute| attribute.attname == column.colname)
                    {
                        continue;
                    }
                    // A foreign key on a column added by this statement has
                    // nothing to check only when PostgreSQL skips the
                    // validation: every added column has no default and the
                    // statement adds no other FK.
                    for node in &column.constraints {
                        if let Some(N::Constraint(constraint)) = node.as_ref()
                            && validates(constraint)
                            && !(skip_validation && constraint_is_foreign_key(constraint))
                        {
                            found = true;
                        }
                    }
                }
                _ => {}
            }
        }
        if found {
            vec![RawFinding {
                span: 0..0,
                message: format!("adding a constraint validates every row of {table} under a lock"),
                help: Some(
                    "add it NOT VALID, then VALIDATE CONSTRAINT in a later migration".to_owned(),
                ),
                fix: Vec::new(),
            }]
        } else {
            Vec::new()
        }
    }

    fn set_not_null_scan(
        &self,
        catalog: &PgCatalog,
        alter: &typedpg_pg_query::ast::AlterTableStmt,
    ) -> Vec<RawFinding> {
        let Some(relation) = alter.relation.as_ref() else {
            return Vec::new();
        };
        let Some((relid, table)) = self.table(catalog, relation) else {
            return Vec::new();
        };
        let mut columns = Vec::new();
        for command in &alter.cmds {
            let Some(command) = alter_command(command.as_ref()) else {
                continue;
            };
            if command.subtype != AlterTableType::AtSetNotNull {
                continue;
            }
            let already = catalog
                .attributes_of(relid)
                .iter()
                .any(|attribute| attribute.attname == command.name && attribute.attnotnull);
            if !already && !proven_not_null(catalog, relid, &command.name) {
                columns.push(quote_identifier(&command.name));
            }
        }
        if !columns.is_empty() {
            vec![RawFinding {
                span: 0..0,
                message: format!("SET NOT NULL scans every row of {table} under a lock"),
                help: Some(format!(
                    "add CHECK ({} IS NOT NULL) NOT VALID, VALIDATE it, then SET NOT NULL",
                    columns.join(" IS NOT NULL AND ")
                )),
                fix: Vec::new(),
            }]
        } else {
            Vec::new()
        }
    }

    fn table_rewrite(
        &self,
        catalog: &PgCatalog,
        alter: &typedpg_pg_query::ast::AlterTableStmt,
    ) -> Vec<RawFinding> {
        let Some(relation) = alter.relation.as_ref() else {
            return Vec::new();
        };
        let Some((relid, table)) = self.table(catalog, relation) else {
            return Vec::new();
        };
        // SET LOGGED / UNLOGGED, SET ACCESS METHOD and SET TABLESPACE act
        // on the table's storage: a partitioned table has none (they only
        // set what its future partitions get), a foreign table neither.
        let has_storage = catalog
            .class_row(relid)
            .is_some_and(|class| class.relkind == RelKind::Table);
        // A rewrite a column change causes (ALTER TYPE, a volatile or
        // identity default, a stored generated column), and one of the whole
        // table (persistence, access method).
        let mut found = false;
        let mut table_level = false;
        let mut moved = false;
        for command in &alter.cmds {
            let Some(command) = alter_command(command.as_ref()) else {
                continue;
            };
            match command.subtype {
                AlterTableType::AtAlterColumnType => {
                    let Some(N::ColumnDef(column)) = command.def.as_ref() else {
                        continue;
                    };
                    let Some(type_name) = column.type_name.as_ref() else {
                        found = true;
                        continue;
                    };
                    // The USING expression (`column.raw_default`) is
                    // planned like PostgreSQL's ATColumnChangeRequiresRewrite
                    // does: a bare reference to the same column under a
                    // binary-coercible change computes nothing and does not
                    // rewrite.
                    if catalog
                        .column_type_rewrites(
                            relid,
                            &command.name,
                            type_name,
                            column.raw_default.as_ref(),
                        )
                        .unwrap_or(true)
                    {
                        found = true;
                    }
                }
                AlterTableType::AtAddColumn => {
                    let Some(N::ColumnDef(column)) = command.def.as_ref() else {
                        continue;
                    };
                    // `ADD COLUMN IF NOT EXISTS` on a column that already
                    // exists adds nothing: no default to evaluate, no row to
                    // rewrite.
                    if command.missing_ok
                        && catalog
                            .attributes_of(relid)
                            .iter()
                            .any(|attribute| attribute.attname == column.colname)
                    {
                        continue;
                    }
                    // `serial` expands to a `nextval()` DEFAULT, volatile:
                    // every existing row gets its own value.
                    let mut volatile_default = column_type_name(column).is_some_and(|name| {
                        matches!(
                            name,
                            "smallserial"
                                | "serial2"
                                | "serial"
                                | "serial4"
                                | "bigserial"
                                | "serial8"
                        )
                    });
                    let mut generated_stored = column.generated == "s";
                    for constraint in &column.constraints {
                        let Some(N::Constraint(constraint)) = constraint.as_ref() else {
                            continue;
                        };
                        match constraint.contype {
                            ConstrType::ConstrDefault => {
                                volatile_default |= volatile(catalog, constraint.raw_expr.as_ref());
                            }
                            // An identity column's default is the
                            // sequence's next value (`NextValueExpr`),
                            // volatile like `nextval()` (ATExecAddColumn).
                            ConstrType::ConstrIdentity => volatile_default = true,
                            ConstrType::ConstrGenerated => {
                                // `generated_kind` is `s` for a stored
                                // column (`v` is virtual, with no storage to
                                // rewrite).
                                generated_stored |= constraint.generated_kind == "s";
                            }
                            _ => {}
                        }
                    }
                    if generated_stored || volatile_default {
                        found = true;
                    }
                }
                // ATPrepChangePersistence: setting the persistence the table
                // already has does nothing; changing it rewrites the table.
                AlterTableType::AtSetLogged if has_storage => {
                    table_level |= catalog.lint_relation_persistence(relid) == 'u';
                }
                AlterTableType::AtSetUnLogged if has_storage => {
                    table_level |= catalog.lint_relation_persistence(relid) == 'p';
                }
                // ATPrepSetAccessMethod: a new access method rewrites the
                // table. The catalog does not track a table's access method,
                // so every table is taken to use `heap` (the default
                // `default_table_access_method`): `SET ACCESS METHOD heap`
                // and `SET ACCESS METHOD DEFAULT` change nothing.
                AlterTableType::AtSetAccessMethod if has_storage => {
                    table_level |= !command.name.is_empty() && command.name != "heap";
                }
                // ATExecSetTableSpace copies every block of the table and
                // its indexes. Tablespaces are not tracked, so a move to the
                // tablespace the table is already in (a no-op) is reported
                // too.
                AlterTableType::AtSetTableSpace if has_storage => moved = true,
                _ => {}
            }
        }
        if found || table_level {
            let help = if found {
                "rewrite in steps: add a new column, backfill it in batches, then swap"
            } else {
                "create a new table with the new setting, copy the rows in batches, then swap the \
                 tables"
            };
            vec![RawFinding {
                span: 0..0,
                message: format!(
                    "ALTER TABLE rewrites every row of {table} under an ACCESS EXCLUSIVE lock"
                ),
                help: Some(help.to_owned()),
                fix: Vec::new(),
            }]
        } else if moved {
            vec![RawFinding {
                span: 0..0,
                message: format!(
                    "SET TABLESPACE copies every block of {table} under an ACCESS EXCLUSIVE lock"
                ),
                help: Some(
                    "create a new table in the new tablespace, copy the rows in batches, then swap \
                     the tables"
                        .to_owned(),
                ),
                fix: Vec::new(),
            }]
        } else {
            Vec::new()
        }
    }

    /// `DROP INDEX` without `CONCURRENTLY` takes ACCESS EXCLUSIVE on the
    /// index's table (RemoveRelations → RangeVarCallbackForDropRelation),
    /// held until the transaction ends.
    fn drop_index_blocking(
        &self,
        catalog: &PgCatalog,
        drop: &typedpg_pg_query::ast::DropStmt,
    ) -> Vec<RawFinding> {
        if drop.remove_type != ObjectType::ObjectIndex || drop.concurrent {
            return Vec::new();
        }
        let mut table = None;
        for object in &drop.objects {
            let Some(index) = object_relation(catalog, object.as_ref()) else {
                continue;
            };
            // A partitioned index cannot be dropped concurrently ("cannot
            // drop partitioned index concurrently"): there is nothing
            // better to suggest.
            if index.relkind != RelKind::Index || !self.pre_existing.contains(&index.oid) {
                continue;
            }
            let Some(parent) = catalog
                .iter_indexes()
                .find(|row| row.indexrelid == index.oid)
                .and_then(|row| catalog.class_row(row.indrelid))
            else {
                continue;
            };
            if self.pre_existing.contains(&parent.oid) {
                table = Some(qualified_name(catalog, parent));
                break;
            }
        }
        let Some(table) = table else {
            return Vec::new();
        };
        // DROP INDEX CONCURRENTLY drops one index per statement, without
        // CASCADE.
        let help = if drop.objects.len() > 1 {
            "use one DROP INDEX CONCURRENTLY per index, in a migration whose first line is \
             -- no-transaction"
        } else {
            "use DROP INDEX CONCURRENTLY, in a migration whose first line is -- no-transaction"
        };
        vec![RawFinding {
            span: 0..0,
            message: format!(
                "DROP INDEX without CONCURRENTLY takes an ACCESS EXCLUSIVE lock on {table}, \
                 blocking every query on the table"
            ),
            help: Some(help.to_owned()),
            fix: Vec::new(),
        }]
    }

    /// `REINDEX` without `CONCURRENTLY` (SHARE on the table, ACCESS
    /// EXCLUSIVE on each index), `VACUUM FULL`, `CLUSTER` and `REPACK`
    /// without `CONCURRENTLY` (ACCESS EXCLUSIVE on the table) hold their
    /// lock for the whole rebuild.
    fn blocking_rebuild(&self, catalog: &PgCatalog, node: &N) -> Vec<RawFinding> {
        let (kind, target) = match node {
            N::ReindexStmt(reindex) => {
                if option_enabled(&reindex.params, "concurrently") {
                    return Vec::new();
                }
                use typedpg_pg_query::ast::ReindexObjectType as R;
                let target = match reindex.kind {
                    R::ReindexObjectIndex | R::ReindexObjectTable => reindex
                        .relation
                        .as_ref()
                        .and_then(|relation| self.rebuilt_table(catalog, relation)),
                    R::ReindexObjectSchema => self
                        .any_pre_existing_table(catalog, Some(&reindex.name))
                        .then(|| {
                            format!("every table of schema {}", quote_identifier(&reindex.name))
                        }),
                    R::ReindexObjectDatabase => self
                        .any_pre_existing_table(catalog, None)
                        .then(|| "every table of the database".to_owned()),
                    // Only the system catalogs.
                    R::ReindexObjectSystem => None,
                };
                let Some(target) = target else {
                    return Vec::new();
                };
                return vec![RawFinding {
                    span: 0..0,
                    message: format!(
                        "REINDEX without CONCURRENTLY blocks writes on {target}, and reads that \
                         use the indexes it rebuilds, for the whole rebuild"
                    ),
                    help: Some(
                        "use REINDEX CONCURRENTLY, in a migration whose first line is \
                         -- no-transaction"
                            .to_owned(),
                    ),
                    fix: Vec::new(),
                }];
            }
            N::VacuumStmt(vacuum) => {
                if !vacuum.is_vacuumcmd || !option_enabled(&vacuum.options, "full") {
                    return Vec::new();
                }
                let target = if vacuum.rels.is_empty() {
                    self.any_pre_existing_table(catalog, None)
                        .then(|| "every table".to_owned())
                } else {
                    vacuum
                        .rels
                        .iter()
                        .find_map(|relation| match relation.as_ref()? {
                            N::VacuumRelation(relation) => {
                                self.rebuilt_table(catalog, relation.relation.as_ref()?)
                            }
                            _ => None,
                        })
                };
                ("VACUUM FULL", target)
            }
            N::RepackStmt(repack) => {
                if option_enabled(&repack.params, "concurrently") {
                    return Vec::new();
                }
                let kind = if repack.command == typedpg_pg_query::ast::RepackCommand::Cluster {
                    "CLUSTER"
                } else {
                    "REPACK"
                };
                let target = match repack
                    .relation
                    .as_ref()
                    .and_then(|relation| relation.relation.as_ref())
                {
                    Some(relation) => self.rebuilt_table(catalog, relation),
                    None => self
                        .any_pre_existing_table(catalog, None)
                        .then(|| "every table".to_owned()),
                };
                (kind, target)
            }
            _ => return Vec::new(),
        };
        let Some(target) = target else {
            return Vec::new();
        };
        vec![RawFinding {
            span: 0..0,
            message: format!(
                "{kind} rewrites {target} under an ACCESS EXCLUSIVE lock, blocking every query on \
                 it for the whole rebuild"
            ),
            help: Some(
                "use REPACK (CONCURRENTLY), in a migration whose first line is -- no-transaction"
                    .to_owned(),
            ),
            fix: Vec::new(),
        }]
    }

    /// The table a rebuild of `relation` (a table, a materialized view or an
    /// index) locks, when it existed before the file.
    fn rebuilt_table(
        &self,
        catalog: &PgCatalog,
        relation: &typedpg_pg_query::ast::RangeVar,
    ) -> Option<String> {
        let class = resolve_relation(catalog, relation)?;
        if !self.pre_existing.contains(&class.oid) {
            return None;
        }
        let table = match class.relkind {
            RelKind::Index | RelKind::PartitionedIndex => catalog
                .iter_indexes()
                .find(|row| row.indexrelid == class.oid)
                .and_then(|row| catalog.class_row(row.indrelid))?,
            RelKind::Table | RelKind::Partitioned | RelKind::MaterializedView => class,
            _ => return None,
        };
        Some(qualified_name(catalog, table))
    }

    /// Whether a table (or materialized view) of a user schema — of
    /// `schema` when given — existed before the file.
    fn any_pre_existing_table(&self, catalog: &PgCatalog, schema: Option<&str>) -> bool {
        catalog.iter_classes().any(|class| {
            matches!(class.relkind, RelKind::Table | RelKind::MaterializedView)
                && self.pre_existing.contains(&class.oid)
                && catalog
                    .namespace_name(class.relnamespace)
                    .is_some_and(|name| match schema {
                        Some(schema) => name == schema,
                        None => name != "information_schema" && !name.starts_with("pg_"),
                    })
        })
    }

    /// `DETACH PARTITION` without `CONCURRENTLY` takes ACCESS EXCLUSIVE on
    /// the partitioned table and the partition (AlterTableGetLockLevel).
    fn detach_partition_blocking(
        &self,
        catalog: &PgCatalog,
        alter: &typedpg_pg_query::ast::AlterTableStmt,
    ) -> Vec<RawFinding> {
        let detaches = alter.cmds.iter().any(|command| {
            alter_command(command.as_ref()).is_some_and(|command| {
                command.subtype == AlterTableType::AtDetachPartition
                    && matches!(
                        command.def.as_ref(),
                        Some(N::PartitionCmd(partition)) if !partition.concurrent
                    )
            })
        });
        if !detaches {
            return Vec::new();
        }
        let Some((relid, table)) = alter
            .relation
            .as_ref()
            .and_then(|relation| self.table(catalog, relation))
        else {
            return Vec::new();
        };
        // DETACH PARTITION … CONCURRENTLY is refused while the table has a
        // default partition (ATExecDetachPartition).
        let help = if catalog.lint_has_default_partition(relid) {
            format!(
                "CONCURRENTLY is not allowed while {table} has a default partition: set a short \
                 lock_timeout and retry the detach on timeout"
            )
        } else {
            "use DETACH PARTITION … CONCURRENTLY, in a migration whose first line is \
             -- no-transaction"
                .to_owned()
        };
        vec![RawFinding {
            span: 0..0,
            message: format!(
                "DETACH PARTITION without CONCURRENTLY takes an ACCESS EXCLUSIVE lock on {table}, \
                 blocking every query on the partitioned table"
            ),
            help: Some(help),
            fix: Vec::new(),
        }]
    }

    /// `VALIDATE CONSTRAINT` of a constraint the same transaction added `NOT
    /// VALID`: `ADD CONSTRAINT`'s lock (ACCESS EXCLUSIVE for a CHECK, SHARE
    /// ROW EXCLUSIVE for a foreign key) is held until the transaction ends,
    /// so the validation scan runs under it, as a plain `ADD CONSTRAINT`'s
    /// would.
    fn validate_in_same_transaction(
        &self,
        catalog: &PgCatalog,
        alter: &typedpg_pg_query::ast::AlterTableStmt,
    ) -> Vec<RawFinding> {
        let Some((relid, table)) = alter
            .relation
            .as_ref()
            .and_then(|relation| self.table(catalog, relation))
        else {
            return Vec::new();
        };
        // The constraints this statement adds NOT VALID, for a VALIDATE in
        // the same statement.
        let mut added_here = HashSet::new();
        for command in &alter.cmds {
            let Some(command) = alter_command(command.as_ref()) else {
                continue;
            };
            match (command.subtype, command.def.as_ref()) {
                (AlterTableType::AtAddConstraint, Some(N::Constraint(constraint)))
                    if constraint.skip_validation && !constraint.conname.is_empty() =>
                {
                    added_here.insert(constraint.conname.as_str());
                }
                (AlterTableType::AtValidateConstraint, _) => {
                    let earlier = catalog.iter_constraints().any(|constraint| {
                        constraint.conrelid == relid
                            && constraint.conname == command.name
                            && self.not_valid_in_transaction.contains(&constraint.oid)
                    });
                    if !earlier && !added_here.contains(command.name.as_str()) {
                        continue;
                    }
                    return vec![RawFinding {
                        span: 0..0,
                        message: format!(
                            "VALIDATE CONSTRAINT {} runs in the transaction that added it NOT \
                             VALID: ADD CONSTRAINT's lock on {table} is still held, so the scan \
                             blocks writes on the table",
                            quote_identifier(&command.name)
                        ),
                        help: Some(
                            "validate it in a later migration (or after a COMMIT), where VALIDATE \
                             CONSTRAINT only takes a SHARE UPDATE EXCLUSIVE lock"
                                .to_owned(),
                        ),
                        fix: Vec::new(),
                    }];
                }
                _ => {}
            }
        }
        Vec::new()
    }

    /// `ATTACH PARTITION` scans the partition (under ACCESS EXCLUSIVE) to
    /// check every row fits the bound, unless its constraints imply the
    /// partition constraint (PartConstraintImpliedByRelConstraint). The
    /// lint does not build the partition constraint from the bound to prove
    /// the implication: it takes any valid, enforced CHECK on the partition
    /// that reads a partition key column as one that implies it, and
    /// reports only a partition with none.
    fn attach_partition_scan(
        &self,
        catalog: &PgCatalog,
        alter: &typedpg_pg_query::ast::AlterTableStmt,
    ) -> Vec<RawFinding> {
        let Some(parent) = alter
            .relation
            .as_ref()
            .and_then(|relation| resolve_relation(catalog, relation))
        else {
            return Vec::new();
        };
        if catalog.lint_partition_key_columns(parent.oid).is_none() {
            return Vec::new();
        }
        for command in &alter.cmds {
            let Some(command) = alter_command(command.as_ref()) else {
                continue;
            };
            if command.subtype != AlterTableType::AtAttachPartition {
                continue;
            }
            let Some(N::PartitionCmd(partition)) = command.def.as_ref() else {
                continue;
            };
            let (Some(class), Some(bound)) = (
                partition
                    .name
                    .as_ref()
                    .and_then(|relation| resolve_relation(catalog, relation)),
                partition.bound.as_ref(),
            ) else {
                continue;
            };
            if !self.pre_existing.contains(&class.oid)
                || !matches!(class.relkind, RelKind::Table | RelKind::Partitioned)
            {
                continue;
            }
            let name = qualified_name(catalog, class);
            let mut found = Vec::new();
            if catalog.lint_attach_partition_scans(parent.oid, class.oid, bound) {
                let help = if bound.strategy == "h" {
                    format!(
                        "no CHECK constraint can imply a hash partition's bound: attach \
                         {name} while it is empty, or create the partition with PARTITION OF \
                         and copy the rows into it"
                    )
                } else if bound.is_default {
                    format!(
                        "the default partition's constraint (no other partition's bound) is not \
                         one a CHECK constraint on {name} usually implies: attach it while it is \
                         empty"
                    )
                } else {
                    format!(
                        "first add a CHECK constraint on {name} that implies the bound, with \
                         NOT NULL on the key columns (NOT VALID, then VALIDATE CONSTRAINT in an \
                         earlier migration): PostgreSQL then skips the scan"
                    )
                };
                found.push(RawFinding {
                    span: 0..0,
                    message: format!(
                        "ATTACH PARTITION scans every row of {name} under an ACCESS EXCLUSIVE \
                         lock to check it fits the partition bound"
                    ),
                    help: Some(help),
                    fix: Vec::new(),
                });
            }
            // A default partition holds the rows no other partition takes:
            // PostgreSQL scans it, under the same lock, for rows that belong
            // to the new partition (`get_proposed_default_constraint`, which
            // its CHECK constraints would have to refute).
            if !bound.is_default
                && let Some(default) = catalog
                    .lint_default_partition(parent.oid)
                    .filter(|default| self.pre_existing.contains(default))
                    .and_then(|default| catalog.class_row(default))
            {
                let default_name = qualified_name(catalog, default);
                found.push(RawFinding {
                    span: 0..0,
                    message: format!(
                        "ATTACH PARTITION scans every row of the default partition \
                         {default_name} under an ACCESS EXCLUSIVE lock, for rows that belong to \
                         {name}"
                    ),
                    help: Some(format!(
                        "keep the default partition small: move rows out of {default_name} \
                         before attaching partitions for them"
                    )),
                    fix: Vec::new(),
                });
            }
            if !found.is_empty() {
                return found;
            }
        }
        Vec::new()
    }

    fn missing_lock_timeout(&mut self, catalog: &PgCatalog, node: &N) -> Vec<RawFinding> {
        // `statement_rules` already followed the statement's own `SET` /
        // `BEGIN` / `COMMIT`.
        if self.lock_timeout_effective() || self.lock_timeout_reported {
            return Vec::new();
        }
        let Some(target) = lock_target(catalog, &self.pre_existing, node) else {
            return Vec::new();
        };
        let Some(class) = catalog.class_row(target.relation) else {
            return Vec::new();
        };
        self.lock_timeout_reported = true;
        let kind = target.kind;
        let table = match target.index.and_then(|index| catalog.class_row(index)) {
            Some(index) => format!(
                "{} (the table of {})",
                qualified_name(catalog, class),
                qualified_name(catalog, index)
            ),
            None => qualified_name(catalog, class),
        };
        let blocked = if target.blocks_reads {
            "every query on the table"
        } else {
            "every write to the table"
        };
        let help = if self.file_runs_in_transaction() {
            "start the migration with SET LOCAL lock_timeout = '5s' and retry on timeout"
        } else {
            "start the migration with SET lock_timeout = '5s' and retry on timeout"
        };
        vec![RawFinding {
            span: 0..0,
            message: format!(
                "{kind} waits for a lock on {table} with no lock_timeout: queued behind a long \
                 transaction, it blocks {blocked}"
            ),
            help: Some(help.to_owned()),
            fix: Vec::new(),
        }]
    }

    /// Follow the `lock_timeout` and transaction state changes of `node`.
    fn track_lock_timeout(&mut self, catalog: &PgCatalog, node: &N) {
        match node {
            N::TransactionStmt(transaction) => self.track_transaction(transaction),
            N::VariableSetStmt(set) => {
                if let Some(value) =
                    lock_timeout_setting(set, self.transaction_open, self.lock_timeout_effective())
                {
                    self.set_lock_timeout(value, set.is_local);
                }
            }
            N::SelectStmt(select) => {
                if let Some((value, local)) = set_config_lock_timeout(catalog, select) {
                    self.set_lock_timeout(value, local);
                }
            }
            _ => {}
        }
    }

    /// Follow `BEGIN` / `COMMIT` / `ROLLBACK` / `SAVEPOINT` and their
    /// variants. PostgreSQL rolls GUC changes back with their transaction
    /// (or subtransaction), so each `BEGIN` snapshots the timeout state,
    /// `ROLLBACK` restores the `BEGIN` snapshot, `ROLLBACK TO` restores the
    /// savepoint's, and `RELEASE` drops it. A `COMMIT` keeps the session
    /// value and drops the local one.
    fn track_transaction(&mut self, transaction: &typedpg_pg_query::ast::TransactionStmt) {
        match transaction.kind {
            TransactionStmtKind::TransStmtBegin | TransactionStmtKind::TransStmtStart => {
                // `BEGIN` inside an open transaction is a warning and does
                // nothing; only an outer one starts a new snapshot.
                if !self.transaction_open {
                    self.transaction_open = true;
                    self.transaction_snapshot =
                        Some((self.lock_timeout_set, self.lock_timeout_local));
                    self.savepoints.clear();
                }
            }
            TransactionStmtKind::TransStmtCommit => {
                self.end_transaction(false);
                self.begin_chain(transaction.chain);
            }
            TransactionStmtKind::TransStmtRollback => {
                self.end_transaction(true);
                self.begin_chain(transaction.chain);
            }
            TransactionStmtKind::TransStmtSavepoint => {
                if self.transaction_open && !transaction.savepoint_name.is_empty() {
                    self.savepoints.push((
                        transaction.savepoint_name.clone(),
                        (self.lock_timeout_set, self.lock_timeout_local),
                    ));
                } else {
                    self.reset_lock_timeout_proof();
                }
            }
            TransactionStmtKind::TransStmtRelease => {
                let Some(index) = self.savepoint_index(&transaction.savepoint_name) else {
                    self.reset_lock_timeout_proof();
                    return;
                };
                self.savepoints.truncate(index);
            }
            TransactionStmtKind::TransStmtRollbackTo => {
                let Some(index) = self.savepoint_index(&transaction.savepoint_name) else {
                    self.reset_lock_timeout_proof();
                    return;
                };
                // `ROLLBACK TO` keeps the savepoint itself defined (so the
                // same point can be rolled back to again) and destroys the
                // ones established after it.
                let (_, snapshot) = self.savepoints[index].clone();
                self.lock_timeout_set = snapshot.0;
                self.lock_timeout_local = snapshot.1;
                self.savepoints.truncate(index + 1);
            }
            // Two-phase commit moves the transaction's changes outside this
            // file's reach: do not claim any timeout survives it.
            TransactionStmtKind::TransStmtPrepare
            | TransactionStmtKind::TransStmtCommitPrepared
            | TransactionStmtKind::TransStmtRollbackPrepared => self.reset_lock_timeout_proof(),
        }
    }

    /// The index of the innermost open savepoint named `name`.
    fn savepoint_index(&self, name: &str) -> Option<usize> {
        if !self.transaction_open || name.is_empty() {
            return None;
        }
        self.savepoints.iter().rposition(|(open, _)| open == name)
    }

    /// Finish the open transaction: `ROLLBACK` restores the state from
    /// `BEGIN`, `COMMIT` keeps the session value. Either way the local
    /// override and every savepoint are gone.
    fn end_transaction(&mut self, rollback: bool) {
        if self.transaction_open
            && rollback
            && let Some((session, _)) = self.transaction_snapshot
        {
            self.lock_timeout_set = session;
        }
        self.transaction_open = false;
        self.lock_timeout_local = None;
        self.transaction_snapshot = None;
        self.savepoints.clear();
        self.not_valid_in_transaction.clear();
    }

    /// Start a new transaction after `COMMIT AND CHAIN` / `ROLLBACK AND
    /// CHAIN`, snapshotting the value the previous transaction left.
    fn begin_chain(&mut self, chain: bool) {
        if !chain {
            return;
        }
        self.transaction_open = true;
        self.transaction_snapshot = Some((self.lock_timeout_set, None));
        self.lock_timeout_local = None;
        self.savepoints.clear();
        self.not_valid_in_transaction.clear();
    }

    /// Forget every proof that a timeout is set: a statement this tracker
    /// cannot model leaves the state unknown, so the next blocking statement
    /// reports instead of staying silent on a false claim.
    fn reset_lock_timeout_proof(&mut self) {
        self.lock_timeout_set = false;
        self.lock_timeout_local = None;
        self.transaction_open = false;
        self.transaction_snapshot = None;
        self.savepoints.clear();
        self.not_valid_in_transaction.clear();
    }

    /// Apply a `SET` (session) or `SET LOCAL` / `set_config(…, true)`
    /// (transaction) of `lock_timeout`. `None` is a value the lint cannot
    /// evaluate: it clears any earlier proof.
    fn set_lock_timeout(&mut self, value: Option<bool>, local: bool) {
        let value = value.unwrap_or(false);
        if local {
            // Outside a transaction block `SET LOCAL` warns and does
            // nothing.
            if self.transaction_open {
                self.lock_timeout_local = Some(value);
            }
        } else {
            self.lock_timeout_set = value;
            self.lock_timeout_local = None;
        }
    }

    /// The timeout the next statement runs with.
    fn lock_timeout_effective(&self) -> bool {
        if self.transaction_open {
            self.lock_timeout_local.unwrap_or(self.lock_timeout_set)
        } else {
            self.lock_timeout_set
        }
    }

    /// Whether the current file runs inside the runner's transaction.
    fn file_runs_in_transaction(&self) -> bool {
        self.use_transaction
            && !typedpg_core::migration_marker::has_no_transaction_marker(
                &self.files[self.current].sql,
            )
    }

    fn track_column_creators(&mut self, catalog: &PgCatalog, statement: &MigrationStatement<'_>) {
        match statement.node {
            N::CreateStmt(stmt) => {
                if let Some(class) = stmt
                    .relation
                    .as_ref()
                    .and_then(|relation| resolve_relation(catalog, relation))
                {
                    // `CREATE TABLE IF NOT EXISTS` on an existing table is
                    // a no-op: its columns keep the statements that gave
                    // them their types.
                    if !self.before_classes.contains(&class.oid) {
                        self.record_columns(catalog, class.oid);
                    }
                }
            }
            // `CREATE TABLE … AS` / `SELECT … INTO` define every column of
            // the new relation.
            N::CreateTableAsStmt(stmt) if stmt.objtype == ObjectType::ObjectTable => {
                if let Some(class) = stmt
                    .into
                    .as_ref()
                    .and_then(|into| into.rel.as_ref())
                    .and_then(|relation| resolve_relation(catalog, relation))
                {
                    // `CREATE TABLE AS … IF NOT EXISTS` on an existing
                    // relation is a no-op too.
                    if !self.before_classes.contains(&class.oid) {
                        self.record_columns(catalog, class.oid);
                    }
                }
            }
            N::AlterTableStmt(alter) => {
                let Some(class) = alter
                    .relation
                    .as_ref()
                    .and_then(|relation| resolve_relation(catalog, relation))
                else {
                    return;
                };
                for command in &alter.cmds {
                    let Some(command) = alter_command(command.as_ref()) else {
                        continue;
                    };
                    let Some(def) = command.def.as_ref() else {
                        continue;
                    };
                    match command.subtype {
                        AlterTableType::AtAddColumn => {
                            let N::ColumnDef(column) = def else {
                                continue;
                            };
                            let Some(attribute) = catalog
                                .attributes_of(class.oid)
                                .iter()
                                .find(|attribute| attribute.attname == column.colname)
                            else {
                                continue;
                            };
                            // `ADD COLUMN IF NOT EXISTS` on an existing
                            // column changed no type.
                            if self.before_relation == Some(class.oid)
                                && self.before_columns.contains(&attribute.attnum)
                            {
                                continue;
                            }
                            self.record_column(class.oid, attribute.attnum);
                        }
                        AlterTableType::AtAlterColumnType => {
                            let Some(attribute) = catalog
                                .attributes_of(class.oid)
                                .iter()
                                .find(|attribute| attribute.attname == command.name)
                            else {
                                continue;
                            };
                            self.record_column(class.oid, attribute.attnum);
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn record_columns(&mut self, catalog: &PgCatalog, relid: PgClassOid) {
        for attribute in catalog.attributes_of(relid) {
            if attribute.attnum > 0 {
                self.record_column(relid, attribute.attnum);
            }
        }
    }

    fn record_column(&mut self, relid: PgClassOid, attnum: i16) {
        self.column_creators.insert(
            (relid, attnum),
            Creator {
                file: self.current,
                span: self.current_span.clone(),
                suppressed: self.current_suppressed.clone(),
            },
        );
    }

    fn finish(&mut self, catalog: &PgCatalog) {
        self.schema_rule(catalog, Rule::FkWithoutIndex, |linter, catalog| {
            linter.fk_without_index(catalog)
        });
        self.schema_rule(catalog, Rule::RedundantIndex, |linter, catalog| {
            linter.redundant_index(catalog)
        });
        self.schema_rule(catalog, Rule::FkTypeMismatch, |linter, catalog| {
            linter.fk_type_mismatch(catalog)
        });
        self.schema_rule(catalog, Rule::PreferTimestamptz, |linter, catalog| {
            linter.prefer_timestamptz(catalog)
        });
        self.schema_rule(catalog, Rule::PreferIdentity, |linter, catalog| {
            linter.prefer_identity(catalog)
        });
        self.schema_rule(catalog, Rule::PreferText, |linter, catalog| {
            linter.prefer_text(catalog)
        });
        self.schema_rule(catalog, Rule::PreferBigintPk, |linter, catalog| {
            linter.prefer_bigint_pk(catalog)
        });
        self.schema_rule(catalog, Rule::PreferJsonb, |linter, catalog| {
            linter.prefer_jsonb(catalog)
        });
        self.schema_rule(catalog, Rule::MissingPrimaryKey, |linter, catalog| {
            linter.missing_primary_key(catalog)
        });
        self.schema_rule(catalog, Rule::RlsPolicyNotEnforced, |linter, catalog| {
            linter.rls_policy_not_enforced(catalog)
        });
        self.schema_rule(catalog, Rule::RlsNoPolicy, |linter, catalog| {
            linter.rls_no_policy(catalog)
        });
        self.schema_rule(
            catalog,
            Rule::SecurityDefinerSearchPath,
            |linter, catalog| linter.security_definer_search_path(catalog),
        );
    }

    fn schema_rule(
        &mut self,
        catalog: &PgCatalog,
        rule: Rule,
        find: impl Fn(&MigrationLinter<'_>, &PgCatalog) -> Vec<SchemaFinding>,
    ) {
        let Some(severity) = self.options.level(rule) else {
            return;
        };
        for SchemaFinding {
            object,
            raw,
            help_in_new_migration,
        } in find(self, catalog)
        {
            let creator = match object {
                CreatedObject::Constraint(oid) => self.created_constraints.get(&oid),
                CreatedObject::Index(oid) => self.created_indexes.get(&oid),
                CreatedObject::Column(relid, attnum) => self.column_creators.get(&(relid, attnum)),
                CreatedObject::Relation(relid) => self.created_relations.get(&relid),
                CreatedObject::Policy(relid, name) => self
                    .created_policies
                    .get(&(relid, name))
                    .or_else(|| self.created_relations.get(&relid)),
                CreatedObject::RowSecurity(relid) => self
                    .rls_enablers
                    .get(&relid)
                    .or_else(|| self.created_relations.get(&relid)),
                CreatedObject::Routine(oid) => self.created_routines.get(&oid),
            };
            let Some(creator) = creator else {
                continue;
            };
            if creator.suppressed.contains(&rule) {
                continue;
            }
            let help = if self.files[creator.file].lint == MigrationLint::SchemaOnly {
                help_in_new_migration.or(raw.help)
            } else {
                raw.help
            };
            self.findings.push(MigrationFinding {
                file: creator.file,
                finding: Finding {
                    rule,
                    severity,
                    span: creator.span.clone(),
                    message: raw.message,
                    help,
                    fix: Vec::new(),
                },
            });
        }
    }

    fn fk_without_index(&self, catalog: &PgCatalog) -> Vec<SchemaFinding> {
        let mut out = Vec::new();
        for constraint in catalog
            .iter_constraints()
            .filter(|constraint| constraint.contype == ConType::ForeignKey)
        {
            let Some(class) = catalog.class_row(constraint.conrelid) else {
                continue;
            };
            let columns: Vec<&str> = constraint
                .conkey
                .iter()
                .filter_map(|&attnum| {
                    catalog
                        .attributes_of(constraint.conrelid)
                        .iter()
                        .find(|attribute| attribute.attnum == attnum)
                        .map(|attribute| attribute.attname.as_str())
                })
                .collect();
            if columns.len() != constraint.conkey.len() || columns.is_empty() {
                continue;
            }
            let key: HashSet<i16> = constraint.conkey.iter().copied().collect();
            let key_count = key.len();
            let indexed = catalog
                .iter_indexes()
                .filter(|index| index.indrelid == constraint.conrelid)
                // The check looks rows up by equality: a btree serves any
                // key, a hash index a single column.
                .filter(
                    |index| match catalog.index_access_method(index.indexrelid) {
                        "btree" => true,
                        "hash" => key_count == 1,
                        _ => false,
                    },
                )
                .any(|index| {
                    let key_attributes = usize::try_from(index.indnkeyatts).unwrap_or(0);
                    if key_attributes < key_count || index.indkey.len() < key_count {
                        return false;
                    }
                    // Only the leading key positions must be plain columns:
                    // an expression later in the key (or in an INCLUDE
                    // column) runs after the equality lookup has found the
                    // rows, so it does not stop the index from serving the
                    // foreign-key check.
                    if index.indkey[..key_count].contains(&0) {
                        return false;
                    }
                    let leading: HashSet<i16> =
                        index.indkey.iter().take(key_count).copied().collect();
                    // The lookup's equality implies each referencing column
                    // is NOT NULL, so a partial index that only requires
                    // that serves it too.
                    leading == key && index_serves_foreign_key(catalog, index, &columns)
                });
            if indexed {
                continue;
            }
            let table = qualified_name(catalog, class);
            let columns = columns
                .iter()
                .map(|column| quote_identifier(column))
                .collect::<Vec<_>>()
                .join(", ");
            out.push(SchemaFinding {
                object: CreatedObject::Constraint(constraint.oid),
                raw: RawFinding {
                    span: 0..0,
                    message: format!(
                        "foreign key {} on {table} ({columns}) has no index",
                        quote_identifier(&constraint.conname)
                    ),
                    help: Some(format!("CREATE INDEX ON {table} ({columns})")),
                    fix: Vec::new(),
                },
                help_in_new_migration: Some(format!(
                    "CREATE INDEX CONCURRENTLY ON {table} ({columns}), in a new migration whose \
                     first line is -- no-transaction"
                )),
            });
        }
        out
    }

    fn fk_type_mismatch(&self, catalog: &PgCatalog) -> Vec<SchemaFinding> {
        let mut out = Vec::new();
        for constraint in catalog
            .iter_constraints()
            .filter(|constraint| constraint.contype == ConType::ForeignKey)
        {
            let Some(class) = catalog.class_row(constraint.conrelid) else {
                continue;
            };
            let Some(referenced) = constraint.confrelid.and_then(|oid| catalog.class_row(oid))
            else {
                continue;
            };
            let mut mismatch = None;
            for (&conkey, &confkey) in constraint.conkey.iter().zip(&constraint.confkey) {
                let Some(attribute) = catalog
                    .attributes_of(constraint.conrelid)
                    .iter()
                    .find(|attribute| attribute.attnum == conkey)
                else {
                    continue;
                };
                let Some(referenced_attribute) = catalog
                    .attributes_of(referenced.oid)
                    .iter()
                    .find(|attribute| attribute.attnum == confkey)
                else {
                    continue;
                };
                let type_oid = base_type(catalog, attribute.atttypid);
                let referenced_type_oid = base_type(catalog, referenced_attribute.atttypid);
                if type_oid != referenced_type_oid {
                    mismatch = Some((
                        attribute,
                        referenced_attribute,
                        type_oid,
                        referenced_type_oid,
                    ));
                    break;
                }
            }
            let Some((attribute, referenced_attribute, type_oid, referenced_type_oid)) = mismatch
            else {
                continue;
            };
            let table = qualified_name(catalog, class);
            let referenced_table = qualified_name(catalog, referenced);
            let column = quote_identifier(&attribute.attname);
            let referenced_column = quote_identifier(&referenced_attribute.attname);
            let type_name = type_label(catalog, type_oid);
            let referenced_type_name = type_label(catalog, referenced_type_oid);
            out.push(SchemaFinding {
                object: CreatedObject::Constraint(constraint.oid),
                raw: RawFinding {
                    span: 0..0,
                    message: format!(
                        "foreign key {} compares {table}.{column} ({type_name}) with \
                         {referenced_table}.{referenced_column} ({referenced_type_name})",
                        quote_identifier(&constraint.conname)
                    ),
                    help: Some(format!(
                        "give {column} the type {referenced_type_name}, so the join compares \
                         without a cast"
                    )),
                    fix: Vec::new(),
                },
                help_in_new_migration: Some(format!(
                    "in a new migration, give {column} the type {referenced_type_name}, so the \
                     join compares without a cast"
                )),
            });
        }
        out
    }

    fn prefer_timestamptz(&self, catalog: &PgCatalog) -> Vec<SchemaFinding> {
        let mut out = Vec::new();
        for class in reportable_tables(catalog) {
            let table = qualified_name(catalog, class);
            for attribute in columns(catalog, class) {
                if !is_pg_catalog_type(
                    catalog,
                    inner_type(catalog, attribute.atttypid),
                    "timestamp",
                ) {
                    continue;
                }
                let column = quote_identifier(&attribute.attname);
                out.push(SchemaFinding {
                    object: CreatedObject::Column(class.oid, attribute.attnum),
                    raw: RawFinding {
                        span: 0..0,
                        message: format!(
                            "{table}.{column} is timestamp without time zone: it stores no \
                             offset, so the same instant reads differently per session TimeZone"
                        ),
                        help: Some("use timestamptz".to_owned()),
                        fix: Vec::new(),
                    },
                    help_in_new_migration: Some(
                        "use timestamptz in a new migration, rewriting the values as it goes"
                            .to_owned(),
                    ),
                });
            }
        }
        out
    }

    fn prefer_identity(&self, catalog: &PgCatalog) -> Vec<SchemaFinding> {
        let mut out = Vec::new();
        for class in reportable_tables(catalog) {
            let table = qualified_name(catalog, class);
            for attribute in columns(catalog, class) {
                if attribute.attidentity.is_some()
                    || !catalog.lint_column_is_serial(class.oid, attribute.attnum)
                {
                    continue;
                }
                let column = quote_identifier(&attribute.attname);
                out.push(SchemaFinding {
                    object: CreatedObject::Column(class.oid, attribute.attnum),
                    raw: RawFinding {
                        span: 0..0,
                        message: format!("{table}.{column} is a serial column"),
                        help: Some(
                            "use GENERATED BY DEFAULT AS IDENTITY: the sequence then belongs to \
                             the column (permissions, dumps, ALTER TYPE follow it)"
                                .to_owned(),
                        ),
                        fix: Vec::new(),
                    },
                    help_in_new_migration: Some(
                        "use GENERATED BY DEFAULT AS IDENTITY in a new migration".to_owned(),
                    ),
                });
            }
        }
        out
    }

    fn prefer_text(&self, catalog: &PgCatalog) -> Vec<SchemaFinding> {
        let mut out = Vec::new();
        for class in reportable_tables(catalog) {
            let table = qualified_name(catalog, class);
            for attribute in columns(catalog, class) {
                let type_oid = inner_type(catalog, attribute.atttypid);
                let (kind, requires_typmod) = if is_pg_catalog_type(catalog, type_oid, "varchar") {
                    ("varchar", true)
                } else if is_pg_catalog_type(catalog, type_oid, "bpchar") {
                    ("char", false)
                } else {
                    continue;
                };
                let typmod = catalog.lint_effective_typmod(attribute.atttypid, attribute.atttypmod);
                let Some(length) = typmod.map(|typmod| typmod - 4).or_else(|| {
                    // A bare `char` is `char(1)`; a bare `varchar` is
                    // unlimited and fine.
                    (!requires_typmod).then_some(1)
                }) else {
                    continue;
                };
                if length < 1 {
                    continue;
                }
                let column = quote_identifier(&attribute.attname);
                let array = catalog
                    .type_row(base_type(catalog, attribute.atttypid))
                    .is_some_and(|ty| ty.typcategory == TypCategory::Array);
                let brackets = if array { "[]" } else { "" };
                let message = if kind == "varchar" {
                    format!("{table}.{column} is varchar({length}){brackets}")
                } else {
                    format!("{table}.{column} is char({length}){brackets}: char pads with spaces")
                };
                // `char_length` takes one string: an array's elements have no
                // CHECK as simple to offer.
                let help = if array {
                    "use text[]".to_owned()
                } else {
                    format!(
                        "use text, with a CHECK (char_length({column}) <= {length}) if the limit \
                         matters"
                    )
                };
                out.push(SchemaFinding {
                    object: CreatedObject::Column(class.oid, attribute.attnum),
                    raw: RawFinding {
                        span: 0..0,
                        message,
                        help: Some(help.clone()),
                        fix: Vec::new(),
                    },
                    help_in_new_migration: Some(format!("{help}, in a new migration")),
                });
            }
        }
        out
    }

    fn prefer_bigint_pk(&self, catalog: &PgCatalog) -> Vec<SchemaFinding> {
        let mut out = Vec::new();
        for constraint in catalog
            .iter_constraints()
            .filter(|constraint| constraint.contype == ConType::PrimaryKey)
        {
            let Some(class) = catalog.class_row(constraint.conrelid) else {
                continue;
            };
            if !is_reportable_table(catalog, class) {
                continue;
            }
            let table = qualified_name(catalog, class);
            for &attnum in &constraint.conkey {
                let Some(attribute) = columns(catalog, class)
                    .into_iter()
                    .find(|attribute| attribute.attnum == attnum)
                else {
                    continue;
                };
                let type_oid = base_type(catalog, attribute.atttypid);
                let (type_name, limit) = if is_pg_catalog_type(catalog, type_oid, "int4") {
                    ("int4", "2147483647")
                } else if is_pg_catalog_type(catalog, type_oid, "int2") {
                    ("int2", "32767")
                } else {
                    continue;
                };
                let column = quote_identifier(&attribute.attname);
                out.push(SchemaFinding {
                    object: CreatedObject::Column(class.oid, attribute.attnum),
                    raw: RawFinding {
                        span: 0..0,
                        message: format!(
                            "{table}.{column} is an {type_name} primary key: it runs out at {limit}"
                        ),
                        help: Some("use bigint".to_owned()),
                        fix: Vec::new(),
                    },
                    help_in_new_migration: Some("use bigint in a new migration".to_owned()),
                });
            }
        }
        out
    }

    fn prefer_jsonb(&self, catalog: &PgCatalog) -> Vec<SchemaFinding> {
        let mut out = Vec::new();
        for class in reportable_tables(catalog) {
            let table = qualified_name(catalog, class);
            for attribute in columns(catalog, class) {
                if !is_pg_catalog_type(catalog, inner_type(catalog, attribute.atttypid), "json") {
                    continue;
                }
                let column = quote_identifier(&attribute.attname);
                out.push(SchemaFinding {
                    object: CreatedObject::Column(class.oid, attribute.attnum),
                    raw: RawFinding {
                        span: 0..0,
                        message: format!(
                            "{table}.{column} is json: it has no equality operator, so DISTINCT, \
                             GROUP BY, UNION and = fail on it, and every read parses the text again"
                        ),
                        help: Some(
                            "use jsonb, unless the exact text (key order, duplicate keys, \
                             whitespace) must be kept"
                                .to_owned(),
                        ),
                        fix: Vec::new(),
                    },
                    help_in_new_migration: Some("use jsonb in a new migration".to_owned()),
                });
            }
        }
        out
    }

    /// Tables without a primary key. A partition is skipped: it gets its
    /// partitioned table's primary key, and when that has none the parent
    /// is the one reported. An inheritance child is reported: a primary key
    /// is not inherited (only partitions clone their parent's). Temporary
    /// tables are skipped.
    fn missing_primary_key(&self, catalog: &PgCatalog) -> Vec<SchemaFinding> {
        let with_primary_key: HashSet<PgClassOid> = catalog
            .iter_constraints()
            .filter(|constraint| constraint.contype == ConType::PrimaryKey)
            .map(|constraint| constraint.conrelid)
            .collect();
        let mut tables: Vec<&PgClass> = catalog
            .iter_classes()
            .filter(|class| {
                matches!(class.relkind, RelKind::Table | RelKind::Partitioned)
                    && !with_primary_key.contains(&class.oid)
                    && catalog.lint_relation_persistence(class.oid) != 't'
                    && catalog
                        .namespace_name(class.relnamespace)
                        .is_some_and(|name| {
                            name != "information_schema" && !name.starts_with("pg_")
                        })
                    && !is_partition(catalog, class.oid)
            })
            .collect();
        tables.sort_by_key(|class| class.oid);
        tables
            .into_iter()
            .map(|class| {
                let table = qualified_name(catalog, class);
                SchemaFinding {
                    object: CreatedObject::Relation(class.oid),
                    raw: RawFinding {
                        span: 0..0,
                        message: format!("{table} has no primary key"),
                        help: Some(
                            "add a PRIMARY KEY: without one, rows have no identity (duplicates go \
                             unnoticed, and logical replication cannot publish UPDATEs and \
                             DELETEs of the table)"
                                .to_owned(),
                        ),
                        fix: Vec::new(),
                    },
                    help_in_new_migration: Some(
                        "in a new migration, CREATE UNIQUE INDEX CONCURRENTLY on the key, then \
                         ADD PRIMARY KEY USING INDEX"
                            .to_owned(),
                    ),
                }
            })
            .collect()
    }

    /// Policies on a table whose row-level security is off: PostgreSQL
    /// ignores them.
    fn rls_policy_not_enforced(&self, catalog: &PgCatalog) -> Vec<SchemaFinding> {
        let mut out = Vec::new();
        for class in policy_tables(catalog) {
            if catalog.lint_relation_has_row_security(class.oid) {
                continue;
            }
            let policies = catalog.lint_relation_policies(class.oid);
            let Some(first) = policies.first() else {
                continue;
            };
            let table = qualified_name(catalog, class);
            let names = policies
                .iter()
                .map(|name| quote_identifier(name))
                .collect::<Vec<_>>()
                .join(", ");
            let subject = if policies.len() == 1 {
                format!("policy {names} on {table} does nothing")
            } else {
                format!("policies {names} on {table} do nothing")
            };
            out.push(SchemaFinding {
                object: CreatedObject::Policy(class.oid, (*first).to_owned()),
                raw: RawFinding {
                    span: 0..0,
                    message: format!("{subject}: row-level security is not enabled on the table"),
                    help: Some(format!("ALTER TABLE {table} ENABLE ROW LEVEL SECURITY")),
                    fix: Vec::new(),
                },
                help_in_new_migration: Some(format!(
                    "ALTER TABLE {table} ENABLE ROW LEVEL SECURITY, in a new migration"
                )),
            });
        }
        out
    }

    /// Row-level security enabled on a table with no policy: PostgreSQL
    /// then applies a default-deny policy (`get_row_security_policies`).
    fn rls_no_policy(&self, catalog: &PgCatalog) -> Vec<SchemaFinding> {
        let mut tables: Vec<&PgClass> = catalog
            .iter_classes()
            .filter(|class| {
                matches!(class.relkind, RelKind::Table | RelKind::Partitioned)
                    && catalog.lint_relation_has_row_security(class.oid)
                    && catalog.lint_relation_policies(class.oid).is_empty()
            })
            .collect();
        tables.sort_by_key(|class| class.oid);
        tables
            .into_iter()
            .map(|class| {
                let table = qualified_name(catalog, class);
                let exempt = if catalog.lint_relation_forces_row_security(class.oid) {
                    "superusers and BYPASSRLS roles"
                } else {
                    "its owner, superusers and BYPASSRLS roles"
                };
                SchemaFinding {
                    object: CreatedObject::RowSecurity(class.oid),
                    raw: RawFinding {
                        span: 0..0,
                        message: format!(
                            "row-level security is enabled on {table}, which has no policy: every \
                             role but {exempt} sees no rows and can write none"
                        ),
                        help: Some(format!(
                            "CREATE POLICY for the roles that use {table}, or leave it as is if \
                             denying them is the intent"
                        )),
                        fix: Vec::new(),
                    },
                    help_in_new_migration: Some(format!(
                        "CREATE POLICY for the roles that use {table} in a new migration, or \
                         leave it as is if denying them is the intent"
                    )),
                }
            })
            .collect()
    }

    /// `SECURITY DEFINER` routines that resolve names along the caller's
    /// search path: a caller who can create objects in a schema on it (or
    /// in `pg_temp`) can shadow a table, function or operator the body uses,
    /// and run code with the owner's privileges.
    fn security_definer_search_path(&self, catalog: &PgCatalog) -> Vec<SchemaFinding> {
        let mut out = Vec::new();
        for (oid, security_definer, caller_search_path) in catalog.lint_routine_security() {
            if !security_definer || !caller_search_path {
                continue;
            }
            let Some(proc) = catalog.pg_proc().get(&oid) else {
                continue;
            };
            let schema = catalog
                .namespace_name(proc.pronamespace)
                .unwrap_or("public");
            let name = QualifiedName::new(schema, &proc.proname).to_string();
            let kind = if proc.prokind == ProKind::Procedure {
                "procedure"
            } else {
                "function"
            };
            out.push(SchemaFinding {
                object: CreatedObject::Routine(oid),
                raw: RawFinding {
                    span: 0..0,
                    message: format!(
                        "SECURITY DEFINER {kind} {name} has no SET search_path: it runs with its \
                         owner's privileges but resolves names along the caller's search_path, \
                         where the caller can shadow them"
                    ),
                    help: Some(format!(
                        "add SET search_path = pg_catalog, pg_temp to the {kind}, and qualify the \
                         objects it uses with their schema"
                    )),
                    fix: Vec::new(),
                },
                help_in_new_migration: Some(format!(
                    "in a new migration, ALTER {} {name} SET search_path = pg_catalog, pg_temp, \
                     and qualify the objects it uses with their schema",
                    kind.to_ascii_uppercase()
                )),
            });
        }
        out
    }

    fn redundant_index(&self, catalog: &PgCatalog) -> Vec<SchemaFinding> {
        let mut indexes: Vec<&PgIndex> = catalog.iter_indexes().collect();
        indexes.sort_by_key(|index| index.indexrelid);
        // The indexes each one is redundant with, exact duplicates first,
        // then in OID order.
        let mut covered_by: HashMap<PgClassOid, Vec<&PgIndex>> = HashMap::new();
        for index in &indexes {
            // A constraint's index cannot be dropped with DROP INDEX, and ON
            // CONFLICT ON CONSTRAINT depends on it: never report it as the
            // redundant one. When it duplicates a standalone index, that
            // standalone index is the one to drop.
            if catalog.index_backs_constraint(index.indexrelid) {
                continue;
            }
            let mut covers_it: Vec<(&PgIndex, bool)> = indexes
                .iter()
                .filter_map(|other| {
                    let exact = covers(catalog, &self.orderings, index, other)?;
                    Some((*other, exact))
                })
                // Of two standalone exact duplicates, only the one created
                // later is redundant with the one already there.
                .filter(|(other, exact)| {
                    !(*exact
                        && !catalog.index_backs_constraint(other.indexrelid)
                        && self.created_later(other.indexrelid, index.indexrelid))
                })
                .collect();
            covers_it.sort_by_key(|(_, exact)| !exact);
            if !covers_it.is_empty() {
                covered_by.insert(
                    index.indexrelid,
                    covers_it.into_iter().map(|(other, _)| other).collect(),
                );
            }
        }
        // Every one of ours that something covers is reported, so each
        // finding names an index that stays: a cover that is not reported
        // itself, or, through the reported ones, the one that covers them
        // (covering is transitive). With `(email)`, `(email)` again and
        // `(email, name)`, both `(email)` indexes name `(email, name)`.
        let reported = |oid: PgClassOid| {
            covered_by.contains_key(&oid) && self.created_indexes.contains_key(&oid)
        };
        let survivor = |index: PgClassOid| -> Option<PgClassOid> {
            let mut current = index;
            let mut seen = HashSet::from([index]);
            loop {
                let covers_it = covered_by.get(&current)?;
                let next = covers_it
                    .iter()
                    .find(|other| !reported(other.indexrelid))
                    .or_else(|| covers_it.first())?
                    .indexrelid;
                if !reported(next) || !seen.insert(next) {
                    return Some(next);
                }
                current = next;
            }
        };
        let mut out = Vec::new();
        for index in &indexes {
            if !reported(index.indexrelid) {
                continue;
            }
            let Some(other) = survivor(index.indexrelid) else {
                continue;
            };
            let (Some(name), Some(other_name)) = (
                catalog.class_row(index.indexrelid),
                catalog.class_row(other),
            ) else {
                continue;
            };
            out.push(SchemaFinding {
                object: CreatedObject::Index(index.indexrelid),
                raw: RawFinding {
                    span: 0..0,
                    message: format!(
                        "index {} is redundant with {}",
                        quote_identifier(&name.relname),
                        quote_identifier(&other_name.relname)
                    ),
                    help: None,
                    fix: Vec::new(),
                },
                help_in_new_migration: Some(format!(
                    "DROP INDEX CONCURRENTLY {}, in a new migration whose first line is \
                     -- no-transaction",
                    quote_identifier(&name.relname)
                )),
            });
        }
        out
    }

    /// Whether our migrations created index `a` after index `b`.
    fn created_later(&self, a: PgClassOid, b: PgClassOid) -> bool {
        match (self.created_indexes.get(&a), self.created_indexes.get(&b)) {
            (Some(a), Some(b)) => (a.file, a.span.start) > (b.file, b.span.start),
            _ => false,
        }
    }
}

impl MigrationObserver for MigrationLinter<'_> {
    fn before(&mut self, catalog: &PgCatalog, statement: &MigrationStatement<'_>) {
        // The relations before the statement: a `CREATE TABLE IF NOT
        // EXISTS` that finds its table already there creates nothing.
        self.before_classes = catalog.iter_classes().map(|class| class.oid).collect();
        // Every file's indexes are tracked, for their order: an extra
        // migration's index can make one of ours redundant.
        self.before_indexes = catalog
            .iter_indexes()
            .map(|index| index.indexrelid)
            .collect();
        if self.files[self.current].lint == MigrationLint::None {
            return;
        }
        self.current_span = self.trimmed(&statement.span);
        self.current_suppressed = self.suppressions(&statement.span);
        self.before_relation = None;
        self.before_columns.clear();
        if let N::AlterTableStmt(alter) = statement.node
            && let Some(class) = alter
                .relation
                .as_ref()
                .and_then(|relation| resolve_relation(catalog, relation))
        {
            self.before_relation = Some(class.oid);
            self.before_columns = catalog
                .attributes_of(class.oid)
                .iter()
                .map(|attribute| attribute.attnum)
                .collect();
        }
        self.before_constraints = catalog
            .iter_constraints()
            .map(|constraint| constraint.oid)
            .collect();
        if let N::CreateFunctionStmt(_) = statement.node {
            self.before_routines = catalog
                .lint_routine_security()
                .into_iter()
                .map(|(oid, _, _)| oid)
                .collect();
        }
        if self.files[self.current].lint == MigrationLint::All {
            self.statement_rules(catalog, statement);
        }
        self.last_statement_end = statement.span.end;
    }

    fn after(&mut self, catalog: &PgCatalog, statement: &MigrationStatement<'_>) {
        let indexes: HashSet<PgClassOid> = catalog
            .iter_indexes()
            .map(|index| index.indexrelid)
            .collect();
        // A `CREATE INDEX` on a partitioned table also makes its partitions'
        // indexes: all of them in the statement's order.
        if let N::IndexStmt(stmt) = statement.node {
            let order = key_order(stmt);
            for oid in indexes.difference(&self.before_indexes) {
                self.orderings.insert(*oid, order.clone());
            }
        }
        if self.files[self.current].lint == MigrationLint::None {
            return;
        }
        self.track_column_creators(catalog, statement);
        let constraints: HashSet<PgConstraintOid> = catalog
            .iter_constraints()
            .map(|constraint| constraint.oid)
            .collect();
        for oid in indexes.difference(&self.before_indexes) {
            self.created_indexes.insert(
                *oid,
                Creator {
                    file: self.current,
                    span: self.current_span.clone(),
                    suppressed: self.current_suppressed.clone(),
                },
            );
        }
        for oid in constraints.difference(&self.before_constraints) {
            self.created_constraints.insert(
                *oid,
                Creator {
                    file: self.current,
                    span: self.current_span.clone(),
                    suppressed: self.current_suppressed.clone(),
                },
            );
        }
        // A constraint added NOT VALID inside an open transaction keeps its
        // ADD CONSTRAINT lock until that transaction ends.
        if self.files[self.current].lint == MigrationLint::All
            && self.transaction_open
            && matches!(statement.node, N::AlterTableStmt(_))
        {
            for constraint in catalog.iter_constraints() {
                if !constraint.convalidated
                    && !self.before_constraints.contains(&constraint.oid)
                    && self.pre_existing.contains(&constraint.conrelid)
                {
                    self.not_valid_in_transaction.insert(constraint.oid);
                }
            }
        }
        self.track_object_creators(catalog, statement);
    }
}

impl MigrationLinter<'_> {
    fn creator(&self) -> Creator {
        Creator {
            file: self.current,
            span: self.current_span.clone(),
            suppressed: self.current_suppressed.clone(),
        }
    }

    /// Record the statement that created a table, a policy or a routine, or
    /// enabled a table's row-level security, for the schema rules that
    /// report on them.
    fn track_object_creators(&mut self, catalog: &PgCatalog, statement: &MigrationStatement<'_>) {
        match statement.node {
            N::CreateStmt(stmt) => {
                if let Some(class) = stmt
                    .relation
                    .as_ref()
                    .and_then(|relation| resolve_relation(catalog, relation))
                    && !self.before_classes.contains(&class.oid)
                {
                    self.created_relations.insert(class.oid, self.creator());
                }
            }
            N::CreateTableAsStmt(stmt) if stmt.objtype == ObjectType::ObjectTable => {
                if let Some(class) = stmt
                    .into
                    .as_ref()
                    .and_then(|into| into.rel.as_ref())
                    .and_then(|relation| resolve_relation(catalog, relation))
                    && !self.before_classes.contains(&class.oid)
                {
                    self.created_relations.insert(class.oid, self.creator());
                }
            }
            N::CreatePolicyStmt(stmt) => {
                if let Some(class) = stmt
                    .table
                    .as_ref()
                    .and_then(|relation| resolve_relation(catalog, relation))
                {
                    self.created_policies
                        .insert((class.oid, stmt.policy_name.clone()), self.creator());
                }
            }
            N::AlterTableStmt(alter) => {
                let enables = alter.cmds.iter().any(|command| {
                    alter_command(command.as_ref()).is_some_and(|command| {
                        command.subtype == AlterTableType::AtEnableRowSecurity
                    })
                });
                if enables
                    && let Some(class) = alter
                        .relation
                        .as_ref()
                        .and_then(|relation| resolve_relation(catalog, relation))
                {
                    self.rls_enablers.insert(class.oid, self.creator());
                }
            }
            N::CreateFunctionStmt(stmt) => {
                let routines: Vec<PgProcOid> = catalog
                    .lint_routine_security()
                    .into_iter()
                    .map(|(oid, _, _)| oid)
                    .collect();
                let mut created: Vec<PgProcOid> = routines
                    .iter()
                    .copied()
                    .filter(|oid| !self.before_routines.contains(oid))
                    .collect();
                // `CREATE OR REPLACE` of an existing routine keeps its OID:
                // find it by name, when the name alone tells.
                if created.is_empty() && stmt.replace {
                    let names: Vec<&str> = stmt
                        .funcname
                        .iter()
                        .filter_map(|n| string_node(n.as_ref()))
                        .collect();
                    let (schema, name) = match names.as_slice() {
                        [name] => (None, *name),
                        [schema, name] => (Some(*schema), *name),
                        _ => return,
                    };
                    let candidates: Vec<PgProcOid> = catalog
                        .find_functions(schema, name)
                        .into_iter()
                        .map(|proc| proc.oid)
                        .filter(|oid| routines.contains(oid))
                        .collect();
                    if let [oid] = candidates.as_slice() {
                        created.push(*oid);
                    }
                }
                for oid in created {
                    self.created_routines.insert(oid, self.creator());
                }
            }
            _ => {}
        }
    }
}

fn validates(constraint: &typedpg_pg_query::ast::Constraint) -> bool {
    matches!(
        constraint.contype,
        ConstrType::ConstrForeign | ConstrType::ConstrCheck
    ) && !constraint.skip_validation
}

/// Whether a `ColumnDef` gives the column a DEFAULT: an explicit one, or the
/// `nextval()` the `serial` pseudo-types expand to.
fn column_has_default(column: &typedpg_pg_query::ast::ColumnDef) -> bool {
    column.raw_default.is_some()
        || column.constraints.iter().any(|node| {
            matches!(
                node.as_ref(),
                Some(N::Constraint(constraint))
                    if constraint.contype == ConstrType::ConstrDefault
            )
        })
        || column_type_name(column).is_some_and(|name| {
            matches!(
                name,
                "smallserial" | "serial2" | "serial" | "serial4" | "bigserial" | "serial8"
            )
        })
}

/// The last name of the column's declared type.
fn column_type_name(column: &typedpg_pg_query::ast::ColumnDef) -> Option<&str> {
    let type_name = column.type_name.as_ref()?;
    match type_name.names.last()?.as_ref()? {
        N::String(name) => Some(&name.sval),
        _ => None,
    }
}

/// Whether PostgreSQL's `transformColumnDefinition` leaves a `raw_default`
/// on the column: an explicit or serial DEFAULT, or a GENERATED expression
/// (stored or virtual, whose expression the parser stores as
/// `raw_default`). Identity columns do not get one. This is exactly the
/// condition `transformAlterTableStmt` checks to clear `skipValidation`.
fn column_has_raw_default(column: &typedpg_pg_query::ast::ColumnDef) -> bool {
    column_has_default(column)
        || !column.generated.is_empty()
        || column.constraints.iter().any(|node| {
            matches!(
                node.as_ref(),
                Some(N::Constraint(constraint))
                    if constraint.contype == ConstrType::ConstrGenerated
            )
        })
}

fn alter_command(node: Option<&Node>) -> Option<&typedpg_pg_query::ast::AlterTableCmd> {
    match node? {
        N::AlterTableCmd(command) => Some(command),
        _ => None,
    }
}

/// Resolve a `RangeVar` to the class the catalog holds now.
fn resolve_relation<'a>(
    catalog: &'a PgCatalog,
    relation: &typedpg_pg_query::ast::RangeVar,
) -> Option<&'a PgClass> {
    let schema = (!relation.schemaname.is_empty()).then_some(relation.schemaname.as_str());
    catalog.resolve_table(schema, &relation.relname)
}

/// The ordinary and partitioned tables the schema opinion rules inspect:
/// user schemas, never a partition or inheritance child (its parent's columns
/// already carry the answer).
fn reportable_tables(catalog: &PgCatalog) -> Vec<&PgClass> {
    let mut tables: Vec<&PgClass> = catalog
        .iter_classes()
        .filter(|class| is_reportable_table(catalog, class))
        .collect();
    tables.sort_by_key(|class| class.oid);
    tables
}

fn is_reportable_table(catalog: &PgCatalog, class: &PgClass) -> bool {
    matches!(class.relkind, RelKind::Table | RelKind::Partitioned)
        && catalog
            .namespace_name(class.relnamespace)
            .is_some_and(|name| name != "information_schema" && !name.starts_with("pg_"))
        && !catalog
            .pg_inherits()
            .iter()
            .any(|inherits| inherits.inhrelid == class.oid)
}

/// Whether `relid` is a partition: a child of a partitioned table.
fn is_partition(catalog: &PgCatalog, relid: PgClassOid) -> bool {
    catalog.pg_inherits().iter().any(|inherits| {
        inherits.inhrelid == relid
            && catalog
                .class_row(inherits.inhparent)
                .is_some_and(|parent| parent.relkind == RelKind::Partitioned)
    })
}

/// The tables with row-level security policies, by OID.
fn policy_tables(catalog: &PgCatalog) -> Vec<&PgClass> {
    let mut tables: Vec<&PgClass> = catalog
        .iter_classes()
        .filter(|class| {
            matches!(class.relkind, RelKind::Table | RelKind::Partitioned)
                && !catalog.lint_relation_policies(class.oid).is_empty()
        })
        .collect();
    tables.sort_by_key(|class| class.oid);
    tables
}

fn columns<'a>(catalog: &'a PgCatalog, class: &PgClass) -> Vec<&'a PgAttribute> {
    catalog
        .attributes_of(class.oid)
        .iter()
        .filter(|attribute| attribute.attnum > 0)
        .collect()
}

/// A type with its domain chain stripped: a domain over `int8` is `int8`.
fn base_type(catalog: &PgCatalog, oid: PgTypeOid) -> PgTypeOid {
    let mut current = oid;
    for _ in 0..64 {
        let Some(base) = catalog.type_row(current).and_then(|ty| ty.typbasetype) else {
            break;
        };
        current = base;
    }
    current
}

/// The element type of an array column (after domain resolution), or the
/// type itself.
fn inner_type(catalog: &PgCatalog, oid: PgTypeOid) -> PgTypeOid {
    let base = base_type(catalog, oid);
    let Some(ty) = catalog.type_row(base) else {
        return base;
    };
    if ty.typcategory != TypCategory::Array {
        return base;
    }
    ty.typelem
        .map_or(base, |element| base_type(catalog, element))
}

fn is_pg_catalog_type(catalog: &PgCatalog, oid: PgTypeOid, name: &str) -> bool {
    catalog.type_row(oid).is_some_and(|ty| {
        ty.typname == name && catalog.namespace_name(ty.typnamespace) == Some("pg_catalog")
    })
}

fn type_label(catalog: &PgCatalog, oid: PgTypeOid) -> &str {
    catalog
        .type_row(oid)
        .map_or("unknown", |ty| ty.typname.as_str())
}

/// `SELECT [pg_catalog.]set_config('lock_timeout', <value>, <is_local>)` as
/// a plain top-level statement with constant arguments: the value (`Some`
/// when the lint can evaluate it, `None` when it cannot) and whether it is
/// local to the transaction.
///
/// Only `pg_catalog.set_config` is the built-in; an unqualified name may
/// resolve to a user function when `search_path` finds one first, so it is
/// trusted only while the catalog holds no other `set_config`.
fn set_config_lock_timeout(
    catalog: &PgCatalog,
    select: &typedpg_pg_query::ast::SelectStmt,
) -> Option<(Option<bool>, bool)> {
    use typedpg_pg_query::ast::SetOperation;

    if select.op != SetOperation::SetopNone
        || !select.from_clause.is_empty()
        || select.where_clause.is_some()
        || !select.group_clause.is_empty()
        || select.having_clause.is_some()
        || !select.window_clause.is_empty()
        || !select.distinct_clause.is_empty()
        || !select.sort_clause.is_empty()
        || select.limit_count.is_some()
        || select.limit_offset.is_some()
    {
        return None;
    }
    let [target] = select.target_list.as_slice() else {
        return None;
    };
    let Some(N::ResTarget(target)) = target.as_ref() else {
        return None;
    };
    let Some(N::FuncCall(call)) = target.val.as_ref() else {
        return None;
    };
    let names: Vec<&str> = call
        .funcname
        .iter()
        .filter_map(|node| match node {
            Some(N::String(string)) => Some(string.sval.as_str()),
            _ => None,
        })
        .collect();
    if !set_config_is_builtin(catalog, &names) {
        return None;
    }
    let [setting, value, local] = call.args.as_slice() else {
        return None;
    };
    // Only a string literal (optionally cast once to `text`) has text this
    // lint knows: a cast chain evaluates the value away
    // (`'0.4'::numeric::integer::text` is `'0'`).
    if !plain_text_literal(setting.as_ref())?.eq_ignore_ascii_case("lock_timeout") {
        return None;
    }
    // An unknown `is_local` decides where the assignment lands; drop the
    // proof rather than guess.
    let Some(local) = constant_bool(local.as_ref()) else {
        return Some((None, false));
    };
    Some((set_config_value(value.as_ref()), local))
}

/// Whether the called function is PostgreSQL's `set_config`. `pg_catalog`
/// always wins when named; an unqualified `set_config` is only known to be
/// it while no user-defined function of that name exists.
fn set_config_is_builtin(catalog: &PgCatalog, names: &[&str]) -> bool {
    match names {
        ["pg_catalog", "set_config"] => true,
        ["set_config"] => !catalog.pg_proc().values().any(|proc| {
            proc.proname == "set_config"
                && catalog.namespace_name(proc.pronamespace) != Some("pg_catalog")
        }),
        _ => false,
    }
}

/// The text of a string literal, uncast or cast once to `text`.
fn plain_text_literal(node: Option<&Node>) -> Option<&str> {
    let node = match node? {
        N::TypeCast(cast) => {
            if !is_text_type(cast.type_name.as_ref()?) {
                return None;
            }
            cast.arg.as_ref()?
        }
        node => node,
    };
    match node {
        N::AConst(constant) => match constant.val.as_ref()? {
            AConstValue::String(string) => Some(&string.sval),
            _ => None,
        },
        N::String(string) => Some(&string.sval),
        _ => None,
    }
}

/// Whether a cast's target type is `text`.
fn is_text_type(type_name: &typedpg_pg_query::ast::TypeName) -> bool {
    let names: Vec<&str> = type_name
        .names
        .iter()
        .filter_map(|node| match node {
            Some(N::String(string)) => Some(string.sval.as_str()),
            _ => None,
        })
        .collect();
    matches!(names.as_slice(), ["text"] | ["pg_catalog", "text"])
}

/// The value `set_config('lock_timeout', <value>, …)` sets: `Some` for a
/// string literal (optionally cast once to `text`), `None` when another
/// expression computes it. A NULL literal resets the setting to its default
/// (`0`, disabled).
fn set_config_value(node: Option<&Node>) -> Option<bool> {
    let node = match node? {
        N::TypeCast(cast) => {
            if !is_text_type(cast.type_name.as_ref()?) {
                return None;
            }
            cast.arg.as_ref()?
        }
        node => node,
    };
    match node {
        N::AConst(constant) if constant.isnull => Some(false),
        N::AConst(constant) => match constant.val.as_ref()? {
            AConstValue::String(string) => Some(duration_nonzero(&string.sval)),
            _ => None,
        },
        N::String(string) => Some(duration_nonzero(&string.sval)),
        _ => None,
    }
}

/// The constant `node` is, through casts.
fn constant_node(node: &Node) -> Option<&Node> {
    match node {
        N::TypeCast(cast) => constant_node(cast.arg.as_ref()?),
        _ => Some(node),
    }
}

fn constant_bool(node: Option<&Node>) -> Option<bool> {
    match constant_node(node?)? {
        N::AConst(constant) => match constant.val.as_ref()? {
            AConstValue::Boolean(boolean) => Some(boolean.boolval),
            _ => None,
        },
        N::Boolean(boolean) => Some(boolean.boolval),
        _ => None,
    }
}

/// The effective value of a `SET`/`RESET` of `lock_timeout`, when the
/// statement sets it: `Some(Some(true))` for a proven non-zero value,
/// `Some(Some(false))` for zero, `DEFAULT`, `RESET` or `RESET ALL`,
/// `Some(None)` for a value the lint cannot evaluate, and `None` for
/// another setting.
///
/// `SET LOCAL` outside a transaction block is a no-op (PostgreSQL warns and
/// keeps the session value), so it is `None` when `in_transaction` is false.
fn lock_timeout_setting(
    set: &typedpg_pg_query::ast::VariableSetStmt,
    in_transaction: bool,
    effective: bool,
) -> Option<Option<bool>> {
    let kind = set.kind;
    // `RESET ALL` carries no setting name: it clears every setting,
    // `lock_timeout` included.
    if kind == VariableSetKind::VarResetAll {
        return Some(Some(false));
    }
    if !set.name.eq_ignore_ascii_case("lock_timeout") {
        return None;
    }
    if set.is_local && !in_transaction {
        return None;
    }
    match kind {
        VariableSetKind::VarSetValue => {
            Some(set.args.first().and_then(|n| setting_value(n.as_ref())))
        }
        VariableSetKind::VarSetDefault | VariableSetKind::VarReset => Some(Some(false)),
        // `SET … FROM CURRENT` keeps the value the statement runs with.
        VariableSetKind::VarSetCurrent => Some(Some(effective)),
        _ => Some(None),
    }
}

/// The value of `SET … TO <value>`: `Some` when its text is known, `None`
/// when an expression computes it.
fn setting_value(node: Option<&Node>) -> Option<bool> {
    match node? {
        N::AConst(constant) if constant.isnull => None,
        N::AConst(constant) => match constant.val.as_ref()? {
            AConstValue::Integer(value) => Some(value.ival != 0),
            AConstValue::Float(value) => Some(duration_nonzero(&value.fval)),
            AConstValue::String(value) => Some(duration_nonzero(&value.sval)),
            _ => None,
        },
        N::Integer(value) => Some(value.ival != 0),
        N::Float(value) => Some(duration_nonzero(&value.fval)),
        N::String(value) => Some(duration_nonzero(&value.sval)),
        _ => None,
    }
}

/// Whether a PostgreSQL time value (`5s`, `5000`, `0`, `1min`) is non-zero
/// once PostgreSQL has rounded it to whole milliseconds. A spelling we
/// cannot parse is taken as non-zero: the rule then stays quiet rather than
/// reporting a migration that did set a timeout.
fn duration_nonzero(value: &str) -> bool {
    duration_milliseconds(value).is_none_or(|milliseconds| milliseconds != 0)
}

/// A PostgreSQL time value in milliseconds, rounded the way `parse_int` /
/// `convert_to_base_unit` do for `lock_timeout` (guc.c): the conversion
/// rounds to the next smaller unit (a microsecond) and then to an integer
/// millisecond, with ties to even. `None` when the spelling is not a number.
///
/// `lock_timeout` is disabled exactly when the result is zero, so
/// `'0.4ms'` and `'0.5ms'` are zero but `'0.6ms'` is not.
fn duration_milliseconds(value: &str) -> Option<i64> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("default") {
        return Some(0);
    }
    let lower = value.to_ascii_lowercase();
    // Longest unit first: `min` before `ms`, `ms` / `us` before `s`.
    let unit = ["min", "us", "ms", "h", "d", "s"]
        .into_iter()
        .find(|unit| lower.ends_with(unit));
    let (number, multiplier) = match unit {
        Some("min") => (&lower[..lower.len() - 3], 60_000.0),
        Some("us") => (&lower[..lower.len() - 2], 0.001),
        Some("ms") => (&lower[..lower.len() - 2], 1.0),
        Some("h") => (&lower[..lower.len() - 1], 3_600_000.0),
        Some("d") => (&lower[..lower.len() - 1], 86_400_000.0),
        Some("s") => (&lower[..lower.len() - 1], 1_000.0),
        _ => (lower.as_str(), 1.0),
    };
    let number = parse_pg_number(number)?;
    if !number.is_finite() {
        return Some(i64::MAX);
    }
    let microseconds = (number * multiplier * 1000.0).round_ties_even();
    let milliseconds = (microseconds / 1000.0).round_ties_even();
    if !milliseconds.is_finite() {
        return Some(i64::MAX);
    }
    Some(milliseconds as i64)
}

/// The number part of a PostgreSQL setting value: decimal or scientific, or
/// the `0x` / `0o` / `0b` integer literals `strtol` accepts.
fn parse_pg_number(value: &str) -> Option<f64> {
    let value = value.trim();
    let (negative, digits) = match value.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, value.strip_prefix('+').unwrap_or(value)),
    };
    let digits = digits.replace('_', "");
    let parsed = if let Some(hex) = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        u128::from_str_radix(hex, 16).ok().map(|value| value as f64)
    } else if let Some(octal) = digits
        .strip_prefix("0o")
        .or_else(|| digits.strip_prefix("0O"))
    {
        u128::from_str_radix(octal, 8)
            .ok()
            .map(|value| value as f64)
    } else if let Some(binary) = digits
        .strip_prefix("0b")
        .or_else(|| digits.strip_prefix("0B"))
    {
        u128::from_str_radix(binary, 2)
            .ok()
            .map(|value| value as f64)
    } else {
        digits.parse::<f64>().ok()
    }?;
    Some(if negative { -parsed } else { parsed })
}

/// What a statement waits to lock: the table whose queries queue behind it.
struct LockTarget {
    /// The statement's kind, as the message names it.
    kind: &'static str,
    /// The table (or other relation) locked.
    relation: PgClassOid,
    /// The index the statement is about, when it names one: a `DROP INDEX`
    /// or `REINDEX INDEX` locks the index's table.
    index: Option<PgClassOid>,
    /// The lock conflicts with `ACCESS SHARE` (it is `ACCESS EXCLUSIVE`), so
    /// even a plain `SELECT` queues behind it; otherwise only writers do.
    blocks_reads: bool,
}

impl LockTarget {
    fn new((kind, relation): (&'static str, PgClassOid), blocks_reads: bool) -> LockTarget {
        LockTarget {
            kind,
            relation,
            index: None,
            blocks_reads,
        }
    }
}

/// The first statement that takes a write-blocking lock on a relation that
/// existed when the file started: what it locks, and how strongly (each
/// lock level as PostgreSQL 19 takes it: `AlterTableGetLockLevel`,
/// `RangeVarCallbackForDropRelation`, `ReindexIndex` / `ReindexTable`,
/// `DefineIndex`, `CreateTrigger`, `CreatePolicy`, `DefineQueryRewrite`).
fn lock_target(
    catalog: &PgCatalog,
    pre_existing: &HashSet<PgClassOid>,
    node: &N,
) -> Option<LockTarget> {
    let exclusive = |target| Some(LockTarget::new(target, true));
    match node {
        N::AlterTableStmt(alter) => alter_table_lock_target(catalog, pre_existing, alter),
        // A foreign key's referenced table gets SHARE ROW EXCLUSIVE.
        N::CreateStmt(create) => create_table_foreign_keys(create)
            .into_iter()
            .find_map(|relation| existing_relation(catalog, pre_existing, relation, "CREATE TABLE"))
            .map(|target| LockTarget::new(target, false)),
        // SHARE.
        N::IndexStmt(index) => {
            let relation = index.relation.as_ref()?;
            if index.concurrent {
                return None;
            }
            existing_relation(catalog, pre_existing, relation, "CREATE INDEX")
                .map(|target| LockTarget::new(target, false))
        }
        N::DropStmt(drop) => drop_lock_target(catalog, pre_existing, drop),
        N::TruncateStmt(truncate) => {
            first_relation(catalog, pre_existing, &truncate.relations, "TRUNCATE")
                .and_then(exclusive)
        }
        // SHARE ROW EXCLUSIVE.
        N::CreateTrigStmt(trigger) => existing_relation(
            catalog,
            pre_existing,
            trigger.relation.as_ref()?,
            "CREATE TRIGGER",
        )
        .map(|target| LockTarget::new(target, false)),
        N::CreatePolicyStmt(policy) => existing_relation(
            catalog,
            pre_existing,
            policy.table.as_ref()?,
            "CREATE POLICY",
        )
        .and_then(exclusive),
        N::AlterPolicyStmt(policy) => existing_relation(
            catalog,
            pre_existing,
            policy.table.as_ref()?,
            "ALTER POLICY",
        )
        .and_then(exclusive),
        N::RuleStmt(rule) => existing_relation(
            catalog,
            pre_existing,
            rule.relation.as_ref()?,
            "CREATE RULE",
        )
        .and_then(exclusive),
        N::RenameStmt(rename) => {
            if !matches!(
                rename.rename_type,
                ObjectType::ObjectTable | ObjectType::ObjectColumn
            ) {
                return None;
            }
            existing_relation(
                catalog,
                pre_existing,
                rename.relation.as_ref()?,
                "ALTER TABLE",
            )
            .and_then(exclusive)
        }
        N::LockStmt(lock) => {
            if lock_mode_blocks_writes(lock.mode) {
                first_relation(catalog, pre_existing, &lock.relations, "LOCK TABLE")
                    .map(|target| LockTarget::new(target, lock_mode_blocks_reads(lock.mode)))
            } else {
                None
            }
        }
        N::RefreshMatViewStmt(refresh) => {
            if refresh.concurrent {
                return None;
            }
            existing_relation(
                catalog,
                pre_existing,
                refresh.relation.as_ref()?,
                "REFRESH MATERIALIZED VIEW",
            )
            .and_then(exclusive)
        }
        N::RepackStmt(repack) => {
            // REPACK (CONCURRENTLY) holds ShareUpdateExclusiveLock until
            // its final swap.
            if option_enabled(&repack.params, "concurrently") {
                return None;
            }
            let command = if repack.command == typedpg_pg_query::ast::RepackCommand::Cluster {
                "CLUSTER"
            } else {
                "REPACK"
            };
            existing_relation(
                catalog,
                pre_existing,
                repack.relation.as_ref()?.relation.as_ref()?,
                command,
            )
            .and_then(exclusive)
        }
        N::VacuumStmt(vacuum) => {
            if !vacuum.is_vacuumcmd || !option_enabled(&vacuum.options, "full") {
                return None;
            }
            vacuum
                .rels
                .iter()
                .find_map(|relation| {
                    let N::VacuumRelation(relation) = relation.as_ref()? else {
                        return None;
                    };
                    existing_relation(
                        catalog,
                        pre_existing,
                        relation.relation.as_ref()?,
                        "VACUUM FULL",
                    )
                })
                .and_then(exclusive)
        }
        // SHARE on the table (and, once granted, ACCESS EXCLUSIVE on each
        // index it rebuilds).
        N::ReindexStmt(reindex) => {
            if option_enabled(&reindex.params, "concurrently") {
                return None;
            }
            let relation = reindex.relation.as_ref()?;
            if reindex.kind == typedpg_pg_query::ast::ReindexObjectType::ReindexObjectIndex {
                let (_, index) =
                    existing_relation(catalog, pre_existing, relation, "REINDEX INDEX")?;
                return index_lock_target("REINDEX INDEX", catalog, pre_existing, index, false);
            }
            existing_relation(catalog, pre_existing, relation, "REINDEX")
                .map(|target| LockTarget::new(target, false))
        }
        _ => None,
    }
}

/// The lock a statement about `index` takes on the index's table.
fn index_lock_target(
    kind: &'static str,
    catalog: &PgCatalog,
    pre_existing: &HashSet<PgClassOid>,
    index: PgClassOid,
    blocks_reads: bool,
) -> Option<LockTarget> {
    let table = catalog
        .iter_indexes()
        .find(|row| row.indexrelid == index)?
        .indrelid;
    pre_existing.contains(&table).then_some(LockTarget {
        kind,
        relation: table,
        index: Some(index),
        blocks_reads,
    })
}

fn existing_relation(
    catalog: &PgCatalog,
    pre_existing: &HashSet<PgClassOid>,
    relation: &typedpg_pg_query::ast::RangeVar,
    kind: &'static str,
) -> Option<(&'static str, PgClassOid)> {
    let class = resolve_relation(catalog, relation)?;
    pre_existing
        .contains(&class.oid)
        .then_some((kind, class.oid))
}

fn first_relation(
    catalog: &PgCatalog,
    pre_existing: &HashSet<PgClassOid>,
    nodes: &[Option<Node>],
    kind: &'static str,
) -> Option<(&'static str, PgClassOid)> {
    nodes
        .iter()
        .filter_map(|node| match node.as_ref()? {
            N::RangeVar(relation) => Some(relation),
            _ => None,
        })
        .find_map(|relation| existing_relation(catalog, pre_existing, relation, kind))
}

fn alter_table_lock_target(
    catalog: &PgCatalog,
    pre_existing: &HashSet<PgClassOid>,
    alter: &typedpg_pg_query::ast::AlterTableStmt,
) -> Option<LockTarget> {
    let mut strong = false;
    let mut exclusive = false;
    let mut referenced = Vec::new();
    let mut attached = Vec::new();
    for node in &alter.cmds {
        let Some(command) = alter_command(node.as_ref()) else {
            continue;
        };
        if !only_share_update_exclusive(command) {
            strong = true;
            exclusive |= !share_row_exclusive(command);
        }
        if let Some(foreign_key) = added_foreign_key(command)
            && let Some(table) = foreign_key.pktable.as_ref()
        {
            referenced.push(table);
        }
        // ATTACH PARTITION takes ACCESS EXCLUSIVE on the table being
        // attached (and on a default partition, whose bound the catalog
        // does not model), while the parent only gets SHARE UPDATE
        // EXCLUSIVE. The attached table can hold rows, so it is the lock
        // worth reporting.
        if command.subtype == AlterTableType::AtAttachPartition
            && let Some(N::PartitionCmd(partition)) = command.def.as_ref()
            && let Some(name) = partition.name.as_ref()
        {
            attached.push(name);
        }
    }
    if strong
        && let Some(relation) = alter.relation.as_ref()
        && let Some(target) = existing_relation(catalog, pre_existing, relation, "ALTER TABLE")
    {
        return Some(LockTarget::new(target, exclusive));
    }
    attached
        .into_iter()
        .find_map(|relation| existing_relation(catalog, pre_existing, relation, "ALTER TABLE"))
        .map(|target| LockTarget::new(target, true))
        .or_else(|| {
            // The referenced table of a foreign key gets SHARE ROW
            // EXCLUSIVE.
            referenced
                .into_iter()
                .find_map(|relation| {
                    existing_relation(catalog, pre_existing, relation, "ALTER TABLE")
                })
                .map(|target| LockTarget::new(target, false))
        })
}

/// Whether an `ALTER TABLE` subcommand takes `SHARE ROW EXCLUSIVE`, which
/// blocks writers but not readers (`AlterTableGetLockLevel`): enabling or
/// disabling triggers, and adding a foreign key.
fn share_row_exclusive(command: &typedpg_pg_query::ast::AlterTableCmd) -> bool {
    match command.subtype {
        AlterTableType::AtEnableTrig
        | AlterTableType::AtEnableAlwaysTrig
        | AlterTableType::AtEnableReplicaTrig
        | AlterTableType::AtEnableTrigAll
        | AlterTableType::AtEnableTrigUser
        | AlterTableType::AtDisableTrig
        | AlterTableType::AtDisableTrigAll
        | AlterTableType::AtDisableTrigUser => true,
        AlterTableType::AtAddConstraint => matches!(
            command.def.as_ref(),
            Some(N::Constraint(constraint)) if constraint_is_foreign_key(constraint)
        ),
        _ => false,
    }
}

/// `LOCK TABLE` takes the mode it names. `SHARE` is the weakest mode that
/// conflicts with a writer's `ROW EXCLUSIVE`, so only it and the stronger
/// modes can block every query on the table; `ACCESS SHARE`, `ROW SHARE`,
/// `ROW EXCLUSIVE` and `SHARE UPDATE EXCLUSIVE` do not. PostgreSQL's
/// `LOCKMODE` values come from `lockdefs.h`: `AccessShareLock` is 1 and
/// `AccessExclusiveLock` 8; `LOCK TABLE` with no mode is the latter.
fn lock_mode_blocks_writes(mode: i32) -> bool {
    const SHARE: i32 = 5;
    mode == 0 || mode >= SHARE
}

/// Whether a `LOCK TABLE` mode conflicts with a reader's `ACCESS SHARE`:
/// only `ACCESS EXCLUSIVE` (8, the default) does.
fn lock_mode_blocks_reads(mode: i32) -> bool {
    const ACCESS_EXCLUSIVE: i32 = 8;
    mode == 0 || mode == ACCESS_EXCLUSIVE
}

/// Whether an `ALTER TABLE` subcommand only takes `SHARE UPDATE EXCLUSIVE`
/// on the table (PostgreSQL's `AlterTableGetLockLevel`).
fn only_share_update_exclusive(command: &typedpg_pg_query::ast::AlterTableCmd) -> bool {
    match command.subtype {
        AlterTableType::AtValidateConstraint
        | AlterTableType::AtSetStatistics
        | AlterTableType::AtSetOptions
        | AlterTableType::AtResetOptions
        | AlterTableType::AtClusterOn
        | AlterTableType::AtDropCluster
        | AlterTableType::AtAttachPartition
        | AlterTableType::AtDetachPartitionFinalize => true,
        AlterTableType::AtSetRelOptions | AlterTableType::AtResetRelOptions => {
            rel_options_are_share_update_exclusive(command)
        }
        AlterTableType::AtDetachPartition => matches!(
            command.def.as_ref(),
            Some(N::PartitionCmd(partition)) if partition.concurrent
        ),
        _ => false,
    }
}

/// Whether a `SET (storage_parameter)` / `RESET (storage_parameter)` takes
/// only `SHARE UPDATE EXCLUSIVE`. PostgreSQL picks the lock per option
/// (`AlterTableGetRelOptionsLockLevel`): every table option the analyzer
/// models is weak except `user_catalog_table`, which needs `ACCESS
/// EXCLUSIVE`.
fn rel_options_are_share_update_exclusive(command: &typedpg_pg_query::ast::AlterTableCmd) -> bool {
    let Some(N::List(list)) = command.def.as_ref() else {
        return false;
    };
    list.items.iter().all(|node| {
        !matches!(
            node.as_ref(),
            Some(N::DefElem(def)) if def.defname.eq_ignore_ascii_case("user_catalog_table")
        )
    })
}

fn added_foreign_key(
    command: &typedpg_pg_query::ast::AlterTableCmd,
) -> Option<&typedpg_pg_query::ast::Constraint> {
    match (command.subtype, command.def.as_ref()) {
        (AlterTableType::AtAddConstraint, Some(N::Constraint(constraint)))
            if constraint_is_foreign_key(constraint) =>
        {
            Some(&**constraint)
        }
        (AlterTableType::AtAddColumn, Some(N::ColumnDef(column))) => column
            .constraints
            .iter()
            .filter_map(|constraint| match constraint.as_ref()? {
                N::Constraint(constraint) if constraint_is_foreign_key(constraint) => {
                    Some(&**constraint)
                }
                _ => None,
            })
            .next(),
        _ => None,
    }
}

fn create_table_foreign_keys(
    create: &typedpg_pg_query::ast::CreateStmt,
) -> Vec<&typedpg_pg_query::ast::RangeVar> {
    let mut out = Vec::new();
    for node in create.table_elts.iter().chain(&create.constraints) {
        match node.as_ref() {
            Some(N::Constraint(constraint)) if constraint_is_foreign_key(constraint) => {
                out.extend(constraint.pktable.as_deref());
            }
            Some(N::ColumnDef(column)) => {
                for constraint in &column.constraints {
                    if let Some(N::Constraint(constraint)) = constraint.as_ref()
                        && constraint_is_foreign_key(constraint)
                    {
                        out.extend(constraint.pktable.as_deref());
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn constraint_is_foreign_key(constraint: &typedpg_pg_query::ast::Constraint) -> bool {
    constraint.contype == ConstrType::ConstrForeign
}

fn drop_lock_target(
    catalog: &PgCatalog,
    pre_existing: &HashSet<PgClassOid>,
    drop: &typedpg_pg_query::ast::DropStmt,
) -> Option<LockTarget> {
    let index = match drop.remove_type {
        ObjectType::ObjectTable | ObjectType::ObjectForeignTable => false,
        ObjectType::ObjectIndex if !drop.concurrent => true,
        _ => return None,
    };
    for object in &drop.objects {
        let Some(class) = object_relation(catalog, object.as_ref()) else {
            continue;
        };
        if !pre_existing.contains(&class.oid) {
            continue;
        }
        // Either way the table gets ACCESS EXCLUSIVE.
        if !index {
            return Some(LockTarget::new(("DROP TABLE", class.oid), true));
        }
        if let Some(target) =
            index_lock_target("DROP INDEX", catalog, pre_existing, class.oid, true)
        {
            return Some(target);
        }
    }
    None
}

/// The relation a `DROP` object (a `[schema.]name` list) names.
fn object_relation<'a>(catalog: &'a PgCatalog, object: Option<&Node>) -> Option<&'a PgClass> {
    let Some(N::List(list)) = object else {
        return None;
    };
    let names: Vec<&str> = list
        .items
        .iter()
        .filter_map(|n| string_node(n.as_ref()))
        .collect();
    match names.as_slice() {
        [name] => catalog.resolve_table(None, name),
        [schema, name] => catalog.resolve_table(Some(schema), name),
        _ => None,
    }
}

fn string_node(node: Option<&Node>) -> Option<&str> {
    match node? {
        N::String(string) => Some(string.sval.as_str()),
        _ => None,
    }
}

/// Whether `options` has `name` set to a true value (a bare option counts).
fn option_enabled(options: &[Option<Node>], name: &str) -> bool {
    options.iter().any(|option| match option.as_ref() {
        Some(N::DefElem(option)) if option.defname.eq_ignore_ascii_case(name) => {
            def_boolean(option).unwrap_or(true)
        }
        _ => false,
    })
}

fn def_boolean(option: &typedpg_pg_query::ast::DefElem) -> Option<bool> {
    match option.arg.as_ref() {
        None => Some(true),
        Some(N::Integer(integer)) => Some(integer.ival != 0),
        Some(N::Boolean(boolean)) => Some(boolean.boolval),
        Some(N::String(string)) => match string.sval.to_ascii_lowercase().as_str() {
            "true" | "on" => Some(true),
            "false" | "off" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn proven_not_null(catalog: &PgCatalog, relid: PgClassOid, column: &str) -> bool {
    catalog.iter_constraints().any(|constraint| {
        constraint.conrelid == relid
            && constraint.contype == ConType::Check
            && constraint.convalidated
            && constraint.conenforced
            && !constraint.connoinherit
            && catalog
                .check_expression(constraint.oid)
                .is_some_and(|expr| check_proves_not_null(expr, column))
    })
}

/// Whether `node` is `column IS NOT NULL`, or an AND with such a top-level
/// conjunct (other conjuncts may test anything).
fn check_proves_not_null(node: &Node, column: &str) -> bool {
    match node {
        N::BoolExpr(expr) if expr.boolop == BoolExprType::AndExpr => {
            !expr.args.is_empty()
                && expr.args.iter().any(|arg| {
                    arg.as_ref()
                        .is_some_and(|arg| check_proves_not_null(arg, column))
                })
        }
        N::NullTest(test) => {
            test.nulltesttype == NullTestType::IsNotNull
                && !test.argisrow
                && test
                    .arg
                    .as_ref()
                    .and_then(column_name)
                    .is_some_and(|name| name == column)
        }
        _ => false,
    }
}

fn keys_plain(index: &PgIndex) -> bool {
    index.indpred.is_none() && index.indexprs.is_empty() && index.indkey.iter().all(|&key| key != 0)
}

/// Whether the foreign-key check can use `index`: it is not partial, or its
/// predicate only requires that columns among the foreign key's referencing
/// `columns` are NOT NULL. The check looks rows up with `col = $1 …`, which
/// implies exactly that.
fn index_serves_foreign_key(catalog: &PgCatalog, index: &PgIndex, columns: &[&str]) -> bool {
    if index.indpred.is_none() {
        return true;
    }
    catalog
        .index_predicate(index.indexrelid)
        .is_some_and(|predicate| not_null_and(&predicate, columns))
}

/// Whether `node` is `col IS NOT NULL`, or an AND of such tests (including
/// nested ANDs and parentheses), for columns among `columns` only.
fn not_null_and(node: &Node, columns: &[&str]) -> bool {
    match node {
        N::BoolExpr(expr) if expr.boolop == BoolExprType::AndExpr => {
            !expr.args.is_empty()
                && expr
                    .args
                    .iter()
                    .all(|arg| arg.as_ref().is_some_and(|arg| not_null_and(arg, columns)))
        }
        N::NullTest(test) => {
            test.nulltesttype == NullTestType::IsNotNull
                && !test.argisrow
                && test
                    .arg
                    .as_ref()
                    .and_then(column_name)
                    .is_some_and(|name| columns.contains(&name))
        }
        _ => false,
    }
}

/// The name a `ColumnRef` names, when it is one (a qualified reference's last
/// field, as an index predicate may spell `t.col`).
fn column_name(node: &Node) -> Option<&str> {
    let N::ColumnRef(reference) = node else {
        return None;
    };
    let (last, qualifiers) = reference.fields.split_last()?;
    if !qualifiers
        .iter()
        .all(|field| matches!(field.as_ref(), Some(N::String(_))))
    {
        return None;
    }
    match last.as_ref() {
        Some(N::String(name)) => Some(&name.sval),
        _ => None,
    }
}

/// Whether `other` covers `index`, and if so whether the two are exact
/// duplicates.
///
/// `other` covers `index` when it leads with the same key columns, in the
/// same order and with the same operator classes, collations and sort
/// orders, and `index` adds nothing: no uniqueness `other` lacks, and no
/// `INCLUDE` columns (a covering index serves index-only scans `other` may
/// not), unless the two are identical.
fn covers(
    catalog: &PgCatalog,
    orderings: &HashMap<PgClassOid, Vec<(bool, bool)>>,
    index: &PgIndex,
    other: &PgIndex,
) -> Option<bool> {
    let key_count = usize::try_from(index.indnkeyatts).unwrap_or(0);
    if !keys_plain(index) || key_count == 0 || index.indkey.len() < key_count {
        return None;
    }
    let covering = index.indkey.len() > key_count;
    let nulls_not_distinct =
        |index: &PgIndex| catalog.lint_index_nulls_not_distinct(index.indexrelid);
    let deferrable = |index: &PgIndex| catalog.lint_index_is_deferrable(index.indexrelid);
    let order = |index: &PgIndex, n: usize| -> Vec<(bool, bool)> {
        let mut order = orderings
            .get(&index.indexrelid)
            .cloned()
            .unwrap_or_default();
        order.resize(n, (false, false));
        order.truncate(n);
        order
    };
    if other.indexrelid == index.indexrelid
        || other.indrelid != index.indrelid
        || !keys_plain(other)
        || catalog.index_access_method(other.indexrelid)
            != catalog.index_access_method(index.indexrelid)
        // An immediate unique index enforces its uniqueness at each
        // statement, a deferrable one only at commit: neither replaces the
        // other.
        || deferrable(index) != deferrable(other)
    {
        return None;
    }
    let other_keys = usize::try_from(other.indnkeyatts).unwrap_or(0);
    if other_keys < key_count || other.indkey.len() < key_count {
        return None;
    }
    if index.indkey[..key_count] != other.indkey[..key_count]
        || index.indclass.get(..key_count) != other.indclass.get(..key_count)
        || index.indcollation.get(..key_count) != other.indcollation.get(..key_count)
        || order(index, key_count) != order(other, key_count)
    {
        return None;
    }
    let index_nulls = nulls_not_distinct(index);
    let other_nulls = nulls_not_distinct(other);
    if other_keys == key_count
        && other.indkey == index.indkey
        && other.indisunique == index.indisunique
        && other_nulls == index_nulls
    {
        return Some(true);
    }
    if index.indisunique || covering {
        // A unique NULLS DISTINCT index adds nothing over a unique NULLS NOT
        // DISTINCT index on the same columns: the latter enforces strictly
        // more (it also rejects duplicate NULLs) and serves the same
        // equality lookups. The reverse is not true.
        return (!covering
            && index.indisunique
            && other.indisunique
            && other_keys == key_count
            && other.indkey == index.indkey
            && !index_nulls
            && other_nulls)
            .then_some(false);
    }
    Some(false)
}

/// `stmt`'s key columns' order: `(descending, nulls first)` each, nulls
/// first by default exactly when descending, as PostgreSQL's.
fn key_order(stmt: &typedpg_pg_query::ast::IndexStmt) -> Vec<(bool, bool)> {
    stmt.index_params
        .iter()
        .map(|param| match param.as_ref() {
            Some(N::IndexElem(elem)) => {
                let descending = elem.ordering == SortByDir::SortbyDesc;
                let nulls_first = match elem.nulls_ordering {
                    SortByNulls::SortbyNullsFirst => true,
                    SortByNulls::SortbyNullsLast => false,
                    _ => descending,
                };
                (descending, nulls_first)
            }
            _ => (false, false),
        })
        .collect()
}

fn volatile(catalog: &PgCatalog, node: Option<&Node>) -> bool {
    let Some(inner) = node else {
        return false;
    };
    inner
        .nodes()
        .any(|(node, _)| matches!(node, typedpg_pg_query::NodeRef::FuncCall(call) if catalog.call_is_volatile(call)))
}

fn comments(sql: &str) -> Vec<Comment> {
    let Ok(scanned) = typedpg_pg_query::scan(sql) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for token in &scanned.tokens {
        if !matches!(
            token.token,
            typedpg_pg_query::ast::Token::SqlComment | typedpg_pg_query::ast::Token::CComment
        ) {
            continue;
        }
        let (Ok(start), Ok(end)) = (usize::try_from(token.start), usize::try_from(token.end))
        else {
            continue;
        };
        let kind = match allow_comment(&sql[start..end]) {
            AllowComment::None => continue,
            AllowComment::Allow(names) => CommentKind::Allow(names),
            AllowComment::Malformed => CommentKind::Malformed,
        };
        out.push(Comment { start, end, kind });
    }
    out
}

fn qualified_name(catalog: &PgCatalog, class: &typedpg_analyzer::PgClass) -> String {
    let schema = catalog
        .namespace_name(class.relnamespace)
        .unwrap_or("public");
    QualifiedName::new(schema, &class.relname).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use typedpg_core::config::{LintConfig, LintLevel};

    const BASE: &str = r#"
        CREATE TABLE users (id integer PRIMARY KEY, email text);
        CREATE TABLE posts (id integer PRIMARY KEY, author_id integer);
        CREATE TABLE pairs (a integer, b integer, PRIMARY KEY (a, b));
        CREATE INDEX users_email_idx ON users (email);
        ALTER TABLE users ADD CONSTRAINT users_email_nn CHECK (email IS NOT NULL) NOT VALID;
    "#;

    fn lint(
        files: &[(&str, &str, MigrationLint)],
        use_transaction: bool,
        options: &LintOptions,
    ) -> Vec<MigrationFinding> {
        let files: Vec<MigrationFile> = files
            .iter()
            .map(|(path, sql, lint)| MigrationFile {
                path: (*path).to_owned(),
                sql: (*sql).to_owned(),
                lint: *lint,
            })
            .collect();
        lint_migrations(&files, use_transaction, options)
            .unwrap()
            .findings
    }

    fn second(sql: &str) -> Vec<MigrationFinding> {
        second_with(sql, true, &LintOptions::default())
    }

    fn second_with(
        sql: &str,
        use_transaction: bool,
        options: &LintOptions,
    ) -> Vec<MigrationFinding> {
        lint(
            &[
                ("001_base.sql", BASE, MigrationLint::All),
                ("002_change.sql", sql, MigrationLint::All),
            ],
            use_transaction,
            options,
        )
    }

    fn opinion_options(rules: &[Rule]) -> LintOptions {
        let mut config = LintConfig::default();
        for rule in rules {
            config
                .levels
                .insert(rule.name().to_owned(), LintLevel::Info);
        }
        LintOptions::from_config(&config)
    }

    fn only(findings: &[MigrationFinding], rule: Rule) -> Vec<MigrationFinding> {
        findings
            .iter()
            .filter(|finding| finding.finding.rule == rule)
            .cloned()
            .collect()
    }

    // ── fk_type_mismatch ───────────────────────────────────────────────────

    #[test]
    fn fk_type_mismatch_reports_differing_base_types() {
        let findings = second(
            "ALTER TABLE posts ALTER COLUMN author_id TYPE bigint;
             ALTER TABLE posts ADD CONSTRAINT posts_author_fk
                 FOREIGN KEY (author_id) REFERENCES users (id);",
        );
        let found = only(&findings, Rule::FkTypeMismatch);
        assert_eq!(found.len(), 1, "{findings:?}");
        assert_eq!(
            found[0].finding.message,
            "foreign key posts_author_fk compares public.posts.author_id (int8) with \
             public.users.id (int4)"
        );
        assert_eq!(
            found[0].finding.help.as_deref(),
            Some("give author_id the type int4, so the join compares without a cast")
        );
    }

    #[test]
    fn fk_type_mismatch_resolves_domains_and_ignores_typmod() {
        let findings = lint(
            &[(
                "001_base.sql",
                "CREATE DOMAIN big_id AS bigint;
                     CREATE TABLE parents (id bigint PRIMARY KEY);
                     CREATE TABLE children (id big_id);
                     ALTER TABLE children ADD CONSTRAINT children_fk
                         FOREIGN KEY (id) REFERENCES parents (id);",
                MigrationLint::All,
            )],
            true,
            &LintOptions::default(),
        );
        assert!(
            only(&findings, Rule::FkTypeMismatch).is_empty(),
            "{findings:?}"
        );

        let findings = lint(
            &[(
                "001_base.sql",
                "CREATE TABLE parents (code varchar(10) PRIMARY KEY);
                     CREATE TABLE children (code varchar(20));
                     ALTER TABLE children ADD CONSTRAINT children_fk
                         FOREIGN KEY (code) REFERENCES parents (code);",
                MigrationLint::All,
            )],
            true,
            &LintOptions::default(),
        );
        assert!(
            only(&findings, Rule::FkTypeMismatch).is_empty(),
            "{findings:?}"
        );
    }

    #[test]
    fn fk_type_mismatch_reports_text_against_varchar() {
        let findings = lint(
            &[(
                "001_base.sql",
                "CREATE TABLE parents (code varchar(10) PRIMARY KEY);
                     CREATE TABLE children (code text);
                     ALTER TABLE children ADD CONSTRAINT children_fk
                         FOREIGN KEY (code) REFERENCES parents (code);",
                MigrationLint::All,
            )],
            true,
            &LintOptions::default(),
        );
        let found = only(&findings, Rule::FkTypeMismatch);
        assert_eq!(found.len(), 1, "{findings:?}");
        assert!(found[0].finding.message.contains("(text)"));
        assert!(found[0].finding.message.contains("(varchar)"));
    }

    // ── missing_lock_timeout ───────────────────────────────────────────────

    #[test]
    fn missing_lock_timeout_reports_the_first_blocking_statement() {
        let text = "ALTER TABLE users ADD COLUMN nickname text;";
        let findings = second(text);
        let found = only(&findings, Rule::MissingLockTimeout);
        assert_eq!(found.len(), 1, "{findings:?}");
        assert_eq!(
            found[0].finding.message,
            "ALTER TABLE waits for a lock on public.users with no lock_timeout: queued behind \
             a long transaction, it blocks every query on the table"
        );
        assert_eq!(
            found[0].finding.help.as_deref(),
            Some("start the migration with SET LOCAL lock_timeout = '5s' and retry on timeout")
        );
        assert_eq!(
            &text[found[0].finding.span.clone()],
            "ALTER TABLE users ADD COLUMN nickname text"
        );
    }

    #[test]
    fn missing_lock_timeout_ignores_tables_created_in_the_same_file() {
        let findings = lint(
            &[(
                "001_all.sql",
                &format!("{BASE}\nALTER TABLE users ADD COLUMN nickname text;"),
                MigrationLint::All,
            )],
            true,
            &LintOptions::default(),
        );
        assert!(
            only(&findings, Rule::MissingLockTimeout).is_empty(),
            "{findings:?}"
        );
    }

    #[test]
    fn missing_lock_timeout_reads_set_lock_timeout() {
        for sql in [
            "SET LOCAL lock_timeout = '3s';
             ALTER TABLE users ADD COLUMN nickname text;",
            "SET lock_timeout TO 5000;
             ALTER TABLE users ADD COLUMN nickname text;",
            "SET SESSION lock_timeout = '3s';
             ALTER TABLE users ADD COLUMN nickname text;",
            // `set_config` sets the same GUC as `SET` (and its third
            // argument as `SET LOCAL` does).
            "SELECT set_config('lock_timeout', '3s', false);
             ALTER TABLE users ADD COLUMN nickname text;",
            "SELECT pg_catalog.set_config('lock_timeout', '3s', true);
             ALTER TABLE users ADD COLUMN nickname text;",
            "SELECT set_config('lock_timeout'::text, '3s'::text, false);
             ALTER TABLE users ADD COLUMN nickname text;",
        ] {
            let findings = second(sql);
            assert!(
                only(&findings, Rule::MissingLockTimeout).is_empty(),
                "{sql}: {findings:?}"
            );
        }
        // A zero value disables the timeout, and a call whose arguments are
        // not constants is not trusted.
        for sql in [
            "SELECT set_config('lock_timeout', '0', false);
             ALTER TABLE users ADD COLUMN nickname text;",
            "SELECT set_config('lock_timeout', 0::text, true);
             ALTER TABLE users ADD COLUMN nickname text;",
            "SELECT set_config('lock_timeout', random()::text, false);
             ALTER TABLE users ADD COLUMN nickname text;",
            "SELECT set_config('lock_timeout', '3s', false) WHERE false;
             ALTER TABLE users ADD COLUMN nickname text;",
        ] {
            let findings = second(sql);
            assert_eq!(
                only(&findings, Rule::MissingLockTimeout).len(),
                1,
                "{sql}: {findings:?}"
            );
        }
        for sql in [
            "SET lock_timeout = 0;
             ALTER TABLE users ADD COLUMN nickname text;",
            "SET lock_timeout = '0';
             ALTER TABLE users ADD COLUMN nickname text;",
            "SET lock_timeout = DEFAULT;
             ALTER TABLE users ADD COLUMN nickname text;",
            "SET SESSION lock_timeout = 0;
             ALTER TABLE users ADD COLUMN nickname text;",
            "RESET lock_timeout;
             ALTER TABLE users ADD COLUMN nickname text;",
            "RESET ALL;
             ALTER TABLE users ADD COLUMN nickname text;",
            "SET lock_timeout = '3s';
             RESET ALL;
             ALTER TABLE users ADD COLUMN nickname text;",
            "SET lock_timeout = '3s';
             SET lock_timeout TO DEFAULT;
             ALTER TABLE users ADD COLUMN nickname text;",
        ] {
            let findings = second(sql);
            assert_eq!(
                only(&findings, Rule::MissingLockTimeout).len(),
                1,
                "{sql}: {findings:?}"
            );
        }
    }

    #[test]
    fn missing_lock_timeout_ignores_set_local_outside_a_transaction() {
        let sql = "SET LOCAL lock_timeout = '3s';
                   ALTER TABLE users ADD COLUMN nickname text;";
        for findings in [
            second_with(sql, false, &LintOptions::default()),
            second_with(
                &format!("-- no-transaction\n{sql}"),
                true,
                &LintOptions::default(),
            ),
        ] {
            assert_eq!(
                only(&findings, Rule::MissingLockTimeout).len(),
                1,
                "{findings:?}"
            );
        }
    }

    #[test]
    fn missing_lock_timeout_tracks_an_explicit_transaction() {
        // An explicit transaction lets SET LOCAL / set_config(…, true) hold
        // until its end, even in a -- no-transaction file.
        for sql in [
            "BEGIN;
             SET LOCAL lock_timeout = '3s';
             ALTER TABLE users ADD COLUMN nickname text;
             COMMIT;",
            "BEGIN;
             SELECT set_config('lock_timeout', '3s', true);
             ALTER TABLE users ADD COLUMN nickname text;
             COMMIT;",
            "START TRANSACTION;
             SET LOCAL lock_timeout = '3s';
             ALTER TABLE users ADD COLUMN nickname text;
             END;",
        ] {
            for use_transaction in [false, true] {
                let findings = second_with(
                    &format!("-- no-transaction\n{sql}"),
                    use_transaction,
                    &LintOptions::default(),
                );
                assert!(
                    only(&findings, Rule::MissingLockTimeout).is_empty(),
                    "{sql}: {findings:?}"
                );
            }
        }

        // After COMMIT the LOCAL value is gone: the next statement has no
        // timeout again. A session SET, by contrast, survives.
        let findings = second_with(
            "-- no-transaction
             BEGIN;
             SET LOCAL lock_timeout = '3s';
             COMMIT;
             ALTER TABLE users ADD COLUMN nickname text;",
            false,
            &LintOptions::default(),
        );
        assert_eq!(
            only(&findings, Rule::MissingLockTimeout).len(),
            1,
            "{findings:?}"
        );
        let findings = second_with(
            "-- no-transaction
             BEGIN;
             SET lock_timeout = '3s';
             COMMIT;
             ALTER TABLE users ADD COLUMN nickname text;",
            false,
            &LintOptions::default(),
        );
        assert!(
            only(&findings, Rule::MissingLockTimeout).is_empty(),
            "{findings:?}"
        );
        // A LOCAL zero overrides the session value inside the transaction.
        let findings = second_with(
            "-- no-transaction
             SET lock_timeout = '3s';
             BEGIN;
             SET LOCAL lock_timeout = 0;
             ALTER TABLE users ADD COLUMN nickname text;
             COMMIT;",
            false,
            &LintOptions::default(),
        );
        assert_eq!(
            only(&findings, Rule::MissingLockTimeout).len(),
            1,
            "{findings:?}"
        );
    }

    #[test]
    fn missing_lock_timeout_tracks_savepoints_and_rollbacks() {
        // `-- no-transaction`: the explicit `BEGIN` and its savepoints are
        // the only transaction state.
        let reported = [
            // ROLLBACK TO restores the savepoint: the SET LOCAL after it is
            // gone.
            "BEGIN; SAVEPOINT s; SET LOCAL lock_timeout = '3s'; ROLLBACK TO s; \
             ALTER TABLE users ADD COLUMN nickname text; COMMIT;",
            // A session SET is rolled back with the transaction.
            "BEGIN; SET lock_timeout = '3s'; ROLLBACK; \
             ALTER TABLE users ADD COLUMN nickname text;",
            // ROLLBACK TO also undoes a session SET made after the savepoint.
            "BEGIN; SAVEPOINT s; SET lock_timeout = '3s'; ROLLBACK TO s; \
             ALTER TABLE users ADD COLUMN nickname text; COMMIT;",
            // A full ROLLBACK restores the state from BEGIN even after a
            // RELEASE kept the session value for the statement inside.
            "BEGIN; SAVEPOINT s; SET lock_timeout = '3s'; RELEASE s; ROLLBACK; \
             ALTER TABLE users ADD COLUMN nickname text;",
            // Nested savepoints restore the requested one.
            "BEGIN; SAVEPOINT a; SET LOCAL lock_timeout = '3s'; SAVEPOINT b; \
             ROLLBACK TO a; ALTER TABLE users ADD COLUMN nickname text; COMMIT;",
            // ROLLBACK AND CHAIN starts a new transaction from the rolled
            // back state.
            "BEGIN; SET lock_timeout = '3s'; ROLLBACK AND CHAIN; \
             ALTER TABLE users ADD COLUMN nickname text; ROLLBACK;",
        ];
        for sql in reported {
            let findings = second_with(
                &format!("-- no-transaction\n{sql}"),
                false,
                &LintOptions::default(),
            );
            assert_eq!(
                only(&findings, Rule::MissingLockTimeout).len(),
                1,
                "{sql}: {findings:?}"
            );
        }
        let not_reported = [
            // COMMIT keeps the session value.
            "BEGIN; SET lock_timeout = '3s'; COMMIT; \
             ALTER TABLE users ADD COLUMN nickname text;",
            // RELEASE keeps the changes the savepoint saw.
            "BEGIN; SAVEPOINT s; SET LOCAL lock_timeout = '3s'; RELEASE s; \
             ALTER TABLE users ADD COLUMN nickname text; COMMIT;",
            // COMMIT AND CHAIN keeps the session value in the new
            // transaction.
            "BEGIN; SET lock_timeout = '3s'; COMMIT AND CHAIN; \
             ALTER TABLE users ADD COLUMN nickname text; ROLLBACK;",
        ];
        for sql in not_reported {
            let findings = second_with(
                &format!("-- no-transaction\n{sql}"),
                false,
                &LintOptions::default(),
            );
            assert!(
                only(&findings, Rule::MissingLockTimeout).is_empty(),
                "{sql}: {findings:?}"
            );
        }
    }

    #[test]
    fn missing_lock_timeout_evaluates_set_config_values_strictly() {
        // NULL resets the setting to its default (0, disabled).
        let reported = [
            "SELECT set_config('lock_timeout', NULL, false);
             ALTER TABLE users ADD COLUMN nickname text;",
            // An unresolved value clears an earlier proof.
            "SET lock_timeout = '3s';
             SELECT set_config('lock_timeout', random()::text, false);
             ALTER TABLE users ADD COLUMN nickname text;",
            // A cast chain is evaluated: this one is '0'.
            "SELECT set_config('lock_timeout', '0.4'::numeric::integer::text, false);
             ALTER TABLE users ADD COLUMN nickname text;",
            // Only a ::text cast of a string literal is trusted.
            "SELECT set_config('lock_timeout', '3s'::varchar, false);
             ALTER TABLE users ADD COLUMN nickname text;",
            // PostgreSQL rounds to whole milliseconds: 0.4ms and 0.5ms are
            // 0 (disabled), 0.6ms is 1ms.
            "SELECT set_config('lock_timeout', '0.4ms', false);
             ALTER TABLE users ADD COLUMN nickname text;",
            "SELECT set_config('lock_timeout', '0.5ms', false);
             ALTER TABLE users ADD COLUMN nickname text;",
            "SET lock_timeout = 0.4;
             ALTER TABLE users ADD COLUMN nickname text;",
        ];
        for sql in reported {
            let findings = second(sql);
            assert_eq!(
                only(&findings, Rule::MissingLockTimeout).len(),
                1,
                "{sql}: {findings:?}"
            );
        }
        for sql in [
            "SELECT set_config('lock_timeout', '0.6ms', false);
             ALTER TABLE users ADD COLUMN nickname text;",
            "SELECT set_config('lock_timeout', '3s'::text, false);
             ALTER TABLE users ADD COLUMN nickname text;",
            "SELECT pg_catalog.set_config('lock_timeout', '3s', false);
             ALTER TABLE users ADD COLUMN nickname text;",
        ] {
            let findings = second(sql);
            assert!(
                only(&findings, Rule::MissingLockTimeout).is_empty(),
                "{sql}: {findings:?}"
            );
        }
    }

    #[test]
    fn missing_lock_timeout_does_not_trust_a_shadowed_set_config() {
        let findings = second(
            "CREATE FUNCTION public.set_config(text, text, boolean) RETURNS text
                 LANGUAGE sql AS 'SELECT $2';
             SET search_path = public, pg_catalog;
             SELECT set_config('lock_timeout', '3s', false);
             ALTER TABLE users ADD COLUMN nickname text;",
        );
        assert_eq!(
            only(&findings, Rule::MissingLockTimeout).len(),
            1,
            "{findings:?}"
        );
    }

    #[test]
    fn missing_lock_timeout_reports_at_most_once_per_file() {
        let text = "ALTER TABLE users ADD COLUMN first text;
                    ALTER TABLE users ADD COLUMN second text;";
        let found = only(&second(text), Rule::MissingLockTimeout);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            &text[found[0].finding.span.clone()],
            "ALTER TABLE users ADD COLUMN first text"
        );
    }

    #[test]
    fn missing_lock_timeout_handles_create_table_foreign_keys() {
        let findings = second(
            "CREATE TABLE new_posts (
                 id integer PRIMARY KEY,
                 author_id integer REFERENCES users (id)
             );",
        );
        let found = only(&findings, Rule::MissingLockTimeout);
        assert_eq!(found.len(), 1, "{findings:?}");
        assert!(
            found[0]
                .finding
                .message
                .starts_with("CREATE TABLE waits for a lock on public.users"),
            "{found:?}"
        );
    }

    #[test]
    fn missing_lock_timeout_ignores_share_update_exclusive_subcommands() {
        for sql in [
            "ALTER TABLE users VALIDATE CONSTRAINT users_email_nn;",
            "ALTER TABLE users ALTER COLUMN email SET STATISTICS 100;",
            "ALTER TABLE users ALTER COLUMN email SET (n_distinct = 100);",
            "ALTER TABLE users ALTER COLUMN email RESET (n_distinct);",
            "ALTER TABLE users CLUSTER ON users_email_idx;",
            "ALTER TABLE users SET WITHOUT CLUSTER;",
            "ALTER TABLE users SET (fillfactor = 70);",
            "ALTER TABLE users RESET (fillfactor);",
        ] {
            let findings = second(sql);
            assert!(
                only(&findings, Rule::MissingLockTimeout).is_empty(),
                "{sql}: {findings:?}"
            );
        }
    }

    #[test]
    fn missing_lock_timeout_reports_access_exclusive_storage_options() {
        let findings = second("ALTER TABLE users SET (user_catalog_table = true);");
        assert_eq!(
            only(&findings, Rule::MissingLockTimeout).len(),
            1,
            "{findings:?}"
        );
    }

    #[test]
    fn missing_lock_timeout_honors_the_lock_table_mode() {
        for mode in [
            "ACCESS SHARE",
            "ROW SHARE",
            "ROW EXCLUSIVE",
            "SHARE UPDATE EXCLUSIVE",
        ] {
            let sql = format!("LOCK TABLE users IN {mode} MODE;");
            let findings = second(&sql);
            assert!(
                only(&findings, Rule::MissingLockTimeout).is_empty(),
                "{sql}: {findings:?}"
            );
        }
        for mode in [
            "SHARE",
            "SHARE ROW EXCLUSIVE",
            "EXCLUSIVE",
            "ACCESS EXCLUSIVE",
        ] {
            let sql = format!("LOCK TABLE users IN {mode} MODE;");
            let findings = second(&sql);
            let found = only(&findings, Rule::MissingLockTimeout);
            assert_eq!(found.len(), 1, "{sql}: {findings:?}");
            assert!(
                found[0]
                    .finding
                    .message
                    .starts_with("LOCK TABLE waits for a lock on public.users"),
                "{sql}: {found:?}"
            );
        }
    }

    const PARTITIONED: &str = r#"
        CREATE TABLE measurements (id integer, at timestamp) PARTITION BY RANGE (id);
        CREATE TABLE measurements_1 PARTITION OF measurements FOR VALUES FROM (0) TO (100);
        CREATE TABLE measurements_new (id integer, at timestamp);
    "#;

    #[test]
    fn missing_lock_timeout_reports_an_attached_partition() {
        let text = "ALTER TABLE measurements ATTACH PARTITION measurements_new \
                    FOR VALUES FROM (100) TO (200);";
        let findings = lint(
            &[
                ("001_base.sql", PARTITIONED, MigrationLint::All),
                ("002_attach.sql", text, MigrationLint::All),
            ],
            true,
            &LintOptions::default(),
        );
        let found = only(&findings, Rule::MissingLockTimeout);
        assert_eq!(found.len(), 1, "{findings:?}");
        assert_eq!(
            found[0].finding.message,
            "ALTER TABLE waits for a lock on public.measurements_new with no lock_timeout: \
             queued behind a long transaction, it blocks every query on the table"
        );
        assert_eq!(
            &text[found[0].finding.span.clone()],
            "ALTER TABLE measurements ATTACH PARTITION measurements_new \
             FOR VALUES FROM (100) TO (200)"
        );
    }

    #[test]
    fn missing_lock_timeout_reports_a_detached_partition() {
        let text = "ALTER TABLE measurements DETACH PARTITION measurements_1;";
        let findings = lint(
            &[
                ("001_base.sql", PARTITIONED, MigrationLint::All),
                ("002_detach.sql", text, MigrationLint::All),
            ],
            true,
            &LintOptions::default(),
        );
        let found = only(&findings, Rule::MissingLockTimeout);
        assert_eq!(found.len(), 1, "{findings:?}");
        assert_eq!(
            found[0].finding.message,
            "ALTER TABLE waits for a lock on public.measurements with no lock_timeout: \
             queued behind a long transaction, it blocks every query on the table"
        );
    }

    #[test]
    fn missing_lock_timeout_ignores_a_concurrent_detach() {
        let findings = lint(
            &[
                ("001_base.sql", PARTITIONED, MigrationLint::All),
                (
                    "002_detach.sql",
                    "ALTER TABLE measurements DETACH PARTITION measurements_1 CONCURRENTLY;",
                    MigrationLint::All,
                ),
            ],
            false,
            &LintOptions::default(),
        );
        assert!(
            only(&findings, Rule::MissingLockTimeout).is_empty(),
            "{findings:?}"
        );
    }

    #[test]
    fn missing_lock_timeout_ignores_a_detach_finalize() {
        let parsed = typedpg_pg_query::parse(
            "ALTER TABLE measurements DETACH PARTITION measurements_1 FINALIZE",
        )
        .unwrap();
        let node = parsed.tree.stmts[0].stmt.as_ref().unwrap();
        let N::AlterTableStmt(alter) = node else {
            panic!("expected ALTER TABLE");
        };
        let command = alter
            .cmds
            .first()
            .and_then(|n| alter_command(n.as_ref()))
            .unwrap();
        assert!(only_share_update_exclusive(command), "{command:?}");
    }

    #[test]
    fn missing_lock_timeout_reports_the_statement_kind() {
        for (sql, kind) in [
            ("DROP TABLE posts;", "DROP TABLE"),
            ("DROP INDEX users_email_idx;", "DROP INDEX"),
            ("TRUNCATE users;", "TRUNCATE"),
            ("ALTER TABLE users RENAME TO accounts;", "ALTER TABLE"),
            ("LOCK TABLE users;", "LOCK TABLE"),
            ("CLUSTER users USING users_email_idx;", "CLUSTER"),
            ("VACUUM FULL users;", "VACUUM FULL"),
            ("REINDEX TABLE users;", "REINDEX"),
        ] {
            let findings = if sql.starts_with("VACUUM") {
                second_with(sql, false, &LintOptions::default())
            } else {
                second(sql)
            };
            let found = only(&findings, Rule::MissingLockTimeout);
            assert_eq!(found.len(), 1, "{sql}: {findings:?}");
            assert!(
                found[0]
                    .finding
                    .message
                    .starts_with(&format!("{kind} waits for a lock")),
                "{sql}: {found:?}"
            );
        }
    }

    #[test]
    fn missing_lock_timeout_names_the_table_and_what_queues_behind_it() {
        let reads = "queued behind a long transaction, it blocks every query on the table";
        let writes = "queued behind a long transaction, it blocks every write to the table";
        for (sql, message) in [
            // DROP INDEX locks its table ACCESS EXCLUSIVE.
            (
                "DROP INDEX users_email_idx;",
                format!(
                    "DROP INDEX waits for a lock on public.users (the table of \
                     public.users_email_idx) with no lock_timeout: {reads}"
                ),
            ),
            // REINDEX INDEX waits for SHARE on the table.
            (
                "REINDEX INDEX users_email_idx;",
                format!(
                    "REINDEX INDEX waits for a lock on public.users (the table of \
                     public.users_email_idx) with no lock_timeout: {writes}"
                ),
            ),
            (
                "REINDEX TABLE users;",
                format!("REINDEX waits for a lock on public.users with no lock_timeout: {writes}"),
            ),
            (
                "CREATE INDEX users_id_email_idx ON users (id, email);",
                format!(
                    "CREATE INDEX waits for a lock on public.users with no lock_timeout: {writes}"
                ),
            ),
            (
                "CREATE TRIGGER t BEFORE UPDATE ON users FOR EACH ROW EXECUTE FUNCTION \
                 suppress_redundant_updates_trigger();",
                format!(
                    "CREATE TRIGGER waits for a lock on public.users with no lock_timeout: {writes}"
                ),
            ),
            (
                "ALTER TABLE users DISABLE TRIGGER ALL;",
                format!(
                    "ALTER TABLE waits for a lock on public.users with no lock_timeout: {writes}"
                ),
            ),
            (
                "ALTER TABLE posts ADD CONSTRAINT posts_author_fk FOREIGN KEY (author_id) \
                 REFERENCES users (id) NOT VALID;",
                format!(
                    "ALTER TABLE waits for a lock on public.posts with no lock_timeout: {writes}"
                ),
            ),
            // One ACCESS EXCLUSIVE subcommand makes the whole statement's.
            (
                "ALTER TABLE users DISABLE TRIGGER ALL, ADD COLUMN nickname text;",
                format!(
                    "ALTER TABLE waits for a lock on public.users with no lock_timeout: {reads}"
                ),
            ),
            (
                "LOCK TABLE users IN EXCLUSIVE MODE;",
                format!(
                    "LOCK TABLE waits for a lock on public.users with no lock_timeout: {writes}"
                ),
            ),
            (
                "LOCK TABLE users;",
                format!(
                    "LOCK TABLE waits for a lock on public.users with no lock_timeout: {reads}"
                ),
            ),
            (
                "TRUNCATE users;",
                format!("TRUNCATE waits for a lock on public.users with no lock_timeout: {reads}"),
            ),
        ] {
            let findings = second(sql);
            let found = only(&findings, Rule::MissingLockTimeout);
            assert_eq!(found.len(), 1, "{sql}: {findings:?}");
            assert_eq!(found[0].finding.message, message, "{sql}");
        }
    }

    #[test]
    fn redundant_index_reports_every_redundant_index_in_one_run() {
        let messages = |sql: &str| -> Vec<String> {
            let findings = lint(
                &[("001_all.sql", sql, MigrationLint::All)],
                true,
                &LintOptions::default(),
            );
            only(&findings, Rule::RedundantIndex)
                .iter()
                .map(|found| found.finding.message.clone())
                .collect()
        };
        // Both `(email)` indexes go, and each names the index that stays.
        assert_eq!(
            messages(
                "CREATE TABLE users (id integer PRIMARY KEY, email text, name text);
                 CREATE INDEX users_email_a ON users (email);
                 CREATE INDEX users_email_b ON users (email);
                 CREATE INDEX users_email_name ON users (email, name);"
            ),
            [
                "index users_email_a is redundant with users_email_name",
                "index users_email_b is redundant with users_email_name",
            ]
        );
        // A chain: each one's survivor is the widest index.
        assert_eq!(
            messages(
                "CREATE TABLE t (a integer, b integer, c integer);
                 CREATE INDEX t_a ON t (a);
                 CREATE INDEX t_ab ON t (a, b);
                 CREATE INDEX t_abc ON t (a, b, c);"
            ),
            [
                "index t_a is redundant with t_abc",
                "index t_ab is redundant with t_abc",
            ]
        );
        // Without a wider index, only the later duplicate goes.
        assert_eq!(
            messages(
                "CREATE TABLE users (id integer PRIMARY KEY, email text);
                 CREATE INDEX users_email_a ON users (email);
                 CREATE INDEX users_email_b ON users (email);"
            ),
            ["index users_email_b is redundant with users_email_a"]
        );
        // A unique index stays (it enforces something); the plain ones it
        // covers name it.
        assert_eq!(
            messages(
                "CREATE TABLE users (id integer PRIMARY KEY, email text, name text);
                 CREATE INDEX users_email_a ON users (email);
                 CREATE INDEX users_email_b ON users (email);
                 CREATE UNIQUE INDEX users_email_name ON users (email, name);"
            ),
            [
                "index users_email_a is redundant with users_email_name",
                "index users_email_b is redundant with users_email_name",
            ]
        );
    }

    #[test]
    fn create_index_blocking_says_add_constraint_blocks_reads_too() {
        let findings = second("ALTER TABLE users ADD CONSTRAINT users_email_key UNIQUE (email);");
        let found = only(&findings, Rule::CreateIndexBlocking);
        assert_eq!(found.len(), 1, "{findings:?}");
        assert_eq!(
            found[0].finding.message,
            "adding the constraint builds its index under an ACCESS EXCLUSIVE lock, blocking \
             reads and writes on public.users for the whole build"
        );
        assert_eq!(
            found[0].finding.help.as_deref(),
            Some(
                "CREATE UNIQUE INDEX CONCURRENTLY first (in a migration whose first line is \
                 -- no-transaction), then ADD CONSTRAINT … UNIQUE / PRIMARY KEY USING INDEX, \
                 which only takes the lock briefly"
            )
        );
        // An exclusion constraint can't adopt a concurrently built index.
        let findings = second(
            "ALTER TABLE users ADD CONSTRAINT users_id_excl EXCLUDE USING btree (id WITH =);",
        );
        let found = only(&findings, Rule::CreateIndexBlocking);
        assert_eq!(found.len(), 1, "{findings:?}");
        assert_eq!(
            found[0].finding.help.as_deref(),
            Some(
                "an EXCLUDE constraint can't adopt an index built CONCURRENTLY: add it while \
                 the table is small, or when the table can be locked for the whole build"
            )
        );
    }

    #[test]
    fn missing_lock_timeout_ignores_concurrent_operations() {
        let findings = lint(
            &[
                ("001_base.sql", BASE, MigrationLint::All),
                (
                    "002_change.sql",
                    "CREATE INDEX CONCURRENTLY users_email_concurrent_idx ON users (email);",
                    MigrationLint::All,
                ),
            ],
            false,
            &LintOptions::default(),
        );
        assert!(
            only(&findings, Rule::MissingLockTimeout).is_empty(),
            "{findings:?}"
        );
    }

    #[test]
    fn missing_lock_timeout_help_depends_on_the_transaction() {
        let sql = "ALTER TABLE users ADD COLUMN nickname text;";
        let inside = second(sql);
        assert_eq!(
            only(&inside, Rule::MissingLockTimeout)[0]
                .finding
                .help
                .as_deref(),
            Some("start the migration with SET LOCAL lock_timeout = '5s' and retry on timeout")
        );

        let outside = second_with(sql, false, &LintOptions::default());
        assert_eq!(
            only(&outside, Rule::MissingLockTimeout)[0]
                .finding
                .help
                .as_deref(),
            Some("start the migration with SET lock_timeout = '5s' and retry on timeout")
        );

        let marker = second_with(
            &format!("-- no-transaction\n{sql}"),
            true,
            &LintOptions::default(),
        );
        assert_eq!(
            only(&marker, Rule::MissingLockTimeout)[0]
                .finding
                .help
                .as_deref(),
            Some("start the migration with SET lock_timeout = '5s' and retry on timeout")
        );
    }

    // ── opinion rules ──────────────────────────────────────────────────────

    const OPINIONS: &str = r#"
        CREATE TABLE opinions (
            id integer PRIMARY KEY,
            at timestamp,
            tags timestamp[],
            short varchar(10),
            unbound varchar,
            padded char(3),
            body text,
            serial_id serial
        );
    "#;

    fn opinion_findings(rules: &[Rule]) -> Vec<MigrationFinding> {
        lint(
            &[("001_opinions.sql", OPINIONS, MigrationLint::All)],
            true,
            &opinion_options(rules),
        )
    }

    #[test]
    fn opinion_rules_are_off_by_default() {
        let findings = lint(
            &[("001_opinions.sql", OPINIONS, MigrationLint::All)],
            true,
            &LintOptions::default(),
        );
        for rule in [
            Rule::PreferTimestamptz,
            Rule::PreferIdentity,
            Rule::PreferText,
            Rule::PreferBigintPk,
        ] {
            assert!(only(&findings, rule).is_empty(), "{rule}: {findings:?}");
        }
    }

    #[test]
    fn prefer_timestamptz_reports_timestamp_and_arrays() {
        let found = only(
            &opinion_findings(&[Rule::PreferTimestamptz]),
            Rule::PreferTimestamptz,
        );
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(
            found[0].finding.message,
            "public.opinions.at is timestamp without time zone: it stores no offset, so the \
             same instant reads differently per session TimeZone"
        );
        assert_eq!(found[0].finding.help.as_deref(), Some("use timestamptz"));
        assert_eq!(
            found[1].finding.message,
            "public.opinions.tags is timestamp without time zone: it stores no offset, so the \
             same instant reads differently per session TimeZone"
        );
    }

    #[test]
    fn prefer_identity_reports_serial_columns_only() {
        let found = only(
            &opinion_findings(&[Rule::PreferIdentity]),
            Rule::PreferIdentity,
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            found[0].finding.message,
            "public.opinions.serial_id is a serial column"
        );
        assert_eq!(
            found[0].finding.help.as_deref(),
            Some(
                "use GENERATED BY DEFAULT AS IDENTITY: the sequence then belongs to the column \
                 (permissions, dumps, ALTER TYPE follow it)"
            )
        );

        let findings = lint(
            &[(
                "001_identity.sql",
                "CREATE TABLE identities (id integer GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY);",
                MigrationLint::All,
            )],
            true,
            &opinion_options(&[Rule::PreferIdentity]),
        );
        assert!(
            only(&findings, Rule::PreferIdentity).is_empty(),
            "{findings:?}"
        );
    }

    #[test]
    fn prefer_text_reports_bounded_varchar_and_char() {
        let found = only(&opinion_findings(&[Rule::PreferText]), Rule::PreferText);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(
            found[0].finding.message,
            "public.opinions.short is varchar(10)"
        );
        assert_eq!(
            found[0].finding.help.as_deref(),
            Some("use text, with a CHECK (char_length(short) <= 10) if the limit matters")
        );
        assert_eq!(
            found[1].finding.message,
            "public.opinions.padded is char(3): char pads with spaces"
        );
    }

    #[test]
    fn prefer_text_reports_arrays_and_skips_unlimited_varchar() {
        let findings = lint(
            &[(
                "001_text.sql",
                "CREATE TABLE texts (a varchar(10)[], b varchar, c text, d char(2)[]);",
                MigrationLint::All,
            )],
            true,
            &opinion_options(&[Rule::PreferText]),
        );
        let found = only(&findings, Rule::PreferText);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0].finding.message, "public.texts.a is varchar(10)[]");
        assert_eq!(found[0].finding.help.as_deref(), Some("use text[]"));
        assert_eq!(
            found[1].finding.message,
            "public.texts.d is char(2)[]: char pads with spaces"
        );
        assert_eq!(found[1].finding.help.as_deref(), Some("use text[]"));
    }

    #[test]
    fn prefer_bigint_pk_reports_smallint_int_and_domain_primary_keys() {
        let findings = lint(
            &[(
                "001_pk.sql",
                "CREATE DOMAIN int_domain AS integer;
                 CREATE TABLE pk_small (id smallint PRIMARY KEY);
                 CREATE TABLE pk_int (id integer PRIMARY KEY);
                 CREATE TABLE pk_big (id bigint PRIMARY KEY);
                 CREATE TABLE pk_domain (id int_domain PRIMARY KEY);",
                MigrationLint::All,
            )],
            true,
            &opinion_options(&[Rule::PreferBigintPk]),
        );
        let found = only(&findings, Rule::PreferBigintPk);
        assert_eq!(found.len(), 3, "{findings:?}");
        assert_eq!(
            found[0].finding.message,
            "public.pk_small.id is an int2 primary key: it runs out at 32767"
        );
        assert_eq!(
            found[1].finding.message,
            "public.pk_int.id is an int4 primary key: it runs out at 2147483647"
        );
        assert_eq!(
            found[2].finding.message,
            "public.pk_domain.id is an int4 primary key: it runs out at 2147483647"
        );
        assert_eq!(found[0].finding.help.as_deref(), Some("use bigint"));
    }

    #[test]
    fn opinion_rules_follow_a_later_alter_column_type() {
        let base = "CREATE TABLE events (id bigint PRIMARY KEY, at timestamptz);";
        let findings = lint(
            &[
                ("001_base.sql", base, MigrationLint::All),
                (
                    "002_change.sql",
                    "ALTER TABLE events ALTER COLUMN at TYPE timestamp;",
                    MigrationLint::All,
                ),
            ],
            true,
            &opinion_options(&[Rule::PreferTimestamptz]),
        );
        let found = only(&findings, Rule::PreferTimestamptz);
        assert_eq!(found.len(), 1, "{findings:?}");
        assert_eq!(found[0].file, 1);
        let sql = "ALTER TABLE events ALTER COLUMN at TYPE timestamp;";
        assert_eq!(
            &sql[found[0].finding.span.clone()],
            "ALTER TABLE events ALTER COLUMN at TYPE timestamp"
        );

        let fixed = lint(
            &[
                (
                    "001_base.sql",
                    "CREATE TABLE events (id bigint PRIMARY KEY, at timestamp);",
                    MigrationLint::All,
                ),
                (
                    "002_change.sql",
                    "ALTER TABLE events ALTER COLUMN at TYPE timestamptz;",
                    MigrationLint::All,
                ),
            ],
            true,
            &opinion_options(&[Rule::PreferTimestamptz]),
        );
        assert!(
            only(&fixed, Rule::PreferTimestamptz).is_empty(),
            "{fixed:?}"
        );
    }

    #[test]
    fn opinion_rules_report_a_partitioned_table_once() {
        let findings = lint(
            &[(
                "001_partition.sql",
                "CREATE TABLE events (id integer PRIMARY KEY, at timestamp)
                     PARTITION BY RANGE (id);
                 CREATE TABLE events_1 PARTITION OF events FOR VALUES FROM (0) TO (10);",
                MigrationLint::All,
            )],
            true,
            &opinion_options(&[Rule::PreferTimestamptz, Rule::PreferBigintPk]),
        );
        assert_eq!(
            only(&findings, Rule::PreferTimestamptz).len(),
            1,
            "{findings:?}"
        );
        assert_eq!(
            only(&findings, Rule::PreferBigintPk).len(),
            1,
            "{findings:?}"
        );
    }

    #[test]
    fn opinion_rules_keep_the_original_creator_when_create_table_if_not_exists_is_a_no_op() {
        let base = "CREATE TABLE names (id integer PRIMARY KEY, at timestamp, short varchar(10));";
        let no_op = "-- typedpg-lint: allow(prefer_timestamptz, prefer_text, prefer_bigint_pk)\n\
                     CREATE TABLE IF NOT EXISTS names (
                         id integer PRIMARY KEY,
                         at timestamp,
                         short varchar(10)
                     );";
        let rules = [
            Rule::PreferTimestamptz,
            Rule::PreferText,
            Rule::PreferBigintPk,
        ];
        let findings = lint(
            &[
                ("001_base.sql", base, MigrationLint::All),
                ("002_noop.sql", no_op, MigrationLint::All),
            ],
            true,
            &opinion_options(&rules),
        );
        for rule in rules {
            let found = only(&findings, rule);
            assert_eq!(found.len(), 1, "{rule}: {findings:?}");
            assert_eq!(found[0].file, 0, "{rule}: {findings:?}");
        }
    }

    #[test]
    fn opinion_rules_keep_the_original_creator_when_create_table_as_if_not_exists_is_a_no_op() {
        let base = "CREATE TABLE copies (at timestamp, short varchar(10));";
        let no_op = "-- typedpg-lint: allow(prefer_timestamptz, prefer_text)\n\
                     CREATE TABLE IF NOT EXISTS copies AS
                         SELECT now()::timestamp AS at, 'x'::varchar(10) AS short;";
        let rules = [Rule::PreferTimestamptz, Rule::PreferText];
        let findings = lint(
            &[
                ("001_base.sql", base, MigrationLint::All),
                ("002_noop.sql", no_op, MigrationLint::All),
            ],
            true,
            &opinion_options(&rules),
        );
        for rule in rules {
            let found = only(&findings, rule);
            assert_eq!(found.len(), 1, "{rule}: {findings:?}");
            assert_eq!(found[0].file, 0, "{rule}: {findings:?}");
        }
    }

    #[test]
    fn allow_comments_suppress_the_new_rules_per_statement() {
        let base = ("001_base.sql", BASE, MigrationLint::None);

        let fk_sql = "ALTER TABLE posts ALTER COLUMN author_id TYPE bigint;
                      -- typedpg-lint: allow(fk_type_mismatch)
                      ALTER TABLE posts ADD CONSTRAINT posts_author_fk
                          FOREIGN KEY (author_id) REFERENCES users (id);";
        let findings = lint(
            &[base, ("002_fk.sql", fk_sql, MigrationLint::All)],
            true,
            &LintOptions::default(),
        );
        assert!(
            only(&findings, Rule::FkTypeMismatch).is_empty(),
            "{findings:?}"
        );

        let findings = lint(
            &[
                base,
                (
                    "002_lock.sql",
                    "-- typedpg-lint: allow(missing_lock_timeout)
                     ALTER TABLE users ADD COLUMN nickname text;",
                    MigrationLint::All,
                ),
            ],
            true,
            &LintOptions::default(),
        );
        assert!(
            only(&findings, Rule::MissingLockTimeout).is_empty(),
            "{findings:?}"
        );

        let findings = lint(
            &[
                base,
                (
                    "002_opinions.sql",
                    "-- typedpg-lint: allow(prefer_timestamptz, prefer_identity, prefer_text, prefer_bigint_pk)
                     CREATE TABLE names (
                         id integer PRIMARY KEY,
                         at timestamp,
                         short varchar(10),
                         serial_id serial
                     );",
                    MigrationLint::All,
                ),
            ],
            true,
            &opinion_options(&[
                Rule::PreferTimestamptz,
                Rule::PreferIdentity,
                Rule::PreferText,
                Rule::PreferBigintPk,
            ]),
        );
        for rule in [
            Rule::PreferTimestamptz,
            Rule::PreferIdentity,
            Rule::PreferText,
            Rule::PreferBigintPk,
        ] {
            assert!(only(&findings, rule).is_empty(), "{rule}: {findings:?}");
        }
    }

    #[test]
    fn schema_only_files_report_the_new_schema_rules() {
        let findings = lint(
            &[
                ("001_base.sql", BASE, MigrationLint::All),
                (
                    "002_unchanged.sql",
                    "CREATE TABLE names (
                         id integer PRIMARY KEY,
                         at timestamp,
                         short varchar(10)
                     );",
                    MigrationLint::SchemaOnly,
                ),
            ],
            true,
            &opinion_options(&[
                Rule::PreferTimestamptz,
                Rule::PreferText,
                Rule::PreferBigintPk,
            ]),
        );
        assert!(only(&findings, Rule::MissingLockTimeout).is_empty());
        for rule in [
            Rule::PreferTimestamptz,
            Rule::PreferText,
            Rule::PreferBigintPk,
        ] {
            let found: Vec<_> = only(&findings, rule)
                .into_iter()
                .filter(|finding| finding.file == 1)
                .collect();
            assert_eq!(found.len(), 1, "{rule}: {findings:?}");
            assert!(found[0].finding.help.is_some(), "{rule}: {findings:?}");
        }
    }

    // ── table_rewrite: storage changes and filled columns ──────────────────

    #[test]
    fn table_rewrite_reports_persistence_changes() {
        let found = only(
            &second("ALTER TABLE users SET UNLOGGED;"),
            Rule::TableRewrite,
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            found[0].finding.message,
            "ALTER TABLE rewrites every row of public.users under an ACCESS EXCLUSIVE lock"
        );
        assert_eq!(
            found[0].finding.help.as_deref(),
            Some(
                "create a new table with the new setting, copy the rows in batches, then swap the tables"
            )
        );
        // Setting the persistence the table already has does nothing.
        assert!(only(&second("ALTER TABLE users SET LOGGED;"), Rule::TableRewrite).is_empty());
        let findings = lint(
            &[
                (
                    "001_base.sql",
                    "CREATE UNLOGGED TABLE scratch (id integer PRIMARY KEY);",
                    MigrationLint::All,
                ),
                (
                    "002_logged.sql",
                    "ALTER TABLE scratch SET LOGGED;",
                    MigrationLint::All,
                ),
                (
                    "003_unlogged.sql",
                    "ALTER TABLE scratch SET UNLOGGED;",
                    MigrationLint::All,
                ),
            ],
            true,
            &LintOptions::default(),
        );
        let found = only(&findings, Rule::TableRewrite);
        assert_eq!(found.len(), 2, "{findings:?}");
        assert_eq!((found[0].file, found[1].file), (1, 2), "{findings:?}");
        let findings = lint(
            &[
                (
                    "001_base.sql",
                    "CREATE UNLOGGED TABLE scratch (id integer PRIMARY KEY);",
                    MigrationLint::All,
                ),
                (
                    "002_unlogged.sql",
                    "ALTER TABLE scratch SET UNLOGGED;",
                    MigrationLint::All,
                ),
            ],
            true,
            &LintOptions::default(),
        );
        assert!(
            only(&findings, Rule::TableRewrite).is_empty(),
            "{findings:?}"
        );
    }

    #[test]
    fn table_rewrite_reports_a_new_access_method_and_tablespace() {
        // Tables are taken to use heap, the default access method.
        for sql in [
            "ALTER TABLE users SET ACCESS METHOD heap;",
            "ALTER TABLE users SET ACCESS METHOD DEFAULT;",
        ] {
            assert!(only(&second(sql), Rule::TableRewrite).is_empty(), "{sql}");
        }
        let findings = second(
            "CREATE ACCESS METHOD heap2 TYPE TABLE HANDLER heap_tableam_handler;
             ALTER TABLE users SET ACCESS METHOD heap2;",
        );
        assert_eq!(only(&findings, Rule::TableRewrite).len(), 1, "{findings:?}");

        let found = only(
            &second("ALTER TABLE users SET TABLESPACE fast;"),
            Rule::TableRewrite,
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            found[0].finding.message,
            "SET TABLESPACE copies every block of public.users under an ACCESS EXCLUSIVE lock"
        );
        // A partitioned table has no storage: these only set what its future
        // partitions get.
        let findings = lint(
            &[
                ("001_base.sql", PARTITIONED, MigrationLint::All),
                (
                    "002_move.sql",
                    "ALTER TABLE measurements SET TABLESPACE fast;",
                    MigrationLint::All,
                ),
            ],
            true,
            &LintOptions::default(),
        );
        assert!(
            only(&findings, Rule::TableRewrite).is_empty(),
            "{findings:?}"
        );
    }

    #[test]
    fn table_rewrite_reports_identity_and_serial_columns() {
        for sql in [
            "ALTER TABLE users ADD COLUMN seq integer GENERATED ALWAYS AS IDENTITY;",
            "ALTER TABLE users ADD COLUMN seq bigint GENERATED BY DEFAULT AS IDENTITY;",
            "ALTER TABLE users ADD COLUMN seq serial;",
            "ALTER TABLE users ADD COLUMN seq bigserial;",
            "ALTER TABLE users ADD COLUMN seq integer DEFAULT nextval('users_seq');",
        ] {
            let sql = if sql.contains("users_seq") {
                format!("CREATE SEQUENCE users_seq; {sql}")
            } else {
                sql.to_owned()
            };
            let findings = second(&sql);
            assert_eq!(
                only(&findings, Rule::TableRewrite).len(),
                1,
                "{sql}: {findings:?}"
            );
        }
        // A plain column, a constant default and a virtual generated column
        // are only catalog changes.
        for sql in [
            "ALTER TABLE users ADD COLUMN seq integer;",
            "ALTER TABLE users ADD COLUMN seq integer DEFAULT 7;",
            "ALTER TABLE users ADD COLUMN twice integer GENERATED ALWAYS AS (id * 2) VIRTUAL;",
        ] {
            assert!(only(&second(sql), Rule::TableRewrite).is_empty(), "{sql}");
        }
        // A table created in the same file has no rows to rewrite.
        let findings = second(
            "CREATE TABLE fresh (id integer PRIMARY KEY);
             ALTER TABLE fresh ADD COLUMN seq serial, SET UNLOGGED, SET TABLESPACE fast;",
        );
        assert!(
            only(&findings, Rule::TableRewrite).is_empty(),
            "{findings:?}"
        );
    }

    // ── drop_index_blocking ────────────────────────────────────────────────

    #[test]
    fn drop_index_blocking_reports_a_non_concurrent_drop() {
        let found = only(
            &second("DROP INDEX users_email_idx;"),
            Rule::DropIndexBlocking,
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            found[0].finding.message,
            "DROP INDEX without CONCURRENTLY takes an ACCESS EXCLUSIVE lock on public.users, \
             blocking every query on the table"
        );
        assert_eq!(
            found[0].finding.help.as_deref(),
            Some(
                "use DROP INDEX CONCURRENTLY, in a migration whose first line is -- no-transaction"
            )
        );
        let found = only(
            &second(
                "CREATE INDEX posts_author_idx ON posts (author_id);
                 DROP INDEX users_email_idx, posts_author_idx;",
            ),
            Rule::DropIndexBlocking,
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0]
                .finding
                .help
                .as_deref()
                .unwrap()
                .starts_with("use one DROP INDEX CONCURRENTLY per index"),
            "{found:?}"
        );
    }

    #[test]
    fn drop_index_blocking_ignores_concurrent_new_and_schema_only_drops() {
        let findings = second_with(
            "-- no-transaction\nDROP INDEX CONCURRENTLY users_email_idx;",
            true,
            &LintOptions::default(),
        );
        assert!(
            only(&findings, Rule::DropIndexBlocking).is_empty(),
            "{findings:?}"
        );
        let findings = second(
            "CREATE INDEX posts_author_idx ON posts (author_id);
             DROP INDEX posts_author_idx;",
        );
        assert!(
            only(&findings, Rule::DropIndexBlocking).is_empty(),
            "{findings:?}"
        );
        let findings = lint(
            &[
                ("001_base.sql", BASE, MigrationLint::All),
                (
                    "002_drop.sql",
                    "DROP INDEX users_email_idx;",
                    MigrationLint::SchemaOnly,
                ),
            ],
            true,
            &LintOptions::default(),
        );
        assert!(
            only(&findings, Rule::DropIndexBlocking).is_empty(),
            "{findings:?}"
        );
    }

    // ── blocking_rebuild ───────────────────────────────────────────────────

    #[test]
    fn blocking_rebuild_reports_rebuilds_of_existing_tables() {
        for (sql, message) in [
            (
                "REINDEX TABLE users;",
                "REINDEX without CONCURRENTLY blocks writes on public.users, and reads that use \
                 the indexes it rebuilds, for the whole rebuild",
            ),
            (
                "REINDEX INDEX users_email_idx;",
                "REINDEX without CONCURRENTLY blocks writes on public.users, and reads that use \
                 the indexes it rebuilds, for the whole rebuild",
            ),
            (
                "REINDEX SCHEMA public;",
                "REINDEX without CONCURRENTLY blocks writes on every table of schema public, and \
                 reads that use the indexes it rebuilds, for the whole rebuild",
            ),
            (
                "VACUUM FULL users;",
                "VACUUM FULL rewrites public.users under an ACCESS EXCLUSIVE lock, blocking every \
                 query on it for the whole rebuild",
            ),
            (
                "VACUUM (FULL) users;",
                "VACUUM FULL rewrites public.users under an ACCESS EXCLUSIVE lock, blocking every \
                 query on it for the whole rebuild",
            ),
            (
                "VACUUM FULL;",
                "VACUUM FULL rewrites every table under an ACCESS EXCLUSIVE lock, blocking every \
                 query on it for the whole rebuild",
            ),
            (
                "CLUSTER users USING users_email_idx;",
                "CLUSTER rewrites public.users under an ACCESS EXCLUSIVE lock, blocking every \
                 query on it for the whole rebuild",
            ),
            (
                "REPACK users;",
                "REPACK rewrites public.users under an ACCESS EXCLUSIVE lock, blocking every \
                 query on it for the whole rebuild",
            ),
        ] {
            let findings = second_with(sql, false, &LintOptions::default());
            let found = only(&findings, Rule::BlockingRebuild);
            assert_eq!(found.len(), 1, "{sql}: {findings:?}");
            assert_eq!(found[0].finding.message, message, "{sql}");
        }
    }

    #[test]
    fn blocking_rebuild_ignores_concurrent_and_new_rebuilds() {
        for sql in [
            "REINDEX (CONCURRENTLY) TABLE users;",
            "REINDEX TABLE CONCURRENTLY users;",
            "REPACK (CONCURRENTLY) users;",
            "VACUUM users;",
            "VACUUM (FULL false) users;",
            "CREATE TABLE fresh (id integer PRIMARY KEY); REINDEX TABLE fresh;",
            "CREATE TABLE fresh (id integer PRIMARY KEY); VACUUM FULL fresh;",
        ] {
            let findings = second_with(sql, false, &LintOptions::default());
            assert!(
                only(&findings, Rule::BlockingRebuild).is_empty(),
                "{sql}: {findings:?}"
            );
        }
        let findings = lint(
            &[
                ("001_base.sql", BASE, MigrationLint::All),
                (
                    "002_rebuild.sql",
                    "REINDEX TABLE users;",
                    MigrationLint::SchemaOnly,
                ),
            ],
            true,
            &LintOptions::default(),
        );
        assert!(
            only(&findings, Rule::BlockingRebuild).is_empty(),
            "{findings:?}"
        );
    }

    // ── detach_partition_blocking ──────────────────────────────────────────

    #[test]
    fn detach_partition_blocking_reports_a_non_concurrent_detach() {
        let findings = lint(
            &[
                ("001_base.sql", PARTITIONED, MigrationLint::All),
                (
                    "002_detach.sql",
                    "ALTER TABLE measurements DETACH PARTITION measurements_1;",
                    MigrationLint::All,
                ),
            ],
            true,
            &LintOptions::default(),
        );
        let found = only(&findings, Rule::DetachPartitionBlocking);
        assert_eq!(found.len(), 1, "{findings:?}");
        assert_eq!(
            found[0].finding.message,
            "DETACH PARTITION without CONCURRENTLY takes an ACCESS EXCLUSIVE lock on \
             public.measurements, blocking every query on the partitioned table"
        );
        assert_eq!(
            found[0].finding.help.as_deref(),
            Some(
                "use DETACH PARTITION … CONCURRENTLY, in a migration whose first line is -- no-transaction"
            )
        );
        // With a default partition, CONCURRENTLY is refused.
        let findings = lint(
            &[
                (
                    "001_base.sql",
                    &format!(
                        "{PARTITIONED} CREATE TABLE measurements_other PARTITION OF measurements DEFAULT;"
                    ),
                    MigrationLint::All,
                ),
                (
                    "002_detach.sql",
                    "ALTER TABLE measurements DETACH PARTITION measurements_1;",
                    MigrationLint::All,
                ),
            ],
            true,
            &LintOptions::default(),
        );
        let found = only(&findings, Rule::DetachPartitionBlocking);
        assert_eq!(found.len(), 1, "{findings:?}");
        assert!(
            found[0].finding.help.as_deref().unwrap().starts_with(
                "CONCURRENTLY is not allowed while public.measurements has a default partition"
            ),
            "{found:?}"
        );
    }

    #[test]
    fn detach_partition_blocking_ignores_concurrent_and_new_detaches() {
        let findings = lint(
            &[
                ("001_base.sql", PARTITIONED, MigrationLint::All),
                (
                    "002_detach.sql",
                    "ALTER TABLE measurements DETACH PARTITION measurements_1 CONCURRENTLY;",
                    MigrationLint::All,
                ),
            ],
            false,
            &LintOptions::default(),
        );
        assert!(
            only(&findings, Rule::DetachPartitionBlocking).is_empty(),
            "{findings:?}"
        );
        let findings = lint(
            &[(
                "001_all.sql",
                &format!("{PARTITIONED} ALTER TABLE measurements DETACH PARTITION measurements_1;"),
                MigrationLint::All,
            )],
            true,
            &LintOptions::default(),
        );
        assert!(
            only(&findings, Rule::DetachPartitionBlocking).is_empty(),
            "{findings:?}"
        );
    }

    // ── validate_in_same_transaction ───────────────────────────────────────

    #[test]
    fn validate_in_same_transaction_reports_a_validate_under_the_add_lock() {
        let text = "ALTER TABLE users ADD CONSTRAINT users_id_positive CHECK (id > 0) NOT VALID;
                    ALTER TABLE users VALIDATE CONSTRAINT users_id_positive;";
        let found = only(&second(text), Rule::ValidateInSameTransaction);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            &text[found[0].finding.span.clone()],
            "ALTER TABLE users VALIDATE CONSTRAINT users_id_positive"
        );
        assert_eq!(
            found[0].finding.message,
            "VALIDATE CONSTRAINT users_id_positive runs in the transaction that added it NOT \
             VALID: ADD CONSTRAINT's lock on public.users is still held, so the scan blocks \
             writes on the table"
        );
        // A foreign key, both in one statement (ADD CONSTRAINT runs in an
        // earlier ALTER TABLE pass than VALIDATE), and an explicit
        // transaction in a file the runner does not wrap.
        for (sql, use_transaction) in [
            (
                "ALTER TABLE users ADD CONSTRAINT users_id_positive CHECK (id > 0) NOT VALID, \
                 VALIDATE CONSTRAINT users_id_positive;",
                false,
            ),
            (
                "ALTER TABLE posts ADD CONSTRAINT posts_author_fk FOREIGN KEY (author_id) \
                 REFERENCES users (id) NOT VALID;
                 ALTER TABLE posts VALIDATE CONSTRAINT posts_author_fk;",
                true,
            ),
            (
                "BEGIN;
                 ALTER TABLE users ADD CONSTRAINT users_id_positive CHECK (id > 0) NOT VALID;
                 ALTER TABLE users VALIDATE CONSTRAINT users_id_positive;
                 COMMIT;",
                false,
            ),
        ] {
            let findings = second_with(sql, use_transaction, &LintOptions::default());
            assert_eq!(
                only(&findings, Rule::ValidateInSameTransaction).len(),
                1,
                "{sql}: {findings:?}"
            );
        }
        // The transaction is tracked whether or not missing_lock_timeout
        // runs.
        let mut config = LintConfig::default();
        config
            .levels
            .insert("missing_lock_timeout".to_owned(), LintLevel::Off);
        let findings = second_with(text, true, &LintOptions::from_config(&config));
        assert_eq!(
            only(&findings, Rule::ValidateInSameTransaction).len(),
            1,
            "{findings:?}"
        );
    }

    #[test]
    fn validate_in_same_transaction_ignores_a_validate_after_the_commit() {
        let add = "ALTER TABLE users ADD CONSTRAINT users_id_positive CHECK (id > 0) NOT VALID;";
        let validate = "ALTER TABLE users VALIDATE CONSTRAINT users_id_positive;";
        // A later migration.
        let findings = lint(
            &[
                ("001_base.sql", BASE, MigrationLint::All),
                ("002_add.sql", add, MigrationLint::All),
                ("003_validate.sql", validate, MigrationLint::All),
            ],
            true,
            &LintOptions::default(),
        );
        assert!(
            only(&findings, Rule::ValidateInSameTransaction).is_empty(),
            "{findings:?}"
        );
        for (sql, use_transaction) in [
            // Each statement commits on its own.
            (format!("{add}\n{validate}"), false),
            (format!("-- no-transaction\n{add}\n{validate}"), true),
            (format!("BEGIN; {add} COMMIT;\n{validate}"), false),
            // A constraint the file validates while adding it was not added
            // NOT VALID.
            (
                "ALTER TABLE users ADD CONSTRAINT users_id_positive CHECK (id > 0);
                 ALTER TABLE users VALIDATE CONSTRAINT users_id_positive;"
                    .to_owned(),
                true,
            ),
            // A table created in the same file.
            (
                "CREATE TABLE fresh (id integer PRIMARY KEY);
                 ALTER TABLE fresh ADD CONSTRAINT fresh_id_positive CHECK (id > 0) NOT VALID;
                 ALTER TABLE fresh VALIDATE CONSTRAINT fresh_id_positive;"
                    .to_owned(),
                true,
            ),
        ] {
            let findings = second_with(&sql, use_transaction, &LintOptions::default());
            assert!(
                only(&findings, Rule::ValidateInSameTransaction).is_empty(),
                "{sql}: {findings:?}"
            );
        }
    }

    // ── attach_partition_scan ──────────────────────────────────────────────

    #[test]
    fn attach_partition_scan_reports_a_partition_without_a_check() {
        let attach = "ALTER TABLE measurements ATTACH PARTITION measurements_new \
                      FOR VALUES FROM (100) TO (200);";
        let attach_with = |before: &str| {
            lint(
                &[
                    ("001_base.sql", PARTITIONED, MigrationLint::All),
                    ("002_check.sql", before, MigrationLint::All),
                    ("003_attach.sql", attach, MigrationLint::All),
                ],
                true,
                &LintOptions::default(),
            )
        };
        let findings = attach_with("SELECT 1;");
        let found = only(&findings, Rule::AttachPartitionScan);
        assert_eq!(found.len(), 1, "{findings:?}");
        assert_eq!(found[0].file, 2);
        assert_eq!(
            found[0].finding.message,
            "ATTACH PARTITION scans every row of public.measurements_new under an ACCESS \
             EXCLUSIVE lock to check it fits the partition bound"
        );
        // A CHECK that proves nothing: not validated, not enforced, or not
        // on the key.
        for check in [
            "ALTER TABLE measurements_new ADD CONSTRAINT bound CHECK (id >= 100 AND id < 200) NOT VALID;",
            "ALTER TABLE measurements_new ADD CONSTRAINT bound CHECK (id >= 100 AND id < 200) NOT ENFORCED;",
            "ALTER TABLE measurements_new ADD CONSTRAINT recent CHECK (at > '2020-01-01');",
        ] {
            let findings = attach_with(check);
            assert_eq!(
                only(&findings, Rule::AttachPartitionScan).len(),
                1,
                "{check}: {findings:?}"
            );
        }
        assert_eq!(
            found[0].finding.help.as_deref(),
            Some(
                "first add a CHECK constraint on public.measurements_new that implies the bound, \
                 with NOT NULL on the key columns (NOT VALID, then VALIDATE CONSTRAINT in an \
                 earlier migration): PostgreSQL then skips the scan"
            )
        );
        // A CHECK that matches the bound but leaves the key nullable: the
        // partition constraint's `id IS NOT NULL` is not implied (a CHECK
        // passes on NULL), so PostgreSQL still scans.
        let findings = attach_with(
            "ALTER TABLE measurements_new ADD CONSTRAINT bound CHECK (id >= 100 AND id < 200);",
        );
        assert_eq!(
            only(&findings, Rule::AttachPartitionScan).len(),
            1,
            "{findings:?}"
        );
        // With the key NOT NULL (or IS NOT NULL in the check), it is
        // implied.
        for before in [
            "ALTER TABLE measurements_new ALTER COLUMN id SET NOT NULL;
             ALTER TABLE measurements_new ADD CONSTRAINT bound CHECK (id >= 100 AND id < 200);",
            "ALTER TABLE measurements_new ADD CONSTRAINT bound
                 CHECK (id IS NOT NULL AND id >= 100 AND id < 200);",
        ] {
            let findings = attach_with(before);
            assert!(
                only(&findings, Rule::AttachPartitionScan).is_empty(),
                "{before}: {findings:?}"
            );
        }
    }

    /// The `attach_partition_scan` findings of attaching `measurements_new`
    /// with `bound` to a table partitioned by `key`, after `before`.
    fn attach_scans(key: &str, columns: &str, before: &str, bound: &str) -> Vec<String> {
        let base = format!(
            "CREATE TABLE measurements ({columns}) PARTITION BY {key};
             CREATE TABLE measurements_new ({columns});"
        );
        let attach = format!("ALTER TABLE measurements ATTACH PARTITION measurements_new {bound};");
        let findings = lint(
            &[
                ("001_base.sql", &base, MigrationLint::All),
                ("002_before.sql", before, MigrationLint::All),
                ("003_attach.sql", &attach, MigrationLint::All),
            ],
            true,
            &LintOptions::default(),
        );
        only(&findings, Rule::AttachPartitionScan)
            .iter()
            .map(|found| found.finding.message.clone())
            .collect()
    }

    #[test]
    fn attach_partition_scan_proves_range_bounds_from_checks() {
        let columns = "id integer NOT NULL, at timestamp";
        let range = "FOR VALUES FROM (1000) TO (2000)";
        for (check, scans) in [
            // A CHECK on the key that doesn't imply the bound.
            ("CHECK (id > 0)", true),
            ("CHECK (id >= 1000)", true),
            ("CHECK (id >= 1000 AND id <= 2000)", true),
            ("CHECK (id >= 999 AND id < 2000)", true),
            ("CHECK (id >= 1000 OR id < 2000)", true),
            ("CHECK (id <> 5)", true),
            ("CHECK (id::bigint >= 1000 AND id::bigint < 2000)", true),
            ("CHECK (id >= 1000.5 AND id < 2000)", true),
            // Ones that do.
            ("CHECK (id >= 1000 AND id < 2000)", false),
            ("CHECK (id > 1000 AND id < 1500)", false),
            ("CHECK (id BETWEEN 1000 AND 1999)", false),
            ("CHECK (1000 <= id AND 2000 > id)", false),
            ("CHECK (id IN (1000, 1500, 1999))", false),
            ("CHECK (id = ANY (ARRAY[1000, 1001]))", false),
            ("CHECK (id >= '1000' AND id < '2000')", false),
        ] {
            let before = format!("ALTER TABLE measurements_new ADD CONSTRAINT bound {check};");
            assert_eq!(
                !attach_scans("RANGE (id)", columns, &before, range).is_empty(),
                scans,
                "{check}"
            );
        }
        // Two CHECKs together.
        assert!(
            attach_scans(
                "RANGE (id)",
                columns,
                "ALTER TABLE measurements_new ADD CONSTRAINT low CHECK (id >= 1000);
                 ALTER TABLE measurements_new ADD CONSTRAINT high CHECK (id < 2000);",
                range,
            )
            .is_empty()
        );
        // MINVALUE / MAXVALUE leave that side unchecked.
        assert!(
            attach_scans(
                "RANGE (id)",
                columns,
                "ALTER TABLE measurements_new ADD CONSTRAINT bound CHECK (id < 2000);",
                "FOR VALUES FROM (MINVALUE) TO (2000)",
            )
            .is_empty()
        );
        // A timestamp key, its bound and check written alike.
        assert!(
            attach_scans(
                "RANGE (at)",
                "id integer, at timestamp NOT NULL",
                "ALTER TABLE measurements_new ADD CONSTRAINT bound
                     CHECK (at >= '2024-01-01' AND at < '2024-02-01');",
                "FOR VALUES FROM ('2024-01-01') TO ('2024-02-01')",
            )
            .is_empty()
        );
        assert_eq!(
            attach_scans(
                "RANGE (at)",
                "id integer, at timestamp NOT NULL",
                "ALTER TABLE measurements_new ADD CONSTRAINT bound
                     CHECK (at >= '2024-01-01' AND at < '2024-03-01');",
                "FOR VALUES FROM ('2024-01-01') TO ('2024-02-01')",
            )
            .len(),
            1
        );
    }

    /// Each case's `pg` is what PostgreSQL 19.0 did (its DEBUG1 "partition
    /// constraint for table … is implied by existing constraints" message,
    /// or its absence), measured with the same tables; `lint` is what the
    /// rule reports, which differs only where it can't tell and reports.
    /// `CHECK (b = true)` for `IN (true, false)` and `CHECK (b = false)` for
    /// `FROM (false) TO (true)` were once silent although PostgreSQL scans
    /// (it folds the CHECK to `b` / `NOT b`).
    #[test]
    fn attach_partition_scan_agrees_with_postgresql() {
        let one_to_three = "FOR VALUES FROM (1) TO (3)";
        let january = "FOR VALUES FROM ('2024-01-01') TO ('2024-02-01')";
        for (columns, key, check, bound, pg, lint) in [
            (
                "n numeric NOT NULL",
                "RANGE (n)",
                "CHECK (n >= 1.0 AND n < 3.0)",
                one_to_three,
                false,
                false,
            ),
            (
                "n numeric NOT NULL",
                "RANGE (n)",
                "CHECK (n >= 1.5 AND n < 2.5)",
                one_to_three,
                false,
                false,
            ),
            (
                "n numeric NOT NULL",
                "RANGE (n)",
                "CHECK (n >= '1.5' AND n < '2.5')",
                one_to_three,
                false,
                false,
            ),
            (
                "n numeric NOT NULL",
                "RANGE (n)",
                "CHECK (n >= 1 AND n <= 3)",
                one_to_three,
                true,
                true,
            ),
            (
                "n numeric(5,2) NOT NULL",
                "RANGE (n)",
                "CHECK (n >= 1.001 AND n < 2.999)",
                one_to_three,
                false,
                false,
            ),
            (
                "n float8 NOT NULL",
                "RANGE (n)",
                "CHECK (n >= 1 AND n < 3)",
                one_to_three,
                false,
                false,
            ),
            (
                "n bigint NOT NULL",
                "RANGE (n)",
                "CHECK (n >= 1 AND n < 2)",
                one_to_three,
                false,
                false,
            ),
            (
                "n smallint NOT NULL",
                "RANGE (n)",
                "CHECK (n >= 1 AND n < 2)",
                one_to_three,
                false,
                false,
            ),
            (
                "n bigint NOT NULL",
                "RANGE (n)",
                "CHECK (n >= 3000000000 AND n < 3100000000)",
                "FOR VALUES FROM (3000000000) TO (3500000000)",
                false,
                false,
            ),
            (
                "n integer NOT NULL",
                "RANGE (n)",
                "CHECK (n >= 1::bigint AND n < 3)",
                one_to_three,
                false,
                false,
            ),
            (
                "n integer NOT NULL",
                "RANGE (n)",
                "CHECK (n >= 1.5 AND n < 3)",
                one_to_three,
                true,
                true,
            ),
            (
                "d date NOT NULL",
                "RANGE (d)",
                "CHECK (d >= '2024-01-02' AND d < '2024-01-20')",
                january,
                false,
                false,
            ),
            (
                "d date NOT NULL",
                "RANGE (d)",
                "CHECK (d >= DATE '2024-01-01' AND d < DATE '2024-02-01')",
                january,
                false,
                false,
            ),
            (
                "at timestamptz NOT NULL",
                "RANGE (at)",
                "CHECK (at >= '2024-01-02' AND at < '2024-01-20')",
                january,
                false,
                false,
            ),
            (
                "s text COLLATE \"C\" NOT NULL",
                "RANGE (s)",
                "CHECK (s >= 'b' AND s < 'c')",
                "FOR VALUES FROM ('a') TO ('m')",
                false,
                false,
            ),
            (
                "s text NOT NULL",
                "RANGE (s)",
                "CHECK (s >= 'a' AND s < 'm')",
                "FOR VALUES FROM ('a') TO ('m')",
                false,
                false,
            ),
            // The key's collation is not the column's: the CHECK compares
            // under another one.
            (
                "s text NOT NULL",
                "RANGE (s COLLATE \"C\")",
                "CHECK (s >= 'a' AND s < 'm')",
                "FOR VALUES FROM ('a') TO ('m')",
                true,
                true,
            ),
            (
                "code varchar(5) NOT NULL",
                "LIST (code)",
                "CHECK (code IN ('a', 'b'))",
                "FOR VALUES IN ('a', 'b', 'c')",
                false,
                false,
            ),
            (
                "u uuid NOT NULL",
                "LIST (u)",
                "CHECK (u = '00000000-0000-0000-0000-000000000001')",
                "FOR VALUES IN ('00000000-0000-0000-0000-000000000001')",
                false,
                false,
            ),
            (
                "b boolean NOT NULL",
                "LIST (b)",
                "CHECK (b = true)",
                "FOR VALUES IN (true)",
                false,
                false,
            ),
            // Strings order under the key's collation: known for C / POSIX
            // and the other collations ordering by code point; under the
            // database's default collation or an ICU / libc locale the rule
            // can't tell, and reports.
            (
                "s text NOT NULL",
                "RANGE (s)",
                "CHECK (s >= 'b' AND s < 'c')",
                "FOR VALUES FROM ('a') TO ('m')",
                false,
                true,
            ),
            (
                "s text COLLATE \"POSIX\" NOT NULL",
                "RANGE (s)",
                "CHECK (s >= 'b' AND s < 'c')",
                "FOR VALUES FROM ('a') TO ('m')",
                false,
                false,
            ),
            (
                "s text COLLATE \"POSIX\" NOT NULL",
                "RANGE (s)",
                "CHECK (s >= 'B' AND s < 'c')",
                "FOR VALUES FROM ('a') TO ('m')",
                true,
                true,
            ),
            (
                "s text COLLATE \"ucs_basic\" NOT NULL",
                "RANGE (s)",
                "CHECK (s >= 'b' AND s < 'c')",
                "FOR VALUES FROM ('a') TO ('m')",
                false,
                false,
            ),
            (
                "s text COLLATE \"en-x-icu\" NOT NULL",
                "RANGE (s)",
                "CHECK (s >= 'b' AND s < 'c')",
                "FOR VALUES FROM ('a') TO ('m')",
                false,
                true,
            ),
            // date against timestamp compare across types in datetime_ops
            // (a date is its midnight); the operators with timestamptz are
            // only stable, and predtest doesn't use them.
            (
                "d date NOT NULL",
                "RANGE (d)",
                "CHECK (d >= '2024-01-01'::timestamp AND d < '2024-02-01'::timestamp)",
                january,
                false,
                false,
            ),
            (
                "d date NOT NULL",
                "RANGE (d)",
                "CHECK (d >= '2024-01-01 00:00:01'::timestamp AND d < '2024-01-31 12:00'::timestamp)",
                january,
                false,
                false,
            ),
            (
                "d date NOT NULL",
                "RANGE (d)",
                "CHECK (d >= '2023-12-31 23:59:59'::timestamp AND d < '2024-02-01'::timestamp)",
                january,
                true,
                true,
            ),
            (
                "d date NOT NULL",
                "RANGE (d)",
                "CHECK (d >= '2024-01-01'::timestamp AND d <= '2024-02-01'::timestamp)",
                january,
                true,
                true,
            ),
            (
                "d date NOT NULL",
                "RANGE (d)",
                "CHECK (d >= TIMESTAMP '-infinity' AND d < TIMESTAMP '2024-02-01')",
                "FOR VALUES FROM ('-infinity') TO ('2024-02-01')",
                false,
                false,
            ),
            (
                "d date NOT NULL",
                "RANGE (d)",
                "CHECK (d >= '2024-01-01'::timestamptz AND d < '2024-02-01'::timestamptz)",
                january,
                true,
                true,
            ),
            (
                "d date NOT NULL",
                "LIST (d)",
                "CHECK (d IN ('2024-01-01'::timestamp, '2024-01-02'::timestamp))",
                "FOR VALUES IN ('2024-01-01', '2024-01-02', '2024-01-03')",
                false,
                false,
            ),
            (
                "at timestamp NOT NULL",
                "RANGE (at)",
                "CHECK (at >= DATE '2024-01-02' AND at < DATE '2024-01-20')",
                january,
                false,
                false,
            ),
            (
                "at timestamp(0) NOT NULL",
                "RANGE (at)",
                "CHECK (at >= '2024-01-01'::date AND at < '2024-02-01'::date)",
                january,
                false,
                false,
            ),
            (
                "at timestamp NOT NULL",
                "RANGE (at)",
                "CHECK (at >= DATE '2023-12-31' AND at < DATE '2024-01-20')",
                january,
                true,
                true,
            ),
            (
                "at timestamp NOT NULL",
                "RANGE (at)",
                "CHECK (at >= '2024-01-01'::timestamptz AND at < '2024-02-01'::timestamptz)",
                january,
                true,
                true,
            ),
            (
                "at timestamptz NOT NULL",
                "RANGE (at)",
                "CHECK (at >= '2024-01-01'::date AND at < '2024-02-01'::date)",
                january,
                true,
                true,
            ),
            (
                "at timestamptz NOT NULL",
                "RANGE (at)",
                "CHECK (at >= '2024-01-01'::timestamp AND at < '2024-02-01'::timestamp)",
                january,
                true,
                true,
            ),
            // An integer constant for a numeric key is folded to a numeric
            // one; an integer key against a numeric constant is cast, and
            // proves nothing. A float key's int / float4 / numeric constant
            // is not modeled.
            (
                "n numeric NOT NULL",
                "RANGE (n)",
                "CHECK (n >= 1::integer AND n < 3::bigint)",
                one_to_three,
                false,
                false,
            ),
            (
                "n numeric NOT NULL",
                "RANGE (n)",
                "CHECK (n >= '1'::smallint AND n < 2::int)",
                one_to_three,
                false,
                false,
            ),
            (
                "n numeric NOT NULL",
                "RANGE (n)",
                "CHECK (n >= 3000000000::bigint AND n < '3100000000'::int8)",
                "FOR VALUES FROM (3000000000) TO (3500000000)",
                false,
                false,
            ),
            (
                "n numeric NOT NULL",
                "RANGE (n)",
                "CHECK (n >= 1::integer AND n <= 3::integer)",
                one_to_three,
                true,
                true,
            ),
            (
                "n integer NOT NULL",
                "RANGE (n)",
                "CHECK (n >= 1::numeric AND n < 3::numeric)",
                one_to_three,
                true,
                true,
            ),
            (
                "n float8 NOT NULL",
                "RANGE (n)",
                "CHECK (n >= 1::integer AND n < 3::integer)",
                one_to_three,
                false,
                true,
            ),
            // A boolean key's `b = true` / `b <> true` is folded to `b` /
            // `NOT b`, on both sides — a one-value list too — but neither an
            // `= ANY (ARRAY[…])` element nor a range's `>=` / `<` is: `b`
            // implies only `b`, and an unfolded `b = true` implies `b`.
            (
                "b boolean NOT NULL",
                "LIST (b)",
                "CHECK (b)",
                "FOR VALUES IN (true)",
                false,
                false,
            ),
            (
                "b boolean NOT NULL",
                "LIST (b)",
                "CHECK (NOT b)",
                "FOR VALUES IN (false)",
                false,
                false,
            ),
            (
                "b boolean NOT NULL",
                "LIST (b)",
                "CHECK (b = false)",
                "FOR VALUES IN (false)",
                false,
                false,
            ),
            (
                "b boolean NOT NULL",
                "LIST (b)",
                "CHECK (b <> true)",
                "FOR VALUES IN (false)",
                false,
                false,
            ),
            (
                "b boolean NOT NULL",
                "LIST (b)",
                "CHECK (true = b)",
                "FOR VALUES IN (true)",
                false,
                false,
            ),
            (
                "b boolean NOT NULL",
                "LIST (b)",
                "CHECK (NOT NOT b)",
                "FOR VALUES IN (true)",
                false,
                false,
            ),
            (
                "b boolean NOT NULL",
                "LIST (b)",
                "CHECK (NOT (b = false))",
                "FOR VALUES IN (true)",
                false,
                false,
            ),
            (
                "b boolean NOT NULL",
                "LIST (b)",
                "CHECK (b IN (true))",
                "FOR VALUES IN (true)",
                false,
                false,
            ),
            (
                "b boolean NOT NULL",
                "LIST (b)",
                "CHECK (b = ANY (ARRAY[true]))",
                "FOR VALUES IN (true)",
                false,
                false,
            ),
            (
                "b boolean NOT NULL",
                "LIST (b)",
                "CHECK (b IN (true, false))",
                "FOR VALUES IN (true, false)",
                false,
                false,
            ),
            (
                "b boolean",
                "LIST (b)",
                "CHECK (b)",
                "FOR VALUES IN (true, NULL)",
                false,
                false,
            ),
            (
                "b boolean NOT NULL",
                "LIST (b)",
                "CHECK (NOT b)",
                "FOR VALUES IN (true)",
                true,
                true,
            ),
            (
                "b boolean NOT NULL",
                "LIST (b)",
                "CHECK (b)",
                "FOR VALUES IN (true, false)",
                true,
                true,
            ),
            (
                "b boolean NOT NULL",
                "LIST (b)",
                "CHECK (b = true)",
                "FOR VALUES IN (true, false)",
                true,
                true,
            ),
            (
                "b boolean NOT NULL",
                "LIST (b)",
                "CHECK (b = ANY (ARRAY[true, false]))",
                "FOR VALUES IN (true)",
                true,
                true,
            ),
            (
                "b boolean NOT NULL",
                "LIST (b)",
                "CHECK (b IS TRUE)",
                "FOR VALUES IN (true)",
                true,
                true,
            ),
            (
                "b boolean NOT NULL",
                "RANGE (b)",
                "CHECK (b = false)",
                "FOR VALUES FROM (false) TO (true)",
                true,
                true,
            ),
            (
                "b boolean NOT NULL",
                "RANGE (b)",
                "CHECK (b >= false AND b < true)",
                "FOR VALUES FROM (false) TO (true)",
                false,
                false,
            ),
            (
                "b boolean NOT NULL, n integer NOT NULL",
                "RANGE (b, n)",
                "CHECK (b AND n >= 1 AND n < 5)",
                "FOR VALUES FROM (true, 1) TO (true, 5)",
                false,
                false,
            ),
            (
                "b boolean NOT NULL, n integer NOT NULL",
                "RANGE (b, n)",
                "CHECK (b = true AND n >= 1 AND n < 5)",
                "FOR VALUES FROM (true, 1) TO (true, 5)",
                false,
                false,
            ),
        ] {
            let before = format!("ALTER TABLE measurements_new ADD CONSTRAINT bound {check};");
            let reported = !attach_scans(key, columns, &before, bound).is_empty();
            assert_eq!(
                reported, lint,
                "{columns} / {key} / {check} / {bound} (pg: {pg})"
            );
            // A report where PostgreSQL scans is never missing.
            assert!(!pg || reported, "{check}");
        }
        // A domain over boolean or date compares as its base type (both
        // sides read the column through the same relabeling).
        for (columns, key, check, bound, pg) in [
            (
                "b dbool NOT NULL",
                "LIST (b)",
                "CHECK (b = true)",
                "FOR VALUES IN (true)",
                false,
            ),
            (
                "b dbool NOT NULL",
                "LIST (b)",
                "CHECK (b = true)",
                "FOR VALUES IN (true, false)",
                true,
            ),
            (
                "d ddate NOT NULL",
                "RANGE (d)",
                "CHECK (d >= '2024-01-01'::timestamp AND d < '2024-02-01'::timestamp)",
                january,
                false,
            ),
        ] {
            let findings = lint(
                &[
                    (
                        "001_base.sql",
                        &format!(
                            "CREATE DOMAIN dbool AS boolean;
                             CREATE DOMAIN ddate AS date;
                             CREATE TABLE measurements ({columns}) PARTITION BY {key};
                             CREATE TABLE measurements_new ({columns});
                             ALTER TABLE measurements_new ADD CONSTRAINT bound {check};"
                        ),
                        MigrationLint::All,
                    ),
                    (
                        "002_attach.sql",
                        &format!(
                            "ALTER TABLE measurements ATTACH PARTITION measurements_new {bound};"
                        ),
                        MigrationLint::All,
                    ),
                ],
                true,
                &LintOptions::default(),
            );
            assert_eq!(
                !only(&findings, Rule::AttachPartitionScan).is_empty(),
                pg,
                "{columns} / {check} / {bound}: {findings:?}"
            );
        }
        // A NOT NULL constraint that is NOT VALID proves nothing.
        assert_eq!(
            attach_scans(
                "RANGE (id)",
                "id integer",
                "ALTER TABLE measurements_new ADD CONSTRAINT nn NOT NULL id NOT VALID;
                 ALTER TABLE measurements_new ADD CONSTRAINT bound CHECK (id >= 1000 AND id < 2000);",
                "FOR VALUES FROM (1000) TO (2000)",
            )
            .len(),
            1
        );
    }

    #[test]
    fn attach_partition_scan_proves_multi_column_ranges() {
        let columns = "a integer NOT NULL, b integer NOT NULL";
        // FROM (1, 0) TO (2, 0): (a > 1 OR (a = 1 AND b >= 0)) AND
        // (a < 2 OR (a = 2 AND b < 0)).
        for (check, scans) in [
            ("CHECK (a = 1 AND b >= 0)", false),
            ("CHECK (a = 1)", true),
            ("CHECK (a > 1 AND a < 2)", false),
            ("CHECK (a >= 1 AND a < 2)", true),
        ] {
            let before = format!("ALTER TABLE measurements_new ADD CONSTRAINT bound {check};");
            assert_eq!(
                !attach_scans(
                    "RANGE (a, b)",
                    columns,
                    &before,
                    "FOR VALUES FROM (1, 0) TO (2, 0)"
                )
                .is_empty(),
                scans,
                "{check}"
            );
        }
        // FROM (1, 10) TO (1, 20): a = 1 AND b >= 10 AND b < 20.
        assert!(
            attach_scans(
                "RANGE (a, b)",
                columns,
                "ALTER TABLE measurements_new ADD CONSTRAINT bound
                     CHECK (a = 1 AND b >= 10 AND b < 20);",
                "FOR VALUES FROM (1, 10) TO (1, 20)",
            )
            .is_empty()
        );
    }

    #[test]
    fn attach_partition_scan_proves_list_bounds_and_reports_hash() {
        let columns = "region text NOT NULL, id integer";
        for (check, scans) in [
            ("CHECK (region IN ('eu', 'us'))", true),
            ("CHECK (region = 'eu')", false),
            ("CHECK (region IN ('eu'))", false),
            ("CHECK (region <> 'us')", true),
        ] {
            let before = format!("ALTER TABLE measurements_new ADD CONSTRAINT bound {check};");
            assert_eq!(
                !attach_scans(
                    "LIST (region)",
                    columns,
                    &before,
                    "FOR VALUES IN ('eu', 'br')"
                )
                .is_empty(),
                scans,
                "{check}"
            );
        }
        // A list with NULL: `region IS NULL OR region = ANY (…)`.
        assert!(
            attach_scans(
                "LIST (id)",
                "id integer",
                "ALTER TABLE measurements_new ADD CONSTRAINT bound CHECK (id IN (1, 2));",
                "FOR VALUES IN (1, 2, NULL)",
            )
            .is_empty()
        );
        // No CHECK implies a hash bound.
        let found = attach_scans(
            "HASH (id)",
            "id integer NOT NULL",
            "ALTER TABLE measurements_new ADD CONSTRAINT bound CHECK (id >= 0);",
            "FOR VALUES WITH (MODULUS 2, REMAINDER 0)",
        );
        assert_eq!(found.len(), 1, "{found:?}");
    }

    #[test]
    fn attach_partition_scan_reports_the_default_partition_scan() {
        let findings = lint(
            &[
                (
                    "001_base.sql",
                    "CREATE TABLE measurements (id integer NOT NULL) PARTITION BY RANGE (id);
                     CREATE TABLE measurements_other PARTITION OF measurements DEFAULT;
                     CREATE TABLE measurements_new (id integer NOT NULL
                         CHECK (id >= 1000 AND id < 2000));",
                    MigrationLint::All,
                ),
                (
                    "002_attach.sql",
                    "ALTER TABLE measurements ATTACH PARTITION measurements_new \
                     FOR VALUES FROM (1000) TO (2000);",
                    MigrationLint::All,
                ),
            ],
            true,
            &LintOptions::default(),
        );
        let found = only(&findings, Rule::AttachPartitionScan);
        assert_eq!(found.len(), 1, "{findings:?}");
        assert_eq!(
            found[0].finding.message,
            "ATTACH PARTITION scans every row of the default partition \
             public.measurements_other under an ACCESS EXCLUSIVE lock, for rows that belong to \
             public.measurements_new"
        );
    }

    #[test]
    fn attach_partition_scan_follows_partitioned_tables_and_ancestors() {
        // A partitioned table to attach, whose partitions each carry the
        // check: PostgreSQL checks the partitions, and each is implied.
        let findings = lint(
            &[
                (
                    "001_base.sql",
                    "CREATE TABLE measurements (id integer NOT NULL, at date) PARTITION BY RANGE (id);
                     CREATE TABLE measurements_new (id integer NOT NULL, at date)
                         PARTITION BY RANGE (at);
                     CREATE TABLE measurements_new_1 PARTITION OF measurements_new
                         FOR VALUES FROM ('2024-01-01') TO ('2025-01-01');
                     ALTER TABLE measurements_new_1 ADD CONSTRAINT bound
                         CHECK (id >= 1000 AND id < 2000);",
                    MigrationLint::All,
                ),
                (
                    "002_attach.sql",
                    "ALTER TABLE measurements ATTACH PARTITION measurements_new \
                     FOR VALUES FROM (1000) TO (2000);",
                    MigrationLint::All,
                ),
            ],
            true,
            &LintOptions::default(),
        );
        assert!(
            only(&findings, Rule::AttachPartitionScan).is_empty(),
            "{findings:?}"
        );
        // A sub-partition must also satisfy its parent's own bound.
        let base = "CREATE TABLE m (id integer NOT NULL, kind text NOT NULL)
                        PARTITION BY RANGE (id);
                    CREATE TABLE m_1 PARTITION OF m FOR VALUES FROM (0) TO (100)
                        PARTITION BY LIST (kind);
                    CREATE TABLE m_1_new (id integer NOT NULL, kind text NOT NULL);";
        for (check, scans) in [
            ("CHECK (kind = 'a')", true),
            ("CHECK (kind = 'a' AND id >= 0 AND id < 100)", false),
        ] {
            let findings = lint(
                &[
                    (
                        "001_base.sql",
                        &format!("{base} ALTER TABLE m_1_new ADD CONSTRAINT bound {check};"),
                        MigrationLint::All,
                    ),
                    (
                        "002_attach.sql",
                        "ALTER TABLE m_1 ATTACH PARTITION m_1_new FOR VALUES IN ('a');",
                        MigrationLint::All,
                    ),
                ],
                true,
                &LintOptions::default(),
            );
            assert_eq!(
                !only(&findings, Rule::AttachPartitionScan).is_empty(),
                scans,
                "{check}: {findings:?}"
            );
        }
    }

    #[test]
    fn attach_partition_scan_ignores_a_new_table() {
        let findings = lint(
            &[
                ("001_base.sql", PARTITIONED, MigrationLint::All),
                (
                    "002_attach.sql",
                    "CREATE TABLE measurements_2 (id integer, at timestamp);
                     ALTER TABLE measurements ATTACH PARTITION measurements_2 \
                     FOR VALUES FROM (200) TO (300);",
                    MigrationLint::All,
                ),
            ],
            true,
            &LintOptions::default(),
        );
        assert!(
            only(&findings, Rule::AttachPartitionScan).is_empty(),
            "{findings:?}"
        );
    }

    // ── missing_primary_key ────────────────────────────────────────────────

    #[test]
    fn missing_primary_key_reports_tables_without_one() {
        let text = "CREATE TABLE events (id integer, at timestamptz);
                    CREATE TABLE later (id integer);
                    ALTER TABLE later ADD PRIMARY KEY (id);
                    CREATE TEMP TABLE scratch (id integer);
                    CREATE TABLE logs (id integer) PARTITION BY RANGE (id);
                    CREATE TABLE logs_1 PARTITION OF logs FOR VALUES FROM (0) TO (10);
                    CREATE TABLE keyed (id integer PRIMARY KEY);
                    CREATE TABLE keyed_child (extra integer) INHERITS (keyed);";
        let findings = lint(
            &[("001_all.sql", text, MigrationLint::All)],
            true,
            &LintOptions::default(),
        );
        let found = only(&findings, Rule::MissingPrimaryKey);
        let spans: Vec<&str> = found
            .iter()
            .map(|f| &text[f.finding.span.clone()])
            .collect();
        assert_eq!(
            spans,
            [
                "CREATE TABLE events (id integer, at timestamptz)",
                "CREATE TABLE logs (id integer) PARTITION BY RANGE (id)",
                "CREATE TABLE keyed_child (extra integer) INHERITS (keyed)",
            ],
            "{findings:?}"
        );
        assert_eq!(found[0].finding.message, "public.events has no primary key");
        assert_eq!(found[0].finding.severity, Severity::Warning);
    }

    #[test]
    fn missing_primary_key_points_schema_only_files_at_a_new_migration() {
        let findings = lint(
            &[
                ("001_base.sql", BASE, MigrationLint::All),
                (
                    "002_unchanged.sql",
                    "CREATE TABLE events (id integer);",
                    MigrationLint::SchemaOnly,
                ),
            ],
            true,
            &LintOptions::default(),
        );
        let found = only(&findings, Rule::MissingPrimaryKey);
        assert_eq!(found.len(), 1, "{findings:?}");
        assert_eq!(found[0].file, 1);
        assert_eq!(
            found[0].finding.help.as_deref(),
            Some(
                "in a new migration, CREATE UNIQUE INDEX CONCURRENTLY on the key, then ADD PRIMARY KEY USING INDEX"
            )
        );
        let findings = lint(
            &[(
                "001_all.sql",
                "-- typedpg-lint: allow(missing_primary_key)\nCREATE TABLE events (id integer);",
                MigrationLint::All,
            )],
            true,
            &LintOptions::default(),
        );
        assert!(
            only(&findings, Rule::MissingPrimaryKey).is_empty(),
            "{findings:?}"
        );
    }

    // ── rls_policy_not_enforced / rls_no_policy ────────────────────────────

    #[test]
    fn rls_policy_not_enforced_reports_policies_on_a_table_without_rls() {
        let text = "CREATE TABLE notes (id integer PRIMARY KEY, owner text);
                    CREATE POLICY notes_owner ON notes USING (owner = current_user);
                    CREATE POLICY notes_read ON notes FOR SELECT USING (true);";
        let findings = lint(
            &[("001_all.sql", text, MigrationLint::All)],
            true,
            &LintOptions::default(),
        );
        let found = only(&findings, Rule::RlsPolicyNotEnforced);
        assert_eq!(found.len(), 1, "{findings:?}");
        assert_eq!(found[0].finding.severity, Severity::Error);
        assert_eq!(
            &text[found[0].finding.span.clone()],
            "CREATE POLICY notes_owner ON notes USING (owner = current_user)"
        );
        assert_eq!(
            found[0].finding.message,
            "policies notes_owner, notes_read on public.notes do nothing: row-level security is \
             not enabled on the table"
        );
        assert_eq!(
            found[0].finding.help.as_deref(),
            Some("ALTER TABLE public.notes ENABLE ROW LEVEL SECURITY")
        );
        let findings = lint(
            &[(
                "001_all.sql",
                &format!("{text} ALTER TABLE notes ENABLE ROW LEVEL SECURITY;"),
                MigrationLint::All,
            )],
            true,
            &LintOptions::default(),
        );
        assert!(
            only(&findings, Rule::RlsPolicyNotEnforced).is_empty(),
            "{findings:?}"
        );
        assert!(
            only(&findings, Rule::RlsNoPolicy).is_empty(),
            "{findings:?}"
        );
    }

    #[test]
    fn rls_no_policy_reports_rls_without_a_policy() {
        let text = "CREATE TABLE notes (id integer PRIMARY KEY);
                    ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
                    CREATE TABLE secrets (id integer PRIMARY KEY);
                    ALTER TABLE secrets ENABLE ROW LEVEL SECURITY, FORCE ROW LEVEL SECURITY;";
        let findings = lint(
            &[("001_all.sql", text, MigrationLint::All)],
            true,
            &LintOptions::default(),
        );
        let found = only(&findings, Rule::RlsNoPolicy);
        assert_eq!(found.len(), 2, "{findings:?}");
        assert_eq!(found[0].finding.severity, Severity::Info);
        assert_eq!(
            &text[found[0].finding.span.clone()],
            "ALTER TABLE notes ENABLE ROW LEVEL SECURITY"
        );
        assert_eq!(
            found[0].finding.message,
            "row-level security is enabled on public.notes, which has no policy: every role but \
             its owner, superusers and BYPASSRLS roles sees no rows and can write none"
        );
        assert!(
            found[1]
                .finding
                .message
                .contains("every role but superusers and BYPASSRLS roles"),
            "{found:?}"
        );
        // Disabled again: nothing to report.
        let findings = lint(
            &[(
                "001_all.sql",
                "CREATE TABLE notes (id integer PRIMARY KEY);
                 ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
                 ALTER TABLE notes DISABLE ROW LEVEL SECURITY;",
                MigrationLint::All,
            )],
            true,
            &LintOptions::default(),
        );
        assert!(
            only(&findings, Rule::RlsNoPolicy).is_empty(),
            "{findings:?}"
        );
    }

    // ── security_definer_search_path ───────────────────────────────────────

    #[test]
    fn security_definer_search_path_reports_definers_without_a_search_path() {
        let text = "CREATE FUNCTION count_users() RETURNS bigint SECURITY DEFINER
                        LANGUAGE sql AS $$ SELECT count(*) FROM users $$;
                    CREATE FUNCTION pinned() RETURNS bigint SECURITY DEFINER
                        SET search_path = pg_catalog, pg_temp
                        LANGUAGE sql AS $$ SELECT count(*) FROM public.users $$;
                    CREATE FUNCTION invoker() RETURNS bigint
                        LANGUAGE sql AS $$ SELECT count(*) FROM users $$;
                    CREATE FUNCTION atomic() RETURNS bigint SECURITY DEFINER
                        LANGUAGE sql BEGIN ATOMIC SELECT count(*) FROM users; END;
                    CREATE PROCEDURE touch() SECURITY DEFINER
                        LANGUAGE plpgsql AS $$ BEGIN UPDATE users SET email = email; END $$;
                    CREATE FUNCTION altered() RETURNS bigint SECURITY DEFINER
                        LANGUAGE sql AS $$ SELECT count(*) FROM users $$;
                    ALTER FUNCTION altered() SET search_path = pg_catalog, pg_temp;";
        let findings = lint(
            &[
                ("001_base.sql", BASE, MigrationLint::All),
                ("002_functions.sql", text, MigrationLint::All),
            ],
            true,
            &LintOptions::default(),
        );
        let found = only(&findings, Rule::SecurityDefinerSearchPath);
        let messages: Vec<&str> = found.iter().map(|f| f.finding.message.as_str()).collect();
        assert_eq!(
            messages,
            [
                "SECURITY DEFINER function public.count_users has no SET search_path: it runs \
                 with its owner's privileges but resolves names along the caller's search_path, \
                 where the caller can shadow them",
                "SECURITY DEFINER procedure public.touch has no SET search_path: it runs with \
                 its owner's privileges but resolves names along the caller's search_path, where \
                 the caller can shadow them",
            ],
            "{findings:?}"
        );
        assert!(
            text[found[0].finding.span.clone()].starts_with("CREATE FUNCTION count_users()"),
            "{found:?}"
        );
    }

    #[test]
    fn security_definer_search_path_follows_create_or_replace() {
        let findings = lint(
            &[
                (
                    "001_function.sql",
                    "CREATE FUNCTION f() RETURNS integer LANGUAGE sql AS $$ SELECT 1 $$;",
                    MigrationLint::All,
                ),
                (
                    "002_definer.sql",
                    "CREATE OR REPLACE FUNCTION f() RETURNS integer SECURITY DEFINER
                         LANGUAGE sql AS $$ SELECT 1 $$;",
                    MigrationLint::All,
                ),
            ],
            true,
            &LintOptions::default(),
        );
        let found = only(&findings, Rule::SecurityDefinerSearchPath);
        assert_eq!(found.len(), 1, "{findings:?}");
        assert_eq!(found[0].file, 1, "{findings:?}");
    }

    // ── prefer_jsonb ───────────────────────────────────────────────────────

    #[test]
    fn prefer_jsonb_reports_json_columns_when_enabled() {
        let text =
            "CREATE TABLE docs (id integer PRIMARY KEY, body json, bodies json[], fine jsonb);";
        let findings = lint(
            &[("001_docs.sql", text, MigrationLint::All)],
            true,
            &LintOptions::default(),
        );
        assert!(
            only(&findings, Rule::PreferJsonb).is_empty(),
            "{findings:?}"
        );
        let findings = lint(
            &[("001_docs.sql", text, MigrationLint::All)],
            true,
            &opinion_options(&[Rule::PreferJsonb]),
        );
        let found = only(&findings, Rule::PreferJsonb);
        assert_eq!(found.len(), 2, "{findings:?}");
        assert_eq!(
            found[0].finding.message,
            "public.docs.body is json: it has no equality operator, so DISTINCT, GROUP BY, UNION \
             and = fail on it, and every read parses the text again"
        );
    }
}
