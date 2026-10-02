//! CREATE / DROP TRIGGER. Triggers don't change query types, but PG
//! validates them when they are created (`CreateTriggerFiringOn`) and a
//! trigger depends on its function, so the catalog keeps a minimal record.
//! A row trigger on a partitioned table is cloned to every partition, the
//! ones it has and the ones it gets later (`CloneRowTriggersToPartition`).

use typedpg_pg_query::protobuf::{CreateTrigStmt, node};

use super::DdlError;
use crate::oid::{PgClassOid, PgProcOid};
use crate::pg_catalog::{PgCatalog, RelKind};

/// A trigger (`pg_trigger`): its name, function and firing kind.
#[derive(Clone, Debug)]
pub(crate) struct Trigger {
    /// `pg_trigger.oid`: the identity `pg_depend` rows name.
    pub(crate) oid: crate::oid::PgGenericOid,
    pub(crate) name: String,
    pub(crate) function: PgProcOid,
    /// The `TRIGGER_TYPE_INSERT | _DELETE | _UPDATE | _TRUNCATE` bits of
    /// an `INSTEAD OF ... FOR EACH ROW` trigger, else 0 — what the
    /// relcache's `trig_*_instead_row` flags are made of.
    pub(crate) instead_row_events: i32,
    /// A constraint trigger's `pg_constraint.condeferrable`; `None` for a
    /// plain trigger.
    pub(crate) constraint_deferrable: Option<bool>,
    /// FOR EACH ROW.
    pub(crate) row: bool,
    /// `TRIGGER_TYPE_BEFORE` / `_AFTER` / `_INSTEAD`.
    pub(crate) timing: i32,
    /// `TRIGGER_TYPE_INSERT | _DELETE | _UPDATE | _TRUNCATE` bits.
    pub(crate) events: i32,
    /// `UPDATE OF` columns (`tgattr`), by name.
    pub(crate) columns: Vec<String>,
    /// A constraint trigger's `FROM` relation (`tgconstrrelid`).
    pub(crate) constrrel: Option<PgClassOid>,
    /// The partitioned table whose same-named trigger this one clones
    /// (`tgparentid`).
    pub(crate) parent: Option<PgClassOid>,
}

impl Trigger {
    /// The relation has an INSTEAD OF row trigger for this event
    /// (`TRIGGER_TYPE_INSERT` / `_UPDATE` / `_DELETE` bits).
    pub(crate) fn is_instead_row_for(&self, event_bit: i32) -> bool {
        self.instead_row_events & event_bit != 0
    }
}

/// `TRIGGER_TYPE_UPDATE` (trigger.h).
pub(crate) const TRIGGER_TYPE_UPDATE: i32 = 1 << 4;

/// `TRIGGER_TYPE_*` bits (trigger.h).
pub(crate) const TRIGGER_TYPE_BEFORE: i32 = 1 << 1;
pub(crate) const TRIGGER_TYPE_INSERT: i32 = 1 << 2;
pub(crate) const TRIGGER_TYPE_DELETE: i32 = 1 << 3;
const TRIGGER_TYPE_TRUNCATE: i32 = 1 << 5;
const TRIGGER_TYPE_INSTEAD: i32 = 1 << 6;
/// `TRIGGER_TYPE_AFTER` is no bit at all.
const TRIGGER_TYPE_AFTER: i32 = 0;

