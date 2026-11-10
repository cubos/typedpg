//! The call-site rules: `fetch_may_return_many`, `fetch_all_single_row`
//! and `unused_returning`.
//!
//! They run over the query's [`QueryView`](typedpg_analyzer::QueryView) like
//! every other query rule: the methods called on the query result decide what
//! to report, and the analyzer's cardinality proof decides whether the
//! statement can return more than one row. A finding is placed at the method
//! identifier in the calling file — a query can be called from several
//! places, with different methods — while a fix edits the analyzed SQL text.

use typedpg_analyzer::statement_at_most_one_row;
use typedpg_pg_query::protobuf::RawStmt;
use typedpg_pg_query::protobuf::node::Node;

use crate::{Call, CallMethod, Ctx, Language, RawFinding, Rule, SqlEdit};

/// A raw finding placed at a call (or none), before the crate gives it its
/// severity and maps its fix.
pub(crate) struct RawCallFinding {
    pub call: Option<usize>,
    pub finding: RawFinding,
}

/// The findings of `rule` at the call sites `calls`, as written.
pub(crate) fn findings(rule: Rule, ctx: &Ctx<'_>, calls: &[Call]) -> Vec<RawCallFinding> {
    match rule {
        Rule::FetchMayReturnMany => calls
            .iter()
            .enumerate()
            .filter(|(_, call)| {
                matches!(
                    call.method,
                    CallMethod::FetchOne | CallMethod::FetchOptional
                ) && !statement_at_most_one_row(ctx.catalog, ctx.view)
            })
            .map(|(index, call)| RawCallFinding {
                call: Some(index),
                finding: fetch_may_return_many(call, ctx),
            })
            .collect(),
        Rule::FetchAllSingleRow => calls
            .iter()
            .enumerate()
            .filter(|(_, call)| {
                call.method == CallMethod::FetchAll
                    && statement_at_most_one_row(ctx.catalog, ctx.view)
            })
            .map(|(index, call)| RawCallFinding {
                call: Some(index),
                finding: fetch_all_single_row(call),
            })
            .collect(),
        Rule::UnusedReturning => unused_returning(ctx, calls),
        _ => Vec::new(),
    }
}

/// The finding for `fetch_one` / `fetch_optional` on a statement the proof
/// does not bound to one row.
fn fetch_may_return_many(call: &Call, ctx: &Ctx<'_>) -> RawFinding {
    RawFinding {
        span: statement_start(ctx),
        message: format!(
            "`.{}()` fails with TooManyRows if this query returns a second row, and nothing \
             in the schema limits it to one",
            call.name
        ),
        help: Some(match call.language {
            Language::Rust => {
                "filter by a unique key, add LIMIT 1 with an ORDER BY, or use .fetch_all()"
                    .to_owned()
            }
            Language::TypeScript => {
                "filter by a unique key, add LIMIT 1 with an ORDER BY, or use .fetchAll()"
                    .to_owned()
            }
        }),
        fix: Vec::new(),
    }
}

/// The finding for `fetch_all` on a statement the proof bounds to one row.
fn fetch_all_single_row(call: &Call) -> RawFinding {
    RawFinding {
        span: 0..0,
        message: format!("`.{}()` on a query that returns at most one row", call.name),
        help: Some(match call.language {
            Language::Rust => "use .fetch_optional() (or .fetch_one())".to_owned(),
            Language::TypeScript => "use .fetchOptional() (or .fetchOne())".to_owned(),
        }),
        fix: Vec::new(),
    }
}

