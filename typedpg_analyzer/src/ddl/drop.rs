//! DROP statement handler.

use typedpg_pg_query::protobuf::{DropBehavior, DropStmt, ObjectType, node};

use super::DdlError;
use super::util::{extract_names, format_type_for_message, node_string, resolve_type_name};
use super::views;
use crate::oid::{PgCastOid, PgClassOid, PgNamespaceOid, PgOperatorOid, PgProcOid, PgTypeOid};
use crate::pg_catalog::{
    PG_CAST_RELID, PG_CLASS_RELID, PG_EXTENSION_RELID, PG_NAMESPACE_RELID, PG_OPERATOR_RELID,
    PG_PROC_RELID, PG_TYPE_RELID, PgCatalog, PgOperator, RelKind,
};
use crate::qualified_name::QualifiedName;

pub fn drop_objects(interp: &mut PgCatalog, stmt: &DropStmt) -> Result<(), DdlError> {
    // A TOAST table outlives the columns that needed it, which a CASCADE
    // can drop.
    super::tables::toast::note_toast_tables(interp);
    let obj_type = ObjectType::try_from(stmt.remove_type).unwrap_or(ObjectType::Undefined);
    let cascade = matches!(
        DropBehavior::try_from(stmt.behavior),
        Ok(DropBehavior::DropCascade)
    );
    if stmt.concurrent {
        super::indexes::check_drop_concurrently(interp, stmt)?;
    }

    // The relations one DROP names go together (performMultipleDeletions):
    // dependencies among them need no CASCADE.
    let named_relations: Vec<PgClassOid> = stmt
        .objects
        .iter()
        .filter_map(|obj_node| match obj_node.node.as_ref() {
            Some(node::Node::List(list)) => {
                let (schema, name) = extract_names(&list.items, interp);
                let nsoid = interp.namespace_oid(&schema)?;
                interp.class_by_qname.get(&(nsoid, name)).copied()
            }
            _ => None,
        })
        .collect();
    // Every name is looked up before anything is deleted: a name repeated
    // (or two names of one object) is one target, and when several objects
    // are named a dependency blocks them together.
    let targets: Vec<Option<(i32, u32)>> = stmt
        .objects
        .iter()
        .map(|o| super::depend::drop_target_identity(interp, obj_type, o))
        .collect();
    let result = drop_each(interp, stmt, obj_type, cascade, &named_relations, &targets);
    if targets.iter().flatten().count() > 1 {
        return result.map_err(super::depend::multiple_targets_message);
    }
    result
}

fn drop_each(
    interp: &mut PgCatalog,
    stmt: &DropStmt,
    obj_type: ObjectType,
    cascade: bool,
    named_relations: &[PgClassOid],
    targets: &[Option<(i32, u32)>],
) -> Result<(), DdlError> {
    for (i, obj_node) in stmt.objects.iter().enumerate() {
        if targets[i].is_some() && targets[..i].contains(&targets[i]) {
            continue;
        }
        match obj_type {
            ObjectType::ObjectTable
            | ObjectType::ObjectView
            | ObjectType::ObjectMatview
            | ObjectType::ObjectSequence
            | ObjectType::ObjectForeignTable => {
                drop_relation(
                    interp,
                    obj_node,
                    stmt.missing_ok,
                    cascade,
                    obj_type,
                    named_relations,
                )?;
            }
            ObjectType::ObjectType | ObjectType::ObjectDomain => {
                drop_type(interp, obj_node, stmt.missing_ok, cascade)?;
            }
            ObjectType::ObjectFunction | ObjectType::ObjectProcedure => {
                drop_function(interp, obj_node, stmt.missing_ok, cascade, obj_type)?;
            }
            ObjectType::ObjectExtension => {
                drop_extension(interp, obj_node, stmt.missing_ok, cascade)?;
            }
            ObjectType::ObjectCast => {
                drop_cast(interp, obj_node, stmt.missing_ok)?;
            }
            ObjectType::ObjectOperator => {
                drop_operator(interp, obj_node, stmt.missing_ok)?;
            }
            ObjectType::ObjectAggregate => {
                drop_aggregate(interp, obj_node, stmt.missing_ok, cascade)?;
            }
            ObjectType::ObjectSchema => {
                drop_schema(interp, obj_node, stmt.missing_ok, cascade)?;
            }
            ObjectType::ObjectIndex => {
                drop_index(interp, obj_node, stmt.missing_ok)?;
            }
            ObjectType::ObjectTrigger => {
                super::triggers::drop_trigger(interp, obj_node, stmt.missing_ok)?;
            }
            ObjectType::ObjectPolicy => {
                super::policies::drop_policy(interp, obj_node, stmt.missing_ok)?;
            }
            ObjectType::ObjectAccessMethod
            | ObjectType::ObjectOpclass
            | ObjectType::ObjectOpfamily => {
                super::opclass::drop_am_object(interp, obj_type, obj_node, stmt.missing_ok)?;
            }
            ObjectType::ObjectFdw | ObjectType::ObjectForeignServer => {
                super::fdw::drop_foreign_object(
                    interp,
                    obj_type == ObjectType::ObjectFdw,
                    obj_node,
                    stmt.missing_ok,
                    cascade,
                )?;
            }
            ObjectType::ObjectEventTrigger => {
                super::event_triggers::drop_event_trigger(interp, obj_node, stmt.missing_ok)?;
            }
            ObjectType::ObjectTsconfiguration
            | ObjectType::ObjectTsdictionary
            | ObjectType::ObjectTsparser
            | ObjectType::ObjectTstemplate => {
                super::text_search::drop(interp, obj_type, obj_node, stmt.missing_ok)?;
            }
            ObjectType::ObjectConversion => {
                super::conversions::drop_conversion(interp, obj_node, stmt.missing_ok)?;
            }
            ObjectType::ObjectLanguage => {
                super::languages::drop_language(interp, obj_node, stmt.missing_ok)?;
            }
            ObjectType::ObjectPublication => {
                super::publications::drop_publication(interp, obj_node, stmt.missing_ok)?;
            }
            ObjectType::ObjectStatisticExt => {
                super::statistics::drop_statistics(interp, obj_node, stmt.missing_ok)?;
            }
            ObjectType::ObjectRule => {
                super::rules::drop_rule(interp, obj_node, stmt.missing_ok)?;
            }
            ObjectType::ObjectTransform => {
                super::languages::drop_transform(interp, obj_node, stmt.missing_ok)?;
            }
            _ => {}
        }
    }

    Ok(())
}

