//! The `unstable_order` rule: reports an `ORDER BY` whose keys two rows
//! can share, under a `LIMIT`, `OFFSET` / `FETCH` or `DISTINCT ON` that
//! picks rows by that order. PG returns tied rows in no particular order,
//! and not the same one each time: a page cut through a run of ties may
//! repeat or skip rows on the next page, `LIMIT 1` may return either row,
//! and `DISTINCT ON` may keep either row of a group.
//!
//! The keys are unique when, with the columns `WHERE` (or the `ON` of an
//! inner join no outer join null-extends) equates to constants, parameters,
//! columns of an enclosing query level (one value per evaluation) or
//! columns of a table the keys determine, they cover a unique key (NOT NULL, or `NULLS NOT
//! DISTINCT`; `col COLLATE c` counts as `col` under a deterministic `c`)
//! of every table — or of the tables the others determine
//! through equalities to a key (a join on a primary key) — or, for a
//! grouped query, every grouping column (see
//! [`typedpg_analyzer::order_ties`]). A query returning at most one row is
//! not reported (with `DISTINCT ON`, one with at most one row before it: a
//! single group still picks among its rows), nor `FETCH … WITH TIES` (it returns every tie), a
//! `SKIP LOCKED` queue, an `EXISTS` subquery or a batching `IN` in an
//! `UPDATE` / `DELETE` (which don't care which rows), a select whose
//! output is the same for every row, a key that calls a VOLATILE function
//! (`ORDER BY random() LIMIT 5` samples on purpose), or a `DISTINCT ON`
//! whose select list the `DISTINCT ON` expressions determine (which tied
//! row of a group is kept then doesn't show; see
//! [`typedpg_analyzer::distinct_on_output_determined`]). Nothing is
//! reported when the proof can't see the whole FROM clause (a subquery, a
//! CTE, a function, a partitioned or inheritance table, a partial or
//! expression unique index): the keys may be unique there.
//!
//! No `ORDER BY` at all is `limit_without_order`'s finding. No fix is
//! offered: which tiebreaker to add is the author's choice. The help names
//! the fewest undetermined tables' primary keys that, added to the keys,
//! make the order unique.

use typedpg_analyzer::{
    OrderTies, ProVolatile, distinct_on_output_determined, order_ties, select_at_most_one_row,
};
use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{AConstValue, LimitOption, Node, SelectStmt};

