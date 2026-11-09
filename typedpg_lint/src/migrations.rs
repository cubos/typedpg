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
    SortByNulls, VariableSetKind, a_const::Val, node::Node as N,
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
    /// Whether an earlier statement of the current file set `lock_timeout`
    /// to a non-zero value.
    lock_timeout_set: bool,
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
        let Some((_, table)) = self.table(catalog, relation) else {
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
                    for node in &column.constraints {
                        if let Some(N::Constraint(constraint)) = node.node.as_ref()
                            && validates(constraint)
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
                    // USING always computes a new value, even when the cast
                    // alone would be a relabeling.
                    if column.raw_default.is_some() {
                        found = true;
                        continue;
                    }
                    let Some(type_name) = column.type_name.as_ref() else {
                        found = true;
                        continue;
                    };
                    if catalog
                        .column_type_rewrites(relid, &command.name, type_name, None)
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
        // A `SET`/`RESET` of the setting changes it for the statements that
        // follow, whether or not it is this rule's finding.
        if let N::VariableSetStmt(set) = node
            && let Some(nonzero) = lock_timeout_setting(set)
        {
            self.lock_timeout_set = nonzero;
        }
        if self.lock_timeout_set || self.lock_timeout_reported {
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
                    self.record_columns(catalog, class.oid);
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
                    self.record_columns(catalog, class.oid);
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
                .filter(|index| {
                    index.indrelid == constraint.conrelid
                        && index.indexprs.is_empty()
                        && index.indkey.iter().all(|&key| key != 0)
                })
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
                    if index.indkey.len() < key_count {
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
                // Two exact duplicates: only the one created later is
                // redundant with the one already there.
                if exact
                    && let Some(other_creator) = self.created_indexes.get(&other.indexrelid)
                    && (other_creator.file, other_creator.span.start)
                        > (creator.file, creator.span.start)
                {
                    continue;
                }
                if catalog.index_backs_constraint(index.indexrelid) && !exact {
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

/// The effective value of a `SET`/`RESET` of `lock_timeout`, when the
/// statement sets it: `Some(true)` for a non-zero value, `Some(false)` for
/// zero, `DEFAULT` or `RESET`, and `None` for another setting.
fn lock_timeout_setting(set: &typedpg_pg_query::protobuf::VariableSetStmt) -> Option<bool> {
    if !set.name.eq_ignore_ascii_case("lock_timeout") {
        return None;
    }
    match VariableSetKind::try_from(set.kind).ok()? {
        VariableSetKind::VarSetValue => Some(set.args.first().is_some_and(lock_timeout_nonzero)),
        VariableSetKind::VarSetDefault
        | VariableSetKind::VarSetCurrent
        | VariableSetKind::VarReset => Some(false),
        _ => None,
    }
}

fn lock_timeout_nonzero(node: &Node) -> bool {
    match node.node.as_ref() {
        Some(N::AConst(constant)) => match constant.val.as_ref() {
            Some(Val::Ival(value)) => value.ival != 0,
            Some(Val::Fval(value)) => duration_nonzero(&value.fval),
            Some(Val::Sval(value)) => duration_nonzero(&value.sval),
            _ => true,
        },
        Some(N::TypeCast(cast)) => cast.arg.as_deref().is_some_and(lock_timeout_nonzero),
        _ => true,
    }
}

/// Whether a PostgreSQL time value (`5s`, `5000`, `0`, `1min`) is non-zero.
/// A spelling we cannot parse is taken as non-zero: the rule then stays
/// quiet rather than reporting a migration that did set a timeout.
fn duration_nonzero(value: &str) -> bool {
    let value = value.trim();
    if value.eq_ignore_ascii_case("default") {
        return false;
    }
    let lower = value.to_ascii_lowercase();
    let unit = ["us", "ms", "s", "min", "h", "d"]
        .into_iter()
        .find(|unit| lower.ends_with(unit))
        .map_or(0, str::len);
    let number = lower[..lower.len() - unit].trim();
    number.parse::<f64>() != Ok(0.0)
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
        N::LockStmt(lock) => first_relation(catalog, pre_existing, &lock.relations, "LOCK TABLE"),
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
    }
    if strong
        && let Some(relation) = alter.relation.as_ref()
        && let Some(target) = existing_relation(catalog, pre_existing, relation, "ALTER TABLE")
    {
        return Some(target);
    }
    referenced
        .into_iter()
        .find_map(|relation| existing_relation(catalog, pre_existing, relation, "ALTER TABLE"))
}

/// Whether an `ALTER TABLE` subcommand only takes `SHARE UPDATE EXCLUSIVE`
/// on the table (PostgreSQL's lock-strength table).
fn only_share_update_exclusive(command: &typedpg_pg_query::protobuf::AlterTableCmd) -> bool {
    match AlterTableType::try_from(command.subtype) {
        Ok(
            AlterTableType::AtValidateConstraint
            | AlterTableType::AtSetStatistics
            | AlterTableType::AtClusterOn
            | AlterTableType::AtDropCluster
            | AlterTableType::AtSetRelOptions
            | AlterTableType::AtResetRelOptions
            | AlterTableType::AtReplaceRelOptions
            | AlterTableType::AtAttachPartition
            | AlterTableType::AtDetachPartitionFinalize,
        ) => true,
        Ok(AlterTableType::AtDetachPartition) => matches!(
            command.def.as_deref().and_then(|def| def.node.as_ref()),
            Some(N::PartitionCmd(partition)) if partition.concurrent
        ),
        _ => false,
    }
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
            && catalog
                .check_expression(constraint.oid)
                .is_some_and(|expr| is_not_null(expr, column))
    })
}

fn is_not_null(node: &Node, column: &str) -> bool {
    let Some(N::NullTest(test)) = node.node.as_ref() else {
        return false;
    };
    if NullTestType::try_from(test.nulltesttype) != Ok(NullTestType::IsNotNull) || test.argisrow {
        return false;
    }
    let Some(N::ColumnRef(reference)) = test.arg.as_deref().and_then(|arg| arg.node.as_ref())
    else {
        return false;
    };
    let [field] = reference.fields.as_slice() else {
        return false;
    };
    matches!(field.node.as_ref(), Some(N::String(name)) if name.sval == column)
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
        let exact = other_keys == key_count
            && other.indkey == index.indkey
            && other.indisunique == index.indisunique;
        if exact {
            return Some((other, true));
        }
        if index.indisunique || covering {
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
        ] {
            let findings = second(sql);
            assert!(
                only(&findings, Rule::MissingLockTimeout).is_empty(),
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
            "RESET lock_timeout;
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
        let findings = second("ALTER TABLE users VALIDATE CONSTRAINT users_email_nn;");
        assert!(
            only(&findings, Rule::MissingLockTimeout).is_empty(),
            "{findings:?}"
        );
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
