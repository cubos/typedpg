//! Comparing two analyses of one statement — the same SQL checked against
//! two roles.
//!
//! PostgreSQL resolves an unqualified name along the search path the
//! session's role has, so a role with `ALTER ROLE … SET search_path` can see
//! a different table under the same name — and therefore different column
//! types. Code generators (`sql!`, `typedpg gen`) generate from one
//! analysis, so when a statement is checked against several roles, their
//! analyses must agree on everything the generated code depends on.
//!
//! [`first_difference`] finds the first of those things two analyses
//! disagree on, worded for a compiler diagnostic. Every field of the result
//! is compared — offsets included — so a future consumer of another field
//! inherits the check.

use crate::{AnalyzedColumn, AnalyzedParam, AnalyzedQuery, AnalyzedSpread, SpreadKind};

/// The first difference between two analyses of one statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalysisDifference {
    /// What differs, as a singular noun phrase (`column "x"`, `parameter
    /// "$id"`, …): the message reads `{subject} is {first} for …`.
    pub subject: String,
    /// How it resolves under the first analysis.
    pub first: String,
    /// How it resolves under the second analysis.
    pub second: String,
}

impl AnalysisDifference {
    /// The diagnostic for a statement the analyses of `first_role` and
    /// `second_role` disagree on.
    ///
    /// The two analyses come from the same SQL, so the only thing that can
    /// make them differ is where the roles' search paths resolve its
    /// unqualified names; the message says so.
    pub fn message(&self, first_role: &str, second_role: &str) -> String {
        let first_role = quote_role(first_role);
        let second_role = quote_role(second_role);
        format!(
            "query resolves differently for roles {first_role} and {second_role}: {} is {} for \
             {first_role} and {} for {second_role} (the roles' search_path settings differ)",
            self.subject, self.first, self.second
        )
    }
}

/// The role name as the message shows it, double quotes escaped the way
/// PostgreSQL escapes an identifier's.
fn quote_role(role: &str) -> String {
    format!("\"{}\"", role.replace('"', "\"\""))
}

/// The first thing two analyses of the same statement disagree on, or
/// `None` when they agree on everything.
pub fn first_difference(
    first: &AnalyzedQuery,
    second: &AnalyzedQuery,
) -> Option<AnalysisDifference> {
    if let Some(d) = parameters_differ(&first.params, &second.params) {
        return Some(d);
    }
    if let Some(d) = spreads_differ(&first.spreads, &second.spreads) {
        return Some(d);
    }
    if let Some(d) = columns_differ(&first.columns, &second.columns) {
        return Some(d);
    }
    if first.can_run_as_subquery != second.can_run_as_subquery {
        let state = |can: bool| {
            if can {
                "supported".to_owned()
            } else {
                "not supported".to_owned()
            }
        };
        return Some(AnalysisDifference {
            subject: "whether the query can run as a subquery".to_owned(),
            first: state(first.can_run_as_subquery),
            second: state(second.can_run_as_subquery),
        });
    }
    if first.sql != second.sql {
        return Some(AnalysisDifference {
            subject: "the SQL analyzed".to_owned(),
            first: format!("`{}`", first.sql),
            second: format!("`{}`", second.sql),
        });
    }
    let first_rules = rules(first);
    let second_rules = rules(second);
    if first.simplifications != second.simplifications {
        return Some(AnalysisDifference {
            subject: "how the query was simplified".to_owned(),
            first: first_rules,
            second: second_rules,
        });
    }
    if first.values_lists != second.values_lists {
        return Some(AnalysisDifference {
            subject: "the query's VALUES lists".to_owned(),
            first: format!("{}", first.values_lists.len()),
            second: format!("{}", second.values_lists.len()),
        });
    }
    None
}

/// How a query was simplified, as the message says it.
fn rules(a: &AnalyzedQuery) -> String {
    if a.simplifications.is_empty() {
        return "not at all".to_owned();
    }
    let mut names: Vec<&str> = a.simplifications.iter().map(|s| s.rule.name()).collect();
    names.dedup();
    format!("by {}", names.join(", "))
}

