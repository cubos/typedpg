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
    let mut broken: Vec<(&DeployedQuery, AnalyzedQuery, Vec<Change>)> = Vec::new();
    for query in deployment.queries {
        if allowed_rules(&query.sql).contains(&rule) {
            continue;
        }
        // A query that doesn't analyze against its own schema fails there
        // already; the migration doesn't break it.
        let Ok(old) = deployment.catalog.analyze(&query.sql) else {
            continue;
        };
        let changes = changes(&query.sql, &old, deployment.catalog, schema.catalog);
        if !changes.is_empty() {
            broken.push((query, old, changes));
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
    broken: &[(&DeployedQuery, AnalyzedQuery, Vec<Change>)],
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
    broken: &'a [(&'a DeployedQuery, AnalyzedQuery, Vec<Change>)],
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
        for (index, (query, old, changes)) in self.broken.iter().enumerate() {
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
                self::changes(&query.sql, old, self.deployment.catalog, &state)
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

/// What changes for a deployed query analyzed as `old` against `old_catalog`
/// when it runs against `new_catalog`.
fn changes(
    sql: &str,
    old: &AnalyzedQuery,
    old_catalog: &PgCatalog,
    new_catalog: &PgCatalog,
) -> Vec<Change> {
    let new = match new_catalog.analyze_located(sql) {
        Ok(new) => new,
        Err(error) => {
            let message = error.error.to_string();
            let message = message.lines().next().unwrap_or_default().trim();
            return vec![Change {
                subject: "analysis".to_owned(),
                text: format!("it no longer runs: {message}"),
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
    inserts(sql, old_catalog, new_catalog, &mut out);
    out
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

/// An `INSERT` of the query that leaves out a column the new schema makes
/// `NOT NULL` without a default, where the deployed schema let it be
/// omitted: PostgreSQL fails every row at runtime, which no analysis of the
/// statement shows.
fn inserts(sql: &str, old_catalog: &PgCatalog, new_catalog: &PgCatalog, out: &mut Vec<Change>) {
    if !sql.to_ascii_lowercase().contains("insert") {
        return;
    }
    let Ok(view) = new_catalog.query_view(sql) else {
        return;
    };
    for (node, _) in view.tree.nodes() {
        if let NodeRef::InsertStmt(stmt) = node {
            insert(stmt, old_catalog, new_catalog, out);
        }
    }
}

fn insert(
    stmt: &InsertStmt,
    old_catalog: &PgCatalog,
    new_catalog: &PgCatalog,
    out: &mut Vec<Change>,
) {
    let Some(relation) = stmt.relation.as_deref() else {
        return;
    };
    if !relation.catalogname.is_empty() {
        return;
    }
    let schema = (!relation.schemaname.is_empty()).then_some(relation.schemaname.as_str());
    let Some(new_table) = new_catalog.resolve_table(schema, &relation.relname) else {
        return;
    };
    let old_table = old_catalog.resolve_table(schema, &relation.relname);
    let attributes: Vec<_> = new_catalog
        .attributes_of(new_table.oid)
        .iter()
        .filter(|attribute| attribute.attnum > 0)
        .collect();
    // The columns the statement gives a value: its column list, or the
    // table's first N for N values per row (none for DEFAULT VALUES).
    let given: HashSet<&str> = if !stmt.cols.is_empty() {
        stmt.cols
            .iter()
            .filter_map(|col| match col {
                Some(N::ResTarget(target)) => Some(target.name.as_str()),
                _ => None,
            })
            .collect()
    } else {
        match stmt.select_stmt.as_ref() {
            None => HashSet::new(),
            Some(N::SelectStmt(select)) => {
                let Some(width) = crate::rules::insert_width(select) else {
                    return;
                };
                attributes
                    .iter()
                    .take(width)
                    .map(|attribute| attribute.attname.as_str())
                    .collect()
            }
            Some(_) => return,
        }
    };
    let required = |attribute: &typedpg_analyzer::PgAttribute| {
        attribute.attnotnull
            && !attribute.atthasdef
            && attribute.attidentity.is_none()
            && attribute.attgenerated.is_none()
    };
    for attribute in attributes {
        if given.contains(attribute.attname.as_str()) || !required(attribute) {
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
               ALTER TABLE users ADD CONSTRAINT users_kind_a CHECK (kind = 'a');
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
}
