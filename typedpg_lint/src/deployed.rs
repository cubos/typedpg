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
//! CONFLICT DO UPDATE SET` included) or `UPDATE` writing a column whose new
//! definition accepts fewer values — a shorter `varchar(n)` / `char(n)` /
//! `bit(n)` / `varbit(n)`, a `numeric` with fewer digits before the decimal
//! point (a value fails) or after it (a value is rounded), a CHECK of the
//! table or the column's domain that is new or narrower, `NOT VALID` ones
//! included (they bind the rows written). A new CHECK the analyzer can't
//! reduce to the columns written (`CHECK (lo <= hi)`) is reported as one
//! that may reject the deployed code's rows. Not reported: a column a new
//! UNIQUE, primary key or exclusion constraint covers (whether a row breaks
//! it depends on the data), a column whose type changes (its parameter's
//! type change says so), and a timestamp, time or interval precision (it
//! rounds). A result column's type modifier changing is not reported
//! either: the deployed code decodes the same wire type whatever its
//! length or precision.
//!
//! A query runs as the roles the deployed code runs it as (a call site's
//! `role = "..."`, or the database's configured `roles`): it is analyzed as
//! each, so a privilege a migration revokes is reported, and so is a table
//! whose row-level security now denies the role every row (no policy for
//! it). A query without a role has no privileges checked.
//!
//! Each finding is placed on the migration statement that makes the change:
//! the new migrations are applied again one statement at a time, for the
//! queries that break only.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ops::Range;

use typedpg_analyzer::{
    AnalyzedColumn, AnalyzedQuery, IntRange, MigrationObserver, MigrationStatement, PgCatalog,
    Refinement, Type,
};
use typedpg_core::quote_identifier;
use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{InsertStmt, Node as N};

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

/// A deployed query's analysis against the deployed schema, as one of the
/// roles it runs as (`None`: no role).
type RoleAnalysis<'q> = (Option<&'q str>, AnalyzedQuery);

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
                analyze(deployment.catalog, &query.sql, role)
                    .ok()
                    .map(|old| (role, old))
            })
            .collect();
        if olds.is_empty() {
            continue;
        }
        let changes = query_changes(&query.sql, &olds, deployment.catalog, schema.catalog);
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
        let lead_in = self.previous_end..statement.span.end;
        self.previous_end = statement.span.end;
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
            let now: HashSet<String> =
                query_changes(&query.sql, olds, self.deployment.catalog, &state)
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

/// `sql` analyzed against `catalog` as `role` (`None`: the plain analysis).
fn analyze(
    catalog: &PgCatalog,
    sql: &str,
    role: Option<&str>,
) -> Result<AnalyzedQuery, typedpg_analyzer::LocatedError> {
    match role {
        Some(role) => catalog.analyze_as_located(sql, role),
        None => catalog.analyze_located(sql),
    }
}

/// What changes for a deployed query analyzed as `olds` (one analysis per
/// role it runs as) against `old_catalog` when it runs against
/// `new_catalog`: the changes for every role, each subject once.
fn query_changes(
    sql: &str,
    olds: &[RoleAnalysis<'_>],
    old_catalog: &PgCatalog,
    new_catalog: &PgCatalog,
) -> Vec<Change> {
    let mut out: Vec<Change> = Vec::new();
    for (role, old) in olds {
        for change in changes(sql, *role, old, old_catalog, new_catalog) {
            if !out.iter().any(|c| c.subject == change.subject) {
                out.push(change);
            }
        }
    }
    out
}

/// What changes for a deployed query analyzed as `old` against `old_catalog`,
/// as `role`, when it runs against `new_catalog`.
fn changes(
    sql: &str,
    role: Option<&str>,
    old: &AnalyzedQuery,
    old_catalog: &PgCatalog,
    new_catalog: &PgCatalog,
) -> Vec<Change> {
    let new = match analyze(new_catalog, sql, role) {
        Ok(new) => new,
        Err(error) => {
            let first_line = |error: &typedpg_analyzer::LocatedError| {
                let message = error.error.to_string();
                message.lines().next().unwrap_or_default().trim().to_owned()
            };
            // What fails for the role alone (a privilege it lost, a policy
            // it lacks) is said of the role; what fails for anyone is not.
            if let Some(role) = role
                && new_catalog.analyze_located(sql).is_ok()
            {
                return vec![Change {
                    subject: format!("analysis as {role}"),
                    text: format!(
                        "it no longer runs as role \"{role}\": {}",
                        first_line(&error)
                    ),
                }];
            }
            return vec![Change {
                subject: "analysis".to_owned(),
                text: format!("it no longer runs: {}", first_line(&error)),
            }];
        }
    };
    let mut out = Vec::new();
    columns(&old.columns, &new.columns, &mut out);
    for old_param in &old.params {
        let Some(new_param) = new.params.iter().find(|p| p.name == old_param.name) else {
            continue;
        };
        input(
            &format!("parameter ${}", old_param.name),
            (&old_param.pg_type, old_param.nullable),
            (&new_param.pg_type, new_param.nullable),
            &mut out,
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
                &mut out,
            );
        }
    }
    writes(sql, old_catalog, new_catalog, &mut out);
    if let Some(role) = role {
        row_security(sql, role, old_catalog, new_catalog, &mut out);
    }
    out
}