/// CreateTriggerFiringOn: the WHEN condition is transformed as a boolean
/// WHERE clause (EXPR_KIND_TRIGGER_WHEN) over the relation as `old` and
/// `new`, and may only reference the row values the trigger has — none
/// for a statement trigger, no OLD for INSERT, no NEW for DELETE, and for a
/// BEFORE trigger no NEW system column nor generated column (their values
/// don't exist yet).
fn check_when_clause(
    interp: &PgCatalog,
    relid: PgClassOid,
    stmt: &CreateTrigStmt,
    when: &typedpg_pg_query::protobuf::Node,
) -> Result<(), DdlError> {
    use crate::expr::{TypeGoal, infer_expr};
    use crate::nullability::NullabilityContext;
    use crate::param_collector::ParamCollector;
    use crate::qualified_name::QualifiedName;
    use crate::scope::Scope;

    let unsupported = |e: crate::error::AnalyzeError| DdlError::UnsupportedDdl(e.to_string());
    super::expr_kind::check_expr_kind(interp, when, super::expr_kind::ExprKind::TriggerWhen)?;
    crate::resolve::check_no_srf_in_clause(when, interp, "trigger WHEN conditions")
        .map_err(unsupported)?;
    let Some(class) = interp.pg_class.get(&relid) else {
        return Ok(());
    };
    let nspname = interp
        .namespace_name(class.relnamespace)
        .unwrap_or("public")
        .to_owned();
    let attrs = interp.attributes_of(relid).to_vec();
    let mut scope = Scope::default();
    for alias in ["old", "new"] {
        scope.add_dml_target(
            interp,
            alias,
            QualifiedName::new(nspname.clone(), class.relname.clone()),
            &attrs,
        );
    }
    let null_ctx = NullabilityContext::default();
    let mut params = ParamCollector::default();
    let result = infer_expr(
        when,
        crate::expr::Ctx::new(&scope, &null_ctx, interp),
        &mut params,
        TypeGoal::NONE,
    )
    .map_err(unsupported)?;
    // transformWhereClause: coerce_to_boolean.
    if result.type_oid != crate::pg_catalog::oid::BOOL
        && result.type_oid != crate::pg_catalog::oid::UNKNOWN
    {
        return Err(DdlError::UnsupportedDdl(format!(
            "argument of WHEN must be type boolean, not type {}",
            super::util::format_type_for_message(interp, result.type_oid)
        )));
    }

    let before = stmt.timing & TRIGGER_TYPE_BEFORE != 0;
    let has_generated = attrs.iter().any(|a| a.attgenerated.is_some());
    let Some(inner) = when.node.as_ref() else {
        return Ok(());
    };
    for (n, ..) in inner.nodes() {
        let typedpg_pg_query::NodeRef::ColumnRef(cr) = n else {
            continue;
        };
        let fields: Vec<Option<&str>> = cr.fields.iter().map(super::util::node_string).collect();
        let (varno, column) = match fields.as_slice() {
            // `old` / `new` as a whole row, `old.*`, `old.col`.
            [Some(rel)]
                if (*rel == "old" || *rel == "new") && !attrs.iter().any(|a| a.attname == *rel) =>
            {
                (*rel, None)
            }
            [Some(rel), rest] if *rel == "old" || *rel == "new" => (*rel, *rest),
            // An unqualified column is ambiguous between OLD and NEW; the
            // analysis above reported it.
            _ => continue,
        };
        if !stmt.row {
            return Err(DdlError::Parse(
                "statement trigger's WHEN condition cannot reference column values".into(),
            ));
        }
        if varno == "old" {
            if stmt.events & TRIGGER_TYPE_INSERT != 0 {
                return Err(DdlError::Parse(
                    "INSERT trigger's WHEN condition cannot reference OLD values".into(),
                ));
            }
            continue;
        }
        if stmt.events & TRIGGER_TYPE_DELETE != 0 {
            return Err(DdlError::Parse(
                "DELETE trigger's WHEN condition cannot reference NEW values".into(),
            ));
        }
        if !before {
            continue;
        }
        match column {
            None => {
                if has_generated {
                    return Err(DdlError::Parse(
                        "BEFORE trigger's WHEN condition cannot reference NEW generated columns \
                         (A whole-row reference is used and the table contains generated \
                         columns.)"
                            .into(),
                    ));
                }
            }
            Some(col) => match attrs.iter().find(|a| a.attname == col) {
                Some(attr) if attr.attgenerated.is_some() => {
                    return Err(DdlError::Parse(format!(
                        "BEFORE trigger's WHEN condition cannot reference NEW generated columns \
                         (Column \"{col}\" is a generated column.)"
                    )));
                }
                Some(_) => {}
                None if crate::pg_catalog::SYSTEM_COLUMNS
                    .iter()
                    .any(|(n, ..)| *n == col) =>
                {
                    return Err(DdlError::UnsupportedDdl(
                        "BEFORE trigger's WHEN condition cannot reference NEW system columns"
                            .into(),
                    ));
                }
                None => {}
            },
        }
    }
    Ok(())
}

