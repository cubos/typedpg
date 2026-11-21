//! Utility statements a query may be: `TRUNCATE`, `LOCK`, `ANALYZE`,
//! `REFRESH MATERIALIZED VIEW`, `SET` / `RESET`. None returns rows, and none
//! takes parameters (the grammar has no place for one). PostgreSQL parses
//! them at PREPARE and does everything else when they execute — looking up
//! their relations, checking their privileges, rejecting what can't work —
//! so the errors every execution raises are reported here, with
//! PostgreSQL's wording, and the privileges are noted for the role checks
//! (see [`crate::privileges::UtilityCheck`]).

use typedpg_pg_query::ast;

use crate::ddl::acl::bits;
use crate::error::{AnalyzeError, RawError, SourceSpan};
use crate::oid::PgClassOid;
use crate::pg_catalog::{ConType, PgCatalog, RelKind};
use crate::privileges::{UtilityCheck, note_truncated, note_utility};

/// The relation `rv` names, as `RangeVarGetRelidExtended` finds it.
fn lookup<'a>(
    rv: &ast::RangeVar,
    snapshot: &'a PgCatalog,
) -> Result<&'a crate::pg_catalog::PgClass, AnalyzeError> {
    super::from::check_rangevar_catalog(rv)?;
    let schema = (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str());
    snapshot.resolve_table(schema, &rv.relname).ok_or_else(|| {
        crate::scope::undefined_table_error(
            snapshot,
            schema,
            &rv.relname,
            SourceSpan::from_node_qname(rv.location),
        )
    })
}

fn range_vars(list: &[Option<ast::Node>]) -> impl Iterator<Item = &ast::RangeVar> {
    list.iter().filter_map(|n| match n.as_ref() {
        Some(ast::Node::RangeVar(rv)) => Some(&**rv),
        _ => None,
    })
}

/// `errdetail_relkind_not_supported`.
fn relkind_not_supported(relkind: RelKind) -> &'static str {
    match relkind {
        RelKind::Table => "This operation is not supported for tables.",
        RelKind::Index => "This operation is not supported for indexes.",
        RelKind::Sequence => "This operation is not supported for sequences.",
        RelKind::ToastTable => "This operation is not supported for TOAST tables.",
        RelKind::View => "This operation is not supported for views.",
        RelKind::MaterializedView => "This operation is not supported for materialized views.",
        RelKind::CompositeType => "This operation is not supported for composite types.",
        RelKind::ForeignTable => "This operation is not supported for foreign tables.",
        RelKind::Partitioned => "This operation is not supported for partitioned tables.",
        RelKind::PartitionedIndex => "This operation is not supported for partitioned indexes.",
    }
}

// ─── TRUNCATE ─────────────────────────────────────────────────────────────────

/// `truncate_check_rel`: only tables (plain, partitioned, foreign) can be
/// truncated, and no system catalog.
fn truncate_check_rel(
    class: &crate::pg_catalog::PgClass,
    snapshot: &PgCatalog,
    span: Option<SourceSpan>,
) -> Result<(), AnalyzeError> {
    if !matches!(
        class.relkind,
        RelKind::Table | RelKind::Partitioned | RelKind::ForeignTable
    ) {
        return Err(RawError::new(
            AnalyzeError::WrongObjectType(format!("\"{}\" is not a table", class.relname)),
            span,
            None,
        )
        .finalize_implicit());
    }
    if snapshot.is_system_class(class.oid) {
        return Err(RawError::new(
            AnalyzeError::InsufficientPrivilege(format!(
                "permission denied: \"{}\" is a system catalog",
                class.relname
            )),
            span,
            None,
        )
        .finalize_implicit());
    }
    Ok(())
}