/// The tables the query reads or writes whose rows row-level security now
/// denies `role` the command it runs on them, where the deployed schema
/// didn't: a table that enables it without a policy for the role, a policy
/// dropped. The query still analyzes — a SELECT then sees no row, an
/// UPDATE or a DELETE changes none, and an INSERT fails.
fn row_security(
    sql: &str,
    role: &str,
    old_catalog: &PgCatalog,
    new_catalog: &PgCatalog,
    out: &mut Vec<Change>,
) {
    let Ok(view) = new_catalog.query_view(sql) else {
        return;
    };
    // Each table the query uses, with the command it runs on it.
    let mut uses: Vec<(&typedpg_pg_query::ast::RangeVar, char)> = Vec::new();
    for (node, _) in view.tree.nodes() {
        let target = match node {
            NodeRef::InsertStmt(stmt) => stmt.relation.as_deref().map(|r| (r, 'a')),
            NodeRef::UpdateStmt(stmt) => stmt.relation.as_deref().map(|r| (r, 'w')),
            NodeRef::DeleteStmt(stmt) => stmt.relation.as_deref().map(|r| (r, 'd')),
            _ => None,
        };
        uses.extend(target);
    }
    for (node, _) in view.tree.nodes() {
        if let NodeRef::RangeVar(relation) = node
            && !uses.iter().any(|(r, _)| r.location == relation.location)
        {
            uses.push((relation, 'r'));
        }
    }
    for (relation, command) in uses {
        let (Some(old_table), Some(new_table)) =
            (table(old_catalog, relation), table(new_catalog, relation))
        else {
            continue;
        };
        if !new_catalog.row_security_denies(new_table.oid, role, command)
            || old_catalog.row_security_denies(old_table.oid, role, command)
        {
            continue;
        }
        let name = crate::rules::table_name(relation);
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

/// What the query's `INSERT`s and `UPDATE`s write that the new schema
/// rejects where the deployed one didn't: an `INSERT` that leaves out a
/// column now `NOT NULL` without a default, and a column written whose new
/// definition accepts fewer values (see [`narrowed`]).
fn writes(sql: &str, old_catalog: &PgCatalog, new_catalog: &PgCatalog, out: &mut Vec<Change>) {
    let lower = sql.to_ascii_lowercase();
    if !lower.contains("insert") && !lower.contains("update") {
        return;
    }
    let Ok(view) = new_catalog.query_view(sql) else {
        return;
    };
    let names = |targets: &[Option<N>]| -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for target in targets {
            if let Some(N::ResTarget(target)) = target
                && !out.contains(&target.name)
            {
                out.push(target.name.clone());
            }
        }
        out
    };
    for (node, _) in view.tree.nodes() {
        match node {
            NodeRef::InsertStmt(stmt) => {
                let Some(relation) = stmt.relation.as_deref() else {
                    continue;
                };
                let Some(mut written) = insert(stmt, relation, old_catalog, new_catalog, out)
                else {
                    continue;
                };
                // `ON CONFLICT DO UPDATE SET` writes its columns too.
                if let Some(clause) = stmt.on_conflict_clause.as_deref() {
                    for name in names(&clause.target_list) {
                        if !written.contains(&name) {
                            written.push(name);
                        }
                    }
                }
                let statement = format!("its INSERT into {}", crate::rules::table_name(relation));
                narrowed(
                    &statement,
                    relation,
                    &written,
                    old_catalog,
                    new_catalog,
                    out,
                );
            }
            NodeRef::UpdateStmt(stmt) => {
                let Some(relation) = stmt.relation.as_deref() else {
                    continue;
                };
                let written = names(&stmt.target_list);
                let statement = format!("its UPDATE of {}", crate::rules::table_name(relation));
                narrowed(
                    &statement,
                    relation,
                    &written,
                    old_catalog,
                    new_catalog,
                    out,
                );
            }
            _ => {}
        }
    }
}

/// The table `relation` names in `catalog`.
fn table<'c>(
    catalog: &'c PgCatalog,
    relation: &typedpg_pg_query::ast::RangeVar,
) -> Option<&'c typedpg_analyzer::PgClass> {
    if !relation.catalogname.is_empty() {
        return None;
    }
    let schema = (!relation.schemaname.is_empty()).then_some(relation.schemaname.as_str());
    catalog.resolve_table(schema, &relation.relname)
}