use super::limit_without_order::{
    batched_subqueries, constant_target, distinct_on_span, has_skip_locked, invariant_target,
    limit_is_zero, limit_span, offset_span, picks_from_at_most_one_row,
};
use super::{leftmost_start, scan_until, starts_clause};
use crate::{Ctx, RawFinding};

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let batched = batched_subqueries(ctx);
    let mut out = Vec::new();
    for (node, _) in ctx.tree.nodes() {
        let NodeRef::SelectStmt(select) = node else {
            continue;
        };
        if select.sort_clause.is_empty() || select.limit_option == LimitOption::WithTies {
            continue;
        }
        let picks = if distinct_on_span(ctx, select).is_some() {
            "DISTINCT ON"
        } else if let Some(span) = limit_span(ctx, select) {
            if ctx.sql[span].to_ascii_uppercase().starts_with("FETCH") {
                "FETCH"
            } else {
                "LIMIT"
            }
        } else if offset_span(ctx, select).is_some() {
            "OFFSET"
        } else {
            continue;
        };
        let pointer: *const SelectStmt = select;
        if batched.contains(&pointer)
            || has_skip_locked(select)
            || limit_is_zero(select)
            || constant_target(ctx, select)
            || invariant_target(ctx, select)
            || if picks == "DISTINCT ON" {
                picks_from_at_most_one_row(ctx, select)
            } else {
                select_at_most_one_row(ctx.catalog, ctx.view, select)
            }
        {
            continue;
        }
        if picks == "DISTINCT ON" && distinct_on_output_determined(ctx.catalog, ctx.view, select) {
            continue;
        }
        if volatile_order_key(ctx, select) {
            continue;
        }
        let OrderTies::Ties(ties) = order_ties(ctx.catalog, ctx.view, select) else {
            continue;
        };
        let Some(span) = order_by_span(ctx, select) else {
            continue;
        };
        let names: Vec<String> = ties
            .iter()
            .map(|tie| typedpg_core::quote_identifier(&tie.name))
            .collect();
        let what = if picks == "DISTINCT ON" {
            "DISTINCT ON keeps an arbitrary row of the tied ones in each group"
        } else if limit_is_one(select) && offset_span(ctx, select).is_none() {
            "which of the tied rows comes back is arbitrary"
        } else {
            "rows that tie come back in an arbitrary order, so pages may skip or repeat rows"
        };
        let message = format!(
            "the ORDER BY keys can tie: they don't determine a unique row of {} — under {picks}, \
             {what}",
            names.join(", ")
        );
        let tiebreakers: Vec<String> = ties
            .iter()
            .filter(|tie| tie.tiebreak && !tie.primary_key.is_empty())
            .map(|tie| {
                let table = typedpg_core::quote_identifier(&tie.name);
                tie.primary_key
                    .iter()
                    .map(|column| format!("{table}.{}", typedpg_core::quote_identifier(column)))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .collect();
        let help = if tiebreakers.is_empty() {
            "add a unique column as the last ORDER BY key".to_owned()
        } else {
            format!(
                "add a unique tiebreaker as the last ORDER BY key, such as the primary key: {}",
                tiebreakers.join(", ")
            )
        };
        out.push(RawFinding {
            span,
            message,
            help: Some(help),
            fix: Vec::new(),
        });
    }
    out
}

/// Whether any of `select`'s `ORDER BY` keys calls a function or operator
/// that is VOLATILE: the keys then sort by a fresh value, and ties aren't
/// the author's concern (`ORDER BY random() LIMIT 5` samples on purpose).
fn volatile_order_key(ctx: &Ctx<'_>, select: &SelectStmt) -> bool {
    select.sort_clause.iter().any(|item| {
        let Some(Node::SortBy(sort)) = item else {
            return false;
        };
        key_expression(select, sort.node.as_ref())
            .is_some_and(|key| volatile_call(ctx, key.to_ref()))
    })
}

/// The expression an `ORDER BY` item sorts by: an output column's
/// expression for a bare name the select list outputs (SQL92's rule, which
/// PG tries first) or for an ordinal, else the item itself.
fn key_expression<'a>(select: &'a SelectStmt, node: Option<&'a Node>) -> Option<&'a Node> {
    let targets = || {
        select.target_list.iter().filter_map(|target| match target {
            Some(Node::ResTarget(target)) => Some(target),
            _ => None,
        })
    };
    match node? {
        Node::AConst(value) => {
            let Some(AConstValue::Integer(index)) = &value.val else {
                return None;
            };
            let index = usize::try_from(index.ival).ok()?.checked_sub(1)?;
            targets().nth(index)?.val.as_ref()
        }
        key @ Node::ColumnRef(column) => {
            let names: Vec<&str> = column
                .fields
                .iter()
                .filter_map(|field| match field {
                    Some(Node::String(name)) => Some(name.sval.as_str()),
                    _ => None,
                })
                .collect();
            let [name] = names.as_slice() else {
                return Some(key);
            };
            let mut named = targets().filter(|target| {
                target.name == *name
                    || (target.name.is_empty()
                        && matches!(target.val.as_ref(), Some(Node::ColumnRef(reference))
                            if matches!(reference.fields.as_slice(), [Some(Node::String(column))]
                                if column.sval == *name)))
            });
            match named.next() {
                Some(target) => Some(target.val.as_ref()?),
                None => Some(key),
            }
        }
        key => Some(key),
    }
}

/// Whether `root` calls a function or operator any overload of which may be
/// VOLATILE (`random()`, `gen_random_uuid()`, `clock_timestamp()`, …), an
/// unknown one included.
fn volatile_call(ctx: &Ctx<'_>, root: NodeRef<'_>) -> bool {
    let volatile_proc = |oid| {
        ctx.catalog
            .pg_proc()
            .get(&oid)
            .is_none_or(|proc| proc.provolatile == ProVolatile::Volatile)
    };
    let volatile_operator = |name: Option<&str>| {
        name.is_some_and(|name| {
            ctx.catalog.pg_operator().values().any(|operator| {
                operator.oprname == name && operator.oprcode.is_none_or(volatile_proc)
            })
        })
    };
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        match node {
            NodeRef::FuncCall(call) => {
                let names: Vec<&str> = call
                    .funcname
                    .iter()
                    .filter_map(|node| match node {
                        Some(Node::String(name)) => Some(name.sval.as_str()),
                        _ => None,
                    })
                    .collect();
                let procs = match names.as_slice() {
                    [name] => ctx.catalog.find_functions(None, name),
                    [schema, name] => ctx.catalog.find_functions(Some(schema), name),
                    _ => return true,
                };
                if procs.is_empty()
                    || procs
                        .iter()
                        .any(|proc| proc.provolatile == ProVolatile::Volatile)
                {
                    return true;
                }
            }
            NodeRef::AExpr(expr) if volatile_operator(super::operator_name(expr)) => {
                return true;
            }
            _ => {}
        }
        stack.extend(node.children());
    }
    false
}