/// The findings for `execute()` on a top-level `INSERT` / `UPDATE` /
/// `DELETE` / `MERGE` with a `RETURNING` list, whose rows `execute()`
/// discards.
///
/// Reported only when every call is an `execute()`: the caller leaves a
/// query that may escape (be exported, passed to another function, …) out of
/// `calls`. The fix deletes the clause only when every returning target is a
/// plain column reference (`id`, `t.id`, `*`, `t.*`) or a plain constant (an
/// `A_Const`, with no cast): anything else — an operator, a cast, a CASE, a
/// function call, a sublink, a row constructor, … — can raise an error when
/// evaluated, so removing it could change what the statement does and it is
/// reported without a fix. The fix is offered only when the query has exactly
/// one call, the one `execute()`, the target is an ordinary table without
/// enabled row security or rewrite rules (see [`returning_target_is_safe`]),
/// and no `RETURNING` expression may evaluate a virtual column.
fn unused_returning(ctx: &Ctx<'_>, calls: &[Call]) -> Vec<RawCallFinding> {
    if calls.is_empty() || calls.iter().any(|call| call.method != CallMethod::Execute) {
        return Vec::new();
    }
    let Some(raw) = ctx.view.tree.stmts.first() else {
        return Vec::new();
    };
    let Some(statement) = raw.stmt.as_deref().and_then(|node| node.node.as_ref()) else {
        return Vec::new();
    };
    let returning = match statement {
        Node::InsertStmt(stmt) => stmt.returning_clause.as_ref(),
        Node::UpdateStmt(stmt) => stmt.returning_clause.as_ref(),
        Node::DeleteStmt(stmt) => stmt.returning_clause.as_ref(),
        Node::MergeStmt(stmt) => stmt.returning_clause.as_ref(),
        _ => None,
    };
    let Some(returning) = returning else {
        return Vec::new();
    };
    let Some((keyword, end)) = returning_span(ctx, raw) else {
        return Vec::new();
    };
    let fix = if calls.len() == 1
        && returning.exprs.iter().all(is_plain_returning_target)
        && !may_read_virtual_columns(ctx, statement)
        && returning_target_is_safe(ctx, statement)
    {
        vec![SqlEdit {
            range: whitespace_start(ctx.sql, keyword)..end,
            text: String::new(),
        }]
    } else {
        Vec::new()
    };
    calls
        .iter()
        .enumerate()
        .map(|(index, call)| RawCallFinding {
            call: Some(index),
            finding: RawFinding {
                span: keyword..end,
                message: format!("`.{}()` discards the RETURNING rows", call.name),
                help: Some("remove RETURNING, or fetch the rows".to_owned()),
                fix: fix.clone(),
            },
        })
        .collect()
}

/// The first non-comment token of the query: `SELECT`, `INSERT`, …
fn statement_start(ctx: &Ctx<'_>) -> std::ops::Range<usize> {
    ctx.tokens
        .first()
        .map_or(0..0, |token| token.start..token.end)
}

/// The position of the top-level `RETURNING` keyword and the end of its
/// list, in `ctx.sql`.
///
/// The keyword is the last `RETURNING` outside parentheses of the statement
/// (a CTE's own `RETURNING` is nested), and the list ends at the statement's
/// last token, keeping any trailing `;`.
fn returning_span(ctx: &Ctx<'_>, raw: &RawStmt) -> Option<(usize, usize)> {
    let start = usize::try_from(raw.stmt_location).unwrap_or(0);
    let end = match usize::try_from(raw.stmt_len) {
        Ok(0) => ctx.sql.len(),
        Ok(len) => start.saturating_add(len).min(ctx.sql.len()),
        Err(_) => ctx.sql.len(),
    };
    let tokens: Vec<&crate::Tok> = ctx
        .tokens
        .iter()
        .filter(|token| token.start >= start && token.end <= end)
        .collect();
    let mut depth = 0usize;
    let mut keyword = None;
    for (index, token) in tokens.iter().enumerate() {
        let text = &ctx.sql[token.start..token.end];
        if text == "(" {
            depth += 1;
        } else if text == ")" {
            depth = depth.saturating_sub(1);
        } else if depth == 0 && text.eq_ignore_ascii_case("RETURNING") {
            keyword = Some(index);
        }
    }
    let keyword_index = keyword?;
    let keyword = tokens[keyword_index].start;
    let last = tokens[keyword_index..]
        .iter()
        .rev()
        .find(|token| &ctx.sql[token.start..token.end] != ";")?
        .end;
    Some((keyword, last))
}