/// The columns an `INSERT` gives a value, in table order for one without a
/// column list; reporting those it leaves out that the new schema makes
/// `NOT NULL` without a default, where the deployed schema let them be
/// omitted: PostgreSQL fails every row at runtime, which no analysis of the
/// statement shows. `None` when the columns can't be told.
fn insert(
    stmt: &InsertStmt,
    relation: &typedpg_pg_query::ast::RangeVar,
    old_catalog: &PgCatalog,
    new_catalog: &PgCatalog,
    out: &mut Vec<Change>,
) -> Option<Vec<String>> {
    let new_table = table(new_catalog, relation)?;
    let old_table = table(old_catalog, relation);
    let attributes: Vec<_> = new_catalog
        .attributes_of(new_table.oid)
        .iter()
        .filter(|attribute| attribute.attnum > 0)
        .collect();
    // The columns the statement gives a value: its column list, or the
    // table's first N for N values per row (none for DEFAULT VALUES).
    let given: Vec<String> = if !stmt.cols.is_empty() {
        stmt.cols
            .iter()
            .filter_map(|col| match col {
                Some(N::ResTarget(target)) => Some(target.name.clone()),
                _ => None,
            })
            .collect()
    } else {
        match stmt.select_stmt.as_ref() {
            None => Vec::new(),
            Some(N::SelectStmt(select)) => {
                let width = crate::rules::insert_width(select)?;
                attributes
                    .iter()
                    .take(width)
                    .map(|attribute| attribute.attname.clone())
                    .collect()
            }
            Some(_) => return None,
        }
    };
    let required = |attribute: &typedpg_analyzer::PgAttribute| {
        attribute.attnotnull
            && !attribute.atthasdef
            && attribute.attidentity.is_none()
            && attribute.attgenerated.is_none()
    };
    for attribute in attributes {
        if given.contains(&attribute.attname) || !required(attribute) {
            continue;
        }
        // Omitted before too: the deployed code already fails there.
        let required_before = old_table.is_some_and(|table| {
            old_catalog
                .attributes_of(table.oid)
                .iter()
                .find(|a| a.attnum > 0 && a.attname == attribute.attname)
                .is_some_and(required)
        });
        if required_before {
            continue;
        }
        out.push(Change {
            subject: format!(
                "insert {} {}",
                crate::rules::table_name(relation),
                attribute.attname
            ),
            text: format!(
                "its INSERT into {} leaves out column {}, now NOT NULL without a default, so \
                 inserting any row fails",
                crate::rules::table_name(relation),
                quote_identifier(&attribute.attname)
            ),
        });
    }
    Some(given)
}