/// `heap_truncate_find_FKs`: the relations with a foreign key referencing
/// one of `relids` (climbing to the partitioned parent a referenced
/// partition's constraint was cloned from), minus `relids` themselves,
/// sorted by OID.
fn referencing_relations(snapshot: &PgCatalog, relids: &[PgClassOid]) -> Vec<PgClassOid> {
    let mut referenced: Vec<PgClassOid> = relids.to_vec();
    let mut result: Vec<PgClassOid> = Vec::new();
    loop {
        let mut parents = Vec::new();
        result.clear();
        for con in snapshot.pg_constraint.values() {
            // A NOT ENFORCED foreign key has no triggers to break.
            if con.contype != ConType::ForeignKey || !con.conenforced {
                continue;
            }
            let Some(confrelid) = con.confrelid else {
                continue;
            };
            if !referenced.contains(&confrelid) {
                continue;
            }
            if let Some(parent) = crate::ddl::tables::foreign_keys::fk_parent(snapshot, con.oid)
                && !parents
                    .iter()
                    .any(|p: &crate::pg_catalog::PgConstraint| p.oid == parent.oid)
            {
                parents.push(parent);
            }
            if !relids.contains(&con.conrelid) {
                result.push(con.conrelid);
            }
        }
        let mut grew = false;
        for parent in parents {
            if let Some(confrelid) = parent.confrelid
                && !referenced.contains(&confrelid)
            {
                referenced.push(confrelid);
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    result.sort();
    result.dedup();
    result
}

pub(super) fn analyze_truncate(
    stmt: &ast::TruncateStmt,
    snapshot: &PgCatalog,
) -> Result<(), AnalyzeError> {
    let mut rels: Vec<PgClassOid> = Vec::new();
    for rv in range_vars(&stmt.relations) {
        let span = SourceSpan::from_node_qname(rv.location);
        let class = lookup(rv, snapshot)?;
        // RangeVarCallbackForTruncate: the kind, then TRUNCATE privilege.
        truncate_check_rel(class, snapshot, span)?;
        note_utility(UtilityCheck::Relation {
            relid: class.oid,
            any_of: bits::TRUNCATE,
            check_as: None,
        });
        // `TRUNCATE foo, foo` is no error.
        if rels.contains(&class.oid) {
            continue;
        }
        rels.push(class.oid);
        if rv.inh {
            // The children are truncated without a privilege check of their
            // own: the parent's TRUNCATE covers them.
            for child in crate::ddl::tables::inherit::all_inheritors(snapshot, class.oid) {
                if rels.contains(&child) {
                    continue;
                }
                if let Some(child_class) = snapshot.pg_class.get(&child) {
                    truncate_check_rel(child_class, snapshot, span)?;
                }
                rels.push(child);
            }
        } else if class.relkind == RelKind::Partitioned {
            return Err(RawError::new(
                AnalyzeError::WrongObjectType(
                    "cannot truncate only a partitioned table".to_owned(),
                ),
                span,
                Some(
                    "Do not specify the ONLY keyword, or use TRUNCATE ONLY on the partitions \
                     directly."
                        .to_owned(),
                ),
            )
            .finalize_implicit());
        }
    }
    if stmt.behavior == ast::DropBehavior::DropCascade {
        // ExecuteTruncateGuts: every relation referencing one truncated is
        // truncated too, checked as an explicit one.
        loop {
            let added = referencing_relations(snapshot, &rels);
            if added.is_empty() {
                break;
            }
            for relid in added {
                if let Some(class) = snapshot.pg_class.get(&relid) {
                    truncate_check_rel(class, snapshot, None)?;
                    note_utility(UtilityCheck::Relation {
                        relid,
                        any_of: bits::TRUNCATE,
                        check_as: None,
                    });
                }
                rels.push(relid);
            }
        }
    } else {
        // heap_truncate_check_FKs: a relation referencing a truncated one
        // must be truncated with it — whatever the rows.
        if !referencing_relations(snapshot, &rels).is_empty() {
            for &relid in &rels {
                for referencing in referencing_relations(snapshot, &[relid]) {
                    if rels.contains(&referencing) {
                        continue;
                    }
                    let name = |oid: PgClassOid| {
                        snapshot
                            .pg_class
                            .get(&oid)
                            .map(|c| c.relname.clone())
                            .unwrap_or_default()
                    };
                    let (referenced, referencing) = (name(relid), name(referencing));
                    let mut raw = RawError::new(
                        AnalyzeError::FeatureNotSupported(
                            "cannot truncate a table referenced in a foreign key constraint"
                                .to_owned(),
                        ),
                        crate::error::statement_span(),
                        Some(format!(
                            "Truncate table \"{referencing}\" at the same time, or use TRUNCATE \
                             ... CASCADE."
                        )),
                    );
                    raw.notes.push(format!(
                        "Table \"{referencing}\" references \"{referenced}\"."
                    ));
                    return Err(raw.finalize_implicit());
                }
            }
        }
    }
    // RESTART IDENTITY resets the sequences the relations own, which only
    // their owner may do.
    if stmt.restart_seqs {
        for &relid in &rels {
            for seq in crate::ddl::sequences::owned_sequences(snapshot, relid, None) {
                note_utility(UtilityCheck::SequenceOwner { relid: seq });
            }
        }
    }
    // The TRUNCATE statement triggers of every relation truncated fire.
    for &relid in &rels {
        note_truncated(relid);
    }
    Ok(())
}

// ─── LOCK ─────────────────────────────────────────────────────────────────────

/// `AccessShareLock` and `RowExclusiveLock` (lockdefs.h).
const ACCESS_SHARE_LOCK: i32 = 1;
const ROW_EXCLUSIVE_LOCK: i32 = 3;

/// `LockTableAclCheck`: MAINTAIN, UPDATE, DELETE or TRUNCATE permit any
/// lock; SELECT also ACCESS SHARE; INSERT also ROW EXCLUSIVE and below.
fn lock_privileges(mode: i32) -> u32 {
    let mut mask = bits::MAINTAIN | bits::UPDATE | bits::DELETE | bits::TRUNCATE;
    if mode <= ACCESS_SHARE_LOCK {
        mask |= bits::SELECT;
    }
    if mode <= ROW_EXCLUSIVE_LOCK {
        mask |= bits::INSERT;
    }
    mask
}

pub(super) fn analyze_lock(stmt: &ast::LockStmt, snapshot: &PgCatalog) -> Result<(), AnalyzeError> {
    // `LOCK TABLE can only be used in transaction blocks`: whether the
    // statement runs in one is the caller's, not the statement's.
    let mask = lock_privileges(stmt.mode);
    for rv in range_vars(&stmt.relations) {
        let span = SourceSpan::from_node_qname(rv.location);
        let class = lookup(rv, snapshot)?;
        // RangeVarCallbackForLockTable: plain and partitioned tables and
        // views only, then the privilege.
        if !matches!(
            class.relkind,
            RelKind::Table | RelKind::Partitioned | RelKind::View
        ) {
            let mut raw = RawError::new(
                AnalyzeError::WrongObjectType(format!("cannot lock relation \"{}\"", rv.relname)),
                span,
                None,
            );
            raw.notes
                .push(relkind_not_supported(class.relkind).to_owned());
            return Err(raw.finalize_implicit());
        }
        note_utility(UtilityCheck::Relation {
            relid: class.oid,
            any_of: mask,
            check_as: None,
        });
        if class.relkind == RelKind::View {
            lock_view(snapshot, class.oid, mask, &mut vec![class.oid]);
        }
    }
    Ok(())
}

/// `LockViewRecurse`: every table and view the view's query reads is
/// locked too, its privilege checked as the view's owner (the role running
/// the statement for a `security_invoker` view); a view among them is
/// recursed into with its own owner.
fn lock_view(snapshot: &PgCatalog, view: PgClassOid, mask: u32, ancestors: &mut Vec<PgClassOid>) {
    let check_as = (!snapshot.security_invoker_views.contains(&view))
        .then(|| snapshot.owner_of(crate::ddl::acl::AclObject::Relation(view)));
    let Some(access) = snapshot.view_access.get(&view) else {
        return;
    };
    let mut relids: Vec<PgClassOid> = access.all().relation_oids();
    relids.dedup();
    for relid in relids {
        let Some(class) = snapshot.pg_class.get(&relid) else {
            continue;
        };
        if !matches!(
            class.relkind,
            RelKind::Table | RelKind::Partitioned | RelKind::View
        ) || ancestors.contains(&relid)
        {
            continue;
        }
        note_utility(UtilityCheck::Relation {
            relid,
            any_of: mask,
            check_as,
        });
        if class.relkind == RelKind::View {
            ancestors.push(relid);
            lock_view(snapshot, relid, mask, ancestors);
            ancestors.pop();
        }
    }
}

// ─── ANALYZE ──────────────────────────────────────────────────────────────────

/// `defGetBoolean`: no argument, 0 or 1, or `true` / `false` / `on` /
/// `off` (any case).
fn option_boolean(de: &ast::DefElem) -> Result<(), AnalyzeError> {
    let ok = match de.arg.as_ref() {
        None | Some(ast::Node::Boolean(_)) => true,
        Some(ast::Node::Integer(i)) => i.ival == 0 || i.ival == 1,
        Some(ast::Node::String(s)) => matches!(
            s.sval.to_ascii_lowercase().as_str(),
            "true" | "false" | "on" | "off"
        ),
        _ => false,
    };
    if ok {
        return Ok(());
    }
    Err(RawError::new(
        AnalyzeError::SyntaxError(format!("{} requires a Boolean value", de.defname)),
        None,
        None,
    )
    .finalize_implicit())
}

pub(super) fn analyze_vacuum(
    stmt: &ast::VacuumStmt,
    snapshot: &PgCatalog,
) -> Result<(), AnalyzeError> {
    // ExecVacuum: the options first.
    for de in stmt.options.iter().filter_map(|n| match n.as_ref() {
        Some(ast::Node::DefElem(de)) => Some(&**de),
        _ => None,
    }) {
        match de.defname.as_str() {
            "verbose" | "skip_locked" => option_boolean(de)?,
            "buffer_usage_limit" => check_buffer_usage_limit(de)?,
            _ => {
                return Err(RawError::new(
                    AnalyzeError::SyntaxError(format!(
                        "unrecognized ANALYZE option \"{}\"",
                        de.defname
                    )),
                    SourceSpan::from_node_token(de.location),
                    None,
                )
                .finalize_implicit());
            }
        }
    }
    for vrel in stmt.rels.iter().filter_map(|n| match n.as_ref() {
        Some(ast::Node::VacuumRelation(v)) => Some(&**v),
        _ => None,
    }) {
        let Some(rv) = vrel.relation.as_deref() else {
            continue;
        };
        let class = lookup(rv, snapshot)?;
        // analyze_rel skips (with a WARNING) what it can't analyze and what
        // the role may not; do_analyze_rel then checks the column list.
        if !matches!(
            class.relkind,
            RelKind::Table | RelKind::Partitioned | RelKind::MaterializedView
        ) {
            // A foreign table is analyzed only by a wrapper that can: which
            // one serves it is the server's.
            continue;
        }
        let mut seen: Vec<i16> = Vec::new();
        for col in &vrel.va_cols {
            let Some(name) = crate::ddl::util::node_string(col.as_ref()) else {
                continue;
            };
            let attnum = snapshot
                .attribute_by_name(class.oid, name)
                .filter(|a| a.attnum > 0)
                .map(|a| a.attnum);
            let Some(attnum) = attnum else {
                return Err(RawError::new(
                    AnalyzeError::UndefinedColumn(format!(
                        "column \"{name}\" of relation \"{}\" does not exist",
                        class.relname
                    )),
                    None,
                    None,
                )
                .finalize_implicit());
            };
            if seen.contains(&attnum) {
                return Err(RawError::new(
                    AnalyzeError::DuplicateColumn(format!(
                        "column \"{name}\" of relation \"{}\" appears more than once",
                        class.relname
                    )),
                    None,
                    None,
                )
                .finalize_implicit());
            }
            seen.push(attnum);
        }
    }
    Ok(())
}

/// `BUFFER_USAGE_LIMIT`: 0, or between 128 kB and 16 GB (`parse_int` with
/// `GUC_UNIT_KB`).
fn check_buffer_usage_limit(de: &ast::DefElem) -> Result<(), AnalyzeError> {
    let text = match de.arg.as_ref() {
        Some(ast::Node::Integer(i)) => i.ival.to_string(),
        Some(ast::Node::String(s)) => s.sval.clone(),
        Some(ast::Node::Float(f)) => f.fval.clone(),
        _ => String::new(),
    };
    let kb = crate::ddl::guc::parse_memory_kb(&text);
    let valid = kb.is_some_and(|kb| kb == 0.0 || (128.0..=16_777_216.0).contains(&kb));
    if valid {
        return Ok(());
    }
    Err(RawError::new(
        AnalyzeError::Invalid(
            "BUFFER_USAGE_LIMIT option must be 0 or between 128 kB and 16777216 kB".to_owned(),
        ),
        None,
        None,
    )
    .finalize_implicit())
}

// ─── REFRESH MATERIALIZED VIEW ───────────────────────────────────────────────

pub(super) fn analyze_refresh(
    stmt: &ast::RefreshMatViewStmt,
    snapshot: &PgCatalog,
) -> Result<(), AnalyzeError> {
    let Some(rv) = stmt.relation.as_deref() else {
        return Ok(());
    };
    let span = SourceSpan::from_node_qname(rv.location);
    let class = lookup(rv, snapshot)?;
    // RangeVarCallbackMaintainsTable: MAINTAIN, before the kind is looked at.
    note_utility(UtilityCheck::Relation {
        relid: class.oid,
        any_of: bits::MAINTAIN,
        check_as: None,
    });
    if class.relkind != RelKind::MaterializedView {
        return Err(RawError::new(
            AnalyzeError::FeatureNotSupported(format!(
                "\"{}\" is not a materialized view",
                class.relname
            )),
            span,
            None,
        )
        .finalize_implicit());
    }
    // Whether the view is populated depends on what ran before: CONCURRENTLY
    // over an unpopulated one is the caller's to avoid.
    if stmt.concurrent && stmt.skip_data {
        return Err(RawError::new(
            AnalyzeError::SyntaxError(
                "REFRESH options CONCURRENTLY and WITH NO DATA cannot be used together".to_owned(),
            ),
            None,
            None,
        )
        .finalize_implicit());
    }
    if stmt.concurrent {
        let usable = snapshot.pg_index.values().any(|ix| {
            ix.indrelid == class.oid
                && ix.indisunique
                && ix.indpred.is_none()
                && ix.indnatts > 0
                && ix.indkey.iter().all(|&a| a > 0)
        });
        if !usable {
            let schema = snapshot
                .namespace_name(class.relnamespace)
                .unwrap_or_default();
            return Err(RawError::new(
                AnalyzeError::ObjectNotInPrerequisiteState(format!(
                    "cannot refresh materialized view \"{}\" concurrently",
                    crate::QualifiedName::new(schema, class.relname.clone())
                )),
                span,
                Some(
                    "Create a unique index with no WHERE clause on one or more columns of the \
                     materialized view."
                        .to_owned(),
                ),
            )
            .finalize_implicit());
        }
    }
    // The view's query runs as its owner (RefreshMatViewByOid switches to
    // it): what it reads and calls is the owner's to be allowed.
    if !stmt.skip_data
        && let Some(access) = snapshot.view_access.get(&class.oid)
    {
        note_utility(UtilityCheck::AsOwnerOf {
            relid: class.oid,
            req: Box::new(access.all()),
        });
    }
    Ok(())
}

// ─── SET / RESET ──────────────────────────────────────────────────────────────

fn ddl_error(e: crate::ddl::DdlError) -> AnalyzeError {
    let message = e.to_string();
    let kind = match e {
        crate::ddl::DdlError::TypeNotFound(_) => AnalyzeError::UndefinedObject(message),
        _ => AnalyzeError::Invalid(message),
    };
    RawError::new(kind, None, None).finalize_implicit()
}

pub(super) fn analyze_set(
    stmt: &ast::VariableSetStmt,
    snapshot: &PgCatalog,
) -> Result<(), AnalyzeError> {
    use ast::VariableSetKind as K;
    let value = |stmt: &ast::VariableSetStmt| match stmt.args.as_slice() {
        [arg] => crate::ddl::session::const_arg_string(arg.as_ref()),
        _ => None,
    };
    // `SET ROLE` / `SET SESSION AUTHORIZATION`: who the session may become.
    if stmt.name.eq_ignore_ascii_case("role") {
        if stmt.kind == K::VarSetValue
            && let Some(name) = value(stmt)
            && !name.eq_ignore_ascii_case("none")
        {
            note_utility(UtilityCheck::SetRole { name });
        }
        return Ok(());
    }
    if stmt.name.eq_ignore_ascii_case("session_authorization") {
        if stmt.kind == K::VarSetValue
            && let Some(name) = value(stmt)
        {
            note_utility(UtilityCheck::SessionAuthorization { name });
        }
        return Ok(());
    }
    match stmt.kind {
        K::VarSetValue | K::VarSetDefault | K::VarSetCurrent | K::VarReset => {
            let setting = crate::ddl::guc::check_settable(snapshot, &stmt.name)
                .map_err(ddl_error)?
                .cloned();
            if let Some(setting) = setting {
                if stmt.kind == K::VarSetValue
                    && let Some(value) = value(stmt)
                {
                    crate::ddl::guc::check_value(snapshot, &setting, &value).map_err(ddl_error)?;
                }
                if setting.context == "superuser" {
                    note_utility(UtilityCheck::SetParameter {
                        name: setting.name.clone(),
                    });
                }
            }
        }
        // `SET TRANSACTION ...`, `SET SESSION CHARACTERISTICS ...`, `RESET
        // ALL`: nothing a statement on its own can get wrong.
        _ => {}
    }
    Ok(())
}