/// Whether the row count is the literal 1 (`LIMIT 1`, `FETCH FIRST 1 ROW
/// ONLY`).
fn limit_is_one(select: &SelectStmt) -> bool {
    matches!(
        select.limit_count.as_ref(),
        Some(Node::AConst(value))
            if !value.isnull
                && matches!(value.val, Some(AConstValue::Integer(ref count)) if count.ival == 1)
    )
}

/// The `ORDER BY …` clause of `select`.
fn order_by_span(ctx: &Ctx<'_>, select: &SelectStmt) -> Option<std::ops::Range<usize>> {
    let first = match select.sort_clause.first()?.as_ref()? {
        Node::SortBy(sort) => sort.node.as_ref().and_then(leftmost_start)?,
        _ => return None,
    };
    let order = ctx
        .tokens
        .iter()
        .filter(|token| token.end <= first)
        .rfind(|token| ctx.sql[token.start..token.end].eq_ignore_ascii_case("ORDER"))?;
    let (end, _) = scan_until(ctx, first, starts_clause);
    Some(order.start..end.max(first + 1))
}

#[cfg(test)]
mod tests {
    use typedpg_analyzer::PgCatalog;

    use crate::Rule;
    use crate::rules::test_support::lint_rule;

    const RULE: Rule = Rule::UnstableOrder;

