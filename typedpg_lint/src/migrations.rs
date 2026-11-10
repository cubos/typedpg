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
    PgClassOid, PgConstraintOid, PgIndex, PgTypeOid, RelKind, TypCategory,
};
use typedpg_core::{QualifiedName, quote_identifier};
use typedpg_pg_query::protobuf::{
    AlterTableType, BoolExprType, ConstrType, Node, NullTestType, ObjectType, SortByDir,
    SortByNulls, TransactionStmtKind, VariableSetKind, a_const::Val, node::Node as N,
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
    /// still validated. Files unchanged from `--ignore-migrations-from` and
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
                    token.token(),
                    typedpg_pg_query::protobuf::Token::SqlComment
                        | typedpg_pg_query::protobuf::Token::CComment
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
        relation: &typedpg_pg_query::protobuf::RangeVar,
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
        for rule in [
            Rule::CreateIndexBlocking,
            Rule::ConstraintValidating,
            Rule::SetNotNullScan,
            Rule::TableRewrite,
            Rule::MissingLockTimeout,
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
        index: &typedpg_pg_query::protobuf::IndexStmt,
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
        alter: &typedpg_pg_query::protobuf::AlterTableStmt,
    ) -> Vec<RawFinding> {
        let Some(relation) = alter.relation.as_ref() else {
            return Vec::new();
        };
        let Some((_, table)) = self.table(catalog, relation) else {
            return Vec::new();
        };
        let builds_index = |constraint: &typedpg_pg_query::protobuf::Constraint| {
            matches!(
                ConstrType::try_from(constraint.contype),
                Ok(ConstrType::ConstrPrimary
                    | ConstrType::ConstrUnique
                    | ConstrType::ConstrExclusion)
            ) && constraint.indexname.is_empty()
        };
        let mut found = false;
        for command in &alter.cmds {
            let Some(command) = alter_command(command) else {
                continue;
            };
            match (
                AlterTableType::try_from(command.subtype),
                command.def.as_deref().and_then(|def| def.node.as_ref()),
            ) {
                (Ok(AlterTableType::AtAddConstraint), Some(N::Constraint(constraint))) => {
                    found |= builds_index(constraint);
                }
                (Ok(AlterTableType::AtAddColumn), Some(N::ColumnDef(column))) => {
                    found |= column.constraints.iter().any(|node| {
                        matches!(node.node.as_ref(), Some(N::Constraint(c)) if builds_index(c))
                    });
                }
                _ => {}
            }
        }
        if !found {
            return Vec::new();
        }
        vec![RawFinding {
            span: 0..0,
            message: format!(
                "adding the constraint builds its index, blocking writes on {table} for the whole \
                 build"
            ),
            help: Some(
                "CREATE UNIQUE INDEX CONCURRENTLY first (in a migration whose first line is \
                 -- no-transaction), then ADD CONSTRAINT … USING INDEX"
                    .to_owned(),
            ),
            fix: Vec::new(),
        }]
    }

    fn constraint_validating(
        &self,
        catalog: &PgCatalog,
        alter: &typedpg_pg_query::protobuf::AlterTableStmt,
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
            let Some(command) = alter_command(command) else {
                continue;
            };
            match (
                AlterTableType::try_from(command.subtype),
                command.def.as_deref().and_then(|def| def.node.as_ref()),
            ) {
                (Ok(AlterTableType::AtAddColumn), Some(N::ColumnDef(column))) => {
                    skip_validation &= !column_has_raw_default(column);
                }
                (Ok(AlterTableType::AtAddConstraint), Some(N::Constraint(constraint))) => {
                    skip_validation &= !constraint_is_foreign_key(constraint);
                }
                _ => {}
            }
        }
        let mut found = false;
        for command in &alter.cmds {
            let Some(command) = alter_command(command) else {
                continue;
            };
            let Ok(subtype) = AlterTableType::try_from(command.subtype) else {
                continue;
            };
            match subtype {
                AlterTableType::AtAddConstraint => {
                    let Some(N::Constraint(constraint)) =
                        command.def.as_deref().and_then(|def| def.node.as_ref())
                    else {
                        continue;
                    };
                    if validates(constraint) {
                        found = true;
                    }
                }
                AlterTableType::AtAddColumn => {
                    let Some(N::ColumnDef(column)) =
                        command.def.as_deref().and_then(|def| def.node.as_ref())
                    else {
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
                        if let Some(N::Constraint(constraint)) = node.node.as_ref()
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
        alter: &typedpg_pg_query::protobuf::AlterTableStmt,
    ) -> Vec<RawFinding> {
        let Some(relation) = alter.relation.as_ref() else {
            return Vec::new();
        };
        let Some((relid, table)) = self.table(catalog, relation) else {
            return Vec::new();
        };
        let mut columns = Vec::new();
        for command in &alter.cmds {
            let Some(command) = alter_command(command) else {
                continue;
            };
            if AlterTableType::try_from(command.subtype) != Ok(AlterTableType::AtSetNotNull) {
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
        alter: &typedpg_pg_query::protobuf::AlterTableStmt,
    ) -> Vec<RawFinding> {
        let Some(relation) = alter.relation.as_ref() else {
            return Vec::new();
        };
        let Some((relid, table)) = self.table(catalog, relation) else {
            return Vec::new();
        };
        let mut found = false;
        for command in &alter.cmds {
            let Some(command) = alter_command(command) else {
                continue;
            };
            let Ok(subtype) = AlterTableType::try_from(command.subtype) else {
                continue;
            };
            match subtype {
                AlterTableType::AtAlterColumnType => {
                    let Some(N::ColumnDef(column)) =
                        command.def.as_deref().and_then(|def| def.node.as_ref())
                    else {
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
                            column.raw_default.as_deref(),
                        )
                        .unwrap_or(true)
                    {
                        found = true;
                    }
                }
                AlterTableType::AtAddColumn => {
                    let Some(N::ColumnDef(column)) =
                        command.def.as_deref().and_then(|def| def.node.as_ref())
                    else {
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
                    let mut volatile_default = false;
                    let mut generated_stored = column.generated == "s";
                    for constraint in &column.constraints {
                        let Some(N::Constraint(constraint)) = constraint.node.as_ref() else {
                            continue;
                        };
                        match ConstrType::try_from(constraint.contype) {
                            Ok(ConstrType::ConstrDefault) => {
                                volatile_default |= constraint
                                    .raw_expr
                                    .as_deref()
                                    .is_some_and(|default| volatile(catalog, default));
                            }
                            Ok(ConstrType::ConstrGenerated) => {
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
                _ => {}
            }
        }
        if found {
            vec![RawFinding {
                span: 0..0,
                message: format!(
                    "ALTER TABLE rewrites every row of {table} under an ACCESS EXCLUSIVE lock"
                ),
                help: Some(
                    "rewrite in steps: add a new column, backfill it in batches, then swap"
                        .to_owned(),
                ),
                fix: Vec::new(),
            }]
        } else {
            Vec::new()
        }
    }

    fn missing_lock_timeout(&mut self, catalog: &PgCatalog, node: &N) -> Vec<RawFinding> {
        // A `SET`/`RESET`/`set_config` of the setting changes it for the
        // statements that follow, whether or not it is this rule's finding,
        // and `BEGIN`/`COMMIT` decide how long a LOCAL setting lasts.
        self.track_lock_timeout(catalog, node);
        if self.lock_timeout_effective() || self.lock_timeout_reported {
            return Vec::new();
        }
        let Some((kind, relid)) = lock_target(catalog, &self.pre_existing, node) else {
            return Vec::new();
        };
        let Some(class) = catalog.class_row(relid) else {
            return Vec::new();
        };
        self.lock_timeout_reported = true;
        let table = qualified_name(catalog, class);
        let help = if self.file_runs_in_transaction() {
            "start the migration with SET LOCAL lock_timeout = '5s' and retry on timeout"
        } else {
            "start the migration with SET lock_timeout = '5s' and retry on timeout"
        };
        vec![RawFinding {
            span: 0..0,
            message: format!(
                "{kind} waits for a lock on {table} with no lock_timeout: queued behind a long \
                 transaction, it blocks every query on the table"
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
    fn track_transaction(&mut self, transaction: &typedpg_pg_query::protobuf::TransactionStmt) {
        let Ok(kind) = TransactionStmtKind::try_from(transaction.kind) else {
            // An unrecognized transaction control statement can end or
            // start a transaction in ways this tracker does not model:
            // forget every proof, so the next blocking statement reports.
            self.reset_lock_timeout_proof();
            return;
        };
        match kind {
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
            | TransactionStmtKind::TransStmtRollbackPrepared
            | TransactionStmtKind::Undefined => self.reset_lock_timeout_proof(),
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
            N::CreateTableAsStmt(stmt)
                if ObjectType::try_from(stmt.objtype) == Ok(ObjectType::ObjectTable) =>
            {
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
                    let Some(command) = alter_command(command) else {
                        continue;
                    };
                    let (Ok(subtype), Some(def)) = (
                        AlterTableType::try_from(command.subtype),
                        command.def.as_deref().and_then(|def| def.node.as_ref()),
                    ) else {
                        continue;
                    };
                    match subtype {
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
                let message = if kind == "varchar" {
                    format!("{table}.{column} is varchar({length})")
                } else {
                    format!("{table}.{column} is char({length}): char pads with spaces")
                };
                let help = format!(
                    "use text, with a CHECK (char_length({column}) <= {length}) if the limit matters"
                );
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

    fn redundant_index(&self, catalog: &PgCatalog) -> Vec<SchemaFinding> {
        let mut indexes: Vec<&PgIndex> = catalog.iter_indexes().collect();
        indexes.sort_by_key(|index| index.indexrelid);
        let mut out = Vec::new();
        for index in &indexes {
            let Some(creator) = self.created_indexes.get(&index.indexrelid) else {
                continue;
            };
            if let Some((other, exact)) = redundant_with(catalog, &self.orderings, &indexes, index)
            {
                // A constraint's index cannot be dropped with DROP INDEX,
                // and ON CONFLICT ON CONSTRAINT depends on it: never report
                // it as the redundant one. When it duplicates a standalone
                // index, that standalone index is the one to drop (and its
                // own turn below reports it).
                if catalog.index_backs_constraint(index.indexrelid) {
                    continue;
                }
                // Two standalone exact duplicates: only the one created
                // later is redundant with the one already there.
                if exact
                    && !catalog.index_backs_constraint(other.indexrelid)
                    && let Some(other_creator) = self.created_indexes.get(&other.indexrelid)
                    && (other_creator.file, other_creator.span.start)
                        > (creator.file, creator.span.start)
                {
                    continue;
                }
                let (Some(name), Some(other_name)) = (
                    catalog.class_row(index.indexrelid),
                    catalog.class_row(other.indexrelid),
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
        }
        out
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
    }
}

fn validates(constraint: &typedpg_pg_query::protobuf::Constraint) -> bool {
    matches!(
        ConstrType::try_from(constraint.contype),
        Ok(ConstrType::ConstrForeign | ConstrType::ConstrCheck)
    ) && !constraint.skip_validation
}

/// Whether a `ColumnDef` gives the column a DEFAULT: an explicit one, or the
/// `nextval()` the `serial` pseudo-types expand to.
fn column_has_default(column: &typedpg_pg_query::protobuf::ColumnDef) -> bool {
    column.raw_default.is_some()
        || column.constraints.iter().any(|node| {
            matches!(
                node.node.as_ref(),
                Some(N::Constraint(constraint))
                    if ConstrType::try_from(constraint.contype)
                        == Ok(ConstrType::ConstrDefault)
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
fn column_type_name(column: &typedpg_pg_query::protobuf::ColumnDef) -> Option<&str> {
    let type_name = column.type_name.as_ref()?;
    match type_name.names.last()?.node.as_ref()? {
        N::String(name) => Some(&name.sval),
        _ => None,
    }
}

/// Whether PostgreSQL's `transformColumnDefinition` leaves a `raw_default`
/// on the column: an explicit or serial DEFAULT, or a GENERATED expression
/// (stored or virtual, whose expression the parser stores as
/// `raw_default`). Identity columns do not get one. This is exactly the
/// condition `transformAlterTableStmt` checks to clear `skipValidation`.
fn column_has_raw_default(column: &typedpg_pg_query::protobuf::ColumnDef) -> bool {
    column_has_default(column)
        || !column.generated.is_empty()
        || column.constraints.iter().any(|node| {
            matches!(
                node.node.as_ref(),
                Some(N::Constraint(constraint))
                    if ConstrType::try_from(constraint.contype) == Ok(ConstrType::ConstrGenerated)
            )
        })
}

fn alter_command(node: &Node) -> Option<&typedpg_pg_query::protobuf::AlterTableCmd> {
    match node.node.as_ref()? {
        N::AlterTableCmd(command) => Some(command),
        _ => None,
    }
}

/// Resolve a `RangeVar` to the class the catalog holds now.
fn resolve_relation<'a>(
    catalog: &'a PgCatalog,
    relation: &typedpg_pg_query::protobuf::RangeVar,
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
    select: &typedpg_pg_query::protobuf::SelectStmt,
) -> Option<(Option<bool>, bool)> {
    use typedpg_pg_query::protobuf::SetOperation;

    if select.op() != SetOperation::SetopNone
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
    let Some(N::ResTarget(target)) = target.node.as_ref() else {
        return None;
    };
    let Some(N::FuncCall(call)) = target.val.as_deref().and_then(|value| value.node.as_ref())
    else {
        return None;
    };
    let names: Vec<&str> = call
        .funcname
        .iter()
        .filter_map(|node| match &node.node {
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
    if !plain_text_literal(setting)?.eq_ignore_ascii_case("lock_timeout") {
        return None;
    }
    // An unknown `is_local` decides where the assignment lands; drop the
    // proof rather than guess.
    let Some(local) = constant_bool(local) else {
        return Some((None, false));
    };
    Some((set_config_value(value), local))
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
fn plain_text_literal(node: &Node) -> Option<&str> {
    let node = match node.node.as_ref()? {
        N::TypeCast(cast) => {
            if !is_text_type(cast.type_name.as_ref()?) {
                return None;
            }
            cast.arg.as_deref()?
        }
        _ => node,
    };
    match node.node.as_ref()? {
        N::AConst(constant) => match constant.val.as_ref()? {
            Val::Sval(string) => Some(&string.sval),
            _ => None,
        },
        N::String(string) => Some(&string.sval),
        _ => None,
    }
}

/// Whether a cast's target type is `text`.
fn is_text_type(type_name: &typedpg_pg_query::protobuf::TypeName) -> bool {
    let names: Vec<&str> = type_name
        .names
        .iter()
        .filter_map(|node| match &node.node {
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
fn set_config_value(node: &Node) -> Option<bool> {
    let node = match node.node.as_ref()? {
        N::TypeCast(cast) => {
            if !is_text_type(cast.type_name.as_ref()?) {
                return None;
            }
            cast.arg.as_deref()?
        }
        _ => node,
    };
    match node.node.as_ref()? {
        N::AConst(constant) if constant.isnull => Some(false),
        N::AConst(constant) => match constant.val.as_ref()? {
            Val::Sval(string) => Some(duration_nonzero(&string.sval)),
            _ => None,
        },
        N::String(string) => Some(duration_nonzero(&string.sval)),
        _ => None,
    }
}

/// The constant `node` is, through casts.
fn constant_node(node: &Node) -> Option<&Node> {
    match node.node.as_ref()? {
        N::TypeCast(cast) => constant_node(cast.arg.as_deref()?),
        _ => Some(node),
    }
}

fn constant_bool(node: &Node) -> Option<bool> {
    match constant_node(node)?.node.as_ref()? {
        N::AConst(constant) => match constant.val.as_ref()? {
            Val::Boolval(boolean) => Some(boolean.boolval),
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
    set: &typedpg_pg_query::protobuf::VariableSetStmt,
    in_transaction: bool,
    effective: bool,
) -> Option<Option<bool>> {
    let kind = VariableSetKind::try_from(set.kind).ok()?;
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
        VariableSetKind::VarSetValue => Some(set.args.first().and_then(setting_value)),
        VariableSetKind::VarSetDefault | VariableSetKind::VarReset => Some(Some(false)),
        // `SET … FROM CURRENT` keeps the value the statement runs with.
        VariableSetKind::VarSetCurrent => Some(Some(effective)),
        _ => Some(None),
    }
}

/// The value of `SET … TO <value>`: `Some` when its text is known, `None`
/// when an expression computes it.
fn setting_value(node: &Node) -> Option<bool> {
    match node.node.as_ref()? {
        N::AConst(constant) if constant.isnull => None,
        N::AConst(constant) => match constant.val.as_ref()? {
            Val::Ival(value) => Some(value.ival != 0),
            Val::Fval(value) => Some(duration_nonzero(&value.fval)),
            Val::Sval(value) => Some(duration_nonzero(&value.sval)),
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

/// The first statement that takes a write-blocking lock on a relation that
/// existed when the file started, the statement's kind and the relation.
fn lock_target(
    catalog: &PgCatalog,
    pre_existing: &HashSet<PgClassOid>,
    node: &N,
) -> Option<(&'static str, PgClassOid)> {
    match node {
        N::AlterTableStmt(alter) => alter_table_lock_target(catalog, pre_existing, alter),
        N::CreateStmt(create) => {
            create_table_foreign_keys(create)
                .into_iter()
                .find_map(|relation| {
                    existing_relation(catalog, pre_existing, relation, "CREATE TABLE")
                })
        }
        N::IndexStmt(index) => {
            let relation = index.relation.as_ref()?;
            if index.concurrent {
                return None;
            }
            existing_relation(catalog, pre_existing, relation, "CREATE INDEX")
        }
        N::DropStmt(drop) => drop_lock_target(catalog, pre_existing, drop),
        N::TruncateStmt(truncate) => {
            first_relation(catalog, pre_existing, &truncate.relations, "TRUNCATE")
        }
        N::CreateTrigStmt(trigger) => existing_relation(
            catalog,
            pre_existing,
            trigger.relation.as_ref()?,
            "CREATE TRIGGER",
        ),
        N::CreatePolicyStmt(policy) => existing_relation(
            catalog,
            pre_existing,
            policy.table.as_ref()?,
            "CREATE POLICY",
        ),
        N::AlterPolicyStmt(policy) => existing_relation(
            catalog,
            pre_existing,
            policy.table.as_ref()?,
            "ALTER POLICY",
        ),
        N::RuleStmt(rule) => existing_relation(
            catalog,
            pre_existing,
            rule.relation.as_ref()?,
            "CREATE RULE",
        ),
        N::RenameStmt(rename) => {
            if !matches!(
                ObjectType::try_from(rename.rename_type),
                Ok(ObjectType::ObjectTable | ObjectType::ObjectColumn)
            ) {
                return None;
            }
            existing_relation(
                catalog,
                pre_existing,
                rename.relation.as_ref()?,
                "ALTER TABLE",
            )
        }
        N::LockStmt(lock) => {
            if lock_mode_blocks_writes(lock.mode) {
                first_relation(catalog, pre_existing, &lock.relations, "LOCK TABLE")
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
        }
        N::ClusterStmt(cluster) => {
            existing_relation(catalog, pre_existing, cluster.relation.as_ref()?, "CLUSTER")
        }
        N::VacuumStmt(vacuum) => {
            if !vacuum.is_vacuumcmd || !option_enabled(&vacuum.options, "full") {
                return None;
            }
            vacuum.rels.iter().find_map(|relation| {
                let N::VacuumRelation(relation) = relation.node.as_ref()? else {
                    return None;
                };
                existing_relation(
                    catalog,
                    pre_existing,
                    relation.relation.as_ref()?,
                    "VACUUM FULL",
                )
            })
        }
        N::ReindexStmt(reindex) => {
            if option_enabled(&reindex.params, "concurrently") {
                return None;
            }
            existing_relation(catalog, pre_existing, reindex.relation.as_ref()?, "REINDEX")
        }
        _ => None,
    }
}

fn existing_relation(
    catalog: &PgCatalog,
    pre_existing: &HashSet<PgClassOid>,
    relation: &typedpg_pg_query::protobuf::RangeVar,
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
    nodes: &[Node],
    kind: &'static str,
) -> Option<(&'static str, PgClassOid)> {
    nodes
        .iter()
        .filter_map(|node| match node.node.as_ref()? {
            N::RangeVar(relation) => Some(relation),
            _ => None,
        })
        .find_map(|relation| existing_relation(catalog, pre_existing, relation, kind))
}

fn alter_table_lock_target(
    catalog: &PgCatalog,
    pre_existing: &HashSet<PgClassOid>,
    alter: &typedpg_pg_query::protobuf::AlterTableStmt,
) -> Option<(&'static str, PgClassOid)> {
    let mut strong = false;
    let mut referenced = Vec::new();
    let mut attached = Vec::new();
    for node in &alter.cmds {
        let Some(command) = alter_command(node) else {
            continue;
        };
        strong |= !only_share_update_exclusive(command);
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
        if AlterTableType::try_from(command.subtype) == Ok(AlterTableType::AtAttachPartition)
            && let Some(N::PartitionCmd(partition)) =
                command.def.as_deref().and_then(|def| def.node.as_ref())
            && let Some(name) = partition.name.as_ref()
        {
            attached.push(name);
        }
    }
    if strong
        && let Some(relation) = alter.relation.as_ref()
        && let Some(target) = existing_relation(catalog, pre_existing, relation, "ALTER TABLE")
    {
        return Some(target);
    }
    attached
        .into_iter()
        .find_map(|relation| existing_relation(catalog, pre_existing, relation, "ALTER TABLE"))
        .or_else(|| {
            referenced.into_iter().find_map(|relation| {
                existing_relation(catalog, pre_existing, relation, "ALTER TABLE")
            })
        })
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

/// Whether an `ALTER TABLE` subcommand only takes `SHARE UPDATE EXCLUSIVE`
/// on the table (PostgreSQL's `AlterTableGetLockLevel`).
fn only_share_update_exclusive(command: &typedpg_pg_query::protobuf::AlterTableCmd) -> bool {
    match AlterTableType::try_from(command.subtype) {
        Ok(
            AlterTableType::AtValidateConstraint
            | AlterTableType::AtSetStatistics
            | AlterTableType::AtSetOptions
            | AlterTableType::AtResetOptions
            | AlterTableType::AtClusterOn
            | AlterTableType::AtDropCluster
            | AlterTableType::AtAttachPartition
            | AlterTableType::AtDetachPartitionFinalize,
        ) => true,
        Ok(AlterTableType::AtSetRelOptions | AlterTableType::AtResetRelOptions) => {
            rel_options_are_share_update_exclusive(command)
        }
        Ok(AlterTableType::AtDetachPartition) => matches!(
            command.def.as_deref().and_then(|def| def.node.as_ref()),
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
fn rel_options_are_share_update_exclusive(
    command: &typedpg_pg_query::protobuf::AlterTableCmd,
) -> bool {
    let Some(N::List(list)) = command.def.as_deref().and_then(|def| def.node.as_ref()) else {
        return false;
    };
    list.items.iter().all(|node| {
        !matches!(
            node.node.as_ref(),
            Some(N::DefElem(def)) if def.defname.eq_ignore_ascii_case("user_catalog_table")
        )
    })
}

fn added_foreign_key(
    command: &typedpg_pg_query::protobuf::AlterTableCmd,
) -> Option<&typedpg_pg_query::protobuf::Constraint> {
    match (
        AlterTableType::try_from(command.subtype),
        command.def.as_deref().and_then(|def| def.node.as_ref()),
    ) {
        (Ok(AlterTableType::AtAddConstraint), Some(N::Constraint(constraint)))
            if constraint_is_foreign_key(constraint) =>
        {
            Some(&**constraint)
        }
        (Ok(AlterTableType::AtAddColumn), Some(N::ColumnDef(column))) => column
            .constraints
            .iter()
            .filter_map(|constraint| match constraint.node.as_ref()? {
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
    create: &typedpg_pg_query::protobuf::CreateStmt,
) -> Vec<&typedpg_pg_query::protobuf::RangeVar> {
    let mut out = Vec::new();
    for node in create.table_elts.iter().chain(&create.constraints) {
        match node.node.as_ref() {
            Some(N::Constraint(constraint)) if constraint_is_foreign_key(constraint) => {
                out.extend(constraint.pktable.as_ref());
            }
            Some(N::ColumnDef(column)) => {
                for constraint in &column.constraints {
                    if let Some(N::Constraint(constraint)) = constraint.node.as_ref()
                        && constraint_is_foreign_key(constraint)
                    {
                        out.extend(constraint.pktable.as_ref());
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn constraint_is_foreign_key(constraint: &typedpg_pg_query::protobuf::Constraint) -> bool {
    ConstrType::try_from(constraint.contype) == Ok(ConstrType::ConstrForeign)
}

fn drop_lock_target(
    catalog: &PgCatalog,
    pre_existing: &HashSet<PgClassOid>,
    drop: &typedpg_pg_query::protobuf::DropStmt,
) -> Option<(&'static str, PgClassOid)> {
    let kind = match ObjectType::try_from(drop.remove_type) {
        Ok(ObjectType::ObjectTable | ObjectType::ObjectForeignTable) => "DROP TABLE",
        Ok(ObjectType::ObjectIndex) if !drop.concurrent => "DROP INDEX",
        _ => return None,
    };
    for object in &drop.objects {
        let Some(N::List(list)) = object.node.as_ref() else {
            continue;
        };
        let names: Vec<&str> = list.items.iter().filter_map(string_node).collect();
        let (schema, name) = match names.as_slice() {
            [name] => (None, *name),
            [schema, name] => (Some(*schema), *name),
            _ => continue,
        };
        let Some(class) = catalog.resolve_table(schema, name) else {
            continue;
        };
        if pre_existing.contains(&class.oid) {
            return Some((kind, class.oid));
        }
    }
    None
}

fn string_node(node: &Node) -> Option<&str> {
    match node.node.as_ref()? {
        N::String(string) => Some(string.sval.as_str()),
        _ => None,
    }
}

/// Whether `options` has `name` set to a true value (a bare option counts).
fn option_enabled(options: &[Node], name: &str) -> bool {
    options.iter().any(|option| match option.node.as_ref() {
        Some(N::DefElem(option)) if option.defname.eq_ignore_ascii_case(name) => {
            def_boolean(option).unwrap_or(true)
        }
        _ => false,
    })
}

fn def_boolean(option: &typedpg_pg_query::protobuf::DefElem) -> Option<bool> {
    match option.arg.as_deref().and_then(|arg| arg.node.as_ref()) {
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
    match node.node.as_ref() {
        Some(N::BoolExpr(expr))
            if BoolExprType::try_from(expr.boolop) == Ok(BoolExprType::AndExpr) =>
        {
            !expr.args.is_empty()
                && expr
                    .args
                    .iter()
                    .any(|arg| check_proves_not_null(arg, column))
        }
        Some(N::NullTest(test)) => {
            NullTestType::try_from(test.nulltesttype) == Ok(NullTestType::IsNotNull)
                && !test.argisrow
                && test
                    .arg
                    .as_deref()
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
    match node.node.as_ref() {
        Some(N::BoolExpr(expr))
            if BoolExprType::try_from(expr.boolop) == Ok(BoolExprType::AndExpr) =>
        {
            !expr.args.is_empty() && expr.args.iter().all(|arg| not_null_and(arg, columns))
        }
        Some(N::NullTest(test)) => {
            NullTestType::try_from(test.nulltesttype) == Ok(NullTestType::IsNotNull)
                && !test.argisrow
                && test
                    .arg
                    .as_deref()
                    .and_then(column_name)
                    .is_some_and(|name| columns.contains(&name))
        }
        _ => false,
    }
}

/// The name a `ColumnRef` names, when it is one (a qualified reference's last
/// field, as an index predicate may spell `t.col`).
fn column_name(node: &Node) -> Option<&str> {
    let N::ColumnRef(reference) = node.node.as_ref()? else {
        return None;
    };
    let (last, qualifiers) = reference.fields.split_last()?;
    if !qualifiers
        .iter()
        .all(|field| matches!(field.node.as_ref(), Some(N::String(_))))
    {
        return None;
    }
    match last.node.as_ref() {
        Some(N::String(name)) => Some(&name.sval),
        _ => None,
    }
}

/// Another index `index` is redundant with, and whether the two are exact
/// duplicates.
///
/// `index` is redundant when `other` leads with the same key columns, in the
/// same order and with the same operator classes, collations and sort
/// orders, and `index` adds nothing: no uniqueness `other` lacks, and no
/// `INCLUDE` columns (a covering index serves index-only scans `other` may
/// not), unless the two are identical.
fn redundant_with<'a>(
    catalog: &PgCatalog,
    orderings: &HashMap<PgClassOid, Vec<(bool, bool)>>,
    indexes: &[&'a PgIndex],
    index: &PgIndex,
) -> Option<(&'a PgIndex, bool)> {
    let key_count = usize::try_from(index.indnkeyatts).unwrap_or(0);
    if !keys_plain(index) || key_count == 0 || index.indkey.len() < key_count {
        return None;
    }
    let covering = index.indkey.len() > key_count;
    let method = catalog.index_access_method(index.indexrelid);
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
    let mut best: Option<(&PgIndex, bool)> = None;
    for other in indexes {
        if other.indexrelid == index.indexrelid
            || other.indrelid != index.indrelid
            || !keys_plain(other)
            || catalog.index_access_method(other.indexrelid) != method
            // An immediate unique index enforces its uniqueness at each
            // statement, a deferrable one only at commit: neither replaces
            // the other.
            || deferrable(index) != deferrable(other)
        {
            continue;
        }
        let other_keys = usize::try_from(other.indnkeyatts).unwrap_or(0);
        if other_keys < key_count || other.indkey.len() < key_count {
            continue;
        }
        if index.indkey[..key_count] != other.indkey[..key_count]
            || index.indclass.get(..key_count) != other.indclass.get(..key_count)
            || index.indcollation.get(..key_count) != other.indcollation.get(..key_count)
            || order(index, key_count) != order(other, key_count)
        {
            continue;
        }
        let index_nulls = nulls_not_distinct(index);
        let other_nulls = nulls_not_distinct(other);
        let exact = other_keys == key_count
            && other.indkey == index.indkey
            && other.indisunique == index.indisunique
            && other_nulls == index_nulls;
        if exact {
            return Some((other, true));
        }
        if index.indisunique || covering {
            // A unique NULLS DISTINCT index adds nothing over a unique NULLS
            // NOT DISTINCT index on the same columns: the latter enforces
            // strictly more (it also rejects duplicate NULLs) and serves the
            // same equality lookups. The reverse is not true.
            if !covering
                && index.indisunique
                && other.indisunique
                && other_keys == key_count
                && other.indkey == index.indkey
                && !index_nulls
                && other_nulls
            {
                return Some((other, false));
            }
            continue;
        }
        if best.is_none() {
            best = Some((other, false));
        }
    }
    best
}

/// `stmt`'s key columns' order: `(descending, nulls first)` each, nulls
/// first by default exactly when descending, as PostgreSQL's.
fn key_order(stmt: &typedpg_pg_query::protobuf::IndexStmt) -> Vec<(bool, bool)> {
    stmt.index_params
        .iter()
        .map(|param| match param.node.as_ref() {
            Some(N::IndexElem(elem)) => {
                let descending = SortByDir::try_from(elem.ordering) == Ok(SortByDir::SortbyDesc);
                let nulls_first = match SortByNulls::try_from(elem.nulls_ordering) {
                    Ok(SortByNulls::SortbyNullsFirst) => true,
                    Ok(SortByNulls::SortbyNullsLast) => false,
                    _ => descending,
                };
                (descending, nulls_first)
            }
            _ => (false, false),
        })
        .collect()
}

fn volatile(catalog: &PgCatalog, node: &Node) -> bool {
    let Some(inner) = node.node.as_ref() else {
        return false;
    };
    inner
        .nodes()
        .iter()
        .any(|(node, _)| matches!(node, typedpg_pg_query::NodeRef::FuncCall(call) if catalog.call_is_volatile(call)))
}

fn comments(sql: &str) -> Vec<Comment> {
    let Ok(scanned) = typedpg_pg_query::scan(sql) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for token in &scanned.tokens {
        if !matches!(
            token.token(),
            typedpg_pg_query::protobuf::Token::SqlComment
                | typedpg_pg_query::protobuf::Token::CComment
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
        let node = parsed.protobuf.stmts[0]
            .stmt
            .as_ref()
            .and_then(|stmt| stmt.node.as_ref())
            .unwrap();
        let N::AlterTableStmt(alter) = node else {
            panic!("expected ALTER TABLE");
        };
        let command = alter.cmds.first().and_then(alter_command).unwrap();
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
        assert_eq!(found[0].finding.message, "public.texts.a is varchar(10)");
        assert_eq!(
            found[1].finding.message,
            "public.texts.d is char(2): char pads with spaces"
        );
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
}
