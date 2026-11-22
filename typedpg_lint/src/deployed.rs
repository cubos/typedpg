//! The `breaks_deployed_queries` rule: a migration that breaks a query of
//! the code already deployed.
//!
//! A deploy applies the new migrations while the code of the previous
//! deployment keeps running, and that code keeps running during the rollout
//! too. Each of its queries was analyzed against the previous schema; the
//! rule analyzes it again against the new one and reports what the deployed
//! code would trip on: a query that no longer analyzes (a dropped or renamed
//! column or table, a type an operator no longer accepts), a result column
//! that is gone, changes type or may now be NULL, a column list that changes
//! (the generated code checks it), a parameter whose type changes or that no
//! longer accepts NULL, an `INSERT` that leaves out a column that is now
//! `NOT NULL` without a default, and a result value that may now be one the
//! deployed code does not know (a new enum label, a CHECK that admits more).
//!
//! What the deployed code writes is checked too: an `INSERT` (its `ON
//! CONFLICT DO UPDATE SET` included), `UPDATE`, `MERGE` or `copy_in!`
//! writing a column whose new definition accepts fewer values than the
//! value written may be — a shorter `varchar(n)` / `char(n)` / `bit(n)` /
//! `varbit(n)`, a `numeric` with fewer digits before the decimal point (a
//! value fails) or after it (a value is rounded), a new `NOT NULL`, a CHECK
//! of the table or the column's domain with a conjunct the deployed schema
//! doesn't have, `NOT VALID` ones included (they bind the rows written).
//! A parameter or a COPY field may be any value; a value the query computes
//! (a constant, a `CASE` of them, a column of an `INSERT … SELECT`) is
//! checked with what the analyzer knows of it, and is reported only when it
//! may not fit, compared as the column stores it (an untyped literal read
//! as its type, an integer as a `numeric`, a `date` as a `timestamp`;
//! numeric bounds against a narrower precision with exact decimals). A
//! CHECK conjunct the analyzer can't read exactly of one column (`CHECK
//! (lo <= hi)`, `length(name) > 1`) is evaluated over the constants each
//! row writes: silent when TRUE or NULL for every row, reported as
//! rejecting the rows when FALSE for one, and as one that may reject them
//! otherwise. `SET c = DEFAULT` writes NULL where the column (through a
//! view, the view's) has no default. Constraints are compared by what
//! they check, not their name. The tables written are the ones the analysis
//! resolved ([`QueryView::writes`]); a write into an automatically
//! updatable view (or a chain of them, with no DO INSTEAD rule or INSTEAD
//! OF trigger for the command) is checked against the table its rows are
//! stored in, each view column as the table column it stores, and an
//! `INSERT` through it leaves out the table columns the view doesn't
//! expose (unless a view's default, its own or its domain's, fills them
//! in). Not reported: a column
//! a new UNIQUE, primary key or exclusion constraint covers (whether a row
//! breaks it depends on the data), a parameter written into a column whose
//! type changes (its type change says so), and a timestamp, time or
//! interval precision (it rounds). A result column's type modifier changing is not
//! reported either: the deployed code decodes the same wire type whatever
//! its length or precision. A deployed `copy_in!` is also checked for the
//! columns it copies, their types and nullability.
//!
//! A query runs as the roles the deployed code runs it as (a call site's
//! `role = "..."`, or the database's configured `roles`): it is analyzed as
//! each, so a privilege a migration revokes is reported, and so is a table
//! whose row-level security now denies the role every row (no policy for
//! it, or none whose clause may be TRUE). A query without a role has no
//! privileges checked.
//!
//! Each finding is placed on the migration statement that makes the change:
//! the new migrations are applied again one statement at a time, for the
//! queries that break only.
//!
//! [`QueryView::writes`]: typedpg_analyzer::QueryView::writes

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ops::Range;

use typedpg_analyzer::{
    AnalyzedColumn, AnalyzedCopyIn, AnalyzedQuery, CheckReading, IntRange, MigrationObserver,
    MigrationStatement, PartOutcome, PgCatalog, PgClassOid, QualifiedName, QueryView, Refinement,
    Type, WriteCommand, WrittenValue,
};
use typedpg_core::quote_identifier;

use crate::migrations::{MigrationFile, MigrationFinding, MigrationLint};
use crate::{AllowComment, Finding, LintOptions, Rule, allow_comment, allowed_rules};

/// A query of the deployed code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployedQuery {
    /// The query as written.
    pub sql: String,
    /// Where the deployed code has it, as the message names it
    /// (`src/users.rs:12`): every place the same text is written.
    pub locations: Vec<String>,
    /// The roles the deployed code runs it as (a call site's `role =`, or
    /// the database's configured `roles`): it is analyzed as each. Empty
    /// for none: the plain analysis.
    pub roles: Vec<String>,
    /// What `sql` is: a query, or a `copy_in!` target.
    pub kind: DeployedKind,
}

/// What a [`DeployedQuery`]'s text is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DeployedKind {
    /// A query: a `sql!`, a `.sql` file's.
    #[default]
    Query,
    /// A `copy_in!` target, `table (columns)`: the deployed code sends its
    /// rows with `COPY … FROM STDIN (FORMAT binary)`.
    CopyIn,
}

/// The deployment the new migrations are applied under.
pub struct Deployment<'a> {
    /// The git ref of the deployed code, as the message names it.
    pub reference: &'a str,
    /// The schema the deployed code runs against: its own migrations.
    pub catalog: &'a PgCatalog,
    /// The deployed code's queries for this database.
    pub queries: &'a [DeployedQuery],
}

/// The new schema: the migrations that build it and the catalog they built.
pub struct NewSchema<'a> {
    /// Every migration file, as given to [`crate::lint_migrations`].
    pub files: &'a [MigrationFile],
    /// The index in `files` of the first migration the deployment doesn't
    /// have (or has with other contents).
    pub first_new: usize,
    pub use_transaction: bool,
    /// The catalog `files` built, set up for queries as `prepare` sets one.
    pub catalog: &'a PgCatalog,
    /// Sets a catalog built from migrations up for analyzing queries, as the
    /// CLI set up `catalog` (the parameter types assumed finite, …).
    pub prepare: &'a dyn Fn(&mut PgCatalog) -> Result<(), String>,
}

/// A deployed query's analysis, or a COPY target's.
#[derive(Debug, Clone)]
enum Analysis {
    Query(AnalyzedQuery),
    CopyIn(AnalyzedCopyIn),
}

/// A deployed query's analysis against the deployed schema, as one of the
/// roles it runs as (`None`: no role).
type RoleAnalysis<'q> = (Option<&'q str>, Analysis);

/// A deployed query that breaks: its analyses against the deployed schema
/// and what changes.
type Broken<'q> = (&'q DeployedQuery, Vec<RoleAnalysis<'q>>, Vec<Change>);

/// One thing that changes for a deployed query.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Change {
    /// What changes, as a key: the same change seen in two catalogs has the
    /// same subject.
    subject: String,
    /// The change, as the message words it.
    text: String,
}

/// The findings of `breaks_deployed_queries`: every deployed query that
/// analyzed against the deployed schema and breaks against the new one,
/// placed on the statement of `schema.files` that breaks it.
pub fn breaks_deployed_queries(
    deployment: &Deployment<'_>,
    schema: &NewSchema<'_>,
    options: &LintOptions,
) -> Vec<MigrationFinding> {
    let rule = Rule::BreaksDeployedQueries;
    let Some(severity) = options.level(rule) else {
        return Vec::new();
    };
    if schema.first_new >= schema.files.len() {
        return Vec::new();
    }
    // The queries that break, with their analysis against the deployed
    // schema and what changes.
    let mut broken: Vec<Broken<'_>> = Vec::new();
    for query in deployment.queries {
        if allowed_rules(&query.sql).contains(&rule) {
            continue;
        }
        let roles: Vec<Option<&str>> = if query.roles.is_empty() {
            vec![None]
        } else {
            query.roles.iter().map(|role| Some(role.as_str())).collect()
        };
        // A query that doesn't analyze against its own schema (as a role)
        // fails there already; the migration doesn't break it.
        let olds: Vec<RoleAnalysis<'_>> = roles
            .into_iter()
            .filter_map(|role| {
                analyze(deployment.catalog, query, role)
                    .ok()
                    .map(|old| (role, old))
            })
            .collect();
        if olds.is_empty() {
            continue;
        }
        let changes = query_changes(query, &olds, deployment.catalog, schema.catalog);
        if !changes.is_empty() {
            broken.push((query, olds, changes));
        }
    }
    if broken.is_empty() {
        return Vec::new();
    }

    let placements = place(deployment, schema, &broken);
    let mut findings = Vec::new();
    for (index, (query, _, changes)) in broken.iter().enumerate() {
        // The changes of one statement, in the order they were found.
        let mut by_statement: Vec<(Placement, Vec<&str>)> = Vec::new();
        for change in changes {
            let placement = placements
                .get(&(index, change.subject.clone()))
                .cloned()
                .unwrap_or_else(|| fallback_placement(schema));
            match by_statement.iter_mut().find(|(p, _)| *p == placement) {
                // An enum column's labels are its values too: one change
                // may be seen twice.
                Some((_, texts)) if texts.contains(&change.text.as_str()) => {}
                Some((_, texts)) => texts.push(&change.text),
                None => by_statement.push((placement, vec![&change.text])),
            }
        }
        for (placement, texts) in by_statement {
            if placement.suppressed {
                continue;
            }
            let location = match query.locations.as_slice() {
                [] => "a deployed query".to_owned(),
                [one] => format!("the deployed query at {one}"),
                [first, rest @ ..] => format!(
                    "the deployed query at {first} (also at {})",
                    rest.join(", ")
                ),
            };
            findings.push(MigrationFinding {
                file: placement.file,
                finding: Finding {
                    rule,
                    severity,
                    span: placement.span,
                    message: format!(
                        "this migration breaks {location} ({}): {}",
                        deployment.reference,
                        texts.join("; ")
                    ),
                    help: Some(format!(
                        "the code deployed at {} keeps running while the migration is applied \
                         and during the rollout: first deploy code that works with both schemas, \
                         then the migration (expand/contract)",
                        deployment.reference
                    )),
                    fix: Vec::new(),
                },
            });
        }
    }
    findings.sort_by(|a, b| {
        (a.file, a.finding.span.start, &a.finding.message).cmp(&(
            b.file,
            b.finding.span.start,
            &b.finding.message,
        ))
    });
    findings
}

/// Where a finding goes: a statement of a migration file, and whether an
/// allow comment there suppresses the rule.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Placement {
    file: usize,
    span: Range<usize>,
    suppressed: bool,
}

/// The placement of a change no reported statement could be found for (the
/// replay failed, or only an `extra_migrations` file makes it): the first
/// line of the first new migration.
fn fallback_placement(schema: &NewSchema<'_>) -> Placement {
    let file = schema.first_new;
    let sql = &schema.files[file].sql;
    let start = sql.len() - sql.trim_start().len();
    let end = sql[start..]
        .find('\n')
        .map_or(sql.len(), |offset| start + offset);
    Placement {
        file,
        span: start..end,
        suppressed: false,
    }
}

/// For each broken query (by index) and change subject, the first statement
/// of the new migrations after which the change shows.
fn place(
    deployment: &Deployment<'_>,
    schema: &NewSchema<'_>,
    broken: &[Broken<'_>],
) -> HashMap<(usize, String), Placement> {
    let mut placements = HashMap::new();
    let Ok(mut catalog) = PgCatalog::new() else {
        return placements;
    };
    catalog.set_migrations_use_transaction(schema.use_transaction);
    for file in &schema.files[..schema.first_new] {
        if catalog.apply_migration(&file.path, &file.sql).is_err() {
            return placements;
        }
    }
    let mut observer = Replay {
        deployment,
        schema,
        broken,
        file: 0,
        comments: Vec::new(),
        previous_end: 0,
        placements: &mut placements,
    };
    for (file, migration) in schema.files.iter().enumerate().skip(schema.first_new) {
        observer.file = file;
        observer.comments = allow_comments(&migration.sql);
        observer.previous_end = 0;
        if catalog
            .apply_migration_observed(&migration.path, &migration.sql, &mut observer)
            .is_err()
        {
            break;
        }
    }
    placements
}

/// Applies the new migrations one statement at a time, analyzing the broken
/// queries after each.
struct Replay<'a, 'b> {
    deployment: &'a Deployment<'a>,
    schema: &'a NewSchema<'a>,
    broken: &'a [Broken<'a>],
    /// The file being applied.
    file: usize,
    /// Its allow comments: their byte range and the rules they name.
    comments: Vec<(Range<usize>, HashSet<Rule>)>,
    /// Where the previous statement of the file ended: the comments after
    /// it lead into the next one.
    previous_end: usize,
    placements: &'b mut HashMap<(usize, String), Placement>,
}

impl MigrationObserver for Replay<'_, '_> {
    fn after(&mut self, catalog: &PgCatalog, statement: &MigrationStatement<'_>) {
        // The comments on the statement's last line, after its `;`, are
        // its own too.
        let tail =
            crate::trailing_comments_end(&self.schema.files[self.file].sql, statement.span.end);
        let lead_in = self.previous_end..tail;
        self.previous_end = tail;
        // A statement of somebody else's schema (`extra_migrations`) is not
        // reported on: its changes go to the first new migration.
        if self.schema.files[self.file].lint == MigrationLint::None {
            return;
        }
        let pending = self
            .broken
            .iter()
            .enumerate()
            .any(|(index, (_, _, changes))| {
                changes.iter().any(|change| {
                    !self
                        .placements
                        .contains_key(&(index, change.subject.clone()))
                })
            });
        if !pending {
            return;
        }
        let mut state = catalog.clone();
        if (self.schema.prepare)(&mut state).is_err() {
            return;
        }
        let suppressed = self.comments.iter().any(|(range, rules)| {
            range.start >= lead_in.start
                && range.end <= lead_in.end
                && rules.contains(&Rule::BreaksDeployedQueries)
        });
        let placement = Placement {
            file: self.file,
            span: statement.span.clone(),
            suppressed,
        };
        for (index, (query, olds, changes)) in self.broken.iter().enumerate() {
            let unplaced: Vec<&Change> = changes
                .iter()
                .filter(|change| {
                    !self
                        .placements
                        .contains_key(&(index, change.subject.clone()))
                })
                .collect();
            if unplaced.is_empty() {
                continue;
            }
            let now: HashSet<String> = query_changes(query, olds, self.deployment.catalog, &state)
                .into_iter()
                .map(|change| change.subject)
                .collect();
            for change in unplaced {
                if now.contains(&change.subject) {
                    self.placements
                        .insert((index, change.subject.clone()), placement.clone());
                }
            }
        }
    }
}

/// The allow comments of a migration file: their byte range and the rules
/// they name.
fn allow_comments(sql: &str) -> Vec<(Range<usize>, HashSet<Rule>)> {
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
        if let AllowComment::Allow(names) = allow_comment(&sql[start..end]) {
            let rules = names
                .iter()
                .filter_map(|name| Rule::from_name(name))
                .collect();
            out.push((start..end, rules));
        }
    }
    out
}

/// A deployed query (or COPY target) analyzed against `catalog` as `role`
/// (`None`: the plain analysis); the error's first line when it fails.
fn analyze(
    catalog: &PgCatalog,
    query: &DeployedQuery,
    role: Option<&str>,
) -> Result<Analysis, String> {
    let first_line = |message: String| message.lines().next().unwrap_or_default().trim().to_owned();
    match (query.kind, role) {
        (DeployedKind::Query, Some(role)) => catalog
            .analyze_as_located(&query.sql, role)
            .map(Analysis::Query)
            .map_err(|error| first_line(error.error.to_string())),
        (DeployedKind::Query, None) => catalog
            .analyze_located(&query.sql)
            .map(Analysis::Query)
            .map_err(|error| first_line(error.error.to_string())),
        (DeployedKind::CopyIn, Some(role)) => catalog
            .analyze_copy_in_as(&query.sql, role)
            .map(Analysis::CopyIn)
            .map_err(|error| first_line(error.to_string())),
        (DeployedKind::CopyIn, None) => catalog
            .analyze_copy_in_located(&query.sql)
            .map(Analysis::CopyIn)
            .map_err(|error| first_line(error.error.to_string())),
    }
}