fn drop_relation(
    interp: &mut PgCatalog,
    obj_node: &typedpg_pg_query::protobuf::Node,
    missing_ok: bool,
    cascade: bool,
    requested: ObjectType,
    named_relations: &[PgClassOid],
) -> Result<(), DdlError> {
    let names = match obj_node.node.as_ref() {
        Some(node::Node::List(list)) => &list.items,
        _ => return Ok(()),
    };

    // The keyword `DROP` was issued with — `DROP SEQUENCE` must report
    // `sequence "x" does not exist`, not `table "x" …`, so the message
    // prefix lines up with PG's wire-protocol error.
    let requested_kind = match requested {
        ObjectType::ObjectView => "view",
        ObjectType::ObjectMatview => "materialized view",
        ObjectType::ObjectSequence => "sequence",
        ObjectType::ObjectForeignTable => "foreign table",
        _ => "table",
    };

    let (schema, name) = extract_names(names, interp);
    let Some(nsoid) = interp.namespace_oid(&schema) else {
        if missing_ok {
            return Ok(());
        }
        return Err(DdlError::TableNotFound(format!(
            "{requested_kind} \"{name}\" does not exist"
        )));
    };
    let Some(class_oid) = interp.class_by_qname.get(&(nsoid, name.clone())).copied() else {
        if missing_ok {
            return Ok(());
        }
        return Err(DdlError::TableNotFound(format!(
            "{requested_kind} \"{name}\" does not exist"
        )));
    };

    // PG renders DROP errors in terms of the relation's *actual* kind
    // (table, view, materialized view, sequence). Pick the right keyword
    // so dependency-error messages line up with PG's wire-protocol error.
    let actual_relkind = interp.pg_class.get(&class_oid).map(|c| c.relkind);
    let kind = match actual_relkind {
        Some(RelKind::View) => "view",
        Some(RelKind::MaterializedView) => "materialized view",
        Some(RelKind::Sequence) => "sequence",
        Some(RelKind::ForeignTable) => "foreign table",
        _ => "table",
    };

    // Reject a kind mismatch — `DROP SEQUENCE` on a table, `DROP TABLE` on
    // a view, etc. PG: `"x" is not a sequence`.
    let kind_matches = match requested {
        ObjectType::ObjectView => actual_relkind == Some(RelKind::View),
        ObjectType::ObjectMatview => actual_relkind == Some(RelKind::MaterializedView),
        ObjectType::ObjectSequence => actual_relkind == Some(RelKind::Sequence),
        ObjectType::ObjectForeignTable => actual_relkind == Some(RelKind::ForeignTable),
        _ => matches!(actual_relkind, Some(RelKind::Table | RelKind::Partitioned)),
    };
    if !kind_matches {
        return Err(DdlError::TableNotFound(format!(
            "\"{name}\" is not a {requested_kind}"
        )));
    }
    // RangeVarCallbackForDropRelation.
    if interp.is_system_class(class_oid) {
        return Err(DdlError::Parse(format!(
            "permission denied: \"{name}\" is a system catalog"
        )));
    }
    drop_relation_oid(interp, class_oid, &name, kind, cascade, named_relations)
}

