//! What the rows a data-modifying statement writes hold, for RETURNING.
//!
//! Without a BEFORE ROW trigger, a rule or an INSTEAD OF trigger, the row
//! an INSERT stores is exactly its target list — the given values, the
//! defaults `rewriteTargetListIU` fills in for the rest — and the new row
//! of an UPDATE is the old one with the SET values (transformInsertStmt /
//! transformUpdateTargetList). Generated columns are recomputed from the
//! row (`ExecComputeStoredGenerated`, after the BEFORE triggers), and every
//! row ExecConstraints lets through satisfies the CHECK constraints, with
//! triggers or without. An automatically updatable view writes the rows of
//! its base relation (`rewriteTargetView`), its plain columns being the
//! base columns.

use std::collections::{HashMap, HashSet};

use super::*;
use crate::nonnull::{Facts, Literal};
use crate::oid::PgClassOid;

/// What is known of one version of a row, by the base relation's column
/// names.
#[derive(Debug, Clone, Default)]
pub(crate) struct RowKnowledge {
    pub not_null: HashSet<String>,
    pub nulls: HashSet<String>,
    pub equals: HashMap<String, Literal>,
}

impl RowKnowledge {
    /// What `facts` say of FROM entry `alias`'s columns `keep` accepts,
    /// renamed by `rename` (`None`: dropped).
    pub(crate) fn from_facts(
        facts: &Facts,
        alias: &str,
        rename: impl Fn(&str) -> Option<String>,
    ) -> RowKnowledge {
        let mine = |a: &str, c: &str| (a == alias).then(|| rename(c)).flatten();
        RowKnowledge {
            not_null: facts
                .columns
                .iter()
                .filter_map(|(a, c)| mine(a, c))
                .collect(),
            nulls: facts.nulls.iter().filter_map(|(a, c)| mine(a, c)).collect(),
            equals: facts
                .equals
                .iter()
                .filter_map(|((a, c), v)| mine(a, c).map(|c| (c, v.clone())))
                .collect(),
        }
    }

    /// Record what column `column` was written with.
    pub(crate) fn write(&mut self, column: &str, value: &ValueInfo) {
        self.not_null.remove(column);
        self.nulls.remove(column);
        self.equals.remove(column);
        if value.not_null {
            self.not_null.insert(column.to_owned());
        }
        if value.null {
            self.nulls.insert(column.to_owned());
        }
        if let Some(l) = &value.literal {
            self.equals.insert(column.to_owned(), l.clone());
        }
    }
}

/// What one value written to a column is known to be.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ValueInfo {
    pub not_null: bool,
    /// A bare `NULL`.
    pub null: bool,
    /// A constant (see [`crate::nonnull::literal_for`]).
    pub literal: Option<Literal>,
}

impl ValueInfo {
    /// What both of two values (rows of a VALUES list) are.
    pub(crate) fn either(&self, other: &ValueInfo) -> ValueInfo {
        ValueInfo {
            not_null: self.not_null && other.not_null,
            null: self.null && other.null,
            literal: (self.literal == other.literal)
                .then(|| self.literal.clone())
                .flatten(),
        }
    }

    /// Value expression `val`, typed `t`, stored in column `attr`: what
    /// the assignment coercion to the column's type and typmod makes of
    /// it. A cast function may map it to NULL, and a constant is the
    /// stored value only when nothing converts it — no user-defined cast
    /// function, and a typmod coercion that keeps it as written
    /// ([`crate::typmod::keeps_literal`]: `varchar(2)` drops `'ab '`'s
    /// trailing space, `numeric(2,-1)` rounds 15 to 20).
    pub(crate) fn assigned(
        val: &protobuf::Node,
        t: &expr::ExprType,
        attr: &crate::pg_catalog::PgAttribute,
        snapshot: &PgCatalog,
    ) -> ValueInfo {
        let nullable = expr::assignment_nullable(t, attr.atttypid, snapshot);
        let typmod = snapshot.effective_typmod(attr.atttypid, attr.atttypmod);
        let literal = crate::nonnull::literal_for(val, attr.atttypid, snapshot).filter(|l| {
            !nullable
                && !user_cast(t.type_oid, attr.atttypid, snapshot)
                && crate::typmod::keeps_literal(
                    snapshot,
                    snapshot.unwrap_domain(attr.atttypid),
                    typmod,
                    l,
                )
        });
        ValueInfo {
            not_null: !nullable,
            null: is_sql_null_literal(val),
            literal,
        }
    }
}