/// `CREATE [OR REPLACE] [CONSTRAINT] TRIGGER` (CreateTriggerFiringOn).
pub fn create_trigger(interp: &mut PgCatalog, stmt: &CreateTrigStmt) -> Result<(), DdlError> {
    let Some(rv) = stmt.relation.as_ref() else {
        return Ok(());
    };
    let (_, relid) = super::util::lookup_relation(interp, rv)?;
    let Some(class) = interp.pg_class.get(&relid).cloned() else {
        return Ok(());
    };
    check_relkind(interp, relid, stmt)?;
    // A constraint trigger's FROM relation.
    let constrrel = match stmt.constrrel.as_ref() {
        Some(rv) if stmt.isconstraint => Some(super::util::lookup_relation(interp, rv)?.1),
        _ => None,
    };
    check_firing(stmt)?;
    check_transition_tables(interp, relid, &class, stmt)?;
    if let Some(when) = stmt.when_clause.as_deref() {
        check_when_clause(interp, relid, stmt, when)?;
    }

    // The function takes no declared arguments (the trigger's arguments go
    // through TG_ARGV) and must return `trigger`.
    let parts: Vec<&str> = stmt
        .funcname
        .iter()
        .filter_map(super::util::node_string)
        .collect();
    let (schema, name) = match parts.as_slice() {
        [name] => (None, *name),
        [schema, name] => (Some(*schema), *name),
        _ => return Ok(()),
    };
    let Some((_, function)) =
        super::alter::find_proc(interp, schema, name, &|p| p.proargtypes.is_empty())
    else {
        return Err(DdlError::TypeNotFound(format!(
            "function {}() does not exist",
            parts.join(".")
        )));
    };
    let returns_trigger = interp
        .pg_proc
        .get(&function)
        .and_then(|p| interp.pg_type.get(&p.prorettype))
        .is_some_and(|t| t.typname == "trigger");
    if !returns_trigger {
        return Err(DdlError::Parse(format!(
            "function {} must return type trigger",
            parts.join(".")
        )));
    }

    let columns: Vec<String> = stmt
        .columns
        .iter()
        .filter_map(super::util::node_string)
        .map(str::to_owned)
        .collect();
    let trigger = Trigger {
        oid: crate::ddl::depend::PENDING_OID,
        name: stmt.trigname.clone(),
        function,
        instead_row_events: if stmt.timing & TRIGGER_TYPE_INSTEAD != 0 && stmt.row {
            stmt.events
        } else {
            0
        },
        constraint_deferrable: stmt.isconstraint.then_some(stmt.deferrable),
        row: stmt.row,
        timing: stmt.timing,
        events: stmt.events,
        columns,
        constrrel,
        parent: None,
    };
    install(interp, relid, trigger, stmt.replace, false)
}

/// CreateTriggerFiringOn's relation checks, which it also makes on each
/// partition a trigger is cloned to.
fn check_relkind(
    interp: &PgCatalog,
    relid: PgClassOid,
    stmt: &CreateTrigStmt,
) -> Result<(), DdlError> {
    let Some(class) = interp.pg_class.get(&relid) else {
        return Ok(());
    };
    let before_or_after = matches!(stmt.timing, TRIGGER_TYPE_BEFORE | TRIGGER_TYPE_AFTER);
    let relname = &class.relname;
    let err = |msg: String| Err(DdlError::Parse(msg));
    match class.relkind {
        RelKind::Table | RelKind::Partitioned if !before_or_after => {
            return err(format!(
                "\"{relname}\" is a table (Tables cannot have INSTEAD OF triggers.)"
            ));
        }
        RelKind::Partitioned if stmt.row && !stmt.transition_rels.is_empty() => {
            return err(format!(
                "\"{relname}\" is a partitioned table (ROW triggers with transition tables \
                 are not supported on partitioned tables.)"
            ));
        }
        RelKind::View if !before_or_after => {}
        RelKind::View if stmt.row => {
            return err(format!(
                "\"{relname}\" is a view (Views cannot have row-level BEFORE or AFTER \
                 triggers.)"
            ));
        }
        RelKind::View if stmt.events & TRIGGER_TYPE_TRUNCATE != 0 => {
            return err(format!(
                "\"{relname}\" is a view (Views cannot have TRUNCATE triggers.)"
            ));
        }
        RelKind::ForeignTable if !before_or_after => {
            return err(format!(
                "\"{relname}\" is a foreign table (Foreign tables cannot have INSTEAD OF \
                 triggers.)"
            ));
        }
        RelKind::ForeignTable if stmt.isconstraint => {
            return err(format!(
                "\"{relname}\" is a foreign table (Foreign tables cannot have constraint \
                 triggers.)"
            ));
        }
        RelKind::Table | RelKind::Partitioned | RelKind::View | RelKind::ForeignTable => {}
        _ => return err(format!("relation \"{relname}\" cannot have triggers")),
    }
    if interp.is_system_class(relid) {
        return err(format!(
            "permission denied: \"{relname}\" is a system catalog"
        ));
    }
    Ok(())
}