/// Drop relation `class_oid` (named `name`, a `kind`) and what depends on
/// it: its inheritance children need CASCADE (a partition goes with its
/// parent regardless), and so do views, foreign keys in other tables,
/// functions over its row type and defaults using it as a sequence —
/// unless the same DROP names them too.
fn drop_relation_oid(
    interp: &mut PgCatalog,
    class_oid: PgClassOid,
    name: &str,
    kind: &str,
    cascade: bool,
    named_relations: &[PgClassOid],
) -> Result<(), DdlError> {
    // Inheritance children depend on their parent (DEPENDENCY_NORMAL); a
    // partition is part of it (DEPENDENCY_AUTO).
    let partitioned =
        interp.pg_class.get(&class_oid).map(|c| c.relkind) == Some(RelKind::Partitioned);
    let children: Vec<PgClassOid> = super::tables::inherit::children_of(interp, class_oid);
    if !partitioned
        && !cascade
        && let Some(&child) = children.iter().find(|c| !named_relations.contains(c))
    {
        let child_name = interp
            .pg_class
            .get(&child)
            .map(|c| c.relname.clone())
            .unwrap_or_default();
        return Err(DdlError::DependencyError(format!(
            "cannot drop {kind} {name} because other objects depend on it (table \
             {child_name} depends on {kind} {name})"
        )));
    }

    // Views reading the relation, or naming its row type (a cast, a typed
    // literal).
    let mut dependent_views = views::find_dependent_views(interp, class_oid);
    if let Some(row_type) = interp.pg_class.get(&class_oid).and_then(|c| c.reltype) {
        for t in std::iter::once(row_type).chain(interp.array_type_of(row_type)) {
            for v in views::find_views_depending_on_type(interp, t) {
                if v != class_oid && !dependent_views.contains(&v) {
                    dependent_views.push(v);
                }
            }
        }
    }
    if !dependent_views.is_empty() && !cascade {
        let view_names: Vec<String> = dependent_views
            .iter()
            .filter_map(|&v| {
                let c = interp.pg_class.get(&v)?;
                let nsname = interp.namespace_name(c.relnamespace).unwrap_or("?");
                Some(QualifiedName::new(nsname, &c.relname).to_string())
            })
            .collect();
        return Err(DdlError::DependencyError(format!(
            "cannot drop {kind} {name} because other objects depend on it \
             (view(s) {} depend on this)",
            view_names.join(", "),
        )));
    }

    // PG also blocks DROP TABLE when an FK on another table targets us
    // (without CASCADE). Walk pg_constraint for FK rows whose
    // `confrelid` is this relation.
    // The partitioned tables above this partition that aren't going too.
    let mut referenced_ancestors: Vec<PgClassOid> = Vec::new();
    let mut current = class_oid;
    while let Some(parent) = interp
        .pg_inherits
        .iter()
        .find(|i| i.inhrelid == current)
        .map(|i| i.inhparent)
        .filter(|p| interp.pg_class.get(p).map(|c| c.relkind) == Some(RelKind::Partitioned))
    {
        if !named_relations.contains(&parent) {
            referenced_ancestors.push(parent);
        }
        current = parent;
    }
    let dependent_fks: Vec<(crate::pg_catalog::PgConstraint, String)> = interp
        .pg_constraint
        .values()
        .filter(|c| {
            matches!(c.contype, crate::pg_catalog::ConType::ForeignKey)
                && (c.confrelid == Some(class_oid)
                    || c.confrelid.is_some_and(|r| referenced_ancestors.contains(&r)))
                // Its own foreign keys go with it.
                && c.conrelid != class_oid
                && !named_relations.contains(&c.conrelid)
                // A clone goes with its parent.
                && crate::ddl::tables::foreign_keys::fk_parent(interp, c.oid).is_none()
        })
        .filter_map(|c| {
            let owner = interp.pg_class.get(&c.conrelid)?;
            let nsname = interp.namespace_name(owner.relnamespace)?;
            Some((
                c.clone(),
                QualifiedName::new(nsname, &owner.relname).to_string(),
            ))
        })
        .collect();
    if !dependent_fks.is_empty() && !cascade {
        let labels: Vec<String> = dependent_fks
            .iter()
            .map(|(c, owner)| format!("{} on {}", c.conname, owner))
            .collect();
        return Err(DdlError::DependencyError(format!(
            "cannot drop {kind} {name} because other objects depend on it \
             (foreign key constraint(s) {} depend on this)",
            labels.join(", "),
        )));
    }

    // Functions taking or returning the relation's row type.
    let row_types: Vec<crate::oid::PgTypeOid> = interp
        .pg_class
        .get(&class_oid)
        .and_then(|c| c.reltype)
        .into_iter()
        .flat_map(|t| std::iter::once(t).chain(interp.array_type_of(t)))
        .collect();
    let dependent_functions = functions_using_types(interp, &row_types);
    if !dependent_functions.is_empty() && !cascade {
        return Err(DdlError::DependencyError(format!(
            "cannot drop {kind} {name} because other objects depend on it \
             (function {} depends on type {name})",
            describe_function(interp, dependent_functions[0]),
        )));
    }
    // Columns of other relations holding the row type (or its array), and
    // domains / range types over it: each depends on the type, which is
    // internal to the relation.
    let mut dependent_columns: Vec<(PgClassOid, String)> = interp
        .pg_attribute
        .iter()
        .filter(|(relid, _)| **relid != class_oid && !named_relations.contains(relid))
        .filter(|(relid, _)| {
            interp
                .pg_class
                .get(relid)
                .is_some_and(|c| c.relkind != RelKind::View)
        })
        .flat_map(|(&relid, attrs)| {
            attrs
                .iter()
                .filter(|a| row_types.contains(&a.atttypid))
                .map(move |a| (relid, a.attname.clone()))
        })
        .collect();
    dependent_columns.sort();
    let mut dependent_types: Vec<crate::oid::PgTypeOid> = interp
        .pg_type
        .values()
        .filter(|t| t.typbasetype.is_some_and(|b| row_types.contains(&b)))
        .map(|t| t.oid)
        .chain(
            interp
                .pg_range
                .values()
                .filter(|r| row_types.contains(&r.rngsubtype))
                .map(|r| r.rngtypid),
        )
        .collect();
    dependent_types.sort();
    if !cascade {
        if let Some((relid, column)) = dependent_columns.first() {
            let owner = interp.pg_class.get(relid);
            let owner_kind = match owner.map(|c| c.relkind) {
                Some(RelKind::CompositeType) => "composite type",
                Some(RelKind::MaterializedView) => "materialized view",
                Some(RelKind::ForeignTable) => "foreign table",
                _ => "table",
            };
            return Err(DdlError::DependencyError(format!(
                "cannot drop {kind} {name} because other objects depend on it (column {column} \
                 of {owner_kind} {} depends on type {name})",
                owner.map(|c| c.relname.as_str()).unwrap_or("?"),
            )));
        }
        if let Some(t) = dependent_types.first() {
            return Err(DdlError::DependencyError(format!(
                "cannot drop {kind} {name} because other objects depend on it (type {} depends \
                 on type {name})",
                format_type_for_message(interp, *t),
            )));
        }
    }
    // SQL-standard function bodies, and other tables' policies, triggers and
    // rules, reading the relation (its own go with it).
    let object_dependents: Vec<super::coldeps::Dependent> =
        super::coldeps::dependents_on_relation(interp, class_oid)
            .into_iter()
            .filter(|d| {
                !matches!(d,
                    super::coldeps::Dependent::Policy { relid, .. }
                    | super::coldeps::Dependent::Trigger { relid, .. }
                    | super::coldeps::Dependent::Rule { relid, .. }
                        if named_relations.contains(relid))
            })
            .collect();
    if let Some(first) = object_dependents.first()
        && !cascade
    {
        return Err(DdlError::DependencyError(format!(
            "cannot drop {kind} {name} because other objects depend on it ({} depends on \
             {kind} {name})",
            super::coldeps::describe(interp, first),
        )));
    }
    // Column defaults using the sequence (`nextval('s')`).
    let dependent_defaults = defaults_using_sequence(interp, class_oid);
    if let Some(&(relid, attnum)) = dependent_defaults.first()
        && !cascade
    {
        let table = interp
            .pg_class
            .get(&relid)
            .map(|c| c.relname.clone())
            .unwrap_or_default();
        let column = interp
            .attributes_of(relid)
            .iter()
            .find(|a| a.attnum == attnum)
            .map(|a| a.attname.clone())
            .unwrap_or_default();
        return Err(DdlError::DependencyError(format!(
            "cannot drop {kind} {name} because other objects depend on it \
             (default value for column {column} of table {table} depends on {kind} {name})"
        )));
    }

    if !dependent_views.is_empty() {
        views::drop_views(interp, &dependent_views);
    }
    drop_functions_cascade(interp, &dependent_functions);
    for dependent in &object_dependents {
        super::coldeps::drop_dependent(interp, dependent);
    }
    // CASCADE drops those columns (not their relations) and types.
    for (relid, column) in dependent_columns {
        if let Some(attrs) = interp.pg_attribute.get_mut(&relid) {
            attrs.retain(|a| a.attname != column);
        }
    }
    for t in dependent_types {
        let Some(typ) = interp.pg_type.get(&t) else {
            continue;
        };
        let schema = interp
            .namespace_name(typ.typnamespace)
            .unwrap_or_default()
            .to_owned();
        let type_name = typedpg_pg_query::protobuf::Node {
            node: Some(node::Node::TypeName(typedpg_pg_query::protobuf::TypeName {
                names: [schema, typ.typname.clone()]
                    .into_iter()
                    .map(|s| typedpg_pg_query::protobuf::Node {
                        node: Some(node::Node::String(typedpg_pg_query::protobuf::String {
                            sval: s,
                        })),
                    })
                    .collect(),
                ..Default::default()
            })),
        };
        drop_type(interp, &type_name, true, true)?;
    }
    for (relid, attnum) in dependent_defaults {
        // DROP ... CASCADE drops the default expression, not the column.
        if let Some(attrs) = interp.pg_attribute.get_mut(&relid)
            && let Some(a) = attrs.iter_mut().find(|a| a.attnum == attnum)
        {
            a.atthasdef = false;
        }
        interp.attr_default_types.remove(&(relid, attnum));
        interp.attr_default_exprs.remove(&(relid, attnum));
        super::defaults::forget_default_dependencies(interp, relid, attnum);
    }
    if cascade && !dependent_fks.is_empty() {
        let fk_oids: Vec<_> = dependent_fks.iter().map(|(c, _)| c.oid).collect();
        for oid in fk_oids {
            interp.pg_constraint.remove(&oid);
            crate::ddl::tables::foreign_keys::drop_fk_clones(interp, oid);
        }
    }

    let mut going: Vec<PgClassOid> = named_relations.to_vec();
    going.push(class_oid);
    for child in children {
        if named_relations.contains(&child) || !interp.pg_class.contains_key(&child) {
            continue;
        }
        let child_name = interp
            .pg_class
            .get(&child)
            .map(|c| c.relname.clone())
            .unwrap_or_default();
        drop_relation_oid(interp, child, &child_name, "table", cascade, &going)?;
    }

    drop_relation_by_oid(interp, class_oid);
    Ok(())
}