    fn catalog() -> PgCatalog {
        let mut catalog = PgCatalog::new().unwrap();
        catalog
            .apply_sql(
                "CREATE TABLE users (
                    id bigint PRIMARY KEY,
                    tenant int NOT NULL,
                    email text NOT NULL,
                    nick text UNIQUE,
                    name text,
                    created_at timestamptz NOT NULL,
                    UNIQUE (tenant, email)
                 );
                 CREATE TABLE posts (
                    id bigint PRIMARY KEY,
                    user_id bigint NOT NULL REFERENCES users,
                    created_at timestamptz NOT NULL
                 );
                 CREATE TABLE logs (at timestamptz NOT NULL, msg text);
                 CREATE TABLE slugs (id int PRIMARY KEY, slug text NOT NULL);
                 CREATE UNIQUE INDEX ON slugs (lower(slug));
                 CREATE TABLE src (id int PRIMARY KEY);
                 CREATE TABLE ver (
                    source_id int NOT NULL REFERENCES src,
                    version int NOT NULL,
                    digest text,
                    PRIMARY KEY (source_id, version)
                 );
                 CREATE TABLE mig (name text PRIMARY KEY);
                 CREATE TABLE membership (
                    id bigint PRIMARY KEY,
                    user_id bigint NOT NULL REFERENCES users
                 );
                 CREATE TABLE prize (
                    id bigint PRIMARY KEY,
                    membership_id bigint NOT NULL REFERENCES membership
                 );
                 CREATE TABLE source (id int PRIMARY KEY);
                 CREATE TABLE chunk (
                    id int PRIMARY KEY,
                    source_id int NOT NULL REFERENCES source,
                    score real NOT NULL
                 );
                 CREATE TABLE a (id int PRIMARY KEY, b_id int NOT NULL, x int NOT NULL);
                 CREATE TABLE b (id int PRIMARY KEY, c_id int NOT NULL);
                 CREATE TABLE c (id int PRIMARY KEY);
                 CREATE TABLE orders (id int PRIMARY KEY, ref int NOT NULL, total int NOT NULL);
                 CREATE TABLE shipments (id int PRIMARY KEY, ref int NOT NULL);
                 CREATE FUNCTION jitter(int, int) RETURNS int VOLATILE LANGUAGE sql
                    AS 'SELECT $1 + $2 + (random() * 10)::int';
                 CREATE OPERATOR <+> (LEFTARG = int, RIGHTARG = int, FUNCTION = jitter);
                 CREATE COLLATION ci (provider = icu, locale = 'und-u-ks-level2', \
                    deterministic = false);",
            )
            .unwrap();
        catalog
    }

    #[test]
    fn keys_that_can_tie_are_reported() {
        let db = catalog();
        for (sql, help) in [
            (
                "SELECT id FROM users ORDER BY created_at LIMIT 10",
                Some("users.id"),
            ),
            (
                "SELECT id FROM users ORDER BY created_at DESC LIMIT $1 OFFSET $2",
                Some("users.id"),
            ),
            (
                "SELECT id FROM users ORDER BY name OFFSET 20",
                Some("users.id"),
            ),
            (
                "SELECT id FROM users ORDER BY created_at FETCH FIRST 5 ROWS ONLY",
                Some("users.id"),
            ),
            // A nullable UNIQUE column: NULLs tie.
            (
                "SELECT id FROM users ORDER BY nick LIMIT 10",
                Some("users.id"),
            ),
            // Part of a key.
            (
                "SELECT id FROM users ORDER BY tenant LIMIT 10",
                Some("users.id"),
            ),
            // The users' key doesn't determine their posts.
            (
                "SELECT p.id FROM users u JOIN posts p ON p.user_id = u.id ORDER BY u.id LIMIT 10",
                Some("p.id"),
            ),
            (
                "SELECT DISTINCT ON (user_id) id FROM posts ORDER BY user_id, created_at",
                Some("posts.id"),
            ),
            // No primary key to suggest.
            ("SELECT msg FROM logs ORDER BY at LIMIT 10", None),
            // A group per (tenant, name): ordered by tenant alone.
            (
                "SELECT tenant, name, count(*) FROM users GROUP BY tenant, name \
                 ORDER BY tenant LIMIT 10",
                Some("users.id"),
            ),
            (
                "SELECT id FROM users ORDER BY 1 + 1, created_at LIMIT 1",
                Some("users.id"),
            ),
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert!(
                sql[findings[0].span.clone()].starts_with("ORDER BY"),
                "{sql}: {:?}",
                &sql[findings[0].span.clone()]
            );
            let h = findings[0].help.as_deref().unwrap();
            match help {
                Some(pk) => assert!(h.contains(pk), "{sql}: {h}"),
                None => assert_eq!(h, "add a unique column as the last ORDER BY key"),
            }
            assert!(findings[0].fix.is_empty());
        }
    }

    #[test]
    fn distinct_on_picking_from_one_group_is_reported() {
        let db = catalog();
        // WHERE fixes the DISTINCT ON key: one row comes back, but which of
        // the group's rows is arbitrary.
        let sql = "SELECT DISTINCT ON (tenant) id FROM users WHERE tenant = $1 ORDER BY tenant";
        let findings = lint_rule(&db, sql, RULE);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(
            findings[0]
                .message
                .contains("DISTINCT ON keeps an arbitrary row"),
            "{findings:?}"
        );
        // One row before DISTINCT ON: nothing to pick from.
        let sql = "SELECT DISTINCT ON (tenant) id FROM users WHERE id = $1 ORDER BY tenant";
        assert!(lint_rule(&db, sql, RULE).is_empty());
    }

    #[test]
    fn a_single_row_limit_says_which_row_is_arbitrary() {
        let db = catalog();
        for sql in [
            "SELECT id FROM users ORDER BY created_at DESC LIMIT 1",
            "SELECT id FROM users ORDER BY created_at DESC FETCH FIRST 1 ROW ONLY",
            // No count: PG's implicit 1 has no location in the parse tree.
            "SELECT id FROM users ORDER BY created_at DESC FETCH FIRST ROW ONLY",
            "SELECT id FROM users ORDER BY created_at DESC FETCH NEXT ROW ONLY",
            "SELECT id FROM users ORDER BY created_at DESC FETCH FIRST ROWS ONLY",
            "SELECT (SELECT id FROM users ORDER BY created_at DESC FETCH FIRST ROW ONLY)",
            "(SELECT id FROM users ORDER BY created_at DESC) FETCH FIRST ROW ONLY",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert!(
                findings[0]
                    .message
                    .ends_with("which of the tied rows comes back is arbitrary"),
                "{sql}: {findings:?}"
            );
            let picks = if sql.contains("LIMIT") {
                "LIMIT"
            } else {
                "FETCH"
            };
            assert!(
                findings[0].message.contains(&format!("under {picks},")),
                "{sql}: {findings:?}"
            );
        }
        // One row, or a unique key: nothing reported, as with LIMIT 1.
        for sql in [
            "SELECT id FROM users WHERE id = $1 ORDER BY created_at FETCH FIRST ROW ONLY",
            "SELECT id FROM users ORDER BY created_at, id FETCH FIRST ROW ONLY",
        ] {
            assert!(lint_rule(&db, sql, RULE).is_empty(), "{sql}");
        }
        // A page past the first still pages.
        let findings = lint_rule(
            &db,
            "SELECT id FROM users ORDER BY created_at LIMIT 1 OFFSET $1",
            RULE,
        );
        assert!(
            findings[0]
                .message
                .ends_with("pages may skip or repeat rows")
        );
    }

    #[test]
    fn outer_columns_fix_a_key_like_parameters() {
        let db = catalog();
        for sql in [
            "SELECT s.id, (SELECT v.digest FROM ver v WHERE v.source_id = s.id              ORDER BY v.version DESC LIMIT 1) FROM src s",
            "SELECT s.id, l.digest FROM src s CROSS JOIN LATERAL              (SELECT v.digest FROM ver v WHERE v.source_id = s.id              ORDER BY v.version DESC LIMIT 1) l",
            "SELECT s.id, l.digest FROM src s LEFT JOIN LATERAL              (SELECT v.digest FROM ver v WHERE s.id = v.source_id              ORDER BY v.version DESC LIMIT 1) l ON true",
            "SELECT (SELECT v.digest FROM ver v WHERE v.source_id = $1              ORDER BY v.version DESC LIMIT 1)",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
        // Not an equality: the outer column fixes nothing.
        let sql = "SELECT s.id, (SELECT v.digest FROM ver v WHERE v.source_id > s.id \
                   ORDER BY v.version DESC LIMIT 1) FROM src s";
        assert_eq!(lint_rule(&db, sql, RULE).len(), 1, "{sql}");
    }

    #[test]
    fn a_deterministic_collate_keeps_a_unique_key_unique() {
        let db = catalog();
        for sql in [
            "SELECT name FROM mig ORDER BY name COLLATE \"C\" LIMIT 10",
            "SELECT name FROM mig ORDER BY name COLLATE \"C\" DESC LIMIT 10",
            "SELECT name FROM mig ORDER BY name COLLATE pg_catalog.\"POSIX\" LIMIT 10",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
        // A nondeterministic collation compares distinct names equal.
        let sql = "SELECT name FROM mig ORDER BY name COLLATE ci LIMIT 10";
        assert_eq!(lint_rule(&db, sql, RULE).len(), 1, "{sql}");
    }

    #[test]
    fn inner_join_equalities_fix_key_columns() {
        let db = catalog();
        for sql in [
            // The ON pins `v.source_id` to a parameter, as a WHERE would.
            "SELECT v.digest FROM src s JOIN ver v ON v.source_id = $1 AND s.id = v.source_id \
             ORDER BY v.version LIMIT 10",
            // `v.source_id` equals the determined `s.id`, the key's other
            // column is an ORDER BY key.
            "SELECT v.digest FROM src s JOIN ver v ON v.source_id = s.id \
             ORDER BY s.id, v.version LIMIT 10",
            "SELECT DISTINCT ON (s.id) v.digest FROM src s JOIN ver v ON v.source_id = s.id \
             ORDER BY s.id, v.version DESC",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }

    #[test]
    fn the_help_leaves_out_key_columns_already_fixed() {
        let db = catalog();
        // `source_id` is pinned: the tiebreaker is `version` alone.
        let sql = "SELECT digest FROM ver WHERE source_id = $1 ORDER BY digest LIMIT 1";
        let findings = lint_rule(&db, sql, RULE);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(
            findings[0].help.as_deref(),
            Some(
                "add a unique tiebreaker as the last ORDER BY key, such as the primary key: \
                  ver.version"
            )
        );
        // Already an ORDER BY key: the tiebreaker is `version` alone.
        let sql = "SELECT digest FROM ver ORDER BY source_id, digest LIMIT 10";
        let findings = lint_rule(&db, sql, RULE);
        assert_eq!(
            findings[0].help.as_deref(),
            Some(
                "add a unique tiebreaker as the last ORDER BY key, such as the primary key: \
                  ver.version"
            )
        );
    }

    #[test]
    fn a_volatile_order_key_is_not_reported() {
        let db = catalog();
        // A fresh value per row: the query samples on purpose, so ties
        // between rows mean nothing.
        for sql in [
            "SELECT id FROM users ORDER BY random() LIMIT 5",
            "SELECT id FROM users ORDER BY created_at, random() LIMIT 5",
            "SELECT id FROM users ORDER BY random() OFFSET $1",
            "SELECT DISTINCT ON (user_id) id FROM posts ORDER BY user_id, random()",
            // An output column names the volatile expression it stands for.
            "SELECT random() AS r FROM users ORDER BY r LIMIT 5",
            "SELECT random() FROM users ORDER BY 1 LIMIT 5",
            // A volatile operator is a fresh value too.
            "SELECT id FROM users ORDER BY tenant <+> 1 LIMIT 5",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
        // A non-volatile call sorts by its arguments' values: ties remain.
        for sql in [
            "SELECT id FROM users ORDER BY lower(name) LIMIT 5",
            "SELECT id FROM users ORDER BY tenant + 1 LIMIT 5",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert!(
                findings[0].help.as_deref().unwrap().contains("users.id"),
                "{findings:?}"
            );
        }
    }

    #[test]
    fn distinct_on_output_the_keys_determine_is_not_reported() {
        let db = catalog();
        // Every output column is `u`'s, and the DISTINCT ON key is `u`'s
        // primary key: whichever prize/membership row of a group is kept,
        // the output is the same.
        let sql = "SELECT DISTINCT ON (u.id) u.id, u.name, u.email \
                   FROM prize p JOIN membership m ON m.id = p.membership_id \
                   JOIN users u ON u.id = m.user_id \
                   WHERE p.id > $1 ORDER BY u.id LIMIT $2";
        assert!(lint_rule(&db, sql, RULE).is_empty(), "{sql}");
        // The DISTINCT ON key itself is one value per group.
        let sql = "SELECT DISTINCT ON (user_id) user_id FROM posts \
                   ORDER BY user_id, created_at";
        assert!(lint_rule(&db, sql, RULE).is_empty(), "{sql}");
        // Reading an undetermined relation keeps the report: `p.id` differs
        // between the group's rows.
        let sql = "SELECT DISTINCT ON (u.id) u.id, p.id \
                   FROM prize p JOIN membership m ON m.id = p.membership_id \
                   JOIN users u ON u.id = m.user_id \
                   WHERE p.id > $1 ORDER BY u.id LIMIT $2";
        let findings = lint_rule(&db, sql, RULE);
        assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
        // A DISTINCT ON key that isn't a unique key of `u` determines no
        // row of it.
        let sql = "SELECT DISTINCT ON (u.tenant) u.name FROM users u ORDER BY u.tenant";
        let findings = lint_rule(&db, sql, RULE);
        assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
        // An outer column fixes the rest of `ver`'s key, so the group's
        // row is one.
        let sql = "SELECT (SELECT DISTINCT ON (v.version) v.digest FROM ver v \
                   WHERE v.source_id = s.id ORDER BY v.version) FROM src s";
        assert!(lint_rule(&db, sql, RULE).is_empty(), "{sql}");
    }

    #[test]
    fn the_help_names_the_fewest_tables_primary_keys() {
        let db = catalog();
        // `ch.id` determines `ch`, and `s` through the join on its key.
        let sql = "SELECT ch.id FROM chunk ch JOIN source s ON s.id = ch.source_id \
                   ORDER BY ch.score LIMIT 10";
        let findings = lint_rule(&db, sql, RULE);
        assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
        assert!(
            findings[0]
                .message
                .contains("don't determine a unique row of ch, s"),
            "{findings:?}"
        );
        assert_eq!(
            findings[0].help.as_deref(),
            Some(
                "add a unique tiebreaker as the last ORDER BY key, such as the primary key: \
                  ch.id"
            )
        );
        // A chain: `a`'s key determines `b`, and `b`'s `c`.
        let sql = "SELECT a.id FROM a JOIN b ON b.id = a.b_id JOIN c ON c.id = b.c_id \
                   ORDER BY a.x LIMIT 10";
        let findings = lint_rule(&db, sql, RULE);
        assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
        assert_eq!(
            findings[0].help.as_deref(),
            Some(
                "add a unique tiebreaker as the last ORDER BY key, such as the primary key: \
                  a.id"
            )
        );
        // Nothing determines the other: both keys are needed.
        let sql = "SELECT orders.id FROM orders \
                   JOIN shipments ON shipments.ref = orders.ref \
                   ORDER BY orders.total LIMIT 10";
        let findings = lint_rule(&db, sql, RULE);
        assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
        assert_eq!(
            findings[0].help.as_deref(),
            Some(
                "add a unique tiebreaker as the last ORDER BY key, such as the primary key: \
                  orders.id, shipments.id"
            )
        );
    }

    #[test]
    fn unique_keys_are_not_reported() {
        let db = catalog();
        for sql in [
            "SELECT id FROM users ORDER BY created_at, id LIMIT 10",
            "SELECT id FROM users ORDER BY id DESC LIMIT 10",
            "SELECT id AS k FROM users ORDER BY k LIMIT 10",
            "SELECT id FROM users ORDER BY 1 LIMIT 10",
            "SELECT id FROM users ORDER BY tenant, email LIMIT 10",
            // A nullable unique column a condition may prove non-NULL.
            "SELECT id FROM users WHERE nick IS NOT NULL ORDER BY nick LIMIT 10",
            "SELECT id FROM users WHERE nick > 'm' ORDER BY nick LIMIT 10",
            // The rest of the key is pinned by WHERE.
            "SELECT id FROM users WHERE tenant = $1 ORDER BY email LIMIT 10",
            "SELECT id FROM users WHERE tenant = 3 ORDER BY email LIMIT 10",
            // A join on the key: the post determines its user.
            "SELECT p.id FROM posts p JOIN users u ON u.id = p.user_id \
             ORDER BY p.created_at, p.id LIMIT 10",
            // Grouped: one row per group key.
            "SELECT tenant, count(*) FROM users GROUP BY tenant ORDER BY tenant LIMIT 10",
            "SELECT tenant, name FROM users GROUP BY tenant, name ORDER BY name, tenant LIMIT 10",
            "SELECT DISTINCT tenant FROM users ORDER BY tenant LIMIT 10",
            "SELECT DISTINCT ON (user_id) id FROM posts ORDER BY user_id, id",
            // At most one row.
            "SELECT id FROM users WHERE id = $1 ORDER BY created_at LIMIT 1",
            "SELECT count(*) FROM users ORDER BY 1 LIMIT 1",
            // Ties are all returned.
            "SELECT id FROM users ORDER BY created_at FETCH FIRST 5 ROWS WITH TIES",
            // Without LIMIT / OFFSET the order of ties picks nothing.
            "SELECT id FROM users ORDER BY created_at",
            // A queue.
            "SELECT id FROM users ORDER BY created_at LIMIT 1 FOR UPDATE SKIP LOCKED",
            // Existence only.
            "SELECT 1 FROM posts WHERE EXISTS \
             (SELECT 1 FROM users ORDER BY created_at LIMIT 1)",
            // What the proof can't see: an expression unique index, a
            // subquery, a CTE.
            "SELECT id FROM slugs ORDER BY lower(slug) LIMIT 10",
            "SELECT id FROM (SELECT id, created_at FROM users) s ORDER BY created_at LIMIT 10",
            "WITH s AS (SELECT id, created_at FROM users) \
             SELECT id FROM s ORDER BY created_at LIMIT 10",
            // The same output for every row.
            "SELECT 1 FROM users ORDER BY created_at LIMIT 1",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }
}