/// Whether coercing a `source` value to `target` runs a cast function
/// that isn't built in.
fn user_cast(source: PgTypeOid, target: PgTypeOid, snapshot: &PgCatalog) -> bool {
    snapshot
        .cast_by_pair
        .get(&(
            snapshot.unwrap_domain(source),
            snapshot.unwrap_domain(target),
        ))
        .and_then(|oid| snapshot.pg_cast.get(oid))
        .and_then(|c| c.castfunc)
        .and_then(|f| snapshot.pg_proc.get(&f))
        .is_some_and(|f| snapshot.namespace_name(f.pronamespace) != Some("pg_catalog"))
}

/// The relation a statement writes rows of, when they are what it says:
/// a table (or partitioned table) without rules, or an automatically
/// updatable view over one, without rules or INSTEAD OF triggers.
#[derive(Debug, Clone)]
pub(crate) struct WriteTarget {
    /// The statement's target.
    pub relid: PgClassOid,
    /// The table the rows are stored in.
    pub base: PgClassOid,
    /// Each target column the base relation stores as is, and its base
    /// column (all of a table's own).
    pub to_base: HashMap<String, String>,
    /// Each view from the target down to the base table, with the base
    /// column each of its columns stores as is (`levels[0]`, the target's,
    /// is `to_base`). Empty for a table.
    levels: Vec<(PgClassOid, HashMap<String, String>)>,
}

impl WriteTarget {
    pub(crate) fn resolve(snapshot: &PgCatalog, relid: PgClassOid) -> Option<WriteTarget> {
        Self::follow(snapshot, relid, true)
    }

    /// The rows of view `relid` read as rows of its base table: an
    /// automatically updatable view's rows are projections of its single
    /// base relation's, its plain columns the base columns
    /// (`view_query_is_auto_updatable`).
    pub(crate) fn view_rows(snapshot: &PgCatalog, relid: PgClassOid) -> Option<WriteTarget> {
        let is_view = snapshot
            .pg_class
            .get(&relid)
            .is_some_and(|c| c.relkind == crate::pg_catalog::RelKind::View);
        if !is_view {
            return None;
        }
        Self::follow(snapshot, relid, false)
    }

    /// Follow `relid` down to its base table; `writing` refuses rules and
    /// INSTEAD OF triggers, which make a write something else.
    fn follow(snapshot: &PgCatalog, relid: PgClassOid, writing: bool) -> Option<WriteTarget> {
        let own = |rel: PgClassOid| -> HashMap<String, String> {
            snapshot
                .attributes_of(rel)
                .iter()
                .filter(|a| a.attnum > 0)
                .map(|a| (a.attname.clone(), a.attname.clone()))
                .collect()
        };
        // Each level's columns, by the name they have at `cur`.
        let mut levels: Vec<(PgClassOid, HashMap<String, String>)> = Vec::new();
        let mut cur = relid;
        for _ in 0..16 {
            let class = snapshot.pg_class.get(&cur)?;
            if writing && snapshot.rules.get(&cur).is_some_and(|r| !r.is_empty()) {
                return None;
            }
            match class.relkind {
                crate::pg_catalog::RelKind::Table | crate::pg_catalog::RelKind::Partitioned => {
                    return Some(WriteTarget {
                        relid,
                        base: cur,
                        to_base: levels.first().map_or_else(|| own(cur), |l| l.1.clone()),
                        levels,
                    });
                }
                crate::pg_catalog::RelKind::View => {
                    if writing
                        && snapshot
                            .triggers
                            .get(&cur)
                            .is_some_and(|ts| ts.iter().any(|t| t.instead_row_events != 0))
                    {
                        return None;
                    }
                    let upd = snapshot.view_updatability.get(&cur)?;
                    if upd.not_updatable.is_some() {
                        return None;
                    }
                    let base = upd.base?;
                    let view_attrs = snapshot.attributes_of(cur);
                    let base_attrs = snapshot.attributes_of(base);
                    let base_name = |name: &str| -> Option<String> {
                        let pos = view_attrs.iter().position(|a| a.attname == name)?;
                        let attnum = *upd.columns.get(pos)?.as_ref().ok()?;
                        base_attrs
                            .iter()
                            .find(|a| a.attnum == attnum)
                            .map(|a| a.attname.clone())
                    };
                    levels.push((cur, own(cur)));
                    for (_, map) in &mut levels {
                        *map = std::mem::take(map)
                            .into_iter()
                            .filter_map(|(t, v)| base_name(&v).map(|b| (t, b)))
                            .collect();
                    }
                    cur = base;
                }
                _ => return None,
            }
        }
        None
    }

