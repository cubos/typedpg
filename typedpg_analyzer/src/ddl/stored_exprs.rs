//! Keeping the expressions the analyzer stores as written (CHECK
//! constraints, column defaults and generation expressions, partition
//! keys) in step with renames. PG stores them cooked — a column as its
//! attnum, an enum label as its OID — so `ALTER TABLE ... RENAME COLUMN`
//! and `ALTER TYPE ... RENAME VALUE` leave their meaning unchanged; the
//! analyzer keeps the parse tree, whose names have to follow. Read again
//! over the relation, a stored expression's column references are its
//! columns whatever relation name qualifies them ([`over_own_row`]).

use typedpg_pg_query::protobuf::{self, node};

use crate::ddl::tables::check_inherit::StoredExpr;
use crate::oid::{PgClassOid, PgTypeOid};
use crate::pg_catalog::{ConType, PgCatalog, TypType};

/// Run `edit` over every node of `expr` (mutably).
fn edit_nodes(expr: &mut protobuf::Node, edit: &mut dyn FnMut(typedpg_pg_query::NodeMut)) {
    let mut tree = protobuf::ParseResult {
        version: 0,
        stmts: vec![protobuf::RawStmt {
            stmt: Some(Box::new(std::mem::take(expr))),
            stmt_location: 0,
            stmt_len: 0,
        }],
    };
    // SAFETY: the tree is neither moved nor dropped while the pointers are
    // used, and `edit` only overwrites `String` leaves and constants'
    // values — no subtree another pointer refers into is replaced.
    unsafe {
        for (n, _) in tree.nodes_mut() {
            edit(n);
        }
    }
    *expr = tree
        .stmts
        .pop()
        .and_then(|s| s.stmt)
        .map(|b| *b)
        .unwrap_or_default();
}

/// Which field of a column reference in an expression over the row of
/// one relation names the column (transformColumnRef): `c`, `t.c` (two
/// names are always a relation and its column), `s.t.c` (or `t.c.f`, a
/// field of composite column `c`), `d.s.t.c`. `is_column` says whether a
/// name is one of the relation's columns; `relname` is the relation's own
/// name — which a copy inherited from a parent, or taken by LIKE, or
/// written before the relation was renamed, may not use.
fn column_field(fields: &[String], relname: &str, is_column: &dyn Fn(&str) -> bool) -> usize {
    match fields {
        [_] => 0,
        [_, _] => 1,
        [_, t, c] => {
            // `schema.table.column` when the middle name isn't one of the
            // columns (or is the relation) and the last one is.
            if (t == relname || !is_column(t)) && is_column(c) {
                2
            } else {
                1
            }
        }
        _ => fields.len() - 1,
    }
}

/// `expr` with the column references to column `old` of the relation it
/// is over (named `relname`, with columns `columns`) renamed `new`.
fn rename_column_refs(
    expr: &mut protobuf::Node,
    relname: &str,
    columns: &[String],
    old: &str,
    new: &str,
) {
    let is_column = |n: &str| columns.iter().any(|c| c == n);
    edit_nodes(expr, &mut |n| {
        let typedpg_pg_query::NodeMut::ColumnRef(cr) = n else {
            return;
        };
        // SAFETY: see `edit_nodes`; only a `String` leaf is written.
        let fields = unsafe { &mut (*cr).fields };
        if fields
            .iter()
            .any(|f| !matches!(f.node, Some(node::Node::String(_))))
        {
            return;
        }
        let names: Vec<String> = crate::expr::extract_string_fields(fields);
        let idx = column_field(&names, relname, &is_column);
        if names.get(idx).map(String::as_str) == Some(old)
            && let Some(node::Node::String(s)) = fields[idx].node.as_mut()
        {
            s.sval = new.to_owned();
        }
    });
}