fn parameters_differ(
    first: &[AnalyzedParam],
    second: &[AnalyzedParam],
) -> Option<AnalysisDifference> {
    if first.len() != second.len() {
        return Some(AnalysisDifference {
            subject: "the number of parameters".to_owned(),
            first: first.len().to_string(),
            second: second.len().to_string(),
        });
    }
    for (i, (a, b)) in first.iter().zip(second).enumerate() {
        let subject = format!("parameter \"${}\"", a.name);
        if a.name != b.name {
            return Some(AnalysisDifference {
                subject: format!("the name of parameter {}", i + 1),
                first: format!("\"${}\"", a.name),
                second: format!("\"${}\"", b.name),
            });
        }
        if a.pg_type != b.pg_type {
            return Some(AnalysisDifference {
                subject,
                first: a.pg_type.describe(),
                second: b.pg_type.describe(),
            });
        }
        if a.nullable != b.nullable {
            return Some(AnalysisDifference {
                subject: format!("{subject}'s nullability"),
                first: nullability(a.nullable),
                second: nullability(b.nullable),
            });
        }
        if a.sql_offsets != b.sql_offsets {
            return Some(AnalysisDifference {
                subject: format!("the number of placeholders of {subject}"),
                first: a.sql_offsets.len().to_string(),
                second: b.sql_offsets.len().to_string(),
            });
        }
    }
    None
}

fn spreads_differ(
    first: &[AnalyzedSpread],
    second: &[AnalyzedSpread],
) -> Option<AnalysisDifference> {
    if first.len() != second.len() {
        return Some(AnalysisDifference {
            subject: "the number of spreads".to_owned(),
            first: first.len().to_string(),
            second: second.len().to_string(),
        });
    }
    for (a, b) in first.iter().zip(second) {
        let subject = format!("spread \"$..{}\"", a.name);
        if a.name != b.name {
            return Some(AnalysisDifference {
                subject: "the names of the spreads".to_owned(),
                first: format!("\"$..{}\"", a.name),
                second: format!("\"$..{}\"", b.name),
            });
        }
        if a.kind != b.kind {
            return Some(AnalysisDifference {
                subject: format!("{subject}'s kind"),
                first: spread_kind(a.kind),
                second: spread_kind(b.kind),
            });
        }
        if a.offset != b.offset {
            return Some(AnalysisDifference {
                subject: format!("{subject}'s position"),
                first: a.offset.to_string(),
                second: b.offset.to_string(),
            });
        }
        if a.array_form != b.array_form {
            let state = |has: bool| {
                if has {
                    "written as one array parameter".to_owned()
                } else {
                    "written element by element".to_owned()
                }
            };
            return Some(AnalysisDifference {
                subject: format!("how {subject} is sent"),
                first: state(a.array_form.is_some()),
                second: state(b.array_form.is_some()),
            });
        }
        if a.fields.len() != b.fields.len() {
            return Some(AnalysisDifference {
                subject: format!("the number of fields of {subject}"),
                first: a.fields.len().to_string(),
                second: b.fields.len().to_string(),
            });
        }
        for (x, y) in a.fields.iter().zip(&b.fields) {
            let field = format!("field \"{}\" of {subject}", x.name);
            if x.name != y.name {
                return Some(AnalysisDifference {
                    subject: format!("the names of the fields of {subject}"),
                    first: format!("\"{}\"", x.name),
                    second: format!("\"{}\"", y.name),
                });
            }
            if x.pg_type != y.pg_type {
                return Some(AnalysisDifference {
                    subject: field,
                    first: x.pg_type.describe(),
                    second: y.pg_type.describe(),
                });
            }
            if x.nullable != y.nullable {
                return Some(AnalysisDifference {
                    subject: format!("{field}'s nullability"),
                    first: nullability(x.nullable),
                    second: nullability(y.nullable),
                });
            }
        }
    }
    None
}

fn columns_differ(
    first: &[AnalyzedColumn],
    second: &[AnalyzedColumn],
) -> Option<AnalysisDifference> {
    if first.len() != second.len() {
        return Some(AnalysisDifference {
            subject: "the number of columns".to_owned(),
            first: first.len().to_string(),
            second: second.len().to_string(),
        });
    }
    for (i, (a, b)) in first.iter().zip(second).enumerate() {
        if a.name != b.name {
            return Some(AnalysisDifference {
                subject: format!("the name of column {}", i + 1),
                first: format!("\"{}\"", a.name),
                second: format!("\"{}\"", b.name),
            });
        }
        let subject = format!("column \"{}\"", a.name);
        if a.pg_type != b.pg_type {
            return Some(AnalysisDifference {
                subject,
                first: a.pg_type.describe(),
                second: b.pg_type.describe(),
            });
        }
        if a.nullable != b.nullable {
            return Some(AnalysisDifference {
                subject: format!("{subject}'s nullability"),
                first: nullability(a.nullable),
                second: nullability(b.nullable),
            });
        }
        if a.refinement != b.refinement {
            return Some(AnalysisDifference {
                subject: format!("the set of values of {subject}"),
                first: refinement(&a.refinement),
                second: refinement(&b.refinement),
            });
        }
    }
    None
}

