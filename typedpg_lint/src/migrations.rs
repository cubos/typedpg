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
    ConType, DdlError, MigrationObserver, MigrationStatement, PgCatalog, PgClassOid,
    PgConstraintOid, PgIndex, RelKind,
};
use typedpg_core::{QualifiedName, quote_identifier};
use typedpg_pg_query::protobuf::{
    AlterTableType, BoolExprType, ConstrType, Node, NullTestType, SortByDir, SortByNulls,
    node::Node as N,
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
        before_indexes: HashSet::new(),
        before_constraints: HashSet::new(),
        orderings: HashMap::new(),
        comments: Vec::new(),
        used_comments: HashSet::new(),
        reported_comments: HashSet::new(),
        current_suppressed: HashSet::new(),
        current_span: 0..0,
        last_statement_end: 0,
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
    before_indexes: HashSet<PgClassOid>,
    before_constraints: HashSet<PgConstraintOid>,
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
}

impl MigrationLinter<'_> {
    fn begin_file(&mut self, file: usize, catalog: &PgCatalog) {
        self.current = file;
        self.pre_existing = catalog
            .iter_classes()
            .filter(|class| {
                matches!(
                    class.relkind,
                    RelKind::Table | RelKind::Partitioned | RelKind::ForeignTable
                )
            })
            .map(|class| class.oid)
            .collect();
        self.comments = comments(&self.files[file].sql);
        self.used_comments.clear();
        self.reported_comments.clear();
        self.last_statement_end = 0;
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

    fn finish(&mut self, catalog: &PgCatalog) {
        self.schema_rule(catalog, Rule::FkWithoutIndex, |linter, catalog| {
            linter.fk_without_index(catalog)
        });
        self.schema_rule(catalog, Rule::RedundantIndex, |linter, catalog| {
            linter.redundant_index(catalog)
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