/// `expr`, an expression stored for relation `relid` (a CHECK, a
/// generation or index expression), with its column references as PG
/// keeps them — the relation's columns, whatever the relation was called
/// where the expression was written: `t.a` in a CHECK copied to a child
/// from parent `t`, or written before the table was renamed, is the
/// column `a` (and `t.c.f` the field `f` of column `c`).
pub(crate) fn over_own_row(
    interp: &PgCatalog,
    relid: PgClassOid,
    expr: &protobuf::Node,
) -> protobuf::Node {
    let relname = interp
        .pg_class
        .get(&relid)
        .map(|c| c.relname.clone())
        .unwrap_or_default();
    let columns: Vec<String> = interp
        .attributes_of(relid)
        .iter()
        .map(|a| a.attname.clone())
        .collect();
    let is_column = |n: &str| columns.iter().any(|c| c == n);
    let mut tree = protobuf::ParseResult {
        version: 0,
        stmts: vec![protobuf::RawStmt {
            stmt: Some(Box::new(expr.clone())),
            stmt_location: 0,
            stmt_len: 0,
        }],
    };
    // SAFETY: the tree is neither moved nor dropped while the pointers are
    // used, and only column references' field lists are rewritten, after
    // every column reference was collected: a column reference holds no
    // other one, and no pointer into a field list is dereferenced.
    unsafe {
        let column_refs: Vec<_> = tree
            .nodes_mut()
            .into_iter()
            .filter_map(|(n, _)| match n {
                typedpg_pg_query::NodeMut::ColumnRef(cr) => Some(cr),
                _ => None,
            })
            .collect();
        for cr in column_refs {
            let fields = &mut (*cr).fields;
            if fields
                .iter()
                .any(|f| !matches!(f.node, Some(node::Node::String(_))))
            {
                continue;
            }
            let names = crate::expr::extract_string_fields(fields);
            let idx = column_field(&names, &relname, &is_column);
            if idx > 0 && is_column(&names[idx]) {
                fields.drain(..idx);
            }
        }
    }
    tree.stmts
        .pop()
        .and_then(|s| s.stmt)
        .map(|b| *b)
        .unwrap_or_default()
}

/// ALTER TABLE ... RENAME COLUMN `old` TO `new` of relation `relid`: the
/// relation's CHECK constraints, generation expressions and partition key
/// read the column by its new name. Called before `pg_attribute` is
/// updated.
pub(crate) fn rename_column(interp: &mut PgCatalog, relid: PgClassOid, old: &str, new: &str) {
    let relname = interp
        .pg_class
        .get(&relid)
        .map(|c| c.relname.clone())
        .unwrap_or_default();
    let columns: Vec<String> = interp
        .attributes_of(relid)
        .iter()
        .map(|a| a.attname.clone())
        .collect();
    let checks: Vec<_> = interp
        .pg_constraint
        .values()
        .filter(|c| c.conrelid == relid && c.contype == ConType::Check)
        .map(|c| c.oid)
        .collect();
    for oid in checks {
        if let Some(mut def) = interp.check_defs.get(&oid).cloned()
            && let StoredExpr::Written(expr) = &mut def.expr
        {
            rename_column_refs(expr, &relname, &columns, old, new);
            interp.check_defs.insert(oid, def);
        }
    }
    let exprs: Vec<i16> = interp
        .attr_default_exprs
        .keys()
        .filter(|(rel, _)| *rel == relid)
        .map(|(_, attnum)| *attnum)
        .collect();
    for attnum in exprs {
        if let Some(StoredExpr::Written(expr)) = interp.attr_default_exprs.get_mut(&(relid, attnum))
        {
            rename_column_refs(expr, &relname, &columns, old, new);
        }
    }
    if let Some(mut spec) = interp.partition_specs.get(&relid).cloned() {
        spec.rename_key_column(old, new);
        interp.partition_specs.insert(relid, spec);
    }
    // The index expressions and predicates over the relation.
    let indexes: Vec<PgClassOid> = interp
        .pg_index
        .values()
        .filter(|i| i.indrelid == relid)
        .map(|i| i.indexrelid)
        .collect();
    for index in indexes {
        let Some(mut row) = interp.pg_index.get(&index).cloned() else {
            continue;
        };
        for ast in row.indexprs.iter_mut().chain(row.indpred.as_mut()) {
            use prost::Message;
            if let Ok(mut expr) = protobuf::Node::decode(ast.ast.as_slice()) {
                rename_column_refs(&mut expr, &relname, &columns, old, new);
                ast.ast = expr.encode_to_vec();
            }
        }
        interp.pg_index.insert(index, row);
    }
}