    /// A target column storing base column `base` as is (the first by
    /// name, when a view exposes it twice).
    pub(crate) fn target_column(&self, base: &str) -> Option<&str> {
        self.to_base
            .iter()
            .filter(|(_, b)| b.as_str() == base)
            .map(|(t, _)| t.as_str())
            .min()
    }

    /// Whether an INSERT stores its target list as is: no BEFORE ROW
    /// INSERT trigger on the base table or a partition it routes to.
    pub(crate) fn insert_keeps(&self, snapshot: &PgCatalog) -> bool {
        let before_insert = |t: &crate::ddl::triggers::Trigger| {
            t.row
                && t.timing & crate::ddl::triggers::TRIGGER_TYPE_BEFORE != 0
                && t.events & crate::ddl::triggers::TRIGGER_TYPE_INSERT != 0
        };
        std::iter::once(self.base)
            .chain(descendants(snapshot, self.base))
            .all(|r| {
                !snapshot
                    .triggers
                    .get(&r)
                    .is_some_and(|ts| ts.iter().any(before_insert))
            })
    }

    /// Whether an UPDATE's new row is the old one with the SET values
    /// ([`update_keeps_values`] of the base table).
    pub(crate) fn update_keeps(&self, snapshot: &PgCatalog) -> bool {
        update_keeps_values(snapshot, self.base)
    }

    /// The target columns proven non-NULL in a row of the base table
    /// known `k` (by base column names) about: what `k` says, the NOT NULL
    /// and CHECK constraints every stored row satisfies, and the generated
    /// columns' expressions over it.
    pub(crate) fn row_not_null(&self, snapshot: &PgCatalog, k: &RowKnowledge) -> HashSet<String> {
        let base = base_row_not_null(snapshot, self.base, k);
        self.to_base
            .iter()
            .filter(|(_, b)| base.contains(*b))
            .map(|(t, _)| t.clone())
            .collect()
    }

    /// What the INSERT writes to base column `attr` it gives no value
    /// for. rewriteTargetListIU fills in each view's defaults on the way
    /// down: the first view whose columns storing it have a default (or
    /// are of a domain type, whose default isn't modeled) decides, else
    /// the base column's own default.
    pub(crate) fn omitted(
        &self,
        snapshot: &PgCatalog,
        attr: &crate::pg_catalog::PgAttribute,
    ) -> ValueInfo {
        for (view, map) in &self.levels {
            let mut found: Option<ValueInfo> = None;
            for va in snapshot.attributes_of(*view) {
                if map.get(&va.attname) != Some(&attr.attname)
                    || (!va.atthasdef && snapshot.unwrap_domain(va.atttypid) == va.atttypid)
                {
                    continue;
                }
                let v = default_value(snapshot, *view, va);
                found = Some(match found {
                    Some(f) => f.either(&v),
                    None => v,
                });
            }
            if let Some(v) = found {
                return v;
            }
        }
        default_value(snapshot, self.base, attr)
    }

    /// What `col = DEFAULT` writes to target column `col` in an UPDATE:
    /// the target's own default for it — through a view, the view's,
    /// which is NULL without one (rewriteTargetListIU doesn't look below).
    pub(crate) fn default_of(&self, snapshot: &PgCatalog, col: &str) -> ValueInfo {
        match snapshot
            .attributes_of(self.relid)
            .iter()
            .find(|a| a.attname == col)
        {
            Some(a) if self.to_base.contains_key(col) => default_value(snapshot, self.relid, a),
            _ => ValueInfo::default(),
        }
    }
}

/// Whether a BEFORE ROW trigger may rewrite a row `event` writes into
/// table `relid` before ExecConstraints checks it: one for the event on
/// the table or on a partition / inheritance child the row lands in — and,
/// for an UPDATE, a BEFORE ROW INSERT one on a partition a row moves to.
pub(crate) fn before_row_trigger_rewrites(
    snapshot: &PgCatalog,
    relid: PgClassOid,
    event: DmlEvent,
) -> bool {
    let fires = |r: PgClassOid, events: i32| {
        snapshot.triggers.get(&r).is_some_and(|ts| {
            ts.iter().any(|t| {
                t.row
                    && t.timing & crate::ddl::triggers::TRIGGER_TYPE_BEFORE != 0
                    && t.events & events != 0
            })
        })
    };
    let children = descendants(snapshot, relid);
    let child_events = match event {
        DmlEvent::Update => event.trigger_bit() | DmlEvent::Insert.trigger_bit(),
        _ => event.trigger_bit(),
    };
    fires(relid, event.trigger_bit()) || children.into_iter().any(|c| fires(c, child_events))
}