fn nullability(nullable: bool) -> String {
    if nullable {
        "nullable".to_owned()
    } else {
        "not nullable".to_owned()
    }
}

fn spread_kind(kind: SpreadKind) -> String {
    match kind {
        SpreadKind::Rows => "rows of values".to_owned(),
        SpreadKind::List => "a list of values".to_owned(),
    }
}

/// What every non-NULL value of a column is, as the message says it: the
/// values it is known to be one of, or any value.
fn refinement(refinement: &crate::Refinement) -> String {
    let Some(values) = &refinement.values else {
        return "any value".to_owned();
    };
    let listed: Vec<&str> = values.iter().map(String::as_str).collect();
    format!("one of {}", listed.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PgCatalog;

    /// Two roles whose `search_path`s make `items` resolve to tables with
    /// different `x` columns. Roles are cluster-wide, so each test names
    /// its own (`prefix`): the oracle runs the tests on one server.
    fn catalog(prefix: &str) -> PgCatalog {
        let ddl = "
        CREATE ROLE role_a;
        CREATE ROLE role_b;
        CREATE SCHEMA alternate;
        CREATE TABLE public.items (id int4 PRIMARY KEY, x int4 NOT NULL);
        CREATE TABLE alternate.items (id int4 PRIMARY KEY, x text NOT NULL);
        GRANT USAGE ON SCHEMA alternate TO role_a;
        GRANT SELECT ON public.items TO role_a, role_b;
        GRANT SELECT ON alternate.items TO role_a;
        ALTER ROLE role_a SET search_path = alternate, public;
        ALTER ROLE role_b SET search_path = public;
    "
        .replace("role_", &format!("{prefix}_role_"));
        let mut c = PgCatalog::new().unwrap();
        c.apply_migration("0001.sql", &ddl).unwrap();
        c
    }

    #[test]
    fn analyses_that_agree_have_no_difference() {
        let c = catalog("agree");
        let a = c
            .analyze_as("SELECT id FROM items", "agree_role_a")
            .unwrap();
        let b = c
            .analyze_as("SELECT id FROM items", "agree_role_b")
            .unwrap();
        assert_eq!(first_difference(&a, &b), None);
    }

    #[test]
    fn a_column_type_difference_names_the_column_and_both_types() {
        let c = catalog("type");
        let a = c.analyze_as("SELECT x FROM items", "type_role_a").unwrap();
        let b = c.analyze_as("SELECT x FROM items", "type_role_b").unwrap();
        let d = first_difference(&a, &b).expect("the roles resolve x differently");
        assert_eq!(d.subject, "column \"x\"");
        assert_eq!(d.first, "text");
        assert_eq!(d.second, "integer");
        assert_eq!(
            d.message("type_role_a", "type_role_b"),
            "query resolves differently for roles \"type_role_a\" and \"type_role_b\": column \"x\" is \
             text for \"type_role_a\" and integer for \"type_role_b\" (the roles' search_path settings \
             differ)"
        );
    }

    #[test]
    fn a_nullability_difference_is_reported() {
        let mut c = PgCatalog::new().unwrap();
        c.apply_migration(
            "0001.sql",
            "CREATE ROLE null_role_a;
             CREATE ROLE null_role_b;
             CREATE SCHEMA alternate;
             CREATE TABLE public.t (x int4);
             CREATE TABLE alternate.t (x int4 NOT NULL);
             GRANT USAGE ON SCHEMA alternate TO null_role_a;
             GRANT SELECT ON public.t TO null_role_a, null_role_b;
             GRANT SELECT ON alternate.t TO null_role_a;
             ALTER ROLE null_role_a SET search_path = alternate, public;
             ALTER ROLE null_role_b SET search_path = public;",
        )
        .unwrap();
        let a = c.analyze_as("SELECT x FROM t", "null_role_a").unwrap();
        let b = c.analyze_as("SELECT x FROM t", "null_role_b").unwrap();
        let d = first_difference(&a, &b).expect("x is not nullable for one role");
        assert_eq!(d.subject, "column \"x\"'s nullability");
        assert_eq!(d.first, "not nullable");
        assert_eq!(d.second, "nullable");
    }
}