/// ALTER TYPE ... RENAME VALUE `old` TO `new` of enum `enum_oid`: the
/// label constants of that type in CHECK constraints (by the types their
/// literals were resolved to, see [`crate::nonnull::TrustedNodes`]) and in
/// column defaults read the new label. A CHECK whose label can't be
/// rewritten — inside an array literal — is no longer trusted for its
/// comparisons.
pub(crate) fn rename_enum_label(interp: &mut PgCatalog, enum_oid: PgTypeOid, old: &str, new: &str) {
    let is_enum = |interp: &PgCatalog, t: PgTypeOid| interp.unwrap_domain(t) == enum_oid;
    let is_enum_array = |interp: &PgCatalog, t: PgTypeOid| {
        interp
            .pg_type
            .get(&interp.unwrap_domain(t))
            .and_then(|ty| ty.typelem)
            .is_some_and(|e| interp.unwrap_domain(e) == enum_oid)
    };
    // A literal of a polymorphic type: what it was may be the enum.
    let is_pseudo = |interp: &PgCatalog, t: PgTypeOid| {
        interp
            .pg_type
            .get(&t)
            .is_some_and(|ty| ty.typtype == TypType::Pseudo)
    };
    let oids: Vec<_> = interp.check_defs.keys().copied().collect();
    for oid in oids {
        let Some(mut def) = interp.check_defs.get(&oid).cloned() else {
            continue;
        };
        let Some(trusted) = def.trusted.clone() else {
            continue;
        };
        let StoredExpr::Written(expr) = &mut def.expr else {
            continue;
        };
        let mut changed = false;
        let mut stale = false;
        edit_nodes(expr, &mut |n| {
            let typedpg_pg_query::NodeMut::AConst(c) = n else {
                return;
            };
            // SAFETY: see `edit_nodes`; only the constant's string is
            // written.
            let c = unsafe { &mut *c };
            let Some(protobuf::a_const::Val::Sval(s)) = c.val.as_mut() else {
                return;
            };
            let Some(&t) = trusted.literal_types.get(&c.location) else {
                return;
            };
            if is_enum(interp, t) {
                if s.sval == old {
                    s.sval = new.to_owned();
                    changed = true;
                }
            } else if (is_enum_array(interp, t) || is_pseudo(interp, t)) && s.sval.contains(old) {
                stale = true;
            }
        });
        if stale {
            def.trusted = Some(crate::nonnull::TrustedNodes::default());
        }
        if changed || stale {
            interp.check_defs.insert(oid, def);
        }
    }
    // A column default: the label, bare or cast to the column's type.
    let defaults: Vec<(PgClassOid, i16)> = interp
        .attr_default_exprs
        .keys()
        .filter(|(rel, attnum)| {
            interp
                .attributes_of(*rel)
                .iter()
                .any(|a| a.attnum == *attnum && is_enum(interp, a.atttypid))
        })
        .copied()
        .collect();
    for key in defaults {
        let Some(StoredExpr::Written(mut expr)) = interp.attr_default_exprs.get(&key).cloned()
        else {
            continue;
        };
        let constant = match expr.node.as_mut() {
            Some(node::Node::AConst(c)) => Some(c),
            Some(node::Node::TypeCast(tc))
                if tc
                    .type_name
                    .as_ref()
                    .and_then(|t| crate::ddl::util::resolve_type_name(t, interp))
                    .is_some_and(|t| is_enum(interp, t)) =>
            {
                match tc.arg.as_deref_mut().and_then(|a| a.node.as_mut()) {
                    Some(node::Node::AConst(c)) => Some(c),
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some(c) = constant
            && let Some(protobuf::a_const::Val::Sval(s)) = c.val.as_mut()
            && s.sval == old
        {
            s.sval = new.to_owned();
            interp
                .attr_default_exprs
                .insert(key, StoredExpr::Written(expr));
        }
    }
}