/// The inheritance children / partitions of `relid`, recursively.
fn descendants(snapshot: &PgCatalog, relid: PgClassOid) -> Vec<PgClassOid> {
    let mut out = Vec::new();
    let mut todo = vec![relid];
    while let Some(r) = todo.pop() {
        for i in snapshot.pg_inherits.iter().filter(|i| i.inhparent == r) {
            if !out.contains(&i.inhrelid) {
                out.push(i.inhrelid);
                todo.push(i.inhrelid);
            }
        }
    }
    out
}

/// The default an INSERT stores in column `attr` of `relid`: an identity
/// or serial column's sequence value, or its DEFAULT expression. A column
/// without one is NULL — or, domain-typed, the domain's default, which
/// isn't modeled.
pub(crate) fn default_value(
    snapshot: &PgCatalog,
    relid: PgClassOid,
    attr: &crate::pg_catalog::PgAttribute,
) -> ValueInfo {
    use crate::ddl::tables::check_inherit::StoredExpr;
    if attr.attidentity.is_some() {
        return ValueInfo {
            not_null: true,
            ..ValueInfo::default()
        };
    }
    if attr.attgenerated.is_some() {
        return ValueInfo::default();
    }
    match snapshot.attr_default_exprs.get(&(relid, attr.attnum)) {
        Some(StoredExpr::Serial(..)) => ValueInfo {
            not_null: true,
            ..ValueInfo::default()
        },
        Some(StoredExpr::Written(expr)) => {
            let scope = Scope::default();
            let null_ctx = NullabilityContext::default();
            let mut params = ParamCollector::default();
            let (inferred, _) = crate::ddl::depend::collect(|| {
                let _level = QueryLevel::enter();
                expr::infer_expr(
                    expr,
                    expr::Ctx::new(&scope, &null_ctx, snapshot),
                    &mut params,
                    TypeGoal::assignment(attr.atttypid)
                        .with_typmod(snapshot.effective_typmod(attr.atttypid, attr.atttypmod)),
                )
            });
            match inferred {
                Ok(t) => ValueInfo::assigned(expr, &t, attr, snapshot),
                Err(_) => ValueInfo::default(),
            }
        }
        None => ValueInfo {
            not_null: false,
            // No default at all: NULL, unless a domain supplies one.
            null: !attr.atthasdef && snapshot.unwrap_domain(attr.atttypid) == attr.atttypid,
            literal: None,
        },
    }
}