/// The columns `written` of `relation` (by `statement`: `its INSERT into
/// t`) whose new definition accepts fewer values than the deployed one: a
/// narrower type modifier (a shorter `varchar(n)`, `char(n)`, `bit(n)`,
/// `varbit(n)`; fewer digits before a `numeric`'s decimal point, which
/// fail, or after it, which round), a CHECK constraint of the table or the
/// column's domain that is new or narrower (`NOT VALID` ones too: they bind
/// the rows written), and a new CHECK the analyzer can't reduce to the
/// columns written, which may reject the deployed code's rows. A column
/// whose type changes is left to its parameter's type change; one a new
/// UNIQUE, primary key or exclusion constraint covers is not reported (it
/// depends on the data).
fn narrowed(
    statement: &str,
    relation: &typedpg_pg_query::ast::RangeVar,
    written: &[String],
    old_catalog: &PgCatalog,
    new_catalog: &PgCatalog,
    out: &mut Vec<Change>,
) {
    let (Some(old_table), Some(new_table)) =
        (table(old_catalog, relation), table(new_catalog, relation))
    else {
        return;
    };
    let name = crate::rules::table_name(relation);
    // The new constraints the analyzer can't reduce to a written column,
    // with the written columns they read.
    let mut unreduced: Vec<(String, Vec<String>)> = Vec::new();
    let mut reduced: HashSet<String> = HashSet::new();
    for column in written {
        let attribute = |catalog: &PgCatalog, relid| {
            catalog
                .attributes_of(relid)
                .iter()
                .find(|a| a.attnum > 0 && a.attname == *column)
                .cloned()
        };
        let (Some(old), Some(new)) = (
            attribute(old_catalog, old_table.oid),
            attribute(new_catalog, new_table.oid),
        ) else {
            continue;
        };
        if old.atttypid != new.atttypid {
            continue;
        }
        let subject = |what: &str| format!("write {name} {column} {what}");
        let column_text = format!("{statement} writes column {}", quote_identifier(column));
        if let Some(text) = narrower_type(
            old_catalog.lint_value_limit(old.atttypid, old.atttypmod),
            new_catalog.lint_value_limit(new.atttypid, new.atttypmod),
        ) {
            out.push(Change {
                subject: subject("typmod"),
                text: format!(
                    "{column_text}, now {} instead of {}: {text}",
                    new_catalog.lint_format_type(new.atttypid, new.atttypmod),
                    old_catalog.lint_format_type(old.atttypid, old.atttypmod),
                ),
            });
        }

        let old_constraints = old_catalog.column_write_constraints(old_table.oid, column);
        let new_constraints = new_catalog.column_write_constraints(new_table.oid, column);
        let all = |constraints: &[typedpg_analyzer::WriteConstraint]| {
            constraints
                .iter()
                .filter_map(|c| c.allowed.as_ref())
                .fold(Refinement::NONE, |all, allowed| all.and(allowed))
        };
        let (old_allowed, new_allowed) = (all(&old_constraints), all(&new_constraints));
        if new_allowed.widenings(&old_allowed).any() {
            // The constraints that make the difference: those the deployed
            // schema doesn't have as they are.
            let sources: Vec<&str> = new_constraints
                .iter()
                .filter(|c| c.allowed.is_some() && !old_constraints.contains(c))
                .map(|c| c.source.as_str())
                .collect();
            let now = new_catalog
                .describe_column_values(new_table.oid, column, &new_allowed)
                .unwrap_or_else(|| "fewer values".to_owned());
            let before = old_catalog
                .describe_column_values(old_table.oid, column, &old_allowed)
                .map(|before| format!(" instead of {before}"))
                .unwrap_or_default();
            out.push(Change {
                subject: subject("check"),
                text: format!(
                    "{column_text}, which {} now {} to {now}{before}: writing another value fails",
                    if sources.is_empty() {
                        "its constraints".to_owned()
                    } else {
                        sources.join(" and ")
                    },
                    if sources.len() == 1 {
                        "limits it"
                    } else {
                        "limit it"
                    },
                ),
            });
        }
        for constraint in &new_constraints {
            if constraint.allowed.is_some() {
                reduced.insert(constraint.source.clone());
                continue;
            }
            if old_constraints
                .iter()
                .any(|c| c.source == constraint.source)
            {
                continue;
            }
            match unreduced.iter_mut().find(|(s, _)| *s == constraint.source) {
                Some((_, columns)) => columns.push(column.clone()),
                None => unreduced.push((constraint.source.clone(), vec![column.clone()])),
            }
        }
    }
    for (source, columns) in unreduced {
        if reduced.contains(&source) {
            continue;
        }
        let columns: Vec<String> = columns.iter().map(|c| quote_identifier(c)).collect();
        out.push(Change {
            subject: format!("write {name} {source}"),
            text: format!(
                "{statement} writes {} {}, which the new {source} reads: it may reject the rows \
                 the deployed code writes",
                if columns.len() == 1 {
                    "column"
                } else {
                    "columns"
                },
                columns.join(", ")
            ),
        });
    }
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
}