/// CreateTriggerFiringOn: what the trigger type allows.
fn check_firing(stmt: &CreateTrigStmt) -> Result<(), DdlError> {
    let unsupported = |msg: &str| Err(DdlError::UnsupportedDdl(msg.to_owned()));
    if stmt.row && stmt.events & TRIGGER_TYPE_TRUNCATE != 0 {
        return unsupported("TRUNCATE FOR EACH ROW triggers are not supported");
    }
    if stmt.timing & TRIGGER_TYPE_INSTEAD != 0 {
        if !stmt.row {
            return unsupported("INSTEAD OF triggers must be FOR EACH ROW");
        }
        if stmt.when_clause.is_some() {
            return unsupported("INSTEAD OF triggers cannot have WHEN conditions");
        }
        if !stmt.columns.is_empty() {
            return unsupported("INSTEAD OF triggers cannot have column lists");
        }
    }
    Ok(())
}

/// CreateTriggerFiringOn: the REFERENCING clause's transition tables.
fn check_transition_tables(
    interp: &PgCatalog,
    relid: PgClassOid,
    class: &crate::pg_catalog::PgClass,
    stmt: &CreateTrigStmt,
) -> Result<(), DdlError> {
    let relname = &class.relname;
    let mut old_name: Option<&str> = None;
    let mut new_name: Option<&str> = None;
    for rel in &stmt.transition_rels {
        let Some(node::Node::TriggerTransition(tt)) = rel.node.as_ref() else {
            continue;
        };
        let unsupported = |msg: String| Err(DdlError::UnsupportedDdl(msg));
        let definition = |msg: &str| Err(DdlError::Parse(msg.to_owned()));
        if !tt.is_table {
            return unsupported(
                "ROW variable naming in the REFERENCING clause is not supported (Use OLD \
                 TABLE or NEW TABLE for naming transition tables.)"
                    .into(),
            );
        }
        match class.relkind {
            RelKind::ForeignTable => {
                return unsupported(format!(
                    "\"{relname}\" is a foreign table (Triggers on foreign tables cannot have \
                     transition tables.)"
                ));
            }
            RelKind::View => {
                return unsupported(format!(
                    "\"{relname}\" is a view (Triggers on views cannot have transition \
                     tables.)"
                ));
            }
            _ => {}
        }
        if stmt.row {
            let parents: Vec<PgClassOid> = interp
                .pg_inherits
                .iter()
                .filter(|i| i.inhrelid == relid)
                .map(|i| i.inhparent)
                .collect();
            if interp.partition_bounds.contains_key(&relid) {
                return unsupported(
                    "ROW triggers with transition tables are not supported on partitions".into(),
                );
            }
            if !parents.is_empty() {
                return unsupported(
                    "ROW triggers with transition tables are not supported on inheritance \
                     children"
                        .into(),
                );
            }
        }
        if stmt.timing != TRIGGER_TYPE_AFTER {
            return definition("transition table name can only be specified for an AFTER trigger");
        }
        if stmt.events & TRIGGER_TYPE_TRUNCATE != 0 {
            return unsupported(
                "TRUNCATE triggers with transition tables are not supported".into(),
            );
        }
        let events = [
            TRIGGER_TYPE_INSERT,
            TRIGGER_TYPE_UPDATE,
            TRIGGER_TYPE_DELETE,
        ]
        .iter()
        .filter(|&&e| stmt.events & e != 0)
        .count();
        if events != 1 {
            return unsupported(
                "transition tables cannot be specified for triggers with more than one event"
                    .into(),
            );
        }
        if !stmt.columns.is_empty() {
            return unsupported(
                "transition tables cannot be specified for triggers with column lists".into(),
            );
        }
        if tt.is_new {
            if stmt.events & (TRIGGER_TYPE_INSERT | TRIGGER_TYPE_UPDATE) == 0 {
                return definition(
                    "NEW TABLE can only be specified for an INSERT or UPDATE trigger",
                );
            }
            if new_name.is_some() {
                return definition("NEW TABLE cannot be specified multiple times");
            }
            new_name = Some(&tt.name);
        } else {
            if stmt.events & (TRIGGER_TYPE_DELETE | TRIGGER_TYPE_UPDATE) == 0 {
                return definition(
                    "OLD TABLE can only be specified for a DELETE or UPDATE trigger",
                );
            }
            if old_name.is_some() {
                return definition("OLD TABLE cannot be specified multiple times");
            }
            old_name = Some(&tt.name);
        }
    }
    if old_name.is_some() && old_name == new_name {
        return Err(DdlError::Parse(
            "OLD TABLE name and NEW TABLE name cannot be the same".into(),
        ));
    }
    Ok(())
}