/// The start of the whitespace run that ends at `at`, so deleting from here
/// takes the space before `RETURNING` with the clause.
fn whitespace_start(sql: &str, at: usize) -> usize {
    let mut start = at;
    while let Some(c) = sql[..start].chars().next_back() {
        if !c.is_whitespace() {
            break;
        }
        start -= c.len_utf8();
    }
    start
}

/// Whether `node` is a returning target whose evaluation cannot raise: a
/// plain column reference (`id`, `t.id`, `*`, `t.*`) or a plain constant (an
/// `A_Const`, with no cast). Everything else can raise an error at execution
/// — an operator, a cast, a CASE, a function call, a sublink, a row
/// constructor, … — so deleting it from a `RETURNING` the caller discards
/// could still change what the statement does.
///
/// A returning target is a `ResTarget`; its `val` is the expression.
fn is_plain_returning_target(node: &typedpg_pg_query::protobuf::Node) -> bool {
    let Some(Node::ResTarget(target)) = node.node.as_ref() else {
        return false;
    };
    matches!(
        target.val.as_deref().and_then(|val| val.node.as_ref()),
        Some(Node::ColumnRef(_) | Node::AConst(_))
    )
}

/// Whether deleting `RETURNING` from the statement leaves the same rows
/// affected and the same values evaluated.
///
/// Only an ordinary table without rewrite rules or enabled row security
/// qualifies. On a view (or any other relation kind) `RETURNING` can
/// evaluate an expression into the view's query that the write itself never
/// runs, so removing it can drop an error. With row security enabled,
/// PostgreSQL applies the table's SELECT policies to decide which rows the
/// write may affect when `RETURNING` is present; without it they don't, so
/// the rows deleted or updated can differ. A rewrite rule can likewise make
/// the statement's effect depend on whether `RETURNING` is present.
///
/// The same holds for every other relation the statement reads: a
/// `DELETE … USING v` or `UPDATE … FROM v` only needs the join to run the
/// write, and a `RETURNING v.bad` may then evaluate a view expression the
/// write alone never evaluates. A CTE can stand in for any of those
/// relations. So the fix also requires every `FROM` / `USING` item (and a
/// join inside one) to be an ordinary table without row security or
/// rewrite rules, and no `WITH` clause at all.
fn returning_target_is_safe(ctx: &Ctx<'_>, statement: &Node) -> bool {
    let (relation, with, items): (_, _, &[typedpg_pg_query::protobuf::Node]) = match statement {
        Node::InsertStmt(stmt) => (stmt.relation.as_ref(), stmt.with_clause.as_ref(), &[][..]),
        Node::UpdateStmt(stmt) => (
            stmt.relation.as_ref(),
            stmt.with_clause.as_ref(),
            &stmt.from_clause,
        ),
        Node::DeleteStmt(stmt) => (
            stmt.relation.as_ref(),
            stmt.with_clause.as_ref(),
            &stmt.using_clause,
        ),
        _ => return false,
    };
    if with.is_some() {
        return false;
    }
    let Some(relation) = relation else {
        return false;
    };
    relation_is_safe(ctx, relation) && items.iter().all(|item| from_item_is_safe(ctx, item))
}

/// Whether `relation` is an ordinary table without row security or rewrite
/// rules, so reading or writing it cannot make `RETURNING` part of the
/// statement's effect.
fn relation_is_safe(ctx: &Ctx<'_>, relation: &typedpg_pg_query::protobuf::RangeVar) -> bool {
    let schema = (!relation.schemaname.is_empty()).then_some(relation.schemaname.as_str());
    let Some(class) = ctx.catalog.resolve_table(schema, &relation.relname) else {
        return false;
    };
    class.relkind == typedpg_analyzer::RelKind::Table
        && !ctx.catalog.lint_relation_has_row_security(class.oid)
        && !ctx.catalog.lint_relation_has_rules(class.oid)
}

/// Whether a `FROM` / `USING` item is an ordinary table (or a join of them)
/// without row security or rewrite rules.
fn from_item_is_safe(ctx: &Ctx<'_>, item: &typedpg_pg_query::protobuf::Node) -> bool {
    match item.node.as_ref() {
        Some(Node::RangeVar(relation)) => relation_is_safe(ctx, relation),
        Some(Node::JoinExpr(join)) => {
            join.larg
                .as_deref()
                .is_some_and(|left| from_item_is_safe(ctx, left))
                && join
                    .rarg
                    .as_deref()
                    .is_some_and(|right| from_item_is_safe(ctx, right))
        }
        _ => false,
    }
}