/// What changes for a deployed query analyzed as `olds` (one analysis per
/// role it runs as) against `old_catalog` when it runs against
/// `new_catalog`: the changes for every role, each subject once.
fn query_changes(
    query: &DeployedQuery,
    olds: &[RoleAnalysis<'_>],
    old_catalog: &PgCatalog,
    new_catalog: &PgCatalog,
) -> Vec<Change> {
    let mut out: Vec<Change> = Vec::new();
    for (role, old) in olds {
        for change in changes(query, *role, old, old_catalog, new_catalog) {
            if !out.iter().any(|c| c.subject == change.subject) {
                out.push(change);
            }
        }
    }
    out
}

/// What changes for a deployed query analyzed as `old` against
/// `old_catalog`, as `role`, when it runs against `new_catalog`.
fn changes(
    query: &DeployedQuery,
    role: Option<&str>,
    old: &Analysis,
    old_catalog: &PgCatalog,
    new_catalog: &PgCatalog,
) -> Vec<Change> {
    let new = match analyze(new_catalog, query, role) {
        Ok(new) => new,
        Err(error) => {
            // What fails for the role alone (a privilege it lost, a policy
            // it lacks) is said of the role; what fails for anyone is not.
            if let Some(role) = role
                && analyze(new_catalog, query, None).is_ok()
            {
                return vec![Change {
                    subject: format!("analysis as {role}"),
                    text: format!("it no longer runs as role \"{role}\": {error}"),
                }];
            }
            return vec![Change {
                subject: "analysis".to_owned(),
                text: format!("it no longer runs: {error}"),
            }];
        }
    };
    let mut out = Vec::new();
    match (old, &new) {
        (Analysis::Query(old), Analysis::Query(new)) => {
            statement_changes(
                &query.sql,
                role,
                old,
                new,
                old_catalog,
                new_catalog,
                &mut out,
            );
        }
        (Analysis::CopyIn(old), Analysis::CopyIn(new)) => {
            copy_changes(old, new, old_catalog, new_catalog, &mut out);
        }
        _ => {}
    }
    out
}

/// What changes for a deployed statement analyzed as `old` against
/// `old_catalog` and as `new` against `new_catalog`, as `role`.
fn statement_changes(
    sql: &str,
    role: Option<&str>,
    old: &AnalyzedQuery,
    new: &AnalyzedQuery,
    old_catalog: &PgCatalog,
    new_catalog: &PgCatalog,
    out: &mut Vec<Change>,
) {
    columns(&old.columns, &new.columns, out);
    for old_param in &old.params {
        let Some(new_param) = new.params.iter().find(|p| p.name == old_param.name) else {
            continue;
        };
        input(
            &format!("parameter ${}", old_param.name),
            (&old_param.pg_type, old_param.nullable),
            (&new_param.pg_type, new_param.nullable),
            out,
        );
    }
    for old_spread in &old.spreads {
        let Some(new_spread) = new.spreads.iter().find(|s| s.name == old_spread.name) else {
            continue;
        };
        for old_field in &old_spread.fields {
            let Some(new_field) = new_spread.fields.iter().find(|f| f.name == old_field.name)
            else {
                continue;
            };
            input(
                &format!("field \"{}\" of ${}", old_field.name, old_spread.name),
                (&old_field.pg_type, old_field.nullable),
                (&new_field.pg_type, new_field.nullable),
                out,
            );
        }
    }
    // What it writes, and the rows row-level security lets the role see,
    // are read off the analysis as the role: its search path resolves the
    // names, a CTE named like a table is the CTE.
    let lower = sql.to_ascii_lowercase();
    let writes_rows = ["insert", "update", "merge"]
        .iter()
        .any(|word| lower.contains(word));
    if !writes_rows && role.is_none() {
        return;
    }
    let view = match role {
        Some(role) => new_catalog.query_view_as(sql, role),
        None => new_catalog.query_view(sql),
    };
    let Ok(view) = view else {
        return;
    };
    let old_view = match role {
        Some(role) => old_catalog.query_view_as(sql, role),
        None => old_catalog.query_view(sql),
    };
    writes(&view, old_view.as_ref().ok(), old_catalog, new_catalog, out);
    if let Some(role) = role {
        row_security(&view, role, old_catalog, new_catalog, out);
    }
}

/// What changes for a deployed `copy_in!` analyzed as `old` against
/// `old_catalog` and as `new` against `new_catalog`: the columns it sends
/// (a target without a column list copies every column), each column's
/// type and nullability, a column it leaves out that the new schema makes
/// `NOT NULL` without a default, and a column whose new definition accepts
/// fewer values (see [`narrowed`]).
fn copy_changes(
    old: &AnalyzedCopyIn,
    new: &AnalyzedCopyIn,
    old_catalog: &PgCatalog,
    new_catalog: &PgCatalog,
    out: &mut Vec<Change>,
) {
    let Some((schema, relname)) = relation_name(new_catalog, new.relid) else {
        return;
    };
    let name = display_name(&schema, &relname);
    let statement = format!("its COPY into {name}");
    let names = |columns: &[AnalyzedColumn]| {
        columns
            .iter()
            .map(|c| quote_identifier(&c.name))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if names(&old.columns) != names(&new.columns) {
        out.push(Change {
            subject: "copy columns".to_owned(),
            text: format!(
                "{statement} copies columns ({}) instead of ({}), the ones the deployed code \
                 sends",
                names(&new.columns),
                names(&old.columns)
            ),
        });
    }
    for old_column in &old.columns {
        let Some(new_column) = new.columns.iter().find(|c| c.name == old_column.name) else {
            continue;
        };
        input(
            &format!("column \"{}\" of {statement}", old_column.name),
            (&old_column.pg_type, old_column.nullable),
            (&new_column.pg_type, new_column.nullable),
            out,
        );
    }
    let Some(old_table) = old_catalog.resolve_table(Some(&schema), &relname) else {
        return;
    };
    let inserted = Inserted {
        given: new.columns.iter().map(|c| c.name.clone()).collect(),
        exposed: None,
    };
    omitted(
        &statement,
        &name,
        (old_table.oid, new.relid),
        &inserted,
        None,
        old_catalog,
        new_catalog,
        out,
    );
    let written: Vec<(String, Vec<Value<'_>>)> = new
        .columns
        .iter()
        .map(|c| {
            (
                c.name.clone(),
                vec![Value::Input {
                    may_be_null: c.nullable,
                }],
            )
        })
        .collect();
    narrowed(
        &statement,
        &name,
        (old_table.oid, new.relid),
        &written,
        &[],
        old_catalog,
        new_catalog,
        out,
    );
}

/// The schema and name of relation `relid` of `catalog`.
fn relation_name(catalog: &PgCatalog, relid: PgClassOid) -> Option<(String, String)> {
    let class = catalog.pg_class().get(&relid)?;
    let schema = catalog.namespace_name(class.relnamespace)?;
    Some((schema.to_owned(), class.relname.clone()))
}

/// A relation's name as a message says it: qualified outside `public`.
fn display_name(schema: &str, relname: &str) -> String {
    if schema == "public" {
        quote_identifier(relname)
    } else {
        QualifiedName::new(schema, relname).to_string()
    }
}

/// The relations the query reads or writes ([`QueryView::relation_uses`]:
/// as the analyzer resolved them) whose rows row-level security now denies
/// `role` the command it runs on them, where the deployed schema didn't:
/// a table that enables it without a policy for the role, a policy
/// dropped, one whose clause is now never TRUE (`USING (false)`). The
/// query still analyzes — a SELECT then sees no row, an UPDATE or a DELETE
/// changes none, and an INSERT fails. A table the query writes is said of
/// the write alone.
///
/// [`QueryView::relation_uses`]: typedpg_analyzer::QueryView::relation_uses
fn row_security(
    view: &QueryView,
    role: &str,
    old_catalog: &PgCatalog,
    new_catalog: &PgCatalog,
    out: &mut Vec<Change>,
) {
    let uses = view.relation_uses();
    for &(relid, command) in uses {
        if command == 'r' && uses.iter().any(|&(r, c)| r == relid && c != 'r') {
            continue;
        }
        let Some((schema, relname)) = relation_name(new_catalog, relid) else {
            continue;
        };
        let Some(old_table) = old_catalog.resolve_table(Some(&schema), &relname) else {
            continue;
        };
        if !new_catalog.row_security_denies(relid, role, command)
            || old_catalog.row_security_denies(old_table.oid, role, command)
        {
            continue;
        }
        let name = display_name(&schema, &relname);
        let (verb, outcome) = match command {
            'a' => ("INSERT", format!("its INSERT into {name} fails")),
            'w' => ("UPDATE", format!("its UPDATE of {name} changes no row")),
            'd' => ("DELETE", format!("its DELETE from {name} removes no row")),
            _ => ("SELECT", format!("it sees no row of {name}")),
        };
        out.push(Change {
            subject: format!("row security {name} {command} {role}"),
            text: format!(
                "{outcome} as role \"{role}\": row-level security applies to the role and no \
                 policy lets it {verb}"
            ),
        });
    }
}

/// What changes in the result columns the deployed code reads.
fn columns(old: &[AnalyzedColumn], new: &[AnalyzedColumn], out: &mut Vec<Change>) {
    let names = |columns: &[AnalyzedColumn]| {
        columns
            .iter()
            .map(|c| quote_identifier(&c.name))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut gone = false;
    for column in old {
        let Some(now) = new.iter().find(|c| c.name == column.name) else {
            gone = true;
            out.push(Change {
                subject: format!("column {} gone", column.name),
                text: format!("result column \"{}\" is gone", column.name),
            });
            continue;
        };
        output(column, now, out);
    }
    if !gone && (old.len() != new.len() || old.iter().zip(new).any(|(a, b)| a.name != b.name)) {
        out.push(Change {
            subject: "column list".to_owned(),
            text: format!(
                "its result columns become ({}) instead of ({}), and the deployed code checks \
                 them",
                names(new),
                names(old)
            ),
        });
    }
}

/// What changes in one result column the deployed code reads.
fn output(old: &AnalyzedColumn, new: &AnalyzedColumn, out: &mut Vec<Change>) {
    let name = &old.name;
    let subject = |what: &str| format!("column {name} {what}");
    if shape(&old.pg_type) != shape(&new.pg_type) {
        out.push(Change {
            subject: subject("type"),
            text: format!(
                "result column \"{name}\" changes from {} to {}",
                old.pg_type.describe(),
                new.pg_type.describe()
            ),
        });
        return;
    }
    if !old.nullable && new.nullable {
        out.push(Change {
            subject: subject("null"),
            text: format!("result column \"{name}\" may now be NULL"),
        });
    }
    let added = added_labels(&old.pg_type, &new.pg_type);
    if !added.is_empty() {
        out.push(Change {
            subject: subject("labels"),
            text: format!(
                "result column \"{name}\" may now be {}, a value the deployed code does not know",
                quoted(&added)
            ),
        });
    }
    if let Some(text) = widened(&old.refinement, &new.refinement) {
        out.push(Change {
            subject: subject("refinement"),
            text: format!("result column \"{name}\" {text}"),
        });
    }
    record_fields(
        &old.pg_type,
        &new.pg_type,
        &format!("result column \"{name}\""),
        &subject,
        out,
    );
    if let (Some(old_element), Some(new_element)) =
        (array_element(&old.pg_type), array_element(&new.pg_type))
    {
        if old_element.1 == Some(false) && new_element.1 != Some(false) {
            out.push(Change {
                subject: subject("element null"),
                text: format!("the elements of result column \"{name}\" may now be NULL"),
            });
        }
        if let Some(text) = widened(&old.elem_refinement, &new.elem_refinement) {
            out.push(Change {
                subject: subject("element refinement"),
                text: format!("the elements of result column \"{name}\" {text}"),
            });
        }
    }
}

/// What changes in the fields of a record the deployed code reads (`of`:
/// the value they are fields of, as the message names it): a field that
/// may now be NULL, or may now be a value the deployed code does not know.
/// The types match ([`shape`]); nested records are followed.
fn record_fields(
    old: &Type,
    new: &Type,
    of: &str,
    subject: &dyn Fn(&str) -> String,
    out: &mut Vec<Change>,
) {
    let fields = |ty: &Type| match ty {
        Type::Composite { fields, .. } | Type::AnonymousRecord { fields } => Some(fields.clone()),
        _ => None,
    };
    let (Some(old_fields), Some(new_fields)) = (fields(old), fields(new)) else {
        return;
    };
    for (a, b) in old_fields.iter().zip(&new_fields) {
        let field = format!("field \"{}\" of {of}", a.name);
        if !a.nullable && b.nullable {
            out.push(Change {
                subject: subject(&format!("{field} null")),
                text: format!("{field} may now be NULL"),
            });
        }
        if let Some(text) = widened(&a.refinement, &b.refinement) {
            out.push(Change {
                subject: subject(&format!("{field} refinement")),
                text: format!("{field} {text}"),
            });
        }
        if let Some(text) = widened(&a.elem_refinement, &b.elem_refinement) {
            out.push(Change {
                subject: subject(&format!("{field} element refinement")),
                text: format!("the elements of {field} {text}"),
            });
        }
        record_fields(&a.ty, &b.ty, &field, subject, out);
    }
}

/// What changes in a value the deployed code sends (`what`: `parameter $x`),
/// given as its type and whether it may be NULL.
fn input(what: &str, old: (&Type, bool), new: (&Type, bool), out: &mut Vec<Change>) {
    if shape(old.0) != shape(new.0) {
        out.push(Change {
            subject: format!("{what} type"),
            text: format!(
                "{what} changes from {} to {}",
                old.0.describe(),
                new.0.describe()
            ),
        });
        return;
    }
    if old.1 && !new.1 {
        out.push(Change {
            subject: format!("{what} null"),
            text: format!("{what} no longer accepts NULL"),
        });
    }
    let removed = added_labels(new.0, old.0);
    if !removed.is_empty() {
        out.push(Change {
            subject: format!("{what} labels"),
            text: format!("{what} no longer accepts {}", quoted(&removed)),
        });
    }
}

/// A value a deployed statement writes into a column.
#[derive(Debug, Clone, Copy)]
enum Value<'a> {
    /// What the deployed code sends: a parameter, a COPY field, or a value
    /// stored inside the column (`arr[1] = $x`). Nothing is known of it but
    /// whether the new schema still lets it be NULL (a parameter that no
    /// longer may be is reported on its own).
    Input { may_be_null: bool },
    /// NULL: a bare `NULL`, or an UPDATE's `SET c = DEFAULT` of a column
    /// the new schema gives no default — `default`: whether the deployed
    /// schema gave it none either (`None` for a bare `NULL`, NULL in both).
    Null { default: Option<bool> },
    /// An expression the query computes (a constant, a `CASE` of them, a
    /// column of an `INSERT … SELECT`): its type and type modifier, whether
    /// it may be NULL, and what each non-NULL value is.
    Expression {
        type_oid: typedpg_analyzer::PgTypeOid,
        typmod: Option<i32>,
        nullable: bool,
        refinement: &'a Refinement,
    },
}

impl Value<'_> {
    fn may_be_null(&self) -> bool {
        match self {
            Value::Input { may_be_null } => *may_be_null,
            Value::Null { .. } => true,
            Value::Expression { nullable, .. } => *nullable,
        }
    }

    /// Whether it may be NULL where the deployed schema wrote no NULL into
    /// the column: `SET c = DEFAULT` of a column whose default is gone.
    fn null_only_now(&self) -> bool {
        matches!(
            self,
            Value::Null {
                default: Some(false)
            }
        )
    }

    /// What each non-NULL value is once stored into a column of type `to`
    /// (an untyped literal read by the type's input function, an integer
    /// as a `numeric`, a `date` as a `timestamp`); `None` for an input.
    fn stored(&self, to: typedpg_analyzer::PgTypeOid, catalog: &PgCatalog) -> Option<Refinement> {
        match self {
            Value::Expression {
                type_oid,
                refinement,
                ..
            } => Some(catalog.lint_converted_refinement(refinement, *type_oid, to)),
            Value::Input { .. } | Value::Null { .. } => None,
        }
    }

    /// Whether each non-NULL value, stored into a column of type `to`, is
    /// certainly one `allowed` allows.
    fn within(
        &self,
        allowed: &Refinement,
        to: typedpg_analyzer::PgTypeOid,
        catalog: &PgCatalog,
    ) -> bool {
        match self {
            Value::Input { .. } => false,
            Value::Null { .. } => true,
            Value::Expression { .. } => self
                .stored(to, catalog)
                .is_some_and(|stored| !allowed.widenings(&stored).any()),
        }
    }

    /// Whether each value certainly fits a column of type `to` limited to
    /// `limit`, as is: its own type is no wider, or each value it can be
    /// fits.
    fn fits(
        &self,
        limit: typedpg_analyzer::ValueLimit,
        to: typedpg_analyzer::PgTypeOid,
        catalog: &PgCatalog,
    ) -> bool {
        let Value::Expression {
            type_oid,
            typmod,
            refinement,
            ..
        } = self
        else {
            return matches!(self, Value::Null { .. });
        };
        let own = catalog.lint_value_limit(*type_oid, *typmod);
        if narrower_type(own, Some(limit)).is_none() {
            return true;
        }
        if let Some(values) = &refinement.values {
            return values.iter().all(|value| fits_limit(value, limit));
        }
        let typedpg_analyzer::ValueLimit::Numeric { precision, scale } = limit else {
            return false;
        };
        // An integer within bounds.
        if let Some(range) = refinement.range
            && let (Some(lo), Some(hi)) = (range.lo, range.hi)
        {
            let digits = lo.unsigned_abs().max(hi.unsigned_abs()).to_string().len();
            return digits <= usize::try_from(precision - scale).unwrap_or(0) || lo == 0 && hi == 0;
        }
        // Numbers within bounds, as a `numeric`: each fits when no digit
        // after the decimal point is lost — its own type's scale (or the
        // one number's digits) is no larger — and the bounds, rounded to
        // the column's scale as `numeric` rounds (half away from zero),
        // have no more digits before it than the column allows.
        let Some(bounds) = self.stored(to, catalog).and_then(|stored| stored.bounds) else {
            return false;
        };
        let (
            typedpg_analyzer::OrdKind::Numeric,
            Some(typedpg_analyzer::OrdBound {
                point: typedpg_analyzer::OrdPoint::Finite(lo),
                ..
            }),
            Some(typedpg_analyzer::OrdBound {
                point: typedpg_analyzer::OrdPoint::Finite(hi),
                ..
            }),
        ) = (bounds.kind, bounds.lo, bounds.hi)
        else {
            return false;
        };
        let fraction = match own {
            Some(typedpg_analyzer::ValueLimit::Numeric { scale, .. }) => Some(scale),
            _ if lo == hi => i32::try_from(lo.scale()).ok(),
            _ => None,
        };
        fraction.is_some_and(|f| f <= scale)
            && numeric_fits(lo, precision, scale)
            && numeric_fits(hi, precision, scale)
    }
}

/// Whether the number `n`, rounded to `scale` digits after the decimal
/// point as `numeric` rounds (half away from zero), has at most `precision
/// - scale` digits before it: `apply_typmod` accepts it in a
/// `numeric(precision, scale)`. Only for `0 <= scale <= precision`.
fn numeric_fits(n: typedpg_analyzer::Num, precision: i32, scale: i32) -> bool {
    let (Ok(scale), Ok(precision)) = (u32::try_from(scale), u32::try_from(precision)) else {
        return false;
    };
    if scale > precision {
        return false;
    }
    let (mantissa, digits) = (n.mantissa(), n.scale());
    // The digits at the column's scale.
    let rounded = if digits > scale {
        let Some(unit) = 10i128.checked_pow(digits - scale) else {
            return false;
        };
        let (q, r) = (mantissa / unit, (mantissa % unit).abs());
        if r * 2 >= unit {
            q + mantissa.signum()
        } else {
            q
        }
    } else {
        match 10i128
            .checked_pow(scale - digits)
            .and_then(|unit| mantissa.checked_mul(unit))
        {
            Some(m) => m,
            None => return false,
        }
    };
    // Below 10^precision at that scale: below 10^(precision - scale).
    10i128
        .checked_pow(precision)
        .is_none_or(|limit| rounded.unsigned_abs() < limit.unsigned_abs())
}

/// Whether the value printed as `value` fits a column limited to `limit`
/// as is.
fn fits_limit(value: &str, limit: typedpg_analyzer::ValueLimit) -> bool {
    use typedpg_analyzer::ValueLimit::{ExactLength, Length, Numeric};
    let length = |n: i32| usize::try_from(n).unwrap_or(0);
    match limit {
        Length(n) => value.chars().count() <= length(n),
        ExactLength(n) => {
            value.chars().all(|c| c == '0' || c == '1') && value.chars().count() == length(n)
        }
        Numeric { precision, scale } => {
            let unsigned = value.strip_prefix(['-', '+']).unwrap_or(value);
            let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
            if whole.is_empty() && fraction.is_empty()
                || !whole
                    .chars()
                    .chain(fraction.chars())
                    .all(|c| c.is_ascii_digit())
            {
                return false;
            }
            let whole = whole.trim_start_matches('0').len();
            let fraction = fraction.trim_end_matches('0').len();
            whole <= length(precision - scale) && fraction <= length(scale)
        }
    }
}

/// Whether parameter `$number` of `analyzed` (a spread's field, numbered
/// after the parameters) may be NULL.
fn parameter_nullable(analyzed: &AnalyzedQuery, number: i32) -> bool {
    let Some(index) = usize::try_from(number).ok().and_then(|n| n.checked_sub(1)) else {
        return true;
    };
    if let Some(param) = analyzed.params.get(index) {
        return param.nullable;
    }
    let mut index = index - analyzed.params.len();
    for spread in &analyzed.spreads {
        if spread.fields.is_empty() {
            if index == 0 {
                return true;
            }
            index -= 1;
            continue;
        }
        if let Some(field) = spread.fields.get(index) {
            return field.nullable;
        }
        index -= spread.fields.len();
    }
    true
}

/// What the query's data-modifying statements — `INSERT`s (their `ON
/// CONFLICT DO UPDATE SET` included), `UPDATE`s and `MERGE`s, as
/// [`QueryView::writes`] gives them: on the relation the analyzer resolved
/// — write that the new schema rejects where the deployed one didn't: an
/// `INSERT` that leaves out a column now `NOT NULL` without a default (see
/// [`omitted`]), and a column written whose new definition accepts fewer
/// values than the value written can be (see [`narrowed`]). `SET c =
/// DEFAULT` writes the target column's default, NULL when the new schema
/// gives it none (through a view, the view's: PostgreSQL's
/// `rewriteTargetListIU` doesn't look below it for an UPDATE).
///
/// A write into an automatically updatable view (through a chain of them)
/// is checked against the table its rows are stored in, each view column
/// as the base column it stores ([`WriteStatement::base`]); the base
/// columns the view doesn't expose, unless a view's default fills them in,
/// are ones an `INSERT` leaves out. A write into any other view (one with
/// a DO INSTEAD rule or an INSTEAD OF trigger for the command) is not the
/// base table's: it is checked against the view's own columns alone
/// (their types, which the values written are coerced to).
///
/// `old_view`: the same query's analysis against the deployed schema,
/// whose statements are `view`'s in the same order — what an `INSERT` gave
/// there (a view's default it no longer has included) decides whether it
/// left a column out already.
///
/// [`QueryView::writes`]: typedpg_analyzer::QueryView::writes
/// [`WriteStatement::base`]: typedpg_analyzer::WriteStatement::base
fn writes(
    view: &QueryView,
    old_view: Option<&QueryView>,
    old_catalog: &PgCatalog,
    new_catalog: &PgCatalog,
    out: &mut Vec<Change>,
) {
    for (index, statement) in view.writes().iter().enumerate() {
        let Some((schema, relname)) = relation_name(new_catalog, statement.relid) else {
            continue;
        };
        let target = display_name(&schema, &relname);
        // The target as the deployed schema has it.
        let old_target = old_catalog
            .resolve_table(Some(&schema), &relname)
            .map(|t| t.oid);
        // The relation the rows are stored in: its schema, name and OID.
        let stored = match &statement.base {
            Some(base) => {
                let Some((schema, relname)) = relation_name(new_catalog, base.relid) else {
                    continue;
                };
                (schema, relname, base.relid)
            }
            None => (schema, relname, statement.relid),
        };
        // The name a target column has in the stored relation.
        let to_base = |column: &str| -> Option<String> {
            match &statement.base {
                Some(base) => base
                    .columns
                    .iter()
                    .find(|(v, _)| v == column)
                    .map(|(_, b)| b.clone()),
                None => Some(column.to_owned()),
            }
        };
        let Some(old_table) = old_catalog.resolve_table(Some(&stored.0), &stored.1) else {
            continue;
        };
        let name = display_name(&stored.0, &stored.1);
        let into = match &statement.base {
            Some(_) => format!("{target} (a view of {name})"),
            None => target.clone(),
        };
        let text = match statement.command {
            WriteCommand::Insert => format!("its INSERT into {into}"),
            WriteCommand::Update => format!("its UPDATE of {into}"),
            WriteCommand::Merge => format!("its MERGE into {into}"),
        };
        let relids = (old_table.oid, stored.2);
        let mut written: Vec<(String, Vec<Value<'_>>)> = Vec::new();
        // Each target list's values, by stored column, and whether it is
        // an UPDATE's.
        let mut rows: Vec<Row<'_>> = (0..statement.rows)
            .map(|_| (false, HashMap::new()))
            .collect();
        for column in &statement.columns {
            let Some(base_column) = to_base(&column.column) else {
                continue;
            };
            if let Some((update, values)) = rows.get_mut(column.row) {
                *update = column.update;
                values.insert(base_column.clone(), &column.value);
            }
            let value = match &column.value {
                // `SET c = DEFAULT`: the target column's default, NULL
                // without one.
                WrittenValue::Default if column.update => {
                    if new_catalog.lint_column_has_default(statement.relid, &column.column) {
                        continue;
                    }
                    Value::Null {
                        default: old_target
                            .map(|old| !old_catalog.lint_column_has_default(old, &column.column)),
                    }
                }
                // An inserted row's default: what the deployed code doesn't
                // send, as a column it leaves out.
                WrittenValue::Default => continue,
                WrittenValue::Parameter(number) => Value::Input {
                    may_be_null: parameter_nullable(&view.analyzed, *number),
                },
                WrittenValue::Inside => Value::Input { may_be_null: false },
                WrittenValue::Expression {
                    type_oid,
                    refinement,
                    ..
                } if *type_oid == UNKNOWN && refinement.values.is_none() => {
                    // An untyped value of no known contents: a bare NULL.
                    Value::Null { default: None }
                }
                WrittenValue::Expression {
                    type_oid,
                    typmod,
                    nullable,
                    refinement,
                } => Value::Expression {
                    type_oid: *type_oid,
                    typmod: *typmod,
                    nullable: *nullable,
                    refinement,
                },
            };
            match written.iter_mut().find(|(c, _)| *c == base_column) {
                Some((_, values)) => values.push(value),
                None => written.push((base_column, vec![value])),
            }
        }
        if statement.command == WriteCommand::Insert {
            let mut inserted = Inserted {
                given: Vec::new(),
                exposed: None,
            };
            for column in statement.columns.iter().filter(|c| !c.update) {
                if let Some(base_column) = to_base(&column.column)
                    && !inserted.given.contains(&base_column)
                {
                    inserted.given.push(base_column);
                }
            }
            if let Some(base) = &statement.base {
                // What a view's default fills in is given.
                for column in &base.defaulted {
                    if !inserted.given.contains(column) {
                        inserted.given.push(column.clone());
                    }
                }
                inserted.exposed = Some((
                    target.clone(),
                    base.columns.iter().map(|(_, b)| b.clone()).collect(),
                ));
            }
            // What the same INSERT gave against the deployed schema.
            let old_given = old_view
                .and_then(|old| old.writes().get(index))
                .filter(|old| old.command == statement.command)
                .map(given_columns);
            omitted(
                &text,
                &name,
                relids,
                &inserted,
                old_given.as_deref(),
                old_catalog,
                new_catalog,
                out,
            );
        }
        narrowed(
            &text,
            &name,
            relids,
            &written,
            &rows,
            old_catalog,
            new_catalog,
            out,
        );
    }
}

/// The `unknown` pseudo-type: the type of an untyped literal.
const UNKNOWN: typedpg_analyzer::PgTypeOid = typedpg_analyzer::PgTypeOid::from_raw(705);

/// The values one target list of a statement writes (a VALUES row, a SET
/// list, …), by the column of the relation storing its rows, and whether
/// it is an UPDATE's.
type Row<'a> = (bool, HashMap<String, &'a WrittenValue>);

/// The columns an `INSERT` `statement` gives a value for, by the name the
/// relation storing its rows has for them, with those a view's default
/// fills in.
fn given_columns(statement: &typedpg_analyzer::WriteStatement) -> Vec<String> {
    let mut given: Vec<String> = Vec::new();
    for column in statement.columns.iter().filter(|c| !c.update) {
        let base_column = match &statement.base {
            Some(base) => base
                .columns
                .iter()
                .find(|(v, _)| *v == column.column)
                .map(|(_, b)| b.clone()),
            None => Some(column.column.clone()),
        };
        if let Some(base_column) = base_column
            && !given.contains(&base_column)
        {
            given.push(base_column);
        }
    }
    if let Some(base) = &statement.base {
        for column in &base.defaulted {
            if !given.contains(column) {
                given.push(column.clone());
            }
        }
    }
    given
}

/// The columns an `INSERT` (or a COPY) gives a value for, by the name the
/// relation storing its rows has for them (see [`omitted`]).
struct Inserted {
    /// Given a value (`DEFAULT` included: the analysis rejects one that
    /// stores NULL into a NOT NULL column), or filled in by a view's
    /// default.
    given: Vec<String>,
    /// Through a view: its name, as the message says it, and the stored
    /// columns it exposes. `None` for a table.
    exposed: Option<(String, Vec<String>)>,
}

/// A column of relation `relid` of `catalog`, by name.
fn attribute(
    catalog: &PgCatalog,
    relid: PgClassOid,
    column: &str,
) -> Option<typedpg_analyzer::PgAttribute> {
    catalog
        .attributes_of(relid)
        .iter()
        .find(|a| a.attnum > 0 && a.attname == column)
        .cloned()
}

/// The columns of the relation `statement` (`its INSERT into t`, `its COPY
/// into t`) inserts rows into — `relids`: its OID in the old and the new
/// catalog, `name` as the message says it — that it leaves out (gives no
/// value: not one of `inserted.given`; one the view it inserts through
/// doesn't expose is, unless a view's default fills it in) and the new
/// schema makes `NOT NULL` without a default (its own, an identity or
/// generation expression, or its domain's), where the deployed schema let
/// them be omitted — had a default for them, or the deployed statement
/// gave them (`old_given`: the columns it gave against the deployed
/// schema, a view's default included; `None` when not known, the same as
/// now): PostgreSQL fails every row at runtime, which no analysis of the
/// statement shows.
#[allow(clippy::too_many_arguments)]
fn omitted(
    statement: &str,
    name: &str,
    relids: (PgClassOid, PgClassOid),
    inserted: &Inserted,
    old_given: Option<&[String]>,
    old_catalog: &PgCatalog,
    new_catalog: &PgCatalog,
    out: &mut Vec<Change>,
) {
    let required = |catalog: &PgCatalog, relid: PgClassOid, column: &str| {
        attribute(catalog, relid, column)
            .is_some_and(|a| a.attnotnull && !catalog.lint_column_has_default(relid, column))
    };
    for attribute in new_catalog.attributes_of(relids.1) {
        if attribute.attnum <= 0
            || inserted.given.contains(&attribute.attname)
            || !required(new_catalog, relids.1, &attribute.attname)
        {
            continue;
        }
        // Left out before too: the deployed code already fails there.
        if old_given
            .unwrap_or(&inserted.given)
            .iter()
            .all(|c| *c != attribute.attname)
            && required(old_catalog, relids.0, &attribute.attname)
        {
            continue;
        }
        let column = quote_identifier(&attribute.attname);
        let text = if let Some((view, exposed)) = &inserted.exposed
            && !exposed.contains(&attribute.attname)
        {
            format!(
                "{statement} leaves out column {column}, which {view} doesn't expose, now NOT \
                 NULL without a default, so inserting any row fails"
            )
        } else {
            format!(
                "{statement} leaves out column {column}, now NOT NULL without a default, so \
                 inserting any row fails"
            )
        };
        out.push(Change {
            subject: format!("omit {name} {}", attribute.attname),
            text,
        });
    }
}

/// The columns `written` (each with the values written into it) of the
/// relation `statement` (`its INSERT into t`) writes — `relids`: its OID in
/// the old and the new catalog, `name` as the message says it — whose new
/// definition accepts fewer values than the deployed one, and fewer than
/// the values written can be: a narrower type modifier (a shorter
/// `varchar(n)`, `char(n)`, `bit(n)`, `varbit(n)`; fewer digits before a
/// `numeric`'s decimal point, which fail, or after it, which round); a
/// `NOT NULL` a value that may be NULL now breaks; and a CHECK constraint
/// of the table or the column's domain (`NOT VALID` ones too: they bind
/// the rows written) with a conjunct the deployed schema doesn't have —
/// constraints are told apart by what they check, not their name, so one
/// renamed or dropped and added again as it was changes nothing. A value
/// is compared as the column stores it (an untyped literal read as the
/// column's type, an integer as a `numeric`, a `date` as a `timestamp`).
/// A new conjunct the analyzer reads exactly of the column alone (`kind IN
/// ('a', 'b')`, `n > 0`, `n IS NOT NULL`) is reported when the value
/// written may break it — a parameter, or a computed value it doesn't
/// prove fits. One it can't read so (`length(name) > 1`, `lo <= hi`) is
/// evaluated over each target list of `rows` that writes a column it
/// reads ([`PgCatalog::check_part_outcome`]): silent when TRUE or NULL for
/// every one, reported as rejecting the rows when FALSE for one (a
/// constant every row is written), else as one that may reject the
/// deployed code's rows. A parameter written into a column whose type
/// changes is left to its own type change; a column a new UNIQUE, primary
/// key or exclusion constraint covers is not reported (it depends on the
/// data).
#[allow(clippy::too_many_arguments)]
fn narrowed(
    statement: &str,
    name: &str,
    relids: (PgClassOid, PgClassOid),
    written: &[(String, Vec<Value<'_>>)],
    rows: &[Row<'_>],
    old_catalog: &PgCatalog,
    new_catalog: &PgCatalog,
    out: &mut Vec<Change>,
) {
    // The new conjuncts the analyzer can't read exactly of a written
    // column, by constraint: each conjunct's key, with the written columns
    // it reads.
    type Conjuncts = Vec<(String, Vec<String>)>;
    let mut unreduced: Vec<(String, Conjuncts)> = Vec::new();
    for (column, values) in written {
        let (Some(old), Some(new)) = (
            attribute(old_catalog, relids.0, column),
            attribute(new_catalog, relids.1, column),
        ) else {
            continue;
        };
        let values: Vec<&Value<'_>> = values
            .iter()
            .filter(|value| old.atttypid == new.atttypid || !matches!(value, Value::Input { .. }))
            .collect();
        if values.is_empty() {
            continue;
        }
        let subject = |what: &str| format!("write {name} {column} {what}");
        let column_text = format!("{statement} writes column {}", quote_identifier(column));
        let new_limit = new_catalog.lint_value_limit(new.atttypid, new.atttypmod);
        if let Some(limit) = new_limit
            && let Some(text) = narrower_type(
                old_catalog.lint_value_limit(old.atttypid, old.atttypmod),
                new_limit,
            )
            && values
                .iter()
                .any(|value| !value.fits(limit, new.atttypid, new_catalog))
        {
            out.push(Change {
                subject: subject("typmod"),
                text: format!(
                    "{column_text}, now {} instead of {}: {text}",
                    new_catalog.lint_format_type(new.atttypid, new.atttypmod),
                    old_catalog.lint_format_type(old.atttypid, old.atttypmod),
                ),
            });
        }
        // A NOT NULL a parameter breaks is its own change.
        if new.attnotnull {
            let default_dropped = values.iter().any(|value| value.null_only_now());
            let text = if default_dropped {
                Some(format!(
                    "{column_text} as DEFAULT, which is now NULL, the column having no default, \
                     while it is NOT NULL: writing NULL fails"
                ))
            } else if old.attnotnull {
                None
            } else if values
                .iter()
                .any(|value| matches!(value, Value::Null { default: Some(_) }))
            {
                Some(format!(
                    "{column_text}, now NOT NULL, as DEFAULT, which is NULL, the column having \
                     no default: writing NULL fails"
                ))
            } else if values.iter().any(|value| {
                matches!(
                    value,
                    Value::Expression { nullable: true, .. } | Value::Null { .. }
                )
            }) {
                Some(format!(
                    "{column_text}, now NOT NULL, with a value that may be NULL: writing NULL \
                     fails"
                ))
            } else {
                None
            };
            if let Some(text) = text {
                out.push(Change {
                    subject: subject("not null"),
                    text,
                });
            }
        }

        let old_constraints = old_catalog.column_write_constraints(relids.0, column);
        let new_constraints = new_catalog.column_write_constraints(relids.1, column);
        let known: HashSet<&str> = old_constraints
            .iter()
            .flat_map(|c| &c.parts)
            .map(|part| part.key.as_str())
            .collect();
        let all = |constraints: &[typedpg_analyzer::WriteConstraint]| {
            constraints
                .iter()
                .filter_map(|c| c.allowed.as_ref())
                .fold(Refinement::NONE, |all, allowed| all.and(allowed))
        };
        let (old_allowed, new_allowed) = (all(&old_constraints), all(&new_constraints));
        // What the new conjuncts allow, and the constraints they are of.
        let mut allowed = Refinement::NONE;
        let mut limiting: Vec<&str> = Vec::new();
        let mut non_null: Vec<&str> = Vec::new();
        for constraint in &new_constraints {
            for part in &constraint.parts {
                if known.contains(part.key.as_str()) {
                    continue;
                }
                match &part.reading {
                    CheckReading::Allows(r) => {
                        allowed = allowed.and(r);
                        if !limiting.contains(&constraint.source.as_str()) {
                            limiting.push(&constraint.source);
                        }
                    }
                    CheckReading::NotNull => {
                        if !non_null.contains(&constraint.source.as_str()) {
                            non_null.push(&constraint.source);
                        }
                    }
                    // A conjunct of the constraint that reads other columns
                    // only is theirs.
                    CheckReading::Other if part.columns.contains(column) => {
                        let parts =
                            match unreduced.iter().position(|(s, _)| *s == constraint.source) {
                                Some(i) => &mut unreduced[i].1,
                                None => {
                                    unreduced.push((constraint.source.clone(), Vec::new()));
                                    &mut unreduced.last_mut().expect("just pushed").1
                                }
                            };
                        match parts.iter_mut().find(|(key, _)| *key == part.key) {
                            Some((_, columns)) if columns.contains(column) => {}
                            Some((_, columns)) => columns.push(column.clone()),
                            None => parts.push((part.key.clone(), vec![column.clone()])),
                        }
                    }
                    CheckReading::Other => {}
                }
            }
        }
        if !limiting.is_empty()
            && allowed.widenings(&old_allowed).any()
            && values
                .iter()
                .any(|value| !value.within(&allowed, new.atttypid, new_catalog))
        {
            let now = new_catalog
                .describe_column_values(relids.1, column, &new_allowed)
                .unwrap_or_else(|| "fewer values".to_owned());
            let before = old_catalog
                .describe_column_values(relids.0, column, &old_allowed)
                .map(|before| format!(" instead of {before}"))
                .unwrap_or_default();
            out.push(Change {
                subject: subject("check"),
                text: format!(
                    "{column_text}, which {} now {} to {now}{before}: writing another value fails",
                    limiting.join(" and "),
                    if limiting.len() == 1 {
                        "limits it"
                    } else {
                        "limit it"
                    },
                ),
            });
        }
        if !non_null.is_empty() && values.iter().any(|value| value.may_be_null()) {
            out.push(Change {
                subject: subject("check null"),
                text: format!(
                    "{column_text}, which {} now {} non-NULL, with a value that may be NULL: \
                     writing NULL fails",
                    non_null.join(" and "),
                    if non_null.len() == 1 {
                        "requires to be"
                    } else {
                        "require to be"
                    },
                ),
            });
        }
    }
    for (source, parts) in unreduced {
        // What each conjunct makes of the rows: rejected when one rejects
        // a row, silent when every one passes every row.
        let mut rejects = false;
        let mut unknown = false;
        let mut columns: Vec<&str> = Vec::new();
        for (key, part_columns) in &parts {
            for column in part_columns {
                if !columns.contains(&column.as_str()) {
                    columns.push(column);
                }
            }
            match part_outcome(relids.1, key, part_columns, rows, new_catalog) {
                PartOutcome::Fails => rejects = true,
                PartOutcome::Unknown => unknown = true,
                PartOutcome::Passes => {}
            }
        }
        if !rejects && !unknown {
            continue;
        }
        let one = columns.len() == 1;
        let list: Vec<String> = columns.iter().map(|c| quote_identifier(c)).collect();
        let list = format!(
            "{} {}",
            if one { "column" } else { "columns" },
            list.join(", ")
        );
        out.push(Change {
            subject: format!("write {name} {source}"),
            text: if rejects {
                format!(
                    "{statement} writes {list} with {} the new {source} rejects: writing {} fails",
                    if one { "a value" } else { "values" },
                    if one { "it" } else { "them" },
                )
            } else {
                format!(
                    "{statement} writes {list}, which the new {source} reads: it may reject the \
                     rows the deployed code writes"
                )
            },
        });
    }
}

/// What the CHECK conjunct `key` of relation `relid` (of `catalog`), which
/// reads the written columns `columns`, makes of the target lists `rows`
/// writing one of them: `Fails` when one of them fails it, `Passes` when
/// every one passes it, else `Unknown` (and when none writes them).
fn part_outcome(
    relid: PgClassOid,
    key: &str,
    columns: &[String],
    rows: &[Row<'_>],
    catalog: &PgCatalog,
) -> PartOutcome {
    let Some(column) = columns.first() else {
        return PartOutcome::Unknown;
    };
    let mut outcome = None;
    for (update, values) in rows {
        if !columns.iter().any(|c| values.contains_key(c)) {
            continue;
        }
        match catalog.check_part_outcome(relid, column, key, values, *update) {
            PartOutcome::Fails => return PartOutcome::Fails,
            PartOutcome::Unknown => outcome = Some(PartOutcome::Unknown),
            PartOutcome::Passes => {
                outcome.get_or_insert(PartOutcome::Passes);
            }
        }
    }
    outcome.unwrap_or(PartOutcome::Unknown)
}

/// What a column's type modifier going from `old` to `new` rejects or
/// changes in the values written into it; `None` when every value `old`
/// accepts, `new` keeps as is.
fn narrower_type(
    old: Option<typedpg_analyzer::ValueLimit>,
    new: Option<typedpg_analyzer::ValueLimit>,
) -> Option<String> {
    use typedpg_analyzer::ValueLimit::{ExactLength, Length, Numeric};
    match (old, new?) {
        (None, Length(_)) => Some("a longer value fails".to_owned()),
        (Some(Length(a)), Length(b)) if b < a => Some("a longer value fails".to_owned()),
        // A bit string of the old length no longer fits, longer or shorter.
        (Some(ExactLength(a)), ExactLength(b)) if a != b => {
            Some("a value of the old length fails".to_owned())
        }
        (old, Numeric { precision, scale }) => {
            let (old_digits, old_scale) = match old {
                Some(Numeric {
                    precision: p,
                    scale: s,
                }) => (Some(p - s), Some(s)),
                _ => (None, None),
            };
            let digits = precision - scale;
            let plural = |n: i32| if n == 1 { "digit" } else { "digits" };
            let mut parts = Vec::new();
            if old_digits.is_none_or(|old| digits < old) {
                parts.push(format!(
                    "a value with more than {} {} before the decimal point fails",
                    digits.max(0),
                    plural(digits)
                ));
            }
            if old_scale.is_none_or(|old| scale < old) {
                parts.push(format!(
                    "one with more than {} {} after it is rounded",
                    scale.max(0),
                    plural(scale)
                ));
            }
            (!parts.is_empty()).then(|| parts.join(", and "))
        }
        _ => None,
    }
}

/// `ty` without what the deployed code's decoding doesn't depend on: the
/// type modifiers and collations (the same wire type), enum labels and array
/// element nullability (reported on their own).
fn shape(ty: &Type) -> Type {
    match ty {
        Type::Basic {
            schema,
            name,
            extension,
            ..
        } => Type::Basic {
            schema: schema.clone(),
            name: name.clone(),
            extension: extension.clone(),
            typmod: None,
            collation: None,
        },
        Type::Domain {
            schema,
            name,
            base,
            extension,
            ..
        } => Type::Domain {
            schema: schema.clone(),
            name: name.clone(),
            base: Box::new(shape(base)),
            extension: extension.clone(),
            typmod: None,
            collation: None,
        },
        Type::Array { element, .. } => Type::Array {
            element: Box::new(shape(element)),
            element_nullable: None,
        },
        Type::Enum {
            schema,
            name,
            extension,
            ..
        } => Type::Enum {
            schema: schema.clone(),
            name: name.clone(),
            labels: Vec::new(),
            extension: extension.clone(),
        },
        Type::Range {
            schema,
            name,
            subtype,
            extension,
            multirange,
            ..
        } => Type::Range {
            schema: schema.clone(),
            name: name.clone(),
            subtype: Box::new(shape(subtype)),
            extension: extension.clone(),
            typmod: None,
            multirange: *multirange,
        },
        Type::Composite {
            schema,
            name,
            fields,
            extension,
        } => Type::Composite {
            schema: schema.clone(),
            name: name.clone(),
            fields: shape_fields(fields),
            extension: extension.clone(),
        },
        Type::AnonymousRecord { fields } => Type::AnonymousRecord {
            fields: shape_fields(fields),
        },
    }
}

fn shape_fields(fields: &[typedpg_analyzer::RecordField]) -> Vec<typedpg_analyzer::RecordField> {
    fields
        .iter()
        .map(|field| typedpg_analyzer::RecordField {
            name: field.name.clone(),
            ty: shape(&field.ty),
            nullable: false,
            refinement: Refinement::NONE,
            elem_refinement: Refinement::NONE,
        })
        .collect()
}

/// The enum labels `new` has that `old` doesn't, of an enum, an array of
/// one, or a domain over either.
fn added_labels(old: &Type, new: &Type) -> BTreeSet<String> {
    match (old, new) {
        (Type::Enum { labels: a, .. }, Type::Enum { labels: b, .. }) => b
            .iter()
            .filter(|label| !a.contains(label))
            .cloned()
            .collect(),
        (Type::Array { element: a, .. }, Type::Array { element: b, .. }) => added_labels(a, b),
        (Type::Domain { base: a, .. }, Type::Domain { base: b, .. }) => added_labels(a, b),
        _ => BTreeSet::new(),
    }
}

/// An array type's element type and element nullability.
fn array_element(ty: &Type) -> Option<(&Type, Option<bool>)> {
    match ty {
        Type::Array {
            element,
            element_nullable,
        } => Some((element, *element_nullable)),
        Type::Domain { base, .. } => array_element(base),
        _ => None,
    }
}

/// How a value refined as `old` may now break it, refined as `new`, worded
/// after the column's name; `None` when every value of `new` is one of
/// `old`.
fn widened(old: &Refinement, new: &Refinement) -> Option<String> {
    let widenings = old.widenings(new);
    if !widenings.any() {
        return None;
    }
    let mut parts = Vec::new();
    if widenings.values {
        let known = old.values.clone().unwrap_or_default();
        match new.enumerated() {
            Some(values) => {
                let added: BTreeSet<String> = values.difference(&known).cloned().collect();
                parts.push(format!(
                    "may now be {}, a value the deployed code does not know",
                    quoted(&added)
                ));
            }
            None => parts.push(format!("may now be another value than {}", quoted(&known))),
        }
    }
    if widenings.range
        && let Some(range) = old.range
    {
        parts.push(format!("may now lie outside {}", int_range(range)));
    }
    if widenings.bounds {
        parts.push("may now lie outside the bounds it had".to_owned());
    }
    if widenings.finite {
        parts.push("may now be infinite".to_owned());
    }
    Some(parts.join(" and "))
}

/// An integer range as the message says it.
fn int_range(range: IntRange) -> String {
    match (range.lo, range.hi) {
        (Some(lo), Some(hi)) => format!("{lo}..={hi}"),
        (Some(lo), None) => format!("{lo}.."),
        (None, Some(hi)) => format!("..={hi}"),
        (None, None) => "..".to_owned(),
    }
}

/// Values as the message lists them: `'a', 'b'`.
fn quoted(values: &BTreeSet<String>) -> String {
    values
        .iter()
        .map(|value| format!("'{}'", value.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog(migrations: &[&str]) -> PgCatalog {
        let mut catalog = PgCatalog::new().unwrap();
        for (i, sql) in migrations.iter().enumerate() {
            catalog
                .apply_migration(&format!("{i:04}.sql"), sql)
                .unwrap();
        }
        catalog
    }

    /// The findings for `queries` when `new` is applied after `old`.
    fn check(old: &[&str], new: &[&str], queries: &[&str]) -> Vec<MigrationFinding> {
        check_as(old, new, queries, &[])
    }

    /// [`check`], for queries the deployed code runs as `roles`.
    fn check_as(
        old: &[&str],
        new: &[&str],
        queries: &[&str],
        roles: &[&str],
    ) -> Vec<MigrationFinding> {
        check_kind(old, new, queries, roles, DeployedKind::Query)
    }

    /// [`check`], for deployed `copy_in!` targets.
    fn check_copy(old: &[&str], new: &[&str], targets: &[&str]) -> Vec<MigrationFinding> {
        check_kind(old, new, targets, &[], DeployedKind::CopyIn)
    }

    fn check_kind(
        old: &[&str],
        new: &[&str],
        queries: &[&str],
        roles: &[&str],
        kind: DeployedKind,
    ) -> Vec<MigrationFinding> {
        let old_catalog = catalog(old);
        let all: Vec<&str> = old.iter().chain(new).copied().collect();
        let new_catalog = catalog(&all);
        let files: Vec<MigrationFile> = all
            .iter()
            .enumerate()
            .map(|(i, sql)| MigrationFile {
                path: format!("{i:04}.sql"),
                sql: (*sql).to_owned(),
                lint: MigrationLint::All,
            })
            .collect();
        let queries: Vec<DeployedQuery> = queries
            .iter()
            .enumerate()
            .map(|(i, sql)| DeployedQuery {
                sql: (*sql).to_owned(),
                locations: vec![format!("src/q.rs:{}", i + 1)],
                roles: roles.iter().map(|role| (*role).to_owned()).collect(),
                kind,
            })
            .collect();
        breaks_deployed_queries(
            &Deployment {
                reference: "origin/main",
                catalog: &old_catalog,
                queries: &queries,
            },
            &NewSchema {
                files: &files,
                first_new: old.len(),
                use_transaction: true,
                catalog: &new_catalog,
                prepare: &|_| Ok(()),
            },
            &LintOptions::default(),
        )
    }

    fn messages(findings: &[MigrationFinding]) -> Vec<String> {
        findings.iter().map(|f| f.finding.message.clone()).collect()
    }

    const USERS: &str = "CREATE TYPE status AS ENUM ('active', 'blocked');
        CREATE TABLE users (
            id integer PRIMARY KEY,
            email text NOT NULL,
            name text NOT NULL,
            status status NOT NULL,
            kind text NOT NULL CHECK (kind IN ('a', 'b')),
            age integer,
            tags text[] NOT NULL CHECK (array_position(tags, NULL) IS NULL)
        );";

    #[test]
    fn a_dropped_column_breaks_a_query_that_reads_it() {
        let findings = check(
            &[USERS],
            &["SELECT 1;\nALTER TABLE users DROP COLUMN email;"],
            &["SELECT email FROM users WHERE id = $id"],
        );
        assert_eq!(findings.len(), 1, "{findings:?}");
        let finding = &findings[0];
        assert_eq!(finding.file, 1);
        assert_eq!(finding.finding.rule, Rule::BreaksDeployedQueries);
        // Placed on the statement that drops it, not the first one.
        assert_eq!(
            &"SELECT 1;\nALTER TABLE users DROP COLUMN email;"[finding.finding.span.clone()],
            "ALTER TABLE users DROP COLUMN email"
        );
        assert_eq!(
            finding.finding.message,
            "this migration breaks the deployed query at src/q.rs:1 (origin/main): it no longer \
             runs: column \"email\" does not exist"
        );
        assert!(
            finding
                .finding
                .help
                .as_deref()
                .unwrap()
                .contains("expand/contract")
        );
    }

    #[test]
    fn result_column_changes() {
        let findings = check(
            &[USERS],
            &["ALTER TABLE users ALTER COLUMN name DROP NOT NULL;
               ALTER TABLE users ALTER COLUMN id TYPE bigint;
               ALTER TYPE status ADD VALUE 'deleted';
               ALTER TABLE users DROP CONSTRAINT users_kind_check;
               ALTER TABLE users ADD CONSTRAINT users_kind_check CHECK (kind IN ('a', 'b', 'c'));"],
            &["SELECT id, name, status, kind FROM users"],
        );
        let messages = messages(&findings);
        let all = messages.join("\n");
        assert!(
            all.contains("result column \"name\" may now be NULL"),
            "{all}"
        );
        assert!(
            all.contains("result column \"id\" changes from integer to bigint"),
            "{all}"
        );
        assert!(
            all.contains(
                "result column \"status\" may now be 'deleted', a value the deployed code does not \
                 know"
            ),
            "{all}"
        );
        assert!(
            all.contains(
                "result column \"kind\" may now be 'c', a value the deployed code does not know"
            ),
            "{all}"
        );
        // One finding per statement that breaks the query.
        assert_eq!(findings.len(), 4, "{all}");
    }

    #[test]
    fn select_star_reports_a_new_column() {
        let findings = check(
            &["CREATE TABLE t (a integer NOT NULL);"],
            &["ALTER TABLE t ADD COLUMN b integer;"],
            &["SELECT * FROM t", "SELECT a FROM t"],
        );
        assert_eq!(
            messages(&findings),
            [
                "this migration breaks the deployed query at src/q.rs:1 (origin/main): its result \
              columns become (a, b) instead of (a), and the deployed code checks them"
            ]
        );
    }

    #[test]
    fn parameter_changes() {
        let findings = check(
            &["CREATE TABLE t (a integer, b text);"],
            &["ALTER TABLE t ALTER COLUMN a TYPE bigint;
               ALTER TABLE t ALTER COLUMN b SET NOT NULL;"],
            &["UPDATE t SET b = $b WHERE a = $a"],
        );
        let all = messages(&findings).join("\n");
        assert!(
            all.contains("parameter $a changes from integer to bigint"),
            "{all}"
        );
        assert!(all.contains("parameter $b no longer accepts NULL"), "{all}");
    }

    #[test]
    fn an_insert_leaving_out_a_new_not_null_column() {
        let old = "CREATE TABLE t (a integer NOT NULL, b integer);
                   CREATE TABLE u (x integer NOT NULL);";
        // The analysis rejects an INSERT whose rows all leave the column
        // NULL...
        for (new, sql) in [
            (
                "ALTER TABLE t ADD COLUMN c integer NOT NULL;",
                "INSERT INTO t (a) VALUES ($a)",
            ),
            (
                "ALTER TABLE t ALTER COLUMN b SET NOT NULL;",
                "INSERT INTO t VALUES (1)",
            ),
            (
                "ALTER TABLE t ALTER COLUMN b SET NOT NULL;",
                "INSERT INTO t (a) SELECT 1",
            ),
        ] {
            let findings = check(&[old], &[new], &[sql]);
            let all = messages(&findings).join("\n");
            assert_eq!(findings.len(), 1, "{sql}: {all}");
            assert!(all.contains("violates not-null constraint"), "{all}");
        }
        // ...but not one that may insert no row: the rule reports it.
        for (new, sql) in [
            (
                "ALTER TABLE t ADD COLUMN c integer NOT NULL;",
                "INSERT INTO t (a) SELECT x FROM u",
            ),
            (
                "ALTER TABLE t ALTER COLUMN b SET NOT NULL;",
                "INSERT INTO t SELECT x FROM u",
            ),
        ] {
            let findings = check(&[old], &[new], &[sql]);
            let all = messages(&findings).join("\n");
            assert_eq!(findings.len(), 1, "{sql}: {all}");
            assert!(all.contains("its INSERT into t leaves out column"), "{all}");
            assert!(
                all.contains("now NOT NULL without a default, so inserting any row fails"),
                "{all}"
            );
        }
        // A default, an identity, or a column the INSERT gives: no finding.
        for (new, sql) in [
            (
                "ALTER TABLE t ADD COLUMN c integer NOT NULL DEFAULT 0;",
                "INSERT INTO t (a) SELECT x FROM u",
            ),
            (
                "ALTER TABLE t ADD COLUMN c integer GENERATED ALWAYS AS IDENTITY;",
                "INSERT INTO t (a) SELECT x FROM u",
            ),
            (
                "ALTER TABLE t ALTER COLUMN b SET NOT NULL;",
                "INSERT INTO t (a, b) SELECT x, x FROM u",
            ),
            // NOT NULL without a default before too: not this migration's.
            (
                "ALTER TABLE u ADD COLUMN y integer;",
                "INSERT INTO u SELECT a FROM t WHERE false",
            ),
        ] {
            let findings = check(&[old], &[new], &[sql]);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }

    #[test]
    fn a_compatible_migration_reports_nothing() {
        let findings = check(
            &[USERS],
            &["ALTER TABLE users ADD COLUMN bio text;
               CREATE INDEX users_email_idx ON users (email);
               ALTER TABLE users ADD CONSTRAINT users_kind_c CHECK (kind IN ('a', 'b', 'c'));
               CREATE TABLE posts (id integer PRIMARY KEY);"],
            &[
                "SELECT id, email, name, status, kind, age, tags FROM users WHERE id = $id",
                "INSERT INTO users (id, email, name, status, kind, age, tags) \
                 VALUES ($id, $email, $name, $status, $kind, $age, $tags)",
                "SELECT nope FROM users",
            ],
        );
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn allow_comments_suppress_it() {
        let findings = check(
            &[USERS],
            &[
                "-- typedpg-lint: allow(breaks_deployed_queries)\nALTER TABLE users DROP COLUMN email;",
            ],
            &["SELECT email FROM users"],
        );
        assert!(findings.is_empty(), "{findings:?}");
        let findings = check(
            &[USERS],
            &["ALTER TABLE users DROP COLUMN email;"],
            &["-- typedpg-lint: allow(breaks_deployed_queries)\nSELECT email FROM users"],
        );
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn element_and_refinement_widenings() {
        let findings = check(
            &[USERS],
            &["ALTER TABLE users DROP CONSTRAINT users_tags_check;"],
            &["SELECT tags FROM users"],
        );
        assert_eq!(
            messages(&findings),
            [
                "this migration breaks the deployed query at src/q.rs:1 (origin/main): the \
              elements of result column \"tags\" may now be NULL"
            ]
        );
    }

    #[test]
    fn record_field_widenings() {
        let findings = check(
            &[
                "CREATE TABLE r (kind text NOT NULL CHECK (kind IN ('a', 'b')), n integer NOT NULL);",
            ],
            &["ALTER TABLE r DROP CONSTRAINT r_kind_check, ALTER COLUMN n DROP NOT NULL;"],
            &["SELECT r FROM r"],
        );
        assert_eq!(
            messages(&findings),
            [
                "this migration breaks the deployed query at src/q.rs:1 (origin/main): field \
                 \"kind\" of result column \"r\" may now be another value than 'a', 'b'; field \
                 \"n\" of result column \"r\" may now be NULL"
            ]
        );
    }

    /// The one message of `findings`, without what every message starts
    /// with.
    fn only(findings: &[MigrationFinding]) -> String {
        let messages = messages(findings);
        assert_eq!(messages.len(), 1, "{messages:?}");
        messages[0]
            .split_once("(origin/main): ")
            .map(|(_, text)| text.to_owned())
            .unwrap()
    }

    const LIMITS: &str = "CREATE TABLE t (
            id integer PRIMARY KEY,
            name varchar(20),
            code char(4),
            flags bit varying(8),
            n numeric(10, 2),
            m numeric(10, 2),
            at timestamp(6),
            lo integer,
            hi integer,
            age integer
        );";

    #[test]
    fn a_narrower_length_breaks_a_write() {
        let findings = check(
            &[LIMITS],
            &["ALTER TABLE t ALTER COLUMN name TYPE varchar(10);"],
            &[
                "INSERT INTO t (id, name) VALUES ($id, $name)",
                "UPDATE t SET name = $name WHERE id = $id",
                // Reading it doesn't break: the type modifier doesn't
                // change the decoding.
                "SELECT name FROM t",
            ],
        );
        assert_eq!(
            messages(&findings),
            [
                "this migration breaks the deployed query at src/q.rs:1 (origin/main): its INSERT \
                 into t writes column name, now character varying(10) instead of character \
                 varying(20): a longer value fails",
                "this migration breaks the deployed query at src/q.rs:2 (origin/main): its UPDATE \
                 of t writes column name, now character varying(10) instead of character \
                 varying(20): a longer value fails",
            ]
        );
        // Newly limited, char(n) and bit strings, an ON CONFLICT DO UPDATE.
        for (new, sql) in [
            (
                "ALTER TABLE t ALTER COLUMN code TYPE char(2);",
                "UPDATE t SET code = $code",
            ),
            (
                "ALTER TABLE t ALTER COLUMN flags TYPE bit varying(4);",
                "UPDATE t SET flags = $flags",
            ),
            (
                "ALTER TABLE t ALTER COLUMN name TYPE varchar(10);",
                "INSERT INTO t (id) VALUES ($id) ON CONFLICT (id) DO UPDATE SET name = $name",
            ),
        ] {
            let text = only(&check(&[LIMITS], &[new], &[sql]));
            assert!(text.ends_with(": a longer value fails"), "{sql}: {text}");
        }
        let text = only(&check(
            &["CREATE TABLE u (id integer, name varchar);"],
            &["ALTER TABLE u ALTER COLUMN name TYPE varchar(5);"],
            &["INSERT INTO u VALUES ($id, $name)"],
        ));
        assert_eq!(
            text,
            "its INSERT into u writes column name, now character varying(5) instead of \
             character varying: a longer value fails"
        );
        // A bit(n) takes exactly n bits: a wider one breaks too.
        let text = only(&check(
            &["CREATE TABLE b (id integer, mask bit(4));"],
            &["ALTER TABLE b ALTER COLUMN mask TYPE bit(8);"],
            &["UPDATE b SET mask = $mask"],
        ));
        assert_eq!(
            text,
            "its UPDATE of b writes column mask, now bit(8) instead of bit(4): a value of the old \
             length fails"
        );
        // A wider one, a timestamp's precision, more digits: nothing.
        for new in [
            "ALTER TABLE t ALTER COLUMN name TYPE varchar(40);",
            "ALTER TABLE t ALTER COLUMN at TYPE timestamp(3);",
            "ALTER TABLE t ALTER COLUMN n TYPE numeric(12, 2);",
        ] {
            let findings = check(
                &[LIMITS],
                &[new],
                &["UPDATE t SET name = $name, at = $at, n = $n"],
            );
            assert!(findings.is_empty(), "{new}: {findings:?}");
        }
    }

    #[test]
    fn a_narrower_numeric_overflows_or_rounds() {
        let findings = check(
            &[LIMITS],
            &[
                "ALTER TABLE t ALTER COLUMN n TYPE numeric(8, 2);",
                "ALTER TABLE t ALTER COLUMN m TYPE numeric(10, 1);",
            ],
            &["UPDATE t SET n = $n, m = $m WHERE id = $id"],
        );
        assert_eq!(
            messages(&findings),
            [
                "this migration breaks the deployed query at src/q.rs:1 (origin/main): its UPDATE \
                 of t writes column n, now numeric(8,2) instead of numeric(10,2): a value with \
                 more than 6 digits before the decimal point fails",
                "this migration breaks the deployed query at src/q.rs:1 (origin/main): its UPDATE \
                 of t writes column m, now numeric(10,1) instead of numeric(10,2): one with more \
                 than 1 digit after it is rounded",
            ]
        );
        assert_eq!(findings[0].file, 1);
        assert_eq!(findings[1].file, 2);
    }

    #[test]
    fn a_new_check_on_a_written_column() {
        let findings = check(
            &[USERS],
            &["ALTER TABLE users ADD CONSTRAINT users_kind_a CHECK (kind = 'a');"],
            &[
                "INSERT INTO users (id, email, name, status, kind, age, tags) \
                 VALUES ($id, $email, $name, $status, $kind, $age, $tags)",
                // It doesn't write the column.
                "UPDATE users SET name = $name WHERE id = $id",
            ],
        );
        assert_eq!(
            messages(&findings),
            [
                "this migration breaks the deployed query at src/q.rs:1 (origin/main): its INSERT \
                 into users writes column kind, which CHECK constraint \"users_kind_a\" now limits \
                 it to 'a' instead of one of 'a', 'b': writing another value fails"
            ]
        );
    }

    #[test]
    fn a_narrowed_check() {
        let migration = "ALTER TABLE users DROP CONSTRAINT users_kind_check;\n\
                         ALTER TABLE users ADD CONSTRAINT users_kind_check CHECK (kind IN ('b'));";
        let findings = check(&[USERS], &[migration], &["UPDATE users SET kind = $kind"]);
        assert_eq!(
            only(&findings),
            "its UPDATE of users writes column kind, which CHECK constraint \"users_kind_check\" \
             now limits it to 'b' instead of one of 'a', 'b': writing another value fails"
        );
        // Placed on the statement that narrows it.
        assert_eq!(
            &migration[findings[0].finding.span.clone()],
            "ALTER TABLE users ADD CONSTRAINT users_kind_check CHECK (kind IN ('b'))"
        );
    }

    #[test]
    fn a_not_valid_check_binds_the_rows_written() {
        let findings = check(
            &[LIMITS],
            &["ALTER TABLE t ADD CONSTRAINT t_age_positive CHECK (age > 0) NOT VALID;"],
            &["UPDATE t SET age = $age WHERE id = $id"],
        );
        assert_eq!(
            only(&findings),
            "its UPDATE of t writes column age, which CHECK constraint \"t_age_positive\" now \
             limits it to at least 1: writing another value fails"
        );
    }

    #[test]
    fn a_domain_check() {
        let findings = check(
            &["CREATE DOMAIN score AS integer CHECK (VALUE >= 0);
               CREATE TABLE s (id integer, v score);"],
            &["ALTER DOMAIN score ADD CONSTRAINT score_max CHECK (VALUE <= 100);"],
            // No column list: it writes every column.
            &["INSERT INTO s VALUES ($id, $v)"],
        );
        assert_eq!(
            only(&findings),
            "its INSERT into s writes column v, which CHECK constraint \"score_max\" of domain \
             \"score\" now limits it to between 0 and 100 instead of at least 0: writing another \
             value fails"
        );
    }

    #[test]
    fn a_new_multi_column_check_may_reject_the_rows() {
        let findings = check(
            &[LIMITS],
            &["ALTER TABLE t ADD CONSTRAINT t_lo_hi CHECK (lo <= hi);"],
            &[
                "INSERT INTO t (id, lo, hi) VALUES ($id, $lo, $hi)",
                "UPDATE t SET lo = $lo WHERE id = $id",
                // It writes neither column.
                "UPDATE t SET name = $name WHERE id = $id",
            ],
        );
        assert_eq!(
            messages(&findings),
            [
                "this migration breaks the deployed query at src/q.rs:1 (origin/main): its INSERT \
                 into t writes columns lo, hi, which the new CHECK constraint \"t_lo_hi\" reads: \
                 it may reject the rows the deployed code writes",
                "this migration breaks the deployed query at src/q.rs:2 (origin/main): its UPDATE \
                 of t writes column lo, which the new CHECK constraint \"t_lo_hi\" reads: it may \
                 reject the rows the deployed code writes",
            ]
        );
    }

    #[test]
    fn a_new_unique_constraint_is_not_reported() {
        let findings = check(
            &[USERS],
            &[
                "ALTER TABLE users ADD CONSTRAINT users_email_key UNIQUE (email);
               CREATE UNIQUE INDEX users_name_key ON users (name);",
            ],
            &[
                "INSERT INTO users (id, email, name, status, kind, age, tags) \
                 VALUES ($id, $email, $name, $status, $kind, $age, $tags)",
            ],
        );
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn a_query_is_analyzed_as_its_roles() {
        let old = "CREATE ROLE app; CREATE ROLE reporting;
                   CREATE TABLE t (id integer PRIMARY KEY, secret text);
                   GRANT SELECT, UPDATE ON t TO app, reporting;";
        let revoke = ["REVOKE SELECT ON t FROM app;"];
        let sql = "SELECT id FROM t";
        // A revoked privilege breaks the query for the role that runs it...
        assert_eq!(
            only(&check_as(&[old], &revoke, &[sql], &["app"])),
            "it no longer runs as role \"app\": permission denied for table t"
        );
        assert_eq!(
            only(&check_as(&[old], &revoke, &[sql], &["reporting", "app"])),
            "it no longer runs as role \"app\": permission denied for table t"
        );
        // ...not for another role, nor without one.
        assert!(check_as(&[old], &revoke, &[sql], &["reporting"]).is_empty());
        assert!(check(&[old], &revoke, &[sql]).is_empty());
        // Row security without a policy for the role: the query still
        // analyzes, but sees no row, or changes none.
        let rls = ["ALTER TABLE t ENABLE ROW LEVEL SECURITY;
             CREATE POLICY t_reporting ON t FOR SELECT TO reporting USING (true);"];
        assert_eq!(
            only(&check_as(
                &[old],
                &rls,
                &["UPDATE t SET secret = $secret WHERE id = $id RETURNING id"],
                &["app"],
            )),
            "its UPDATE of t changes no row as role \"app\": row-level security applies to the \
             role and no policy lets it UPDATE"
        );
        let findings = check_as(&[old], &rls, &[sql], &["app", "reporting"]);
        assert_eq!(
            only(&findings),
            "it sees no row of t as role \"app\": row-level security applies to the role and no \
             policy lets it SELECT"
        );
        assert!(check_as(&[old], &rls, &[sql], &["reporting"]).is_empty());
        // A schema change breaks every role alike: said once, of no role.
        assert_eq!(
            only(&check_as(
                &[old],
                &["ALTER TABLE t DROP COLUMN secret;"],
                &["SELECT secret FROM t"],
                &["app", "reporting"],
            )),
            "it no longer runs: column \"secret\" does not exist"
        );
    }

    #[test]
    fn a_constant_that_fits_is_not_reported() {
        let check = "ALTER TABLE t ADD CONSTRAINT t_age_positive CHECK (age > 0);";
        // A constant, a CASE of them, a column whose CHECK keeps it in
        // range: each fits.
        for sql in [
            "UPDATE t SET age = 5 WHERE id = $id",
            "INSERT INTO t (id, age) VALUES ($id, 1), ($id2, 7)",
            "UPDATE t SET age = CASE WHEN $flag THEN 1 ELSE 2 END WHERE id = $id",
            "INSERT INTO t (id, age) SELECT id, n FROM u",
        ] {
            let findings = check_both(&[LIMITS, U], &[check], &[sql]);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
        // One that may not: a parameter, a CASE with a value outside, a
        // column without the bounds.
        for sql in [
            "UPDATE t SET age = $age WHERE id = $id",
            "UPDATE t SET age = CASE WHEN $flag THEN 1 ELSE 0 END WHERE id = $id",
            "INSERT INTO t (id, age) SELECT id, id FROM u",
        ] {
            let text = only(&check_both(&[LIMITS, U], &[check], &[sql]));
            assert!(
                text.contains(
                    "writes column age, which CHECK constraint \"t_age_positive\" now limits it to \
                     at least 1"
                ),
                "{sql}: {text}"
            );
        }
        // A type modifier: a short constant, a value of a type no wider.
        let narrower = "ALTER TABLE t ALTER COLUMN name TYPE varchar(10), \
                        ALTER COLUMN n TYPE numeric(8, 2);";
        for sql in [
            "UPDATE t SET name = 'short', n = 12.5 WHERE id = $id",
            "INSERT INTO t (id, name, n) SELECT id, code, n FROM u",
            "UPDATE t SET name = CASE WHEN $flag THEN 'a' ELSE 'bb' END",
        ] {
            let findings = check_both(&[LIMITS, U], &[narrower], &[sql]);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
        let text = only(&check_both(
            &[LIMITS, U],
            &[narrower],
            &["INSERT INTO t (id, name) SELECT id, label FROM u"],
        ));
        assert!(text.ends_with(": a longer value fails"), "{text}");
    }

    /// A table a deployed query copies from.
    const U: &str = "CREATE TABLE u (
            id integer PRIMARY KEY,
            n integer NOT NULL CHECK (n BETWEEN 1 AND 99),
            code varchar(4),
            label text
        );";

    /// [`check`], with the migrations `old` one deployed migration.
    fn check_both(old: &[&str], new: &[&str], queries: &[&str]) -> Vec<MigrationFinding> {
        check(&[&old.join("\n")], new, queries)
    }

    #[test]
    fn a_not_null_check_is_reported_once() {
        let migration = ["ALTER TABLE t ADD CONSTRAINT t_age_set CHECK (age IS NOT NULL);"];
        // A parameter the new schema no longer lets be NULL: its change
        // says it, alone.
        assert_eq!(
            only(&check(
                &[LIMITS],
                &["ALTER TABLE t ALTER COLUMN age SET NOT NULL, \
                   ADD CONSTRAINT t_age_set CHECK (age IS NOT NULL);"],
                &["INSERT INTO t (id, age) VALUES ($id, $age)"]
            )),
            "parameter $age no longer accepts NULL"
        );
        assert_eq!(
            only(&check(
                &[LIMITS],
                &migration,
                &["INSERT INTO t (id, age) VALUES ($id, $age)"]
            )),
            "its INSERT into t writes column age, which CHECK constraint \"t_age_set\" now requires \
             to be non-NULL, with a value that may be NULL: writing NULL fails"
        );
        assert_eq!(
            only(&check(
                &[LIMITS],
                &migration,
                &["UPDATE t SET age = $age WHERE id = $id"]
            )),
            "its UPDATE of t writes column age, which CHECK constraint \"t_age_set\" now requires \
             to be non-NULL, with a value that may be NULL: writing NULL fails"
        );
        assert_eq!(
            only(&check(
                &[LIMITS],
                &migration,
                &["UPDATE t SET age = lo WHERE id = $id"]
            )),
            "its UPDATE of t writes column age, which CHECK constraint \"t_age_set\" now requires \
             to be non-NULL, with a value that may be NULL: writing NULL fails"
        );
        assert!(check(&[LIMITS], &migration, &["UPDATE t SET age = 3"]).is_empty());
    }

    #[test]
    fn the_part_of_a_check_the_analyzer_cant_read_may_reject() {
        let migration = ["ALTER TABLE users ADD CONSTRAINT users_kind_name \
                          CHECK (kind IN ('a') AND length(name) > 1);"];
        assert_eq!(
            only(&check(
                &[USERS],
                &migration,
                &["UPDATE users SET kind = 'a', name = $name"]
            )),
            "its UPDATE of users writes column name, which the new CHECK constraint \
             \"users_kind_name\" reads: it may reject the rows the deployed code writes"
        );
        assert_eq!(
            only(&check(
                &[USERS],
                &migration,
                &["UPDATE users SET kind = $kind, name = $name"]
            )),
            "its UPDATE of users writes column kind, which CHECK constraint \"users_kind_name\" \
             now limits it to 'a' instead of one of 'a', 'b': writing another value fails; its \
             UPDATE of users writes column name, which the new CHECK constraint \
             \"users_kind_name\" reads: it may reject the rows the deployed code writes"
        );
        // Neither column written: nothing.
        assert!(check(&[USERS], &migration, &["UPDATE users SET age = $age"]).is_empty());
    }

    #[test]
    fn constraints_are_compared_by_what_they_check() {
        let old = [
            LIMITS,
            "ALTER TABLE t ADD CONSTRAINT t_lo_hi CHECK (lo <= hi);",
        ];
        let sql = ["UPDATE t SET lo = $lo WHERE id = $id"];
        // Renamed, or dropped and added again as it was: nothing changes.
        for migration in [
            "ALTER TABLE t RENAME CONSTRAINT t_lo_hi TO t_bounds;",
            "ALTER TABLE t DROP CONSTRAINT t_lo_hi;
             ALTER TABLE t ADD CONSTRAINT t_lo_hi CHECK (lo <= hi);",
        ] {
            let findings = check_both(&old, &[migration], &sql);
            assert!(findings.is_empty(), "{migration}: {findings:?}");
        }
        // The same name for another expression: a new constraint.
        let text = only(&check_both(
            &old,
            &["ALTER TABLE t DROP CONSTRAINT t_lo_hi;
               ALTER TABLE t ADD CONSTRAINT t_lo_hi CHECK (lo < hi);"],
            &sql,
        ));
        assert_eq!(
            text,
            "its UPDATE of t writes column lo, which the new CHECK constraint \"t_lo_hi\" reads: \
             it may reject the rows the deployed code writes"
        );
    }

    #[test]
    fn merge_writes_are_checked() {
        let findings = check(
            &[LIMITS],
            &["ALTER TABLE t ALTER COLUMN name TYPE varchar(10);"],
            &[
                "MERGE INTO t USING (SELECT $id::integer AS id, $name::text AS name) s \
               ON t.id = s.id \
               WHEN MATCHED THEN UPDATE SET name = s.name \
               WHEN NOT MATCHED THEN INSERT (id, name) VALUES (s.id, s.name)",
            ],
        );
        assert_eq!(
            only(&findings),
            "its MERGE into t writes column name, now character varying(10) instead of character \
             varying(20): a longer value fails"
        );
        // Its constant fits.
        assert!(
            check(
                &[LIMITS],
                &["ALTER TABLE t ALTER COLUMN name TYPE varchar(10);"],
                &[
                    "MERGE INTO t USING (SELECT $id::integer AS id) s ON t.id = s.id \
                   WHEN NOT MATCHED THEN INSERT (id, name) VALUES (s.id, 'x')"
                ],
            )
            .is_empty()
        );
    }

    #[test]
    fn copy_targets_are_checked() {
        let target = ["t (id, name, age)"];
        assert_eq!(
            only(&check_copy(
                &[LIMITS],
                &["ALTER TABLE t ALTER COLUMN name TYPE varchar(10);"],
                &target
            )),
            "its COPY into t writes column name, now character varying(10) instead of character \
             varying(20): a longer value fails"
        );
        assert_eq!(
            only(&check_copy(
                &[LIMITS],
                &["ALTER TABLE t ADD CONSTRAINT t_age_positive CHECK (age > 0);"],
                &target
            )),
            "its COPY into t writes column age, which CHECK constraint \"t_age_positive\" now \
             limits it to at least 1: writing another value fails"
        );
        assert_eq!(
            only(&check_copy(
                &[LIMITS],
                &["ALTER TABLE t ALTER COLUMN age TYPE bigint;"],
                &target
            )),
            "column \"age\" of its COPY into t changes from integer to bigint"
        );
        assert_eq!(
            only(&check_copy(
                &[LIMITS],
                &["ALTER TABLE t ALTER COLUMN age SET NOT NULL;"],
                &target
            )),
            "column \"age\" of its COPY into t no longer accepts NULL"
        );
        assert_eq!(
            only(&check_copy(
                &[LIMITS],
                &["ALTER TABLE t ADD COLUMN extra integer NOT NULL;"],
                &target
            )),
            "its COPY into t leaves out column extra, now NOT NULL without a default, so \
             inserting any row fails"
        );
        // Without a column list it copies every column.
        assert_eq!(
            only(&check_copy(
                &["CREATE TABLE c (a integer, b text);"],
                &["ALTER TABLE c ADD COLUMN x integer;"],
                &["c"]
            )),
            "its COPY into c copies columns (a, b, x) instead of (a, b), the ones the deployed \
             code sends"
        );
        assert_eq!(
            only(&check_copy(
                &[LIMITS],
                &["ALTER TABLE t DROP COLUMN age;"],
                &target
            )),
            "it no longer runs: column \"age\" of relation \"t\" does not exist"
        );
        assert!(
            check_copy(
                &[LIMITS],
                &["ALTER TABLE t ADD COLUMN extra integer;"],
                &target
            )
            .is_empty()
        );
    }

    #[test]
    fn a_value_not_from_a_parameter_meets_a_changed_type() {
        let old = [LIMITS, U];
        let domain = "CREATE DOMAIN positive AS integer CHECK (VALUE > 0);
                      ALTER TABLE t ALTER COLUMN age TYPE positive;";
        for sql in [
            "INSERT INTO t (id, age) SELECT id, id FROM u",
            "UPDATE t SET age = lo WHERE id = $id",
        ] {
            let text = only(&check_both(&old, &[domain], &[sql]));
            assert!(
                text.contains(
                    "writes column age, which CHECK constraint \"positive_check\" of domain \
                     \"positive\" now limits it to at least 1: writing another value fails"
                ),
                "{sql}: {text}"
            );
        }
        // A value the domain admits, or a parameter (its type change says
        // it): nothing about the write.
        assert!(
            check_both(
                &old,
                &[domain],
                &["INSERT INTO t (id, age) SELECT id, n FROM u"]
            )
            .is_empty()
        );
        assert_eq!(
            only(&check_both(
                &old,
                &[domain],
                &["UPDATE t SET age = $age WHERE id = $id"]
            )),
            "parameter $age changes from integer to public.positive"
        );
        // A NOT NULL a computed value may break.
        assert_eq!(
            only(&check_both(
                &old,
                &["ALTER TABLE t ALTER COLUMN age SET NOT NULL;"],
                &["INSERT INTO t (id, age) SELECT id, lo FROM t"]
            )),
            "its INSERT into t writes column age, now NOT NULL, with a value that may be NULL: \
             writing NULL fails"
        );
    }

    #[test]
    fn a_policy_that_is_never_true_denies() {
        let old = "CREATE ROLE app;
                   CREATE TABLE t (id integer PRIMARY KEY, secret text);
                   GRANT SELECT, UPDATE ON t TO app;";
        let sql = ["SELECT id FROM t"];
        assert_eq!(
            only(&check_as(
                &[old],
                &["ALTER TABLE t ENABLE ROW LEVEL SECURITY;
                   CREATE POLICY t_none ON t FOR SELECT TO app USING (false);"],
                &sql,
                &["app"],
            )),
            "it sees no row of t as role \"app\": row-level security applies to the role and no \
             policy lets it SELECT"
        );
        // A restrictive one that is never true denies whatever the others
        // allow.
        assert_eq!(
            only(&check_as(
                &[old],
                &["ALTER TABLE t ENABLE ROW LEVEL SECURITY;
                   CREATE POLICY t_all ON t FOR SELECT TO app USING (true);
                   CREATE POLICY t_never ON t AS RESTRICTIVE FOR SELECT TO app USING (1 = 2);"],
                &sql,
                &["app"],
            )),
            "it sees no row of t as role \"app\": row-level security applies to the role and no \
             policy lets it SELECT"
        );
        assert!(
            check_as(
                &[old],
                &["ALTER TABLE t ENABLE ROW LEVEL SECURITY;
                   CREATE POLICY t_mine ON t FOR SELECT TO app USING (secret IS NOT NULL);"],
                &sql,
                &["app"],
            )
            .is_empty()
        );
    }

    #[test]
    fn tables_are_resolved_as_the_query_resolves_them() {
        // A CTE named like the table is the CTE.
        let old = "CREATE ROLE app;
                   CREATE TABLE t (id integer PRIMARY KEY, name varchar(20));
                   GRANT SELECT, UPDATE ON t TO app;";
        assert!(
            check_as(
                &[old],
                &["ALTER TABLE t ENABLE ROW LEVEL SECURITY;"],
                &["WITH t AS (SELECT 1 AS id) SELECT id FROM t"],
                &["app"],
            )
            .is_empty()
        );
        // The role's search path picks the table an unqualified name is.
        let old = "CREATE ROLE app;
                   CREATE SCHEMA app_data;
                   CREATE TABLE t (id integer PRIMARY KEY, name varchar(20));
                   CREATE TABLE app_data.t (id integer PRIMARY KEY, name varchar(20));
                   GRANT USAGE ON SCHEMA app_data TO app;
                   GRANT SELECT, UPDATE ON t, app_data.t TO app;
                   ALTER ROLE app SET search_path = app_data, public;";
        let sql = ["UPDATE t SET name = $name WHERE id = $id"];
        assert!(
            check_as(
                &[old],
                &["ALTER TABLE public.t ALTER COLUMN name TYPE varchar(10);"],
                &sql,
                &["app"],
            )
            .is_empty()
        );
        assert_eq!(
            only(&check_as(
                &[old],
                &["ALTER TABLE app_data.t ALTER COLUMN name TYPE varchar(10);"],
                &sql,
                &["app"],
            )),
            "its UPDATE of app_data.t writes column name, now character varying(10) instead of \
             character varying(20): a longer value fails"
        );
    }

    #[test]
    fn a_data_modifying_cte_is_its_own_statement() {
        let findings = check(
            &[LIMITS],
            &["ALTER TABLE t ALTER COLUMN name TYPE varchar(10);"],
            &[
                "WITH x AS (INSERT INTO t (id, name) VALUES ($id, 'short') RETURNING id) \
               UPDATE t SET name = $name FROM x WHERE t.id = x.id",
            ],
        );
        assert_eq!(
            only(&findings),
            "its UPDATE of t writes column name, now character varying(10) instead of character \
             varying(20): a longer value fails"
        );
    }

    /// A table, an automatically updatable view of it renaming a column
    /// and leaving one out, and a view of that view.
    const VIEWS: &str = "CREATE TABLE t (id integer NOT NULL, kind text, name varchar(20), \
                                         note text);
        CREATE TABLE u (x integer NOT NULL);
        CREATE VIEW v AS SELECT id, kind AS k, name FROM t;
        CREATE VIEW w AS SELECT k AS kk, id FROM v;";

    #[test]
    fn a_write_through_a_view_meets_the_base_tables_constraints() {
        let new = "ALTER TABLE t ADD CONSTRAINT t_kind CHECK (kind IN ('a', 'b'));";
        let limits = "limits it to one of 'a', 'b': writing another value fails";
        for (sql, statement) in [
            (
                "INSERT INTO v (id, k) VALUES ($id, $k)",
                "its INSERT into v (a view of t)",
            ),
            (
                "UPDATE v SET k = $k WHERE id = $id",
                "its UPDATE of v (a view of t)",
            ),
            (
                "MERGE INTO v USING u ON v.id = u.x WHEN MATCHED THEN UPDATE SET k = $k",
                "its MERGE into v (a view of t)",
            ),
            (
                "INSERT INTO v (id, k) VALUES ($id, $k) ON CONFLICT DO NOTHING",
                "its INSERT into v (a view of t)",
            ),
            // Through a chain of views.
            (
                "INSERT INTO w (id, kk) VALUES ($id, $kk)",
                "its INSERT into w (a view of t)",
            ),
        ] {
            assert_eq!(
                only(&check(&[VIEWS], &[new], &[sql])),
                format!(
                    "{statement} writes column kind, which CHECK constraint \"t_kind\" now \
                     {limits}"
                ),
                "{sql}"
            );
        }
        // A narrower base column (the views are dropped and created again
        // around it).
        assert_eq!(
            only(&check(
                &[VIEWS],
                &["DROP VIEW w; DROP VIEW v;
                   ALTER TABLE t ALTER COLUMN name TYPE varchar(10);
                   CREATE VIEW v AS SELECT id, kind AS k, name FROM t;"],
                &["UPDATE v SET name = $name WHERE id = $id"],
            )),
            "its UPDATE of v (a view of t) writes column name, now character varying(10) \
             instead of character varying(20): a longer value fails"
        );
        // A view column storing a column the new constraint doesn't read,
        // and a constant that fits: no finding.
        assert!(
            check(
                &[VIEWS],
                &[new],
                &[
                    "UPDATE v SET name = $name WHERE id = $id",
                    "INSERT INTO v (id, k) VALUES ($id, 'a')",
                ],
            )
            .is_empty()
        );
    }

    #[test]
    fn an_insert_through_a_view_leaving_out_a_new_not_null_column() {
        // A base column the view doesn't expose: every insert through the
        // view leaves it out.
        assert_eq!(
            only(&check(
                &[VIEWS],
                &["ALTER TABLE t ALTER COLUMN note SET NOT NULL;"],
                &["INSERT INTO v (id, k, name) SELECT x, 'a', 'n' FROM u"],
            )),
            "its INSERT into v (a view of t) leaves out column note, which v doesn't expose, now \
             NOT NULL without a default, so inserting any row fails"
        );
        // One it exposes (renamed) that the INSERT doesn't give.
        assert_eq!(
            only(&check(
                &[VIEWS],
                &["ALTER TABLE t ALTER COLUMN kind SET NOT NULL;"],
                &["INSERT INTO w (id) SELECT x FROM u"],
            )),
            "its INSERT into w (a view of t) leaves out column kind, now NOT NULL without a \
             default, so inserting any row fails"
        );
        // Given through the view, a default of the base column, or of a
        // view of the chain (on the way down, each view fills its own
        // defaults in): no finding.
        for (new, sql) in [
            (
                "ALTER TABLE t ALTER COLUMN kind SET NOT NULL;",
                "INSERT INTO w (id, kk) SELECT x, 'a' FROM u",
            ),
            (
                "ALTER TABLE t ALTER COLUMN note SET DEFAULT ''; \
                 ALTER TABLE t ALTER COLUMN note SET NOT NULL;",
                "INSERT INTO v (id) SELECT x FROM u",
            ),
            (
                "ALTER VIEW v ALTER COLUMN k SET DEFAULT 'a'; \
                 ALTER TABLE t ALTER COLUMN kind SET NOT NULL;",
                "INSERT INTO w (id) SELECT x FROM u",
            ),
            (
                "ALTER VIEW w ALTER COLUMN kk SET DEFAULT 'a'; \
                 ALTER TABLE t ALTER COLUMN kind SET NOT NULL;",
                "INSERT INTO w (id) SELECT x FROM u",
            ),
        ] {
            let findings = check(&[VIEWS], &[new], &[sql]);
            assert!(findings.is_empty(), "{new} / {sql}: {findings:?}");
        }
        // `DEFAULT` into a view column without a default leaves it out:
        // the view below fills its own default in.
        let findings = check(
            &[VIEWS],
            &["ALTER VIEW v ALTER COLUMN k SET DEFAULT 'a'; \
               ALTER TABLE t ALTER COLUMN kind SET NOT NULL;"],
            &["INSERT INTO w (id, kk) VALUES ($id, DEFAULT)"],
        );
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn a_write_through_a_view_that_isnt_the_base_tables_is_not_checked() {
        let new = "ALTER TABLE t ADD CONSTRAINT t_kind CHECK (kind IN ('a', 'b')); \
                   ALTER TABLE t ALTER COLUMN note SET NOT NULL;";
        // An INSTEAD OF trigger: the trigger decides what is written.
        let trigger = "CREATE FUNCTION v_insert() RETURNS trigger LANGUAGE plpgsql AS $$
                       BEGIN RETURN NEW; END $$;
                       CREATE TRIGGER v_insert INSTEAD OF INSERT OR UPDATE ON v
                           FOR EACH ROW EXECUTE FUNCTION v_insert();";
        // A DO INSTEAD rule: it replaces the statement.
        let rule = "CREATE RULE v_insert AS ON INSERT TO v DO INSTEAD NOTIFY v_inserted;
                    CREATE RULE v_update AS ON UPDATE TO v DO INSTEAD NOTIFY v_updated;";
        let queries = [
            "INSERT INTO v (id, k, name) SELECT x, $k, 'n' FROM u",
            "UPDATE v SET k = $k WHERE id = $id",
        ];
        for setup in [trigger, rule] {
            // (They analyze: the silence is the rule's.)
            let all = [VIEWS, setup, new];
            for sql in queries {
                let view = catalog(&all).query_view(sql);
                assert!(view.is_ok_and(|v| !v.writes().is_empty()), "{sql}");
            }
            let findings = check(&[VIEWS, setup], &[new], &queries);
            assert!(findings.is_empty(), "{setup}: {findings:?}");
        }
    }

    #[test]
    fn only_the_rules_and_triggers_of_the_command_stop_a_write_through_a_view() {
        let new = "ALTER TABLE t ADD CONSTRAINT t_kind CHECK (kind IN ('a', 'b'));";
        let function = "CREATE FUNCTION v_row() RETURNS trigger LANGUAGE plpgsql AS $$
                        BEGIN RETURN NEW; END $$;";
        let insert = "INSERT INTO v (id, k) VALUES ($id, $k)";
        let update = "UPDATE v SET k = $k WHERE id = $id";
        let reported = |statement: &str| {
            format!(
                "{statement} writes column kind, which CHECK constraint \"t_kind\" now limits it \
                 to one of 'a', 'b': writing another value fails"
            )
        };
        // PostgreSQL rewrites the statement into the base table unless a
        // DO INSTEAD rule or an INSTEAD OF trigger for its own command is
        // on the way (checked against PostgreSQL 19: an INSERT through a
        // view with an INSTEAD OF DELETE trigger, a rule ON DELETE DO
        // INSTEAD NOTHING or a rule ON INSERT DO ALSO, on the view or on
        // the table, inserts into the table).
        for (setup, sql, statement) in [
            (
                format!(
                    "{function} CREATE TRIGGER v_delete INSTEAD OF DELETE ON v
                         FOR EACH ROW EXECUTE FUNCTION v_row();"
                ),
                insert,
                "its INSERT into v (a view of t)",
            ),
            (
                format!(
                    "{function} CREATE TRIGGER v_insert INSTEAD OF INSERT ON v
                         FOR EACH ROW EXECUTE FUNCTION v_row();"
                ),
                update,
                "its UPDATE of v (a view of t)",
            ),
            (
                "CREATE RULE v_delete AS ON DELETE TO v DO INSTEAD NOTHING;".to_owned(),
                insert,
                "its INSERT into v (a view of t)",
            ),
            (
                "CREATE RULE v_insert AS ON INSERT TO v DO ALSO NOTIFY v_inserted;".to_owned(),
                insert,
                "its INSERT into v (a view of t)",
            ),
            (
                "CREATE RULE t_delete AS ON DELETE TO t DO INSTEAD NOTHING;".to_owned(),
                update,
                "its UPDATE of v (a view of t)",
            ),
        ] {
            assert_eq!(
                only(&check(&[VIEWS, &setup], &[new], &[sql])),
                reported(statement),
                "{setup} / {sql}"
            );
        }
        // One for the command: what is written is the trigger's or the
        // rule's.
        for (setup, sql) in [
            (
                format!(
                    "{function} CREATE TRIGGER v_insert INSTEAD OF INSERT ON v
                         FOR EACH ROW EXECUTE FUNCTION v_row();"
                ),
                insert,
            ),
            (
                "CREATE RULE t_insert AS ON INSERT TO t DO INSTEAD NOTHING;".to_owned(),
                insert,
            ),
        ] {
            let findings = check(&[VIEWS, &setup], &[new], &[sql]);
            assert!(findings.is_empty(), "{setup} / {sql}: {findings:?}");
        }
    }

    const PAIRS: &str = "CREATE TABLE p (
            id integer PRIMARY KEY,
            name text,
            code text,
            lo integer,
            hi integer
        );
        CREATE TABLE u (x integer NOT NULL);";

    #[test]
    fn a_check_conjunct_evaluated_over_the_constants_written() {
        let rejects = |what: &str, check: &str| {
            format!(
                "{what} with values the new CHECK constraint \"{check}\" rejects: writing them \
                 fails"
            )
        };
        let may_reject = |what: &str, check: &str| {
            format!(
                "{what}, which the new CHECK constraint \"{check}\" reads: it may reject the rows \
                 the deployed code writes"
            )
        };
        let lo_hi = "ALTER TABLE p ADD CONSTRAINT p_lo_hi CHECK (lo <= hi);";
        // Every row is evaluated with the constants it holds — each VALUES
        // row on its own; a NULL passes a CHECK; a CASE of constants is
        // evaluated for each.
        for sql in [
            "INSERT INTO p (id, lo, hi) VALUES (1, 1, 2)",
            "INSERT INTO p (id, lo, hi) VALUES (1, 1, 2), (2, 3, 4)",
            "INSERT INTO p (id, lo, hi) VALUES (1, NULL, 2)",
            "INSERT INTO p (id, lo, hi) SELECT x, 1, 1 FROM u",
            "UPDATE p SET lo = 1, hi = CASE WHEN $big THEN 10 ELSE 2 END WHERE id = $id",
        ] {
            let findings = check(&[PAIRS], &[lo_hi], &[sql]);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
        for (sql, text) in [
            (
                "INSERT INTO p (id, lo, hi) VALUES (1, 3, 2)",
                rejects("its INSERT into p writes columns lo, hi", "p_lo_hi"),
            ),
            // One row that fails fails the statement.
            (
                "INSERT INTO p (id, lo, hi) VALUES (1, 1, 2), (2, 5, 4)",
                rejects("its INSERT into p writes columns lo, hi", "p_lo_hi"),
            ),
            (
                "UPDATE p SET lo = 3, hi = 2 WHERE id = $id",
                rejects("its UPDATE of p writes columns lo, hi", "p_lo_hi"),
            ),
            // A column the UPDATE doesn't set keeps any value.
            (
                "UPDATE p SET lo = 1 WHERE id = $id",
                may_reject("its UPDATE of p writes column lo", "p_lo_hi"),
            ),
            // A column the INSERT leaves out is its default.
            (
                "INSERT INTO p (id, lo) VALUES (1, 1)",
                may_reject("its INSERT into p writes column lo", "p_lo_hi"),
            ),
            (
                "INSERT INTO p (id, lo, hi) VALUES (1, $lo, 2)",
                may_reject("its INSERT into p writes columns lo, hi", "p_lo_hi"),
            ),
            // Each MERGE action writes its own rows: the UPDATE's `hi` and
            // the INSERT's `lo` are not known.
            (
                "MERGE INTO p USING u ON p.id = u.x WHEN MATCHED THEN UPDATE SET lo = 5 \
                 WHEN NOT MATCHED THEN INSERT (id, hi) VALUES (u.x, 1)",
                may_reject("its MERGE into p writes columns lo, hi", "p_lo_hi"),
            ),
            // So does ON CONFLICT DO UPDATE.
            (
                "INSERT INTO p (id, lo, hi) VALUES (1, 1, 2) ON CONFLICT (id) DO UPDATE SET lo = 5",
                may_reject("its INSERT into p writes columns lo, hi", "p_lo_hi"),
            ),
        ] {
            assert_eq!(only(&check(&[PAIRS], &[lo_hi], &[sql])), text, "{sql}");
        }

        // Built-in functions of text: `length`, and `upper` over letters
        // every locale maps the same.
        let name = "ALTER TABLE p ADD CONSTRAINT p_name CHECK (length(name) > 1);";
        let code = "ALTER TABLE p ADD CONSTRAINT p_code CHECK (upper(code) = code);";
        for (new, sql) in [
            (name, "INSERT INTO p (id, name) VALUES (1, 'ab')"),
            (name, "UPDATE p SET name = 'été' WHERE id = $id"),
            (code, "INSERT INTO p (id, code) VALUES (1, 'ABC')"),
            (code, "UPDATE p SET code = 'X-1' WHERE id = $id"),
        ] {
            let findings = check(&[PAIRS], &[new], &[sql]);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
        for (new, sql, text) in [
            (
                name,
                "INSERT INTO p (id, name) VALUES (1, 'a')",
                "its INSERT into p writes column name with a value the new CHECK constraint \
                 \"p_name\" rejects: writing it fails"
                    .to_owned(),
            ),
            (
                code,
                "UPDATE p SET code = 'abc' WHERE id = $id",
                "its UPDATE of p writes column code with a value the new CHECK constraint \
                 \"p_code\" rejects: writing it fails"
                    .to_owned(),
            ),
            // `i` is upper-cased to `İ` under a Turkish locale: not known.
            (
                code,
                "UPDATE p SET code = 'XIZ' WHERE id = $id",
                may_reject("its UPDATE of p writes column code", "p_code"),
            ),
            (
                name,
                "UPDATE p SET name = $name WHERE id = $id",
                may_reject("its UPDATE of p writes column name", "p_name"),
            ),
        ] {
            assert_eq!(only(&check(&[PAIRS], &[new], &[sql])), text, "{sql}");
        }

        // A domain's CHECK.
        let domain = "CREATE DOMAIN code3 AS text; CREATE TABLE d (id integer, c code3);";
        let new = "ALTER DOMAIN code3 ADD CONSTRAINT code3_length CHECK (length(VALUE) = 3);";
        assert!(check(&[domain], &[new], &["INSERT INTO d VALUES (1, 'abc')"]).is_empty());
        assert_eq!(
            only(&check(
                &[domain],
                &[new],
                &["INSERT INTO d VALUES (1, 'ab')"]
            )),
            "its INSERT into d writes column c with a value the new CHECK constraint \
             \"code3_length\" of domain \"code3\" rejects: writing it fails"
        );

        // A BEFORE ROW trigger may change the row before it is checked.
        let trigger = "CREATE FUNCTION p_row() RETURNS trigger LANGUAGE plpgsql AS $$
                       BEGIN RETURN NEW; END $$;
                       CREATE TRIGGER p_row BEFORE INSERT ON p
                           FOR EACH ROW EXECUTE FUNCTION p_row();";
        for sql in [
            "INSERT INTO p (id, lo, hi) VALUES (1, 1, 2)",
            "INSERT INTO p (id, lo, hi) VALUES (1, 3, 2)",
        ] {
            assert_eq!(
                only(&check(&[PAIRS, trigger], &[lo_hi], &[sql])),
                may_reject("its INSERT into p writes columns lo, hi", "p_lo_hi"),
                "{sql}"
            );
        }
    }

    #[test]
    fn numeric_and_datetime_values_fit_by_their_bounds() {
        let old = "CREATE TABLE src (
                p numeric(10, 2) CHECK (p >= 0 AND p <= 99.99),
                q numeric CHECK (q BETWEEN 0 AND 1),
                i integer CHECK (i BETWEEN 1 AND 10),
                d date CHECK (d >= '2020-01-01'),
                tz timestamptz CHECK (tz >= '2020-01-01 00:00+00')
            );
            CREATE TABLE t (id integer, price numeric(10, 2), d date, ts timestamp,
                            tz timestamptz);";
        for (new, sql) in [
            // Up to 99.99: two digits before the point, two after.
            (
                "ALTER TABLE t ALTER COLUMN price TYPE numeric(4, 2);",
                "INSERT INTO t (id, price) SELECT 1, p FROM src",
            ),
            (
                "ALTER TABLE t ALTER COLUMN price TYPE numeric(3, 1);",
                "INSERT INTO t (id, price) SELECT 1, i FROM src",
            ),
            (
                "ALTER TABLE t ADD CHECK (price >= 0 AND price < 1000);",
                "INSERT INTO t (id, price) SELECT 1, p FROM src",
            ),
            // An integer is the same number as a `numeric`.
            (
                "ALTER TABLE t ADD CHECK (price > 0.5);",
                "INSERT INTO t (id, price) SELECT 1, i FROM src",
            ),
            (
                "ALTER TABLE t ADD CHECK (price <= 10);",
                "INSERT INTO t (id, price) VALUES (1, 5)",
            ),
            // A literal read as the column's type.
            (
                "ALTER TABLE t ADD CHECK (d >= '2000-01-01');",
                "INSERT INTO t (id, d) VALUES (1, '2021-05-05')",
            ),
            (
                "ALTER TABLE t ADD CHECK (d >= '2000-01-01');",
                "INSERT INTO t (id, d) SELECT 1, d FROM src",
            ),
            // A date as the midnight of its day.
            (
                "ALTER TABLE t ADD CHECK (ts >= '2020-01-01');",
                "INSERT INTO t (id, ts) SELECT 1, d FROM src",
            ),
            (
                "ALTER TABLE t ADD CHECK (tz >= '2000-01-01 00:00+00');",
                "INSERT INTO t (id, tz) SELECT 1, tz FROM src",
            ),
            (
                "ALTER TABLE t ADD CHECK (tz >= '2000-01-01 00:00+00');",
                "INSERT INTO t (id, tz) VALUES (1, '2021-05-05 10:00+02')",
            ),
        ] {
            let findings = check(&[old], &[new], &[sql]);
            assert!(findings.is_empty(), "{new} / {sql}: {findings:?}");
        }
        for (new, sql, text) in [
            // Rounded to one digit after the point.
            (
                "ALTER TABLE t ALTER COLUMN price TYPE numeric(3, 1);",
                "INSERT INTO t (id, price) SELECT 1, p FROM src",
                "its INSERT into t writes column price, now numeric(3,1) instead of \
                 numeric(10,2): a value with more than 2 digits before the decimal point fails, \
                 and one with more than 1 digit after it is rounded",
            ),
            // 99.99 has two digits before the point.
            (
                "ALTER TABLE t ALTER COLUMN price TYPE numeric(3, 2);",
                "INSERT INTO t (id, price) SELECT 1, p FROM src",
                "its INSERT into t writes column price, now numeric(3,2) instead of \
                 numeric(10,2): a value with more than 1 digit before the decimal point fails",
            ),
            // Any number of digits after the point.
            (
                "ALTER TABLE t ALTER COLUMN price TYPE numeric(4, 2);",
                "INSERT INTO t (id, price) SELECT 1, q FROM src",
                "its INSERT into t writes column price, now numeric(4,2) instead of \
                 numeric(10,2): a value with more than 2 digits before the decimal point fails",
            ),
            (
                "ALTER TABLE t ADD CONSTRAINT t_price CHECK (price > 1);",
                "INSERT INTO t (id, price) SELECT 1, i FROM src",
                "its INSERT into t writes column price, which CHECK constraint \"t_price\" now \
                 limits it to above 1: writing another value fails",
            ),
            (
                "ALTER TABLE t ADD CONSTRAINT t_d CHECK (d >= '2022-01-01');",
                "INSERT INTO t (id, d) SELECT 1, d FROM src",
                "its INSERT into t writes column d, which CHECK constraint \"t_d\" now limits it \
                 to at least 2022-01-01: writing another value fails",
            ),
            // Without a zone, the session's time zone decides.
            (
                "ALTER TABLE t ADD CONSTRAINT t_tz CHECK (tz >= '2000-01-01 00:00+00');",
                "INSERT INTO t (id, tz) VALUES (1, '2021-05-05 10:00')",
                "its INSERT into t writes column tz, which CHECK constraint \"t_tz\" now limits \
                 it to at least 2000-01-01 00:00:00+00: writing another value fails",
            ),
            (
                "ALTER TABLE t ADD CONSTRAINT t_tz CHECK (tz >= '2000-01-01 00:00+00');",
                "INSERT INTO t (id, tz) SELECT 1, d FROM src",
                "its INSERT into t writes column tz, which CHECK constraint \"t_tz\" now limits \
                 it to at least 2000-01-01 00:00:00+00: writing another value fails",
            ),
        ] {
            assert_eq!(only(&check(&[old], &[new], &[sql])), text, "{new} / {sql}");
        }
    }

    #[test]
    fn an_insert_through_a_view_whose_default_is_dropped() {
        // The deployed INSERT gets `kind` from the view's default.
        let old = [
            VIEWS,
            "ALTER VIEW v ALTER COLUMN k SET DEFAULT 'a';
                           ALTER TABLE t ALTER COLUMN kind SET NOT NULL;",
        ];
        assert_eq!(
            only(&check(
                &old,
                &["ALTER VIEW v ALTER COLUMN k DROP DEFAULT;"],
                &["INSERT INTO v (id) SELECT x FROM u"],
            )),
            "its INSERT into v (a view of t) leaves out column kind, now NOT NULL without a \
             default, so inserting any row fails"
        );
        // Left out before as well: it already fails.
        let findings = check(
            &[VIEWS, "ALTER TABLE t ALTER COLUMN kind SET NOT NULL;"],
            &["ALTER TABLE t ADD COLUMN extra integer;"],
            &["INSERT INTO v (id) SELECT x FROM u"],
        );
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn set_to_default_without_a_default_writes_null() {
        let old = "CREATE TABLE t (id integer PRIMARY KEY, note text, \
                   label text NOT NULL DEFAULT '');
                   CREATE VIEW v AS SELECT id, label FROM t;";
        // An UPDATE's: the analysis itself rejects it, as it does a literal
        // NULL there.
        assert_eq!(
            only(&check(
                &[old],
                &["ALTER TABLE t ALTER COLUMN note SET NOT NULL;"],
                &["UPDATE t SET note = DEFAULT WHERE id = $id"],
            )),
            "it no longer runs: null value in column \"note\" of relation \"t\" violates \
             not-null constraint (cannot assign NULL to NOT NULL column `t.note`)"
        );
        assert_eq!(
            only(&check(
                &[old],
                &["ALTER TABLE t ALTER COLUMN label DROP DEFAULT;"],
                &["UPDATE t SET label = DEFAULT WHERE id = $id"],
            )),
            "it no longer runs: null value in column \"label\" of relation \"t\" violates \
             not-null constraint (cannot assign NULL to NOT NULL column `t.label`)"
        );
        // ON CONFLICT DO UPDATE's, which the analysis accepts (it runs only
        // on a conflict).
        assert_eq!(
            only(&check(
                &[old],
                &["ALTER TABLE t ALTER COLUMN note SET NOT NULL;"],
                &["INSERT INTO t (id, note) VALUES ($id, 'x') \
                   ON CONFLICT (id) DO UPDATE SET note = DEFAULT"],
            )),
            "its INSERT into t writes column note, now NOT NULL, as DEFAULT, which is NULL, the \
             column having no default: writing NULL fails"
        );
        assert_eq!(
            only(&check(
                &[old],
                &["ALTER TABLE t ALTER COLUMN label DROP DEFAULT;"],
                &["INSERT INTO t (id, label) VALUES ($id, 'x') \
                   ON CONFLICT (id) DO UPDATE SET label = DEFAULT"],
            )),
            "its INSERT into t writes column label as DEFAULT, which is now NULL, the column \
             having no default, while it is NOT NULL: writing NULL fails"
        );
        // Through a view, the view's default is written, NULL without one
        // (rewriteTargetListIU; checked against PostgreSQL 19), whatever
        // the table's — which the analysis itself rejects.
        assert_eq!(
            only(&check(
                &[old, "ALTER VIEW v ALTER COLUMN label SET DEFAULT 'v';"],
                &["ALTER VIEW v ALTER COLUMN label DROP DEFAULT;"],
                &["UPDATE v SET label = DEFAULT WHERE id = $id"],
            )),
            "it no longer runs: null value in column \"label\" of relation \"t\" violates \
             not-null constraint (cannot assign NULL to NOT NULL column `t.label`)"
        );
        // A default, of the column or of its domain: no finding.
        for (new, sql) in [
            (
                "ALTER TABLE t ALTER COLUMN note SET DEFAULT ''; \
                 ALTER TABLE t ALTER COLUMN note SET NOT NULL;",
                "UPDATE t SET note = DEFAULT WHERE id = $id",
            ),
            (
                "ALTER TABLE t ALTER COLUMN note SET NOT NULL;",
                "UPDATE t SET note = 'x' WHERE id = $id",
            ),
        ] {
            let findings = check(&[old], &[new], &[sql]);
            assert!(findings.is_empty(), "{new} / {sql}: {findings:?}");
        }
        let domain = "CREATE DOMAIN filled AS text DEFAULT 'x';
                      CREATE TABLE f (id integer, c filled);";
        let findings = check(
            &[domain],
            &["ALTER TABLE f ALTER COLUMN c SET NOT NULL;"],
            &["UPDATE f SET c = DEFAULT WHERE id = $id"],
        );
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn a_domain_default_fills_a_column_left_out() {
        let old = "CREATE DOMAIN filled AS text DEFAULT 'x';
                   CREATE DOMAIN empty AS text;
                   CREATE TABLE f (id integer, a filled, b empty);
                   CREATE VIEW fv AS SELECT id, a, b FROM f;
                   CREATE TABLE u (x integer NOT NULL);";
        // PostgreSQL stores the domain's default in a column without one
        // of its own (checked against PostgreSQL 19): no finding.
        for sql in [
            "INSERT INTO f (id, b) VALUES (1, 'b')",
            "INSERT INTO fv (id, b) VALUES (1, 'b')",
        ] {
            let findings = check(
                &[old],
                &["ALTER TABLE f ALTER COLUMN a SET NOT NULL;"],
                &[sql],
            );
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
        // A domain without a default gives none.
        assert_eq!(
            only(&check(
                &[old],
                &["ALTER TABLE f ALTER COLUMN b SET NOT NULL;"],
                &["INSERT INTO f (id, a) SELECT x, 'a' FROM u"],
            )),
            "its INSERT into f leaves out column b, now NOT NULL without a default, so \
             inserting any row fails"
        );
        // Through a view, `b`'s domain gives no default either: the
        // table's column decides.
        assert_eq!(
            only(&check(
                &[old],
                &["ALTER TABLE f ALTER COLUMN b SET NOT NULL;"],
                &["INSERT INTO fv (id, a) SELECT x, 'a' FROM u"],
            )),
            "its INSERT into fv (a view of f) leaves out column b, now NOT NULL without a \
             default, so inserting any row fails"
        );
    }
}