/// The rest of CreateTriggerFiringOn, for a validated trigger on `relid`:
/// the name check against the relation's triggers, the `UPDATE OF`
/// columns, and — for a row trigger on a partitioned table — the clone on
/// each partition. `in_partition`: `trigger` is such a clone.
fn install(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    trigger: Trigger,
    replace: bool,
    in_partition: bool,
) -> Result<(), DdlError> {
    let relname = interp
        .pg_class
        .get(&relid)
        .map(|c| c.relname.clone())
        .unwrap_or_default();
    let existing = interp
        .triggers
        .get(&relid)
        .and_then(|ts| ts.iter().position(|t| t.name == trigger.name));
    if let Some(at) = existing {
        let old = &interp.triggers[&relid][at];
        if !replace {
            return Err(DdlError::DuplicateObject(format!(
                "trigger \"{}\" for relation \"{relname}\" already exists",
                trigger.name
            )));
        }
        if old.parent.is_some() && !in_partition {
            return Err(DdlError::DuplicateObject(format!(
                "trigger \"{}\" for relation \"{relname}\" is an internal or a child trigger",
                trigger.name
            )));
        }
        if old.constraint_deferrable.is_some() {
            return Err(DdlError::DuplicateObject(format!(
                "trigger \"{}\" for relation \"{relname}\" is a constraint trigger",
                trigger.name
            )));
        }
    }
    // The column list: existing user columns, each once.
    for (i, column) in trigger.columns.iter().enumerate() {
        if interp.attribute_by_name(relid, column).is_none() {
            return Err(DdlError::Parse(format!(
                "column \"{column}\" of relation \"{relname}\" does not exist"
            )));
        }
        if trigger.columns[..i].contains(column) {
            return Err(DdlError::Parse(format!(
                "column \"{column}\" specified more than once"
            )));
        }
    }
    let clone_to_partitions =
        trigger.row && interp.pg_class.get(&relid).map(|c| c.relkind) == Some(RelKind::Partitioned);
    let clone = trigger.clone();
    // A replaced trigger keeps its identity; a new one (a partition's clone
    // too) gets its own.
    let oid = match existing {
        Some(at) => interp.triggers[&relid][at].oid,
        None => crate::oid::PgGenericOid::from_nonzero(interp.alloc_oid()?),
    };
    let trigger = Trigger { oid, ..trigger };
    let triggers = interp.triggers.entry(relid).or_default();
    match existing {
        Some(at) => triggers[at] = trigger,
        None => triggers.push(trigger),
    }
    if clone_to_partitions {
        for part in partitions_of(interp, relid) {
            clone_trigger(interp, relid, part, &clone, replace)?;
        }
    }
    Ok(())
}

/// The partitions of `relid`.
fn partitions_of(interp: &PgCatalog, relid: PgClassOid) -> Vec<PgClassOid> {
    interp
        .pg_inherits
        .iter()
        .filter(|i| i.inhparent == relid)
        .map(|i| i.inhrelid)
        .collect()
}