/// The columns of base table `relid` proven non-NULL in a row of it known
/// `k` about (see [`WriteTarget::row_not_null`]). A column of an
/// inheritance parent is NOT NULL only if it is in each child too.
pub(crate) fn base_row_not_null(
    snapshot: &PgCatalog,
    relid: PgClassOid,
    k: &RowKnowledge,
) -> HashSet<String> {
    const ALIAS: &str = "\u{1}row";
    let attrs: Vec<&crate::pg_catalog::PgAttribute> = snapshot
        .attributes_of(relid)
        .iter()
        .filter(|a| a.attnum > 0)
        .collect();
    let children = descendants(snapshot, relid);
    let in_children =
        |name: &str, f: &dyn Fn(PgClassOid, &crate::pg_catalog::PgAttribute) -> bool| {
            children.iter().all(|&d| {
                snapshot
                    .attributes_of(d)
                    .iter()
                    .find(|a| a.attname == name)
                    .is_some_and(|a| f(d, a))
            })
        };
    let base: HashMap<String, bool> = attrs
        .iter()
        .map(|a| {
            (
                a.attname.clone(),
                snapshot.attr_never_null(a)
                    && in_children(&a.attname, &|_, c| snapshot.attr_never_null(c)),
            )
        })
        .collect();
    let mut null_ctx = NullabilityContext::default();
    let checks_apply = snapshot.pg_class.get(&relid).is_some_and(|c| {
        matches!(
            c.relkind,
            crate::pg_catalog::RelKind::Table | crate::pg_catalog::RelKind::Partitioned
        )
    });
    if checks_apply
        && let Some(checks) = crate::nonnull::checks::RelationChecks::of(snapshot, relid)
    {
        null_ctx.register_checks(ALIAS, checks, base.clone());
    }
    let mut facts = Facts::default();
    for c in &k.not_null {
        facts = facts.union(Facts::column(ALIAS, c));
    }
    facts
        .nulls
        .extend(k.nulls.iter().map(|c| (ALIAS.to_owned(), c.clone())));
    facts.equals.extend(
        k.equals
            .iter()
            .map(|(c, v)| ((ALIAS.to_owned(), c.clone()), v.clone())),
    );
    null_ctx.add_where_facts(facts);
    let proven = |ctx: &NullabilityContext| -> HashSet<String> {
        attrs
            .iter()
            .filter(|a| !ctx.is_nullable(ALIAS, &a.attname, base[&a.attname]))
            .map(|a| a.attname.clone())
            .collect()
    };
    let mut not_null = proven(&null_ctx);
    // The generated columns, over what the row is known to hold (in every
    // child, whose expressions may differ).
    let generated: Vec<String> = attrs
        .iter()
        .filter(|a| a.attgenerated.is_some() && !not_null.contains(&a.attname))
        .filter(|a| {
            let input = |c: &str| not_null.contains(c);
            crate::ddl::tables::generation_not_null(snapshot, relid, a, &input)
                && in_children(&a.attname, &|d, c| {
                    crate::ddl::tables::generation_not_null(snapshot, d, c, &input)
                })
        })
        .map(|a| a.attname.clone())
        .collect();
    if !generated.is_empty() {
        let facts = generated.iter().fold(Facts::default(), |acc, c| {
            acc.union(Facts::column(ALIAS, c))
        });
        null_ctx.add_where_facts(facts);
        not_null = proven(&null_ctx);
    }
    not_null
}

/// What the SET list `target_list` writes, by target column, re-inferred
/// in `ctx` — where what is known of the old row holds — on a scratch copy
/// of the parameters. Columns assigned through indirection, or from a
/// sub-SELECT, are left out (nothing is known of them).
pub(crate) fn set_values(
    target_list: &[protobuf::Node],
    target: &WriteTarget,
    table_attrs: &[crate::pg_catalog::PgAttribute],
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> HashMap<String, ValueInfo> {
    let mut out = HashMap::new();
    let mut scratch = params.clone();
    for item in target_list {
        let Some(node::Node::ResTarget(rt)) = item.node.as_ref() else {
            continue;
        };
        if !rt.indirection.is_empty() {
            continue;
        }
        let Some(val) = rt.val.as_deref() else {
            continue;
        };
        let val = match val.node.as_ref() {
            Some(node::Node::MultiAssignRef(mar)) => {
                match mar.source.as_deref().and_then(|s| s.node.as_ref()) {
                    Some(node::Node::RowExpr(row)) => {
                        let colno = usize::try_from(mar.colno).unwrap_or(1).max(1);
                        match row.args.get(colno - 1) {
                            Some(v) => v,
                            None => continue,
                        }
                    }
                    _ => continue,
                }
            }
            _ => val,
        };
        let Some(attr) = table_attrs.iter().find(|a| a.attname == rt.name) else {
            continue;
        };
        let info = if is_set_to_default(val) {
            target.default_of(ctx.snapshot, &attr.attname)
        } else {
            let goal = TypeGoal::assignment(attr.atttypid)
                .with_typmod(ctx.snapshot.effective_typmod(attr.atttypid, attr.atttypmod));
            match expr::infer_expr(val, ctx, &mut scratch, goal) {
                Ok(t) => ValueInfo::assigned(val, &t, attr, ctx.snapshot),
                Err(_) => continue,
            }
        };
        out.insert(attr.attname.clone(), info);
    }
    params.absorb_non_null_reads(&scratch);
    out
}

/// `null_ctx` knowing columns `columns` of FROM entry `alias` non-NULL.
pub(crate) fn prove_columns(
    null_ctx: &mut NullabilityContext,
    alias: &str,
    columns: &HashSet<String>,
) {
    if columns.is_empty() {
        return;
    }
    let facts = columns.iter().fold(Facts::default(), |acc, c| {
        acc.union(Facts::column(alias, c))
    });
    null_ctx.add_where_facts(facts);
}