/// Functions whose arguments or result use one of `types` — PG records a
/// normal dependency from the function on each such type.
fn functions_using_types(
    interp: &PgCatalog,
    types: &[crate::oid::PgTypeOid],
) -> Vec<crate::oid::PgProcOid> {
    if types.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<crate::oid::PgProcOid> = interp
        .pg_proc
        .values()
        .filter(|p| {
            types.contains(&p.prorettype)
                || p.proargtypes.iter().any(|t| types.contains(t))
                || p.proallargtypes.iter().any(|t| types.contains(t))
        })
        .map(|p| p.oid)
        .collect();
    out.sort();
    out
}

/// `name(argtypes)` as PG's `getObjectDescription` shows a function.
fn describe_function(interp: &PgCatalog, oid: crate::oid::PgProcOid) -> String {
    let Some(p) = interp.pg_proc.get(&oid) else {
        return String::new();
    };
    let args = p
        .proargtypes
        .iter()
        .map(|&t| format_type_for_message(interp, t))
        .collect::<Vec<_>>()
        .join(",");
    format!("{}({args})", p.proname)
}

/// DROP ... CASCADE of functions: views calling them go too.
fn drop_functions_cascade(interp: &mut PgCatalog, procs: &[crate::oid::PgProcOid]) {
    for &proc_oid in procs {
        let views_on = views::find_views_depending_on_function(interp, proc_oid);
        if !views_on.is_empty() {
            views::drop_views(interp, &views_on);
        }
        interp.remove_pg_proc(proc_oid);
        let obj = crate::oid::PgGenericOid::from_nonzero(proc_oid.into_nonzero());
        interp.remove_dependencies_of(PG_PROC_RELID, obj);
        interp.remove_dependencies_on(PG_PROC_RELID, obj);
    }
}

/// `(relid, attnum)` of the column defaults that reference sequence `seq`.
fn defaults_using_sequence(interp: &PgCatalog, seq: PgClassOid) -> Vec<(PgClassOid, i16)> {
    let seq_obj = crate::oid::PgGenericOid::from_nonzero(seq.into_nonzero());
    interp
        .iter_pg_depend()
        .filter(|d| {
            d.classid == PG_CLASS_RELID
                && d.refclassid == PG_CLASS_RELID
                && d.refobjid == seq_obj
                && d.objsubid > 0
                && matches!(d.deptype, crate::pg_catalog::DepType::Normal)
        })
        .filter_map(|d| Some((PgClassOid::new(d.objid.get())?, d.objsubid)))
        .collect()
}

/// Remove a relation row + its `pg_attribute` rows + the composite type +
/// the array wrapping the composite. Mirrors what `DROP TABLE` /
/// `DROP VIEW` does in PG.
pub(crate) fn drop_relation_by_oid(interp: &mut PgCatalog, class_oid: PgClassOid) {
    // Sequences the relation owns (serial / identity / OWNED BY) go with it.
    let owned = super::sequences::owned_sequences(interp, class_oid, None);
    let Some(class) = interp.remove_pg_class(class_oid) else {
        return;
    };
    for seq in owned {
        drop_relation_by_oid(interp, seq);
    }
    let class_obj = crate::oid::PgGenericOid::from_nonzero(class_oid.into_nonzero());
    interp.remove_dependencies_of(PG_CLASS_RELID, class_obj);
    interp.remove_dependencies_on(PG_CLASS_RELID, class_obj);
    interp.remove_pg_constraints_of(class_oid);
    interp.remove_pg_rewrites_of(class_oid);
    interp
        .pg_inherits
        .retain(|i| i.inhrelid != class_oid && i.inhparent != class_oid);

    // Tear down indexes whose `indrelid` is this relation, and the matching
    // pg_class rows for each index. PG cascades indexes with the table they
    // sit on; the analyzer mirrors that without an extra DROP CASCADE.
    if matches!(
        class.relkind,
        RelKind::Table | RelKind::Partitioned | RelKind::MaterializedView
    ) {
        let index_oids = interp.remove_pg_indexes_of(class_oid);
        for idx_oid in index_oids {
            interp.remove_pg_class(idx_oid);
            let idx_obj = crate::oid::PgGenericOid::from_nonzero(idx_oid.into_nonzero());
            interp.remove_dependencies_of(PG_CLASS_RELID, idx_obj);
            interp.remove_dependencies_on(PG_CLASS_RELID, idx_obj);
        }
    }

    if let Some(reltype) = class.reltype {
        // Find and drop the array type whose typelem points at the composite.
        if let Some(arr_oid) = interp.array_type_of(reltype) {
            interp.remove_pg_type(arr_oid);
        }
        interp.remove_pg_type(reltype);
    }
}