/// Create the clone of row trigger `trigger` of partitioned table `parent`
/// on its partition `part`.
fn clone_trigger(
    interp: &mut PgCatalog,
    parent: PgClassOid,
    part: PgClassOid,
    trigger: &Trigger,
    replace: bool,
) -> Result<(), DdlError> {
    let stmt = CreateTrigStmt {
        trigname: trigger.name.clone(),
        isconstraint: trigger.constraint_deferrable.is_some(),
        row: true,
        timing: trigger.timing,
        events: trigger.events,
        ..Default::default()
    };
    check_relkind(interp, part, &stmt)?;
    install(
        interp,
        part,
        Trigger {
            parent: Some(parent),
            ..trigger.clone()
        },
        replace,
        true,
    )
}

/// CloneRowTriggersToPartition: a new partition (CREATE TABLE ... PARTITION
/// OF, ATTACH PARTITION) gets a clone of each row trigger of its parent.
pub(crate) fn clone_row_triggers_to_partition(
    interp: &mut PgCatalog,
    parent: PgClassOid,
    part: PgClassOid,
) -> Result<(), DdlError> {
    let row_triggers: Vec<Trigger> = interp
        .triggers
        .get(&parent)
        .into_iter()
        .flatten()
        .filter(|t| t.row)
        .cloned()
        .collect();
    for trigger in &row_triggers {
        clone_trigger(interp, parent, part, trigger, false)?;
    }
    Ok(())
}

/// DropClonedTriggersFromPartition (DETACH PARTITION): the partition's
/// clones go, but for those of constraint triggers with a FROM relation.
pub(crate) fn drop_cloned_triggers(interp: &mut PgCatalog, part: PgClassOid) {
    if let Some(triggers) = interp.triggers.get_mut(&part) {
        triggers.retain(|t| t.parent.is_none() || t.constrrel.is_some());
    }
}

/// Remove trigger `name` of `relid` and, recursively, its clones.
fn remove_with_clones(interp: &mut PgCatalog, relid: PgClassOid, name: &str) {
    if let Some(triggers) = interp.triggers.get_mut(&relid) {
        let dropped = triggers.iter().find(|t| t.name == name).map(|t| t.oid);
        triggers.retain(|t| t.name != name);
        if let Some(oid) = dropped {
            interp.remove_dependencies_of(super::depend::PG_TRIGGER_RELID, oid);
        }
    }
    for part in partitions_of(interp, relid) {
        if interp
            .triggers
            .get(&part)
            .is_some_and(|ts| ts.iter().any(|t| t.name == name && t.parent == Some(relid)))
        {
            remove_with_clones(interp, part, name);
        }
    }
}

/// `DROP TRIGGER [IF EXISTS] name ON table`.
pub(crate) fn drop_trigger(
    interp: &mut PgCatalog,
    obj_node: &typedpg_pg_query::protobuf::Node,
    missing_ok: bool,
) -> Result<(), DdlError> {
    let Some(node::Node::List(list)) = obj_node.node.as_ref() else {
        return Ok(());
    };
    let parts: Vec<String> = list
        .items
        .iter()
        .filter_map(super::util::node_string)
        .map(str::to_owned)
        .collect();
    let Some((trigname, rel)) = parts.split_last() else {
        return Ok(());
    };
    let rv = typedpg_pg_query::protobuf::RangeVar {
        schemaname: if rel.len() == 2 {
            rel[0].clone()
        } else {
            String::new()
        },
        relname: rel.last().cloned().unwrap_or_default(),
        inh: true,
        ..Default::default()
    };
    let relid: PgClassOid = match super::util::lookup_relation(interp, &rv) {
        Ok((_, oid)) => oid,
        // get_object_address_relobject: a missing relation (or schema) is
        // skipped with a notice under IF EXISTS.
        Err(_) if missing_ok => return Ok(()),
        Err(e) => return Err(e),
    };
    let relname = rv.relname.clone();
    let Some(trigger) = interp
        .triggers
        .get(&relid)
        .and_then(|ts| ts.iter().find(|t| &t.name == trigname))
    else {
        if missing_ok {
            return Ok(());
        }
        return Err(DdlError::TypeNotFound(format!(
            "trigger \"{trigname}\" for table \"{relname}\" does not exist"
        )));
    };
    // A clone depends on its parent's trigger (DEPENDENCY_PARTITION_PRI).
    if let Some(parent) = trigger.parent {
        let parent_name = interp
            .pg_class
            .get(&parent)
            .map(|c| c.relname.clone())
            .unwrap_or_default();
        return Err(DdlError::DependencyError(format!(
            "cannot drop trigger {trigname} on table {relname} because trigger {trigname} on \
             table {parent_name} requires it (You can drop trigger {trigname} on table \
             {parent_name} instead.)"
        )));
    }
    remove_with_clones(interp, relid, trigname);
    Ok(())
}