/// Whether the statement's `RETURNING` may read a virtual generated column:
/// reading one evaluates its expression, which can raise, so removing even a
/// plain column reference could drop an error. Conservative: any relation
/// of the statement whose name matches a table with a virtual column (in
/// any schema), and any `FROM` / `USING` item that is not a plain table
/// (or a MERGE, whose source can be anything), count.
fn may_read_virtual_columns(ctx: &Ctx<'_>, statement: &Node) -> bool {
    let (target, from): (_, &[typedpg_pg_query::protobuf::Node]) = match statement {
        Node::InsertStmt(stmt) => (stmt.relation.as_ref(), &[]),
        Node::UpdateStmt(stmt) => (stmt.relation.as_ref(), &stmt.from_clause),
        Node::DeleteStmt(stmt) => (stmt.relation.as_ref(), &stmt.using_clause),
        _ => return true,
    };
    let Some(target) = target else {
        return true;
    };
    let mut names = vec![target.relname.as_str()];
    let mut items: Vec<&typedpg_pg_query::protobuf::Node> = from.iter().collect();
    while let Some(item) = items.pop() {
        match item.node.as_ref() {
            Some(Node::RangeVar(range)) => names.push(range.relname.as_str()),
            Some(Node::JoinExpr(join)) => {
                items.extend(join.larg.as_deref());
                items.extend(join.rarg.as_deref());
            }
            _ => return true,
        }
    }
    ctx.catalog.iter_classes().any(|class| {
        names.contains(&class.relname.as_str())
            && ctx
                .catalog
                .attributes_of(class.oid)
                .iter()
                .any(|attribute| {
                    attribute.attgenerated == Some(typedpg_analyzer::AttGenerated::Virtual)
                })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use typedpg_analyzer::PgCatalog;

    use crate::{CallFinding, Finding, LintOptions, Severity, lint_query_called};

    fn catalog() -> PgCatalog {
        let mut catalog = PgCatalog::new().unwrap();
        catalog
            .apply_sql(
                "CREATE TABLE users (id integer PRIMARY KEY, org_id integer, name text);
                 CREATE TABLE orgs (id integer PRIMARY KEY, name text);",
            )
            .unwrap();
        catalog
    }

    fn rust(method: CallMethod, name: &str) -> Call {
        Call {
            method,
            name: name.to_owned(),
            language: Language::Rust,
        }
    }

    fn ts(method: CallMethod, name: &str) -> Call {
        Call {
            method,
            name: name.to_owned(),
            language: Language::TypeScript,
        }
    }

    fn lint(catalog: &PgCatalog, sql: &str, calls: &[Call]) -> Vec<CallFinding> {
        lint_query_called(catalog, sql, calls, &LintOptions::default()).unwrap()
    }

    fn only(findings: Vec<CallFinding>, rule: Rule) -> Vec<CallFinding> {
        findings
            .into_iter()
            .filter(|placed| placed.finding.rule == rule)
            .collect()
    }

    /// Apply a finding's fix to `sql`.
    fn fixed(sql: &str, finding: &Finding) -> String {
        let mut fixed = sql.to_owned();
        for edit in finding.fix.iter().rev() {
            fixed.replace_range(edit.range.clone(), &edit.text);
        }
        fixed
    }

    #[test]
    fn fetch_one_on_a_unique_key_is_fine() {
        let db = catalog();
        let sql = "SELECT id, name FROM users WHERE id = $1";
        for call in [
            rust(CallMethod::FetchOne, "fetch_one"),
            rust(CallMethod::FetchOptional, "fetch_optional"),
        ] {
            assert!(only(lint(&db, sql, &[call]), Rule::FetchMayReturnMany).is_empty());
        }
    }

    #[test]
    fn fetch_one_on_a_non_unique_filter_may_return_many() {
        let db = catalog();
        let sql = "SELECT id, name FROM users WHERE org_id = $1";
        let findings = only(
            lint(&db, sql, &[rust(CallMethod::FetchOne, "fetch_one")]),
            Rule::FetchMayReturnMany,
        );
        assert_eq!(findings.len(), 1, "{findings:?}");
        let placed = &findings[0];
        assert_eq!(placed.call, Some(0));
        let finding = &placed.finding;
        assert_eq!(finding.severity, Severity::Warning);
        assert_eq!(
            finding.message,
            "`.fetch_one()` fails with TooManyRows if this query returns a second row, and \
             nothing in the schema limits it to one"
        );
        assert_eq!(
            finding.help.as_deref(),
            Some("filter by a unique key, add LIMIT 1 with an ORDER BY, or use .fetch_all()")
        );
    }

    #[test]
    fn each_offending_call_gets_its_own_finding() {
        let db = catalog();
        let sql = "SELECT id, name FROM users WHERE org_id = $1";
        let findings = only(
            lint(
                &db,
                sql,
                &[
                    rust(CallMethod::FetchOne, "fetch_one"),
                    ts(CallMethod::FetchOptional, "fetchOptional"),
                ],
            ),
            Rule::FetchMayReturnMany,
        );
        assert_eq!(findings.len(), 2, "{findings:?}");
        assert_eq!(findings[0].call, Some(0));
        assert_eq!(findings[1].call, Some(1));
        assert_eq!(
            findings[1].finding.message,
            "`.fetchOptional()` fails with TooManyRows if this query returns a second row, and \
             nothing in the schema limits it to one"
        );
        assert_eq!(
            findings[1].finding.help.as_deref(),
            Some("filter by a unique key, add LIMIT 1 with an ORDER BY, or use .fetchAll()")
        );
    }

    #[test]
    fn fetch_optional_names_its_own_method() {
        let db = catalog();
        let sql = "SELECT id, name FROM users WHERE org_id = $1";
        let findings = only(
            lint(
                &db,
                sql,
                &[rust(CallMethod::FetchOptional, "fetch_optional_as")],
            ),
            Rule::FetchMayReturnMany,
        );
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(
            findings[0]
                .finding
                .message
                .starts_with("`.fetch_optional_as()` fails with TooManyRows"),
            "{}",
            findings[0].finding.message
        );
    }

    #[test]
    fn a_limit_of_one_is_proof_enough() {
        let db = catalog();
        let sql = "SELECT id, name FROM users LIMIT 1";
        assert!(
            only(
                lint(&db, sql, &[rust(CallMethod::FetchOne, "fetch_one")]),
                Rule::FetchMayReturnMany
            )
            .is_empty()
        );
    }

    #[test]
    fn fetch_optional_on_an_aggregate_is_fine() {
        let db = catalog();
        let sql = "SELECT count(*) FROM users";
        assert!(
            only(
                lint(
                    &db,
                    sql,
                    &[rust(CallMethod::FetchOptional, "fetch_optional")]
                ),
                Rule::FetchMayReturnMany
            )
            .is_empty()
        );
    }

    #[test]
    fn fetch_one_on_an_aggregate_over_a_subquery_is_fine() {
        let mut db = catalog();
        db.apply_sql(
            "CREATE TABLE a (id integer PRIMARY KEY, due timestamptz);
             CREATE TABLE b (id integer PRIMARY KEY, due timestamptz);",
        )
        .unwrap();
        let sql = "SELECT min(due) AS soonest FROM \
                   (SELECT due FROM a UNION ALL SELECT due FROM b) q";
        assert!(
            only(
                lint(&db, sql, &[rust(CallMethod::FetchOne, "fetch_one")]),
                Rule::FetchMayReturnMany
            )
            .is_empty()
        );
    }

    #[test]
    fn fetch_optional_on_an_enum_key_is_fine() {
        let mut db = catalog();
        db.apply_sql(
            "CREATE TYPE color AS ENUM ('red', 'green');
             CREATE TABLE paints (
                id bigint PRIMARY KEY,
                shop text NOT NULL,
                color color NOT NULL,
                UNIQUE (shop, color)
             );",
        )
        .unwrap();
        let sql = "SELECT id FROM paints WHERE shop = $1 AND color = 'red'";
        assert!(
            only(
                lint(
                    &db,
                    sql,
                    &[rust(CallMethod::FetchOptional, "fetch_optional")]
                ),
                Rule::FetchMayReturnMany
            )
            .is_empty()
        );
    }

    #[test]
    fn fetch_all_on_a_single_row_query_is_reported() {
        let db = catalog();
        let sql = "SELECT id, name FROM users WHERE id = $1";
        let findings = only(
            lint(&db, sql, &[ts(CallMethod::FetchAll, "fetchAll")]),
            Rule::FetchAllSingleRow,
        );
        assert_eq!(findings.len(), 1, "{findings:?}");
        let placed = &findings[0];
        assert_eq!(placed.call, Some(0));
        let finding = &placed.finding;
        assert_eq!(finding.severity, Severity::Info);
        assert_eq!(
            finding.message,
            "`.fetchAll()` on a query that returns at most one row"
        );
        assert_eq!(
            finding.help.as_deref(),
            Some("use .fetchOptional() (or .fetchOne())")
        );
    }

    #[test]
    fn fetch_all_on_a_non_unique_query_is_fine() {
        let db = catalog();
        let sql = "SELECT id, name FROM users WHERE org_id = $1";
        assert!(
            only(
                lint(&db, sql, &[rust(CallMethod::FetchAll, "fetch_all_as")]),
                Rule::FetchAllSingleRow
            )
            .is_empty()
        );
    }

    #[test]
    fn execute_on_a_returning_insert_reports_and_fixes() {
        let db = catalog();
        let sql = "INSERT INTO users (id, name) VALUES (1, 'x') RETURNING id";
        let findings = only(
            lint(&db, sql, &[ts(CallMethod::Execute, "execute")]),
            Rule::UnusedReturning,
        );
        assert_eq!(findings.len(), 1, "{findings:?}");
        let placed = &findings[0];
        assert_eq!(placed.call, Some(0));
        let finding = &placed.finding;
        assert_eq!(finding.severity, Severity::Info);
        assert_eq!(finding.message, "`.execute()` discards the RETURNING rows");
        assert_eq!(
            finding.help.as_deref(),
            Some("remove RETURNING, or fetch the rows")
        );
        assert_eq!(
            fixed(sql, finding),
            "INSERT INTO users (id, name) VALUES (1, 'x')"
        );
    }

    #[test]
    fn a_row_security_target_keeps_the_finding_without_the_fix() {
        let mut db = catalog();
        db.apply_sql(
            "CREATE TABLE secured (id integer PRIMARY KEY);
             ALTER TABLE secured ENABLE ROW LEVEL SECURITY;
             CREATE POLICY readable ON secured FOR SELECT USING (id = 1);
             CREATE POLICY deletable ON secured FOR DELETE USING (true);",
        )
        .unwrap();
        let sql = "DELETE FROM secured WHERE true RETURNING id";
        let findings = only(
            lint(&db, sql, &[rust(CallMethod::Execute, "execute")]),
            Rule::UnusedReturning,
        );
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(
            findings[0].finding.fix.is_empty(),
            "row security makes RETURNING decide which rows the DELETE sees: {:?}",
            findings[0].finding.fix
        );
    }

    #[test]
    fn a_view_target_keeps_the_finding_without_the_fix() {
        let mut db = catalog();
        db.apply_sql(
            "CREATE TABLE backing (id integer NOT NULL);
             CREATE VIEW writable AS SELECT id, 1 / (id - 1) AS bad FROM backing;",
        )
        .unwrap();
        let sql = "INSERT INTO writable (id) VALUES ($1) RETURNING bad";
        let findings = only(
            lint(&db, sql, &[rust(CallMethod::Execute, "execute")]),
            Rule::UnusedReturning,
        );
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(
            findings[0].finding.fix.is_empty(),
            "RETURNING evaluates the view expression the INSERT alone does not: {:?}",
            findings[0].finding.fix
        );
    }

    #[test]
    fn a_view_in_using_or_from_keeps_the_finding_without_the_fix() {
        let mut db = catalog();
        db.apply_sql(
            "CREATE TABLE t (id integer NOT NULL);
             CREATE VIEW v AS SELECT id, 1 / (id - 1) AS bad FROM t;",
        )
        .unwrap();
        for sql in [
            // The DELETE only joins `v` for the row identity; `v.bad` is
            // evaluated by RETURNING alone, so deleting RETURNING would
            // stop raising the division by zero.
            "DELETE FROM t USING v WHERE t.id = v.id RETURNING v.bad",
            "UPDATE users SET name = 'x' FROM v WHERE users.id = v.id RETURNING v.bad",
            // A CTE can stand in for the view the same way.
            "WITH c AS (SELECT id, 1 / (id - 1) AS bad FROM t) \
             DELETE FROM t USING c WHERE t.id = c.id RETURNING c.bad",
        ] {
            let findings = only(
                lint(&db, sql, &[rust(CallMethod::Execute, "execute")]),
                Rule::UnusedReturning,
            );
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert!(
                findings[0].finding.fix.is_empty(),
                "{sql}: RETURNING evaluates an expression the write alone does not: {:?}",
                findings[0].finding.fix
            );
        }
        // An ordinary table in USING keeps the fix: the join runs either way.
        let sql = "DELETE FROM t USING users WHERE t.id = users.id RETURNING t.id";
        let findings = only(
            lint(&db, sql, &[rust(CallMethod::Execute, "execute")]),
            Rule::UnusedReturning,
        );
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(!findings[0].finding.fix.is_empty(), "{findings:?}");
    }

    #[test]
    fn a_table_with_a_rewrite_rule_keeps_the_finding_without_the_fix() {
        let mut db = catalog();
        db.apply_sql(
            "CREATE TABLE ruled (id integer PRIMARY KEY);
             CREATE RULE ruled_noop AS ON UPDATE TO ruled DO INSTEAD NOTHING;",
        )
        .unwrap();
        let sql = "DELETE FROM ruled WHERE true RETURNING id";
        let findings = only(
            lint(&db, sql, &[rust(CallMethod::Execute, "execute")]),
            Rule::UnusedReturning,
        );
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(
            findings[0].finding.fix.is_empty(),
            "a rewrite rule can make RETURNING part of the statement's effect: {:?}",
            findings[0].finding.fix
        );
    }

    #[test]
    fn execute_on_a_returning_update_with_a_call_reports_without_a_fix() {
        let db = catalog();
        let sql = "UPDATE users SET name = 'x' WHERE id = $1 RETURNING now()";
        let findings = only(
            lint(&db, sql, &[rust(CallMethod::Execute, "execute")]),
            Rule::UnusedReturning,
        );
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(
            findings[0].finding.fix.is_empty(),
            "{:?}",
            findings[0].finding.fix
        );
    }

    #[test]
    fn execute_on_a_returning_star_reports_and_fixes() {
        let db = catalog();
        for sql in [
            "UPDATE users SET name = 'x' WHERE id = 1 RETURNING *",
            "UPDATE users SET name = 'x' WHERE id = 1 RETURNING users.*",
        ] {
            let findings = only(
                lint(&db, sql, &[rust(CallMethod::Execute, "execute")]),
                Rule::UnusedReturning,
            );
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            let expected = sql.split(" RETURNING").next().unwrap();
            assert_eq!(fixed(sql, &findings[0].finding), expected, "{sql}");
        }
    }

    #[test]
    fn execute_on_a_returning_value_that_can_raise_reports_without_a_fix() {
        let db = catalog();
        for sql in [
            // An operator: division by zero aborts the statement (and a
            // transaction) even though the caller discards the row.
            "UPDATE users SET name = 'x' WHERE id = 1 RETURNING 1 / id",
            // A cast can raise.
            "UPDATE users SET name = 'x' WHERE id = 1 RETURNING id::text",
            // A function call can raise.
            "UPDATE users SET name = 'x' WHERE id = 1 RETURNING lower(name)",
        ] {
            let findings = only(
                lint(&db, sql, &[rust(CallMethod::Execute, "execute")]),
                Rule::UnusedReturning,
            );
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert!(
                findings[0].finding.fix.is_empty(),
                "{sql}: {:?}",
                findings[0].finding.fix
            );
        }
    }

    #[test]
    fn a_returning_that_may_read_a_virtual_column_has_no_fix() {
        let mut db = catalog();
        db.apply_sql(
            "CREATE TABLE t (id int PRIMARY KEY, z int, bad int GENERATED ALWAYS AS (1 / z) VIRTUAL);",
        )
        .unwrap();
        for sql in [
            "DELETE FROM t WHERE id = 1 RETURNING bad",
            "DELETE FROM t WHERE id = 1 RETURNING id",
            "DELETE FROM t WHERE id = 1 RETURNING *",
            "UPDATE users SET name = 'x' FROM t WHERE users.id = t.id RETURNING users.id",
        ] {
            let findings = only(
                lint(&db, sql, &[rust(CallMethod::Execute, "execute")]),
                Rule::UnusedReturning,
            );
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert!(findings[0].finding.fix.is_empty(), "{sql}");
        }
    }

    #[test]
    fn execute_without_returning_is_fine() {
        let db = catalog();
        let sql = "UPDATE users SET name = 'x' WHERE id = $1";
        assert!(
            only(
                lint(&db, sql, &[rust(CallMethod::Execute, "execute")]),
                Rule::UnusedReturning
            )
            .is_empty()
        );
    }

    #[test]
    fn unused_returning_needs_every_call_to_be_execute() {
        let db = catalog();
        let sql = "INSERT INTO users (id, name) VALUES (1, 'x') RETURNING id";
        let findings = lint(
            &db,
            sql,
            &[
                ts(CallMethod::Execute, "execute"),
                ts(CallMethod::FetchOne, "fetchOne"),
            ],
        );
        assert!(only(findings, Rule::UnusedReturning).is_empty());
    }

    #[test]
    fn unused_returning_with_two_executes_reports_without_a_fix() {
        let db = catalog();
        let sql = "INSERT INTO users (id, name) VALUES (1, 'x') RETURNING id";
        let findings = only(
            lint(
                &db,
                sql,
                &[
                    ts(CallMethod::Execute, "execute"),
                    ts(CallMethod::Execute, "execute"),
                ],
            ),
            Rule::UnusedReturning,
        );
        assert_eq!(findings.len(), 2, "{findings:?}");
        assert!(findings.iter().all(|placed| placed.finding.fix.is_empty()));
        assert_eq!(findings[0].call, Some(0));
        assert_eq!(findings[1].call, Some(1));
    }

    #[test]
    fn no_call_site_reports_none_of_them() {
        let db = catalog();
        for sql in [
            "SELECT id, name FROM users WHERE org_id = $1",
            "SELECT id, name FROM users WHERE id = $1",
            "INSERT INTO users (id, name) VALUES (1, 'x') RETURNING id",
        ] {
            let findings = lint(&db, sql, &[]);
            for rule in [
                Rule::FetchMayReturnMany,
                Rule::FetchAllSingleRow,
                Rule::UnusedReturning,
            ] {
                assert!(only(findings.clone(), rule).is_empty(), "{sql}: {rule}");
            }
        }
    }

    #[test]
    fn an_allow_comment_suppresses_the_call_site_rules() {
        let db = catalog();
        let sql = "-- typedpg-lint: allow(fetch_may_return_many)\n\
                   SELECT id, name FROM users WHERE org_id = $1";
        assert!(
            only(
                lint(&db, sql, &[rust(CallMethod::FetchOne, "fetch_one")]),
                Rule::FetchMayReturnMany
            )
            .is_empty()
        );
        let sql = "-- typedpg-lint: allow(unused_returning)\n\
                   INSERT INTO users (id, name) VALUES (1, 'x') RETURNING id";
        assert!(
            only(
                lint(&db, sql, &[rust(CallMethod::Execute, "execute")]),
                Rule::UnusedReturning
            )
            .is_empty()
        );
    }
}