/// `DROP INDEX [IF EXISTS] name [, …]`. Resolves the index by `pg_class.oid`
/// (relkind = 'i'), tears down both the `pg_class` row and the matching
/// `pg_index` row. Also removes the `pg_constraint` row that
/// `CREATE UNIQUE INDEX` may have synthesized for ON CONFLICT matching.
fn drop_index(
    interp: &mut PgCatalog,
    obj_node: &typedpg_pg_query::protobuf::Node,
    missing_ok: bool,
) -> Result<(), DdlError> {
    let names = match obj_node.node.as_ref() {
        Some(node::Node::List(list)) => &list.items,
        _ => return Ok(()),
    };
    let (schema, name) = extract_names(names, interp);
    let Some(nsoid) = interp.namespace_oid(&schema) else {
        if missing_ok {
            return Ok(());
        }
        return Err(DdlError::DependencyError(format!(
            "index \"{name}\" does not exist"
        )));
    };
    let Some(class_oid) = interp.class_by_qname.get(&(nsoid, name.clone())).copied() else {
        if missing_ok {
            return Ok(());
        }
        return Err(DdlError::DependencyError(format!(
            "index \"{name}\" does not exist"
        )));
    };

    // Reject if the resolved relation isn't an index — PG: `"X" is not an index`.
    if !matches!(
        interp.pg_class.get(&class_oid).map(|c| c.relkind),
        Some(RelKind::Index | RelKind::PartitionedIndex)
    ) {
        return Err(DdlError::DependencyError(format!(
            "\"{name}\" is not an index"
        )));
    }

    // A partition's copy of a partitioned index goes only with its parent
    // (findDependentObjects); the parent takes its copies along.
    if let Some(parent) = crate::ddl::tables::partidx::parent_index_of(interp, class_oid) {
        let parent_name = interp
            .pg_class
            .get(&parent)
            .map(|c| c.relname.clone())
            .unwrap_or_default();
        return Err(DdlError::DependencyError(format!(
            "cannot drop index {name} because index {parent_name} requires it (You can drop \
             index {parent_name} instead.)"
        )));
    }
    // An index backing a constraint goes only with the constraint.
    if let Some(table) = interp.pg_index.get(&class_oid).map(|i| i.indrelid)
        && interp.pg_constraint.values().any(|c| {
            c.conrelid == table
                && c.conname == name
                && matches!(
                    c.contype,
                    crate::pg_catalog::ConType::PrimaryKey
                        | crate::pg_catalog::ConType::Unique
                        | crate::pg_catalog::ConType::Exclusion
                )
        })
    {
        let table_name = interp
            .pg_class
            .get(&table)
            .map(|c| c.relname.clone())
            .unwrap_or_default();
        return Err(DdlError::DependencyError(format!(
            "cannot drop index {name} because constraint {name} on table {table_name} requires \
             it (You can drop constraint {name} on table {table_name} instead.)"
        )));
    }
    for child in crate::ddl::tables::partidx::child_indexes(interp, class_oid) {
        interp.remove_pg_index(child);
        interp.remove_pg_class(child);
    }
    interp.remove_pg_index(class_oid);
    // Drop the synthesized UNIQUE pg_constraint row that ON CONFLICT
    // matching consults — its `conname` mirrors the index name and
    // `conrelid` points at the indexed table.
    let synth_oids: Vec<_> = interp
        .pg_constraint
        .values()
        .filter(|c| matches!(c.contype, crate::pg_catalog::ConType::Unique) && c.conname == name)
        .map(|c| c.oid)
        .collect();
    for oid in synth_oids {
        interp.pg_constraint.remove(&oid);
    }
    interp.remove_pg_class(class_oid);
    let obj = crate::oid::PgGenericOid::from_nonzero(class_oid.into_nonzero());
    interp.remove_dependencies_of(PG_CLASS_RELID, obj);
    interp.remove_dependencies_on(PG_CLASS_RELID, obj);
    Ok(())
}

fn drop_type(
    interp: &mut PgCatalog,
    obj_node: &typedpg_pg_query::protobuf::Node,
    missing_ok: bool,
    cascade: bool,
) -> Result<(), DdlError> {
    let names: &[typedpg_pg_query::protobuf::Node] = match obj_node.node.as_ref() {
        Some(node::Node::TypeName(tn)) => &tn.names,
        Some(node::Node::List(list)) => &list.items,
        _ => return Ok(()),
    };

    let (schema, name) = extract_names(names, interp);
    // typenameType's wording: the name as written.
    let written = names
        .iter()
        .filter_map(node_string)
        .collect::<Vec<_>>()
        .join(".");
    let Some(nsoid) = interp.namespace_oid(&schema) else {
        if missing_ok {
            return Ok(());
        }
        return Err(DdlError::TypeNotFound(format!(
            "schema \"{schema}\" does not exist"
        )));
    };
    let Some(type_oid) = interp.type_by_qname.get(&(nsoid, name.clone())).copied() else {
        if missing_ok {
            return Ok(());
        }
        return Err(DdlError::TypeNotFound(format!(
            "type \"{written}\" does not exist"
        )));
    };
    let array_oid = interp.array_type_of(type_oid);
    // A range type's multirange type and the constructor functions of both
    // are internal to it (DEPENDENCY_INTERNAL): they go along, and only
    // what depends on them in turn needs CASCADE.
    let multirange = interp.pg_range.get(&type_oid).and_then(|r| r.rngmultitypid);
    let own_types: Vec<crate::oid::PgTypeOid> = [Some(type_oid), array_oid]
        .into_iter()
        .chain(
            multirange
                .into_iter()
                .flat_map(|mr| [Some(mr), interp.array_type_of(mr)]),
        )
        .flatten()
        .collect();
    let constructors: Vec<crate::oid::PgProcOid> = interp
        .pg_proc
        .values()
        .filter(|p| {
            (p.prorettype == type_oid || Some(p.prorettype) == multirange)
                && multirange.is_some()
                && interp
                    .pg_type
                    .get(&p.prorettype)
                    .is_some_and(|t| t.typname == p.proname && t.typnamespace == p.pronamespace)
        })
        .map(|p| p.oid)
        .collect();

    // Find tables/composites with columns of this type (or its array form).
    let dependent_relations: Vec<PgClassOid> = interp
        .pg_attribute
        .iter()
        .filter_map(|(&relid, attrs)| {
            attrs
                .iter()
                .any(|a| own_types.contains(&a.atttypid))
                .then_some(relid)
        })
        .collect();

    // Typed tables (`reloftype`) depend on their type.
    let typed_tables = crate::ddl::tables::typed::typed_tables_of(interp, type_oid);
    if let Some(&table) = typed_tables.first()
        && !cascade
    {
        return Err(DdlError::DependencyError(format!(
            "cannot drop type {name} because other objects depend on it (table {} depends \
             on type {name})",
            interp
                .pg_class
                .get(&table)
                .map(|c| c.relname.as_str())
                .unwrap_or("?")
        )));
    }

    if !dependent_relations.is_empty() && !cascade {
        let dep_names: Vec<String> = dependent_relations
            .iter()
            .filter_map(|&v| {
                let c = interp.pg_class.get(&v)?;
                let nsname = interp.namespace_name(c.relnamespace).unwrap_or("?");
                Some(QualifiedName::new(nsname, &c.relname).to_string())
            })
            .collect();
        return Err(DdlError::DependencyError(format!(
            "cannot drop type {name} because other objects depend on it \
             (table(s) {} depend on this type)",
            dep_names.join(", "),
        )));
    }

    // Views can also reach a type through `AstBinding::Type` (CAST targets,
    // typed literals). Those entries live in pg_depend with refclassid =
    // PG_TYPE_RELID, so a separate lookup catches them — block without
    // CASCADE and drop them transitively otherwise.
    let dependent_views = views::find_views_depending_on_type(interp, type_oid);
    if !dependent_views.is_empty() && !cascade {
        let view_names = format_view_list(interp, &dependent_views);
        return Err(DdlError::DependencyError(format!(
            "cannot drop type {name} because other objects depend on it \
             (view(s) {view_names} depend on this type)",
        )));
    }

    // Functions taking or returning the type depend on it too.
    let dependent_functions: Vec<crate::oid::PgProcOid> = functions_using_types(interp, &own_types)
        .into_iter()
        .filter(|f| !constructors.contains(f))
        .collect();
    if !dependent_functions.is_empty() && !cascade {
        return Err(DdlError::DependencyError(format!(
            "cannot drop type {name} because other objects depend on it \
             (function {} depends on type {name})",
            describe_function(interp, dependent_functions[0]),
        )));
    }

    if cascade {
        for &table in &typed_tables {
            drop_relation_by_oid(interp, table);
        }
        for relid in &dependent_relations {
            if let Some(attrs) = interp.pg_attribute.get_mut(relid) {
                attrs.retain(|a| !own_types.contains(&a.atttypid));
            }
        }
        if !dependent_views.is_empty() {
            views::drop_views(interp, &dependent_views);
        }
        drop_functions_cascade(interp, &dependent_functions);
    }

    for proc in constructors {
        interp.remove_pg_proc(proc);
    }
    for mr_type in own_types
        .iter()
        .copied()
        .filter(|t| *t != type_oid && Some(*t) != array_oid)
    {
        interp.remove_pg_type(mr_type);
    }
    if let Some(arr_oid) = array_oid {
        interp.remove_pg_type(arr_oid);
        let arr_obj = crate::oid::PgGenericOid::from_nonzero(arr_oid.into_nonzero());
        interp.remove_dependencies_of(PG_TYPE_RELID, arr_obj);
        interp.remove_dependencies_on(PG_TYPE_RELID, arr_obj);
    }
    interp.remove_pg_type(type_oid);
    let type_obj = crate::oid::PgGenericOid::from_nonzero(type_oid.into_nonzero());
    interp.remove_dependencies_of(PG_TYPE_RELID, type_obj);
    interp.remove_dependencies_on(PG_TYPE_RELID, type_obj);
    Ok(())
}

