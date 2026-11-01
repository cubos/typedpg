//! Migration statements that may let rows skip the triggers enforcing
//! foreign keys, for the whole database, from then on
//! ([`PgCatalog::fk_checks_bypassed`]). Only the foreign-key join removal
//! (`simplify::fk_join`) reads it, and gives up: the checks err towards
//! "bypassed".
//!
//! A statement counts when it
//! - names `session_replication_role` — in its text, or decoded (`U&"..."`
//!   identifiers, escaped strings) in any identifier, setting name or
//!   string constant of its parse tree: `SET`, `set_config`, a routine's
//!   `SET` clause, `ALTER ROLE / DATABASE / SYSTEM SET`, a body;
//! - calls `set_config` with a setting name that isn't a string constant
//!   (PREPARE bodies included);
//! - writes to a system catalog (INSERT, UPDATE, DELETE, MERGE, COPY FROM,
//!   TRUNCATE on `pg_catalog.*`, e.g. `UPDATE pg_trigger SET tgenabled`);
//! - creates or alters a subscription: logical replication's apply workers
//!   run with `session_replication_role = replica`;
//! - runs code that may do any of this: a DO block, a call to a routine
//!   (`SELECT f()`, `CALL p()`), or a write to a table with a trigger, whose
//!   body holds a marker ([`BODY_MARKERS`], or the name of a routine that
//!   does).
//!
//! Not covered, as for any bypass at run time: what the application's
//! sessions do (their own `SET session_replication_role`, routines it calls),
//! and routines the migrations define but never run.

use typedpg_pg_query::NodeRef;
use typedpg_pg_query::protobuf::{RangeVar, a_const, node};

use crate::oid::PgProcOid;
use crate::pg_catalog::PgCatalog;

/// Text in a routine or DO body that may turn foreign-key checks off when
/// it runs (matched lowercased).
const BODY_MARKERS: &[&str] = &[
    "execute",
    "set_config",
    "session_replication_role",
    "pg_trigger",
    "disable trigger",
    "replication",
];

/// The setting that turns foreign-key triggers off.
const ROLE: &str = "session_replication_role";

/// What [`note`] leaves for [`record_routines`]: the routine a CREATE
/// FUNCTION / PROCEDURE defines with a suspicious body.
pub(crate) struct Watch {
    routine: Option<String>,
}

/// Note what statement `stmt` (the one being applied, whose text is
/// [`PgCatalog::statement_sql`]) does to foreign-key checks, before it is
/// applied.
pub(crate) fn note(db: &mut PgCatalog, stmt: &node::Node) -> Watch {
    let text = statement_text(db, stmt);
    let suspicious = BODY_MARKERS.iter().any(|m| text.contains(m))
        || bypass_routine_names(db)
            .iter()
            .any(|n| contains_word(&text, n));
    let routine = match stmt {
        node::Node::CreateFunctionStmt(f) if suspicious => f
            .funcname
            .last()
            .and_then(super::util::node_string)
            .map(str::to_owned),
        _ => None,
    };
    if !db.fk_checks_bypassed && bypasses(db, stmt, &text, suspicious) {
        db.fk_checks_bypassed = true;
    }
    Watch { routine }
}

/// After a CREATE FUNCTION / PROCEDURE with a suspicious body succeeded:
/// remember every routine of that name (the new one, or the one it
/// replaced), so calling it at migration time counts.
pub(crate) fn record_routines(db: &mut PgCatalog, watch: Watch) {
    let Some(name) = watch.routine else {
        return;
    };
    let oids: Vec<PgProcOid> = db
        .pg_proc
        .values()
        .filter(|p| p.proname == name)
        .map(|p| p.oid)
        .collect();
    db.fk_bypass_routines.extend(oids);
}

/// The statement's text and every decoded identifier, setting name and
/// string constant of its parse tree, lowercased.
fn statement_text(db: &PgCatalog, stmt: &node::Node) -> String {
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
    text.to_lowercase()
}

/// `text` holds `word` delimited as an identifier (not inside a longer
/// one).
fn contains_word(text: &str, word: &str) -> bool {
    let ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    text.match_indices(word).any(|(at, _)| {
        !text[..at].chars().next_back().is_some_and(ident)
            && !text[at + word.len()..].chars().next().is_some_and(ident)
    })
}

/// The current names of the routines that may bypass the checks.
fn bypass_routine_names(db: &PgCatalog) -> Vec<String> {
    db.fk_bypass_routines
        .iter()
        .filter_map(|oid| db.pg_proc.get(oid))
        .map(|p| p.proname.to_lowercase())
        .collect()
}

fn bypasses(db: &PgCatalog, stmt: &node::Node, text: &str, suspicious: bool) -> bool {
    if text.contains(ROLE) {
        return true;
    }
    let system_table = |rv: Option<&RangeVar>| {
        rv.is_some_and(|rv| {
            rv.schemaname.eq_ignore_ascii_case("pg_catalog")
                || (rv.schemaname.is_empty() && rv.relname.starts_with("pg_"))
        })
    };
    // A table one of whose triggers runs a suspicious routine.
    let runs_bypass_trigger = |rv: Option<&RangeVar>| {
        rv.and_then(|rv| {
            let schema = (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str());
            db.resolve_table(schema, &rv.relname)
        })
        .and_then(|class| db.triggers.get(&class.oid))
        .is_some_and(|triggers| {
            triggers
                .iter()
                .any(|t| db.fk_bypass_routines.contains(&t.function))
        })
    };
    let written = |rv: Option<&RangeVar>| system_table(rv) || runs_bypass_trigger(rv);
    stmt.nodes().iter().any(|(n, _)| match n {
        NodeRef::FuncCall(fc) => {
            let name: Vec<&str> = fc
                .funcname
                .iter()
                .filter_map(super::util::node_string)
                .collect();
            let set_config = matches!(
                name.as_slice(),
                ["set_config"] | ["pg_catalog", "set_config"]
            ) && !matches!(
                fc.args.first().and_then(|a| a.node.as_ref()),
                Some(node::Node::AConst(typedpg_pg_query::protobuf::AConst {
                    val: Some(a_const::Val::Sval(_)),
                    ..
                }))
            );
            let calls_bypass_routine = match name.as_slice() {
                [name] => Some((None, *name)),
                [schema, name] => Some((Some(*schema), *name)),
                _ => None,
            }
            .is_some_and(|(schema, name)| {
                db.find_functions(schema, name)
                    .iter()
                    .any(|p| db.fk_bypass_routines.contains(&p.oid))
            });
            set_config || calls_bypass_routine
        }
        NodeRef::DoStmt(_) => suspicious,
        NodeRef::InsertStmt(s) => written(s.relation.as_ref()),
        NodeRef::UpdateStmt(s) => written(s.relation.as_ref()),
        NodeRef::DeleteStmt(s) => written(s.relation.as_ref()),
        NodeRef::MergeStmt(s) => written(s.relation.as_ref()),
        NodeRef::CopyStmt(s) => s.is_from && written(s.relation.as_ref()),
        NodeRef::TruncateStmt(s) => s.relations.iter().any(|r| match r.node.as_ref() {
            Some(node::Node::RangeVar(rv)) => written(Some(rv)),
            _ => true,
        }),
        NodeRef::CreateSubscriptionStmt(_) | NodeRef::AlterSubscriptionStmt(_) => true,
        _ => false,
    })
}
