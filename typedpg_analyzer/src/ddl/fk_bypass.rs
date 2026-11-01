//! Migration statements that may let rows skip the triggers enforcing
//! foreign keys, for the whole database, from then on
//! ([`PgCatalog::fk_checks_bypassed`]). Only the foreign-key join removal
//! (`simplify::fk_join`) reads it, and gives up.
//!
//! **The check is deliberately coarse.** A routine can turn the checks off
//! in too many ways to follow — dynamic SQL, a catalog write, a setting —
//! and be reached in too many ways to track: called by a migration, a
//! column DEFAULT, an operator, a trigger fired by a cascade, a rule, or
//! the application at run time. So defining one that *might* is enough:
//! any routine or DO block whose body (normalized: lowercased, comments
//! dropped, whitespace collapsed; `U&` escapes decoded) holds one of
//! [`BODY_MARKERS`] voids the proof, however and whether it is ever run.
//! A removed join that should have stayed returns wrong rows; a join kept
//! that could have gone costs one lookup per row, so every doubt counts
//! against the rewrite.
//!
//! Besides, a statement counts when it
//! - names `session_replication_role` anywhere (its text or a decoded
//!   identifier, setting name or string constant): `SET`, `set_config`, a
//!   routine's `SET` clause, `ALTER ROLE / DATABASE / SYSTEM SET`;
//! - calls `set_config` with a setting name that isn't a string constant
//!   (PREPARE bodies included);
//! - writes to a system catalog (INSERT, UPDATE, DELETE, MERGE, COPY FROM,
//!   TRUNCATE, a rule's action), or defines a view reading one — writing
//!   through it would reach the catalog;
//! - creates or alters a subscription: logical replication's apply workers
//!   run with `session_replication_role = replica`.
//!
//! Not covered, as for any bypass outside the migrations: what the
//! application's sessions do on their own (`SET session_replication_role`
//! from the client, data loaded with triggers off).

use typedpg_pg_query::NodeRef;
use typedpg_pg_query::protobuf::{RangeVar, a_const, node};

use crate::pg_catalog::PgCatalog;

/// Text in a routine or DO body (normalized) that may turn foreign-key
/// checks off when it runs: dynamic SQL, settings, catalog writes,
/// `DISABLE TRIGGER` (`disable` alone, whatever separates it from
/// `trigger`), `ENABLE / DISABLE TRIGGER ALL`.
const BODY_MARKERS: &[&str] = &[
    "execute",
    "set_config",
    "session_replication_role",
    "replication",
    "pg_trigger",
    "pg_class",
    "pg_constraint",
    "disable",
    "trigger all",
];

/// The setting that turns foreign-key triggers off.
const ROLE: &str = "session_replication_role";

/// Note what statement `stmt` (the one being applied, whose text is
/// [`PgCatalog::statement_sql`]) does to foreign-key checks.
pub(crate) fn note(db: &mut PgCatalog, stmt: &node::Node) {
    if db.fk_checks_bypassed {
        return;
    }
    let forms = text_forms(db, stmt);
    let defines_code = matches!(
        stmt,
        node::Node::CreateFunctionStmt(_) | node::Node::DoStmt(_)
    );
    let markers = if defines_code { BODY_MARKERS } else { &[ROLE] };
    if forms.iter().any(|t| markers.iter().any(|m| t.contains(m))) || bypasses(stmt) {
        db.fk_checks_bypassed = true;
    }
}

/// The statement's text with every decoded identifier, setting name and
/// string constant of its parse tree (a routine's body among them),
/// lowercased, in the forms markers are looked for in: as is, with `U&`
/// escapes decoded, and each of those with comments dropped — each with
/// whitespace collapsed. (Comments are dropped without regard to string
/// literals, so the forms without them stand in for what that might hide.)
fn text_forms(db: &PgCatalog, stmt: &node::Node) -> Vec<String> {
    let mut text = db.statement_sql.clone().unwrap_or_default();
    for (n, _) in stmt.nodes() {
        let s = match n {
            NodeRef::String(s) => s.sval.as_str(),
            NodeRef::AConst(c) => match &c.val {
                Some(a_const::Val::Sval(s)) => s.sval.as_str(),
                _ => continue,
            },
            NodeRef::VariableSetStmt(v) => v.name.as_str(),
            NodeRef::DefElem(d) => d.defname.as_str(),
            _ => continue,
        };
        text.push('\n');
        text.push_str(s);
    }
    let text = text.to_lowercase();
    let decoded = decode_unicode_escapes(&text);
    let mut forms = Vec::new();
    for t in [text, decoded] {
        forms.push(collapse_whitespace(&strip_comments(&t)));
        forms.push(collapse_whitespace(&t));
    }
    forms
}