fn drop_extension(
    interp: &mut PgCatalog,
    obj_node: &typedpg_pg_query::protobuf::Node,
    missing_ok: bool,
    _cascade: bool,
) -> Result<(), DdlError> {
    let name = match obj_node.node.as_ref() {
        Some(node::Node::String(s)) => s.sval.clone(),
        _ => return Ok(()),
    };

    let Some(ext_oid) = interp.extension_by_name.get(&name).copied() else {
        if missing_ok {
            return Ok(());
        }
        return Err(DdlError::ExtensionError(format!(
            "extension \"{name}\" does not exist"
        )));
    };

    // Collect every (classid, objid) the extension created via pg_depend.
    let owned: Vec<(PgClassOid, crate::oid::PgGenericOid)> =
        interp.extension_objects(ext_oid).collect();

    for (classid, objid) in owned {
        match classid {
            c if c == PG_TYPE_RELID => {
                if let Some(o) = PgTypeOid::new(objid.get()) {
                    interp.remove_pg_type(o);
                }
                interp.remove_dependencies_of(PG_TYPE_RELID, objid);
                interp.remove_dependencies_on(PG_TYPE_RELID, objid);
            }
            c if c == PG_PROC_RELID => {
                if let Some(o) = PgProcOid::new(objid.get()) {
                    interp.remove_pg_proc(o);
                }
                interp.remove_dependencies_of(PG_PROC_RELID, objid);
                interp.remove_dependencies_on(PG_PROC_RELID, objid);
            }
            c if c == PG_CAST_RELID => {
                if let Some(o) = PgCastOid::new(objid.get()) {
                    interp.remove_pg_cast(o);
                }
                interp.remove_dependencies_of(PG_CAST_RELID, objid);
            }
            c if c == PG_OPERATOR_RELID => {
                if let Some(o) = PgOperatorOid::new(objid.get()) {
                    interp.remove_pg_operator(o);
                }
                interp.remove_dependencies_of(PG_OPERATOR_RELID, objid);
            }
            c if c == PG_CLASS_RELID => {
                if let Some(o) = PgClassOid::new(objid.get()) {
                    drop_relation_by_oid(interp, o);
                }
            }
            _ => {}
        }
    }

    interp.remove_pg_extension(ext_oid);
    let ext_obj = crate::oid::PgGenericOid::from_nonzero(ext_oid.into_nonzero());
    interp.remove_dependencies_of(PG_EXTENSION_RELID, ext_obj);
    interp.remove_dependencies_on(PG_EXTENSION_RELID, ext_obj);
    Ok(())
}

/// `DROP FUNCTION` / `DROP PROCEDURE`.
fn drop_function(
    interp: &mut PgCatalog,
    obj_node: &typedpg_pg_query::protobuf::Node,
    missing_ok: bool,
    cascade: bool,
    expected_kind: ObjectType,
) -> Result<(), DdlError> {
    let Some(node::Node::ObjectWithArgs(owa)) = obj_node.node.as_ref() else {
        return Ok(());
    };

    let parts: Vec<String> = owa
        .objname
        .iter()
        .filter_map(node_string)
        .map(|s| s.to_owned())
        .collect();

    let name = parts.last().cloned().unwrap_or_default();
    // LookupFuncWithArgs (parse_func.c): the argument types must resolve,
    // then the name + types pick exactly one routine, whose kind must match
    // the command.
    let Some(target) =
        super::functions::lookup_func_with_args(interp, expected_kind, owa, missing_ok)?
    else {
        return Ok(());
    };
    let arg_oids: Vec<PgTypeOid> = interp
        .pg_proc
        .get(&target)
        .map(|p| p.proargtypes.clone())
        .unwrap_or_default();
    let want_procedure = expected_kind == ObjectType::ObjectProcedure;
    let kind_word = if want_procedure {
        "procedure"
    } else {
        "function"
    };
    // getObjectDescription's `name(type,type)`.
    let signature = format!(
        "{name}({})",
        format_arg_oids(&arg_oids, interp).replace(", ", ",")
    );
    let target = Some(target);

    if let Some(oid) = target {
        // A trigger depends on its function.
        let dependent_triggers = super::triggers::triggers_using_function(interp, oid);
        if let Some((relid, trigger)) = dependent_triggers.first()
            && !cascade
        {
            let table = interp
                .pg_class
                .get(relid)
                .map(|c| c.relname.clone())
                .unwrap_or_default();
            return Err(DdlError::DependencyError(format!(
                "cannot drop {kind_word} {signature} because other objects depend on it \
                 (trigger {trigger} on table {table} depends on {kind_word} {signature})"
            )));
        }
        for (relid, trigger) in dependent_triggers {
            if let Some(ts) = interp.triggers.get_mut(&relid) {
                ts.retain(|t| t.name != trigger);
            }
        }
        // So does an event trigger.
        let event_triggers = super::event_triggers::event_triggers_using(interp, oid);
        if let Some(trigger) = event_triggers.first()
            && !cascade
        {
            return Err(DdlError::DependencyError(format!(
                "cannot drop {kind_word} {signature} because other objects depend on it \
                 (event trigger {trigger} depends on {kind_word} {signature})"
            )));
        }
        interp.event_triggers.retain(|(_, f)| *f != oid);
        // And a policy or rule calling it.
        let dependent_objects = super::coldeps::dependents_calling(interp, oid);
        if let Some(dependent) = dependent_objects.first()
            && !cascade
        {
            return Err(DdlError::DependencyError(format!(
                "cannot drop {kind_word} {signature} because other objects depend on it \
                 ({} depends on {kind_word} {signature})",
                super::coldeps::describe(interp, dependent)
            )));
        }
        for dependent in &dependent_objects {
            super::coldeps::drop_dependent(interp, dependent);
        }
        let dependent_views = views::find_views_depending_on_function(interp, oid);
        if !dependent_views.is_empty() && !cascade {
            let view_names = format_view_list(interp, &dependent_views);
            let kind = if want_procedure {
                "procedure"
            } else {
                "function"
            };
            return Err(DdlError::DependencyError(format!(
                "cannot drop {kind} {name}({}) because other objects depend on it \
                 (view(s) {view_names} depend on this {kind})",
                format_arg_oids(&arg_oids, interp),
            )));
        }
        if !dependent_views.is_empty() {
            views::drop_views(interp, &dependent_views);
        }
        interp.remove_pg_proc(oid);
        let obj = crate::oid::PgGenericOid::from_nonzero(oid.into_nonzero());
        interp.remove_dependencies_of(PG_PROC_RELID, obj);
        interp.remove_dependencies_on(PG_PROC_RELID, obj);
    }
    Ok(())
}