/// The triggers (as `(relation, name)`) that run function `proc`.
pub(crate) fn triggers_using_function(
    interp: &PgCatalog,
    proc: PgProcOid,
) -> Vec<(PgClassOid, String)> {
    interp
        .triggers
        .iter()
        .flat_map(|(&relid, ts)| {
            ts.iter()
                .filter(move |t| t.function == proc)
                .map(move |t| (relid, t.name.clone()))
        })
        .collect()
}

/// `ALTER TRIGGER name ON table RENAME TO new` (renametrig).
pub(crate) fn rename_trigger(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::protobuf::RenameStmt,
) -> Result<(), DdlError> {
    let Some(rv) = stmt.relation.as_ref() else {
        return Ok(());
    };
    let relid = match super::util::lookup_relation(interp, rv) {
        Ok((_, oid)) => oid,
        Err(_) if stmt.missing_ok => return Ok(()),
        Err(e) => return Err(e),
    };
    // RangeVarCallbackForRenameTrigger.
    if let Some(class) = interp.pg_class.get(&relid) {
        if !matches!(
            class.relkind,
            RelKind::Table | RelKind::Partitioned | RelKind::View | RelKind::ForeignTable
        ) {
            return Err(DdlError::Parse(format!(
                "relation \"{}\" cannot have triggers",
                rv.relname
            )));
        }
        if interp.is_system_class(relid) {
            return Err(DdlError::Parse(format!(
                "permission denied: \"{}\" is a system catalog",
                rv.relname
            )));
        }
    }
    let Some(trigger) = interp
        .triggers
        .get(&relid)
        .and_then(|ts| ts.iter().find(|t| t.name == stmt.subname))
    else {
        return Err(DdlError::TypeNotFound(format!(
            "trigger \"{}\" for table \"{}\" does not exist",
            stmt.subname, rv.relname
        )));
    };
    // A partition's trigger is named after its parent's.
    if let Some(parent) = trigger.parent {
        let parent_name = interp
            .pg_class
            .get(&parent)
            .map(|c| c.relname.clone())
            .unwrap_or_default();
        return Err(DdlError::UnsupportedDdl(format!(
            "cannot rename trigger \"{}\" on table \"{}\" (Rename the trigger on the \
             partitioned table \"{parent_name}\" instead.)",
            stmt.subname, rv.relname
        )));
    }
    rename_with_clones(interp, relid, &stmt.subname, &stmt.newname)
}

/// renametrig_internal on `relid`, then renametrig_partition on its
/// partitions' clones.
fn rename_with_clones(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    old: &str,
    new: &str,
) -> Result<(), DdlError> {
    if old == new {
        return Ok(());
    }
    let relname = interp
        .pg_class
        .get(&relid)
        .map(|c| c.relname.clone())
        .unwrap_or_default();
    let triggers = interp.triggers.entry(relid).or_default();
    if triggers.iter().any(|t| t.name == new) {
        return Err(DdlError::DuplicateObject(format!(
            "trigger \"{new}\" for relation \"{relname}\" already exists"
        )));
    }
    if let Some(t) = triggers.iter_mut().find(|t| t.name == old) {
        t.name = new.to_owned();
    }
    for part in partitions_of(interp, relid) {
        if interp
            .triggers
            .get(&part)
            .is_some_and(|ts| ts.iter().any(|t| t.name == old && t.parent == Some(relid)))
        {
            rename_with_clones(interp, part, old, new)?;
        }
    }
    Ok(())
}