/// `text` without `--` and (nested) `/* */` comments, each replaced by a
/// space.
fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if rest.starts_with("--") {
            let end = rest.find('\n').unwrap_or(rest.len());
            rest = &rest[end..];
            out.push(' ');
        } else if rest.starts_with("/*") {
            let mut depth = 0usize;
            let mut i = 0;
            let bytes = rest.as_bytes();
            while i < bytes.len() {
                if bytes[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if bytes[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            rest = rest.get(i..).unwrap_or("");
            out.push(' ');
        } else {
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    out
}

/// Runs of whitespace as one space.
fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `text` with the escapes of `U&` identifiers and strings decoded:
/// `\XXXX` and `\+XXXXXX`, and the same after each `UESCAPE` character the
/// text declares — wherever they appear.
fn decode_unicode_escapes(text: &str) -> String {
    let mut escapes = vec!['\\'];
    let mut rest = text;
    while let Some(at) = rest.find("uescape") {
        rest = &rest[at + "uescape".len()..];
        if let Some(c) = rest
            .trim_start()
            .strip_prefix('\'')
            .and_then(|s| s.chars().next())
        {
            escapes.push(c);
        }
    }
    let chars: Vec<char> = text.chars().collect();
    let hex = |s: &[char]| {
        (s.iter().all(char::is_ascii_hexdigit))
            .then(|| u32::from_str_radix(&s.iter().collect::<String>(), 16).ok())
            .flatten()
            .and_then(char::from_u32)
    };
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if escapes.contains(&chars[i]) {
            if chars.get(i + 1) == Some(&'+')
                && let Some(c) = chars.get(i + 2..i + 8).and_then(hex)
            {
                out.push(c);
                i += 8;
                continue;
            }
            if let Some(c) = chars.get(i + 1..i + 5).and_then(hex) {
                out.push(c);
                i += 5;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// A relation of the system catalogs, by name.
fn system_table(rv: &RangeVar) -> bool {
    rv.schemaname.eq_ignore_ascii_case("pg_catalog")
        || (rv.schemaname.is_empty() && rv.relname.starts_with("pg_"))
}

/// What in the statement's parse tree may bypass the checks (see the
/// module docs).
fn bypasses(stmt: &node::Node) -> bool {
    let target = |rv: Option<&RangeVar>| rv.is_some_and(system_table);
    stmt.nodes().iter().any(|(n, _)| match n {
        NodeRef::FuncCall(fc) => {
            let name: Vec<&str> = fc
                .funcname
                .iter()
                .filter_map(super::util::node_string)
                .collect();
            matches!(
                name.as_slice(),
                ["set_config"] | ["pg_catalog", "set_config"]
            ) && !matches!(
                fc.args.first().and_then(|a| a.node.as_ref()),
                Some(node::Node::AConst(typedpg_pg_query::protobuf::AConst {
                    val: Some(a_const::Val::Sval(_)),
                    ..
                }))
            )
        }
        NodeRef::InsertStmt(s) => target(s.relation.as_ref()),
        NodeRef::UpdateStmt(s) => target(s.relation.as_ref()),
        NodeRef::DeleteStmt(s) => target(s.relation.as_ref()),
        NodeRef::MergeStmt(s) => target(s.relation.as_ref()),
        NodeRef::CopyStmt(s) => s.is_from && target(s.relation.as_ref()),
        NodeRef::TruncateStmt(s) => s.relations.iter().any(|r| match r.node.as_ref() {
            Some(node::Node::RangeVar(rv)) => system_table(rv),
            _ => true,
        }),
        // A view over a catalog: writes through it reach the catalog.
        NodeRef::ViewStmt(v) => v
            .query
            .as_deref()
            .and_then(|q| q.node.as_ref())
            .is_some_and(|q| {
                q.nodes()
                    .iter()
                    .any(|(n, _)| matches!(n, NodeRef::RangeVar(rv) if system_table(rv)))
            }),
        NodeRef::CreateSubscriptionStmt(_) | NodeRef::AlterSubscriptionStmt(_) => true,
        _ => false,
    })
}