/// Comma-join schema-qualified names of the given view OIDs, for error
/// messages.
fn format_view_list(snapshot: &PgCatalog, view_oids: &[PgClassOid]) -> String {
    view_oids
        .iter()
        .filter_map(|&v| {
            let c = snapshot.pg_class.get(&v)?;
            let nsname = snapshot.namespace_name(c.relnamespace).unwrap_or("?");
            Some(QualifiedName::new(nsname, &c.relname).to_string())
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// DROP AGGREGATE.
fn drop_aggregate(
    interp: &mut PgCatalog,
    obj_node: &typedpg_pg_query::protobuf::Node,
    missing_ok: bool,
    cascade: bool,
) -> Result<(), DdlError> {
    let Some(node::Node::ObjectWithArgs(owa)) = obj_node.node.as_ref() else {
        return Ok(());
    };

    let name = owa
        .objname
        .last()
        .and_then(node_string)
        .unwrap_or_default()
        .to_owned();
    let target = super::functions::lookup_func_with_args(
        interp,
        ObjectType::ObjectAggregate,
        owa,
        missing_ok,
    )?;
    let arg_oids: Vec<PgTypeOid> = target
        .and_then(|oid| interp.pg_proc.get(&oid))
        .map(|p| p.proargtypes.clone())
        .unwrap_or_default();

    if let Some(oid) = target {
        let dependent_views = views::find_views_depending_on_function(interp, oid);
        if !dependent_views.is_empty() && !cascade {
            // PG renders aggregate-with-deps errors using "function"
            // wording (aggregates live in `pg_proc` like ordinary functions
            // for dependency-tracking purposes).
            let view_names = format_view_list(interp, &dependent_views);
            return Err(DdlError::DependencyError(format!(
                "cannot drop function {name}({}) because other objects depend on it \
                 (view(s) {view_names} depend on this aggregate)",
                format_arg_oids(&arg_oids, interp),
            )));
        }
        if !dependent_views.is_empty() {
            views::drop_views(interp, &dependent_views);
        }
        interp.remove_pg_proc(oid);
        let obj = crate::oid::PgGenericOid::from_nonzero(oid.into_nonzero());
        interp.remove_dependencies_of(PG_PROC_RELID, obj);
        interp.remove_dependencies_on(PG_PROC_RELID, obj);
    }
    Ok(())
}

/// DROP OPERATOR name(lefttype, righttype).
fn drop_operator(
    interp: &mut PgCatalog,
    obj_node: &typedpg_pg_query::protobuf::Node,
    missing_ok: bool,
) -> Result<(), DdlError> {
    let Some(node::Node::ObjectWithArgs(owa)) = obj_node.node.as_ref() else {
        return Ok(());
    };

    let parts: Vec<String> = owa
        .objname
        .iter()
        .filter_map(node_string)
        .map(|s| s.to_owned())
        .collect();
    let (schema_opt, op_name) = match parts.as_slice() {
        [name] => (None, name.clone()),
        [schema, name] => (Some(schema.clone()), name.clone()),
        _ => return Ok(()),
    };

    let (left_oid, right_oid) = parse_operator_arg_types(&owa.objargs, interp);
    let Some(right_oid) = right_oid else {
        return Ok(());
    };

    let matches = |o: &PgOperator| o.oprleft == left_oid && o.oprright == right_oid;

    let target = find_operator(interp, schema_opt.as_deref(), &op_name, &matches);

    if target.is_none() && !missing_ok {
        let left_name = left_oid
            .map(|oid| format_type_for_message(interp, oid))
            .unwrap_or_else(|| "NONE".to_string());
        let right_name = format_type_for_message(interp, right_oid);
        return Err(DdlError::DependencyError(format!(
            "operator does not exist: {left_name} {op_name} {right_name}"
        )));
    }

    if let Some(oid) = target {
        interp.remove_pg_operator(oid);
        let obj = crate::oid::PgGenericOid::from_nonzero(oid.into_nonzero());
        interp.remove_dependencies_of(PG_OPERATOR_RELID, obj);
        interp.remove_dependencies_on(PG_OPERATOR_RELID, obj);
    }
    Ok(())
}

/// The operator a `DROP OPERATOR name(left, right)` names, if it exists.
pub(crate) fn operator_target(
    interp: &PgCatalog,
    obj_node: &typedpg_pg_query::protobuf::Node,
) -> Option<PgOperatorOid> {
    let Some(node::Node::ObjectWithArgs(owa)) = obj_node.node.as_ref() else {
        return None;
    };
    let parts: Vec<&str> = owa.objname.iter().filter_map(node_string).collect();
    let (schema, name) = match parts.as_slice() {
        [name] => (None, *name),
        [schema, name] => (Some(*schema), *name),
        _ => return None,
    };
    let (left, right) = parse_operator_arg_types(&owa.objargs, interp);
    let right = right?;
    find_operator(interp, schema, name, &|o: &PgOperator| {
        o.oprleft == left && o.oprright == right
    })
}

pub(crate) fn find_operator(
    snapshot: &PgCatalog,
    schema: Option<&str>,
    name: &str,
    matches: &dyn Fn(&PgOperator) -> bool,
) -> Option<PgOperatorOid> {
    let candidate_schemas: Vec<PgNamespaceOid> = if let Some(s) = schema {
        snapshot.namespace_oid(s).into_iter().collect()
    } else {
        let mut v = Vec::new();
        if let Some(pg_oid) = snapshot.namespace_oid("pg_catalog")
            && !snapshot.search_path.contains(&pg_oid)
        {
            v.push(pg_oid);
        }
        v.extend(snapshot.search_path.iter().copied());
        v
    };
    for nsoid in candidate_schemas {
        if let Some(oids) = snapshot.operator_by_qname.get(&(nsoid, name.to_owned())) {
            for &oid in oids {
                if let Some(o) = snapshot.pg_operator.get(&oid)
                    && matches(o)
                {
                    return Some(oid);
                }
            }
        }
    }
    None
}

/// Parse `(left, right)` type OIDs from the two-element `objargs` of a
/// `DROP OPERATOR`. A `TypeName` with an empty `names` list stands for
/// `NONE`, indicating a prefix operator (no left operand).
fn parse_operator_arg_types(
    objargs: &[typedpg_pg_query::protobuf::Node],
    snapshot: &PgCatalog,
) -> (Option<PgTypeOid>, Option<PgTypeOid>) {
    let resolve = |n: &typedpg_pg_query::protobuf::Node| -> Option<PgTypeOid> {
        if let Some(node::Node::TypeName(tn)) = n.node.as_ref() {
            if tn.names.is_empty() {
                return None;
            }
            return resolve_type_name(tn, snapshot);
        }
        None
    };

    match objargs {
        [l, r] => (resolve(l), resolve(r)),
        [r] => (None, resolve(r)),
        _ => (None, None),
    }
}

/// DROP CAST (source AS target).
fn drop_cast(
    interp: &mut PgCatalog,
    obj_node: &typedpg_pg_query::protobuf::Node,
    missing_ok: bool,
) -> Result<(), DdlError> {
    let items = match obj_node.node.as_ref() {
        Some(node::Node::List(list)) => &list.items,
        _ => return Ok(()),
    };

    let (src_node, tgt_node) = match items.as_slice() {
        [s, t] => (s, t),
        _ => return Ok(()),
    };

    let src_oid = match src_node.node.as_ref() {
        Some(node::Node::TypeName(tn)) => resolve_type_name(tn, interp),
        _ => None,
    };
    let tgt_oid = match tgt_node.node.as_ref() {
        Some(node::Node::TypeName(tn)) => resolve_type_name(tn, interp),
        _ => None,
    };

    let (Some(src), Some(tgt)) = (src_oid, tgt_oid) else {
        if missing_ok {
            return Ok(());
        }
        return Err(DdlError::TypeNotFound("cast source or target type".into()));
    };

    let cast_oid = interp.cast_by_pair.get(&(src, tgt)).copied();
    if cast_oid.is_none() && !missing_ok {
        return Err(DdlError::DependencyError(format!(
            "cast from type {} to type {} does not exist",
            format_type_for_message(interp, src),
            format_type_for_message(interp, tgt),
        )));
    }
    if let Some(oid) = cast_oid {
        interp.remove_pg_cast(oid);
        let obj = crate::oid::PgGenericOid::from_nonzero(oid.into_nonzero());
        interp.remove_dependencies_of(PG_CAST_RELID, obj);
    }
    Ok(())
}

/// Format a list of argument OIDs as PG-aligned user-facing type names for
/// error messages — `int2 → smallint`, `int4 → integer`, etc. — so error
/// strings line up with PG's wire-protocol output.
fn format_arg_oids(oids: &[PgTypeOid], snapshot: &PgCatalog) -> String {
    oids.iter()
        .map(|oid| format_type_for_message(snapshot, *oid))
        .collect::<Vec<_>>()
        .join(", ")
}

/// DROP SCHEMA name [CASCADE | RESTRICT].
fn drop_schema(
    interp: &mut PgCatalog,
    obj_node: &typedpg_pg_query::protobuf::Node,
    missing_ok: bool,
    cascade: bool,
) -> Result<(), DdlError> {
    let name = match obj_node.node.as_ref() {
        Some(node::Node::String(s)) => s.sval.clone(),
        _ => return Ok(()),
    };

    let Some(nsoid) = interp.namespace_oid(&name) else {
        if missing_ok {
            return Ok(());
        }
        return Err(DdlError::DependencyError(format!(
            "schema \"{name}\" does not exist"
        )));
    };

    let has_objects = interp.pg_class.values().any(|c| c.relnamespace == nsoid)
        || interp.pg_type.values().any(|t| t.typnamespace == nsoid)
        || interp.pg_proc.values().any(|p| p.pronamespace == nsoid);

    if has_objects && !cascade {
        return Err(DdlError::DependencyError(format!(
            "cannot drop schema {name} because other objects depend on it"
        )));
    }

    // CASCADE: gather everything in this schema.
    let class_oids: Vec<PgClassOid> = interp
        .pg_class
        .values()
        .filter(|c| c.relnamespace == nsoid)
        .map(|c| c.oid)
        .collect();
    views::drop_views(interp, &class_oids);
    for class_oid in class_oids {
        if interp.pg_class.contains_key(&class_oid) {
            drop_relation_by_oid(interp, class_oid);
        }
    }

    let type_oids: Vec<PgTypeOid> = interp
        .pg_type
        .values()
        .filter(|t| t.typnamespace == nsoid)
        .map(|t| t.oid)
        .collect();
    for type_oid in type_oids {
        interp.remove_pg_type(type_oid);
        let obj = crate::oid::PgGenericOid::from_nonzero(type_oid.into_nonzero());
        interp.remove_dependencies_of(PG_TYPE_RELID, obj);
        interp.remove_dependencies_on(PG_TYPE_RELID, obj);
    }

    let proc_oids: Vec<PgProcOid> = interp
        .pg_proc
        .values()
        .filter(|p| p.pronamespace == nsoid)
        .map(|p| p.oid)
        .collect();
    for proc_oid in proc_oids {
        interp.remove_pg_proc(proc_oid);
        let obj = crate::oid::PgGenericOid::from_nonzero(proc_oid.into_nonzero());
        interp.remove_dependencies_of(PG_PROC_RELID, obj);
        interp.remove_dependencies_on(PG_PROC_RELID, obj);
    }

    let op_oids: Vec<PgOperatorOid> = interp
        .pg_operator
        .values()
        .filter(|o| o.oprnamespace == nsoid)
        .map(|o| o.oid)
        .collect();
    for op_oid in op_oids {
        interp.remove_pg_operator(op_oid);
        let obj = crate::oid::PgGenericOid::from_nonzero(op_oid.into_nonzero());
        interp.remove_dependencies_of(PG_OPERATOR_RELID, obj);
    }

    interp.search_path.retain(|&s| s != nsoid);
    interp.remove_pg_namespace(nsoid);
    let ns_obj = crate::oid::PgGenericOid::from_nonzero(nsoid.into_nonzero());
    interp.remove_dependencies_of(PG_NAMESPACE_RELID, ns_obj);
    interp.remove_dependencies_on(PG_NAMESPACE_RELID, ns_obj);
    Ok(())
}
