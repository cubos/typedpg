//! Proofs the simplification rules share. Each answers only from what PG
//! enforces at every statement — `attnotnull` (validated), the relation
//! kinds that store what they declare — never from what the analyzer's
//! nullability inference trusts (CHECKs, `NOT VALID` or deferrable
//! constraints, foreign keys). Every helper errs on the side of "not
//! proven": a missed simplification is fine, a wrong one is not.

use std::collections::HashSet;

use typedpg_pg_query::NodeRef;
use typedpg_pg_query::protobuf::{
    AExprKind, BoolExprType, ColumnRef, JoinType, Node, SelectStmt, node::Node as N,
};

use super::Ctx;
use crate::oid::{PgClassOid, PgProcOid};
use crate::pg_catalog::{PgCatalog, ProKind, ProVolatile, RelKind};

/// OIDs below this are PG's own objects (`FirstNormalObjectId`).
const FIRST_NORMAL_OBJECT_ID: u32 = 16384;

/// A column reference resolved to the base-table column it reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResolvedColumn {
    pub rel: PgClassOid,
    pub attnum: i16,
    /// An outer join (or a grouping set) can make the reference NULL even
    /// where the column can't be.
    pub nullable_side: bool,
}

/// A FROM item of a [`FromScope`].
#[derive(Debug, Clone)]
enum Item {
    /// A base table: the name it is referenced by (its alias, or its
    /// name, with the schema it was written with when it has no alias).
    Table {
        schema: Option<String>,
        refname: String,
        rel: PgClassOid,
        nullable: bool,
    },
    /// Anything else (a subquery, a function, a CTE, a view, a join with
    /// an alias, a table with column aliases): its columns are unknown, so
    /// a reference that may be to it resolves to nothing. `refname` is the
    /// name it can be qualified by; `None` when it isn't known — PG names
    /// an unaliased function after the function (`FROM unnest(…)` is
    /// `unnest`), an unaliased subquery, VALUES or table function after
    /// rules of its own — and then it may bind any qualifier.
    Opaque { refname: Option<String> },
}

/// The FROM scope of one query level (a SELECT, or an UPDATE/DELETE),
/// with the scopes of the levels it is nested in: what a column reference
/// in its target list, WHERE or HAVING resolves to. (Not its JOIN ... ON
/// clauses, which see fewer items, nor its ORDER BY / GROUP BY, which can
/// name output columns.)
pub(crate) struct FromScope<'a> {
    items: Vec<Item>,
    /// GROUPING SETS / ROLLUP / CUBE: a grouped column can read NULL.
    grouping_sets: bool,
    parent: Option<&'a FromScope<'a>>,
}

impl<'a> FromScope<'a> {
    /// The scope a SELECT's target list, WHERE and HAVING see. A set
    /// operation or a VALUES has no FROM of its own: its scope resolves
    /// nothing.
    pub(crate) fn of_select(
        ctx: &Ctx<'_>,
        select: &SelectStmt,
        parent: Option<&'a FromScope<'a>>,
    ) -> FromScope<'a> {
        if select.op != typedpg_pg_query::protobuf::SetOperation::SetopNone as i32
            || !select.values_lists.is_empty()
        {
            return Self::opaque(parent);
        }
        let ctes = cte_names(ctx);
        let mut items = Vec::new();
        for from in &select.from_clause {
            from_item(ctx, &ctes, from, false, &mut items);
        }
        let grouping_sets = select.group_clause.iter().any(|g| {
            g.node.as_ref().is_some_and(|g| {
                g.nodes()
                    .iter()
                    .any(|(n, _)| matches!(n, NodeRef::GroupingSet(_)))
            })
        });
        FromScope {
            items,
            grouping_sets,
            parent,
        }
    }

    /// The scope an UPDATE's SET and WHERE, or a DELETE's WHERE, see: the
    /// target relation and the FROM / USING items.
    pub(crate) fn of_dml(
        ctx: &Ctx<'_>,
        relation: Option<&typedpg_pg_query::protobuf::RangeVar>,
        from: &[Node],
        parent: Option<&'a FromScope<'a>>,
    ) -> FromScope<'a> {
        let ctes = cte_names(ctx);
        let mut items = Vec::new();
        match relation {
            Some(rv) => range_var(ctx, &ctes, rv, false, &mut items),
            None => items.push(Item::Opaque { refname: None }),
        }
        for f in from {
            from_item(ctx, &ctes, f, false, &mut items);
        }
        FromScope {
            items,
            grouping_sets: false,
            parent,
        }
    }

    /// A level whose references resolve to nothing: one with a FROM item
    /// no column of can be known, the parents included (a reference may be
    /// to it rather than to them).
    pub(crate) fn opaque(parent: Option<&'a FromScope<'a>>) -> FromScope<'a> {
        FromScope {
            items: vec![Item::Opaque { refname: None }],
            grouping_sets: false,
            parent,
        }
    }

    /// Whether `col`, written at this level, may be resolved by one of its
    /// own items rather than a parent level's: `false` only when none of
    /// them can bind it. For moving an expression into this level from the
    /// one around it.
    pub(crate) fn may_bind_here(&self, ctx: &Ctx<'_>, col: &ColumnRef) -> bool {
        let Some(fields) = col
            .fields
            .iter()
            .map(|f| match f.node.as_ref() {
                Some(N::String(s)) => Some(s.sval.as_str()),
                _ => None,
            })
            .collect::<Option<Vec<&str>>>()
        else {
            return true;
        };
        let (table, column) = match fields.as_slice() {
            [c] => (None, *c),
            [t, c] => (Some(*t), *c),
            // A schema-qualified reference: compare by table name only.
            [_, t, c] => (Some(*t), *c),
            _ => return true,
        };
        self.items.iter().any(|item| match (item, table) {
            (Item::Opaque { .. }, None) => true,
            (Item::Opaque { refname }, Some(t)) => refname.as_deref().is_none_or(|r| r == t),
            (Item::Table { refname, .. }, Some(t)) => refname == t,
            (Item::Table { rel, .. }, None) => {
                ctx.catalog.attribute_by_name(*rel, column).is_some() || is_system_column(column)
            }
        })
    }

    /// The column `col` reads, or `None` when it isn't provably a plain
    /// column of a base table in this scope or a parent one.
    pub(crate) fn resolve(&self, ctx: &Ctx<'_>, col: &ColumnRef) -> Option<ResolvedColumn> {
        let fields: Vec<&str> = col
            .fields
            .iter()
            .map(|f| match f.node.as_ref() {
                Some(N::String(s)) => Some(s.sval.as_str()),
                _ => None,
            })
            .collect::<Option<_>>()?;
        let (schema, table, column) = match fields.as_slice() {
            [c] => (None, None, *c),
            [t, c] => (None, Some(*t), *c),
            [s, t, c] => (Some(*s), Some(*t), *c),
            _ => return None,
        };
        let mut level = Some(self);
        while let Some(scope) = level {
            let mut found = None;
            for item in &scope.items {
                let matches = match (item, table) {
                    (Item::Opaque { .. }, None) => return None,
                    (Item::Opaque { refname }, Some(t)) => {
                        if refname.as_deref().is_none_or(|r| r == t) {
                            return None;
                        }
                        false
                    }
                    (
                        Item::Table {
                            schema: item_schema,
                            refname,
                            rel,
                            ..
                        },
                        _,
                    ) => {
                        let named = match (schema, table) {
                            (None, None) => true,
                            (None, Some(t)) => refname == t,
                            (Some(s), Some(t)) => {
                                refname == t
                                    && match item_schema {
                                        Some(is) => is == s,
                                        None => ctx.catalog.pg_class.get(rel).is_some_and(|c| {
                                            ctx.catalog.namespace_name(c.relnamespace) == Some(s)
                                        }),
                                    }
                            }
                            (Some(_), None) => false,
                        };
                        named
                            && (table.is_some()
                                || ctx.catalog.attribute_by_name(*rel, column).is_some()
                                || is_system_column(column))
                    }
                };
                if matches {
                    if found.is_some() {
                        // Ambiguous, or a USING / NATURAL join's merged
                        // column: not one table's.
                        return None;
                    }
                    found = Some(item);
                }
            }
            if let Some(Item::Table { rel, nullable, .. }) = found {
                let attr = ctx.catalog.attribute_by_name(*rel, column)?;
                if attr.attnum <= 0 {
                    return None;
                }
                return Some(ResolvedColumn {
                    rel: *rel,
                    attnum: attr.attnum,
                    nullable_side: *nullable || scope.grouping_sets,
                });
            }
            level = scope.parent;
        }
        None
    }
}

/// PG's system column names, which every table has besides its own.
fn is_system_column(name: &str) -> bool {
    matches!(
        name,
        "tableoid" | "ctid" | "xmin" | "xmax" | "cmin" | "cmax" | "oid"
    )
}

/// Every CTE name declared anywhere in the statement: a relation name
/// equal to one may be a reference to it.
fn cte_names(ctx: &Ctx<'_>) -> HashSet<String> {
    ctx.tree
        .nodes()
        .iter()
        .filter_map(|(n, _)| match n {
            NodeRef::CommonTableExpr(c) => Some(c.ctename.clone()),
            _ => None,
        })
        .collect()
}

/// Add the items a FROM entry brings into scope. `nullable`: the entry is
/// on the nullable side of an outer join.
fn from_item(
    ctx: &Ctx<'_>,
    ctes: &HashSet<String>,
    from: &Node,
    nullable: bool,
    items: &mut Vec<Item>,
) {
    match from.node.as_ref() {
        Some(N::RangeVar(rv)) => range_var(ctx, ctes, rv, nullable, items),
        Some(N::JoinExpr(j)) => {
            if let Some(alias) = &j.alias {
                // The alias hides the joined tables' names.
                items.push(Item::Opaque {
                    refname: Some(alias.aliasname.clone()),
                });
                return;
            }
            if let Some(alias) = &j.join_using_alias {
                items.push(Item::Opaque {
                    refname: Some(alias.aliasname.clone()),
                });
            }
            let (left_nullable, right_nullable) = match j.jointype() {
                JoinType::JoinInner => (false, false),
                JoinType::JoinLeft => (false, true),
                JoinType::JoinRight => (true, false),
                _ => (true, true),
            };
            for (side, side_nullable) in [(&j.larg, left_nullable), (&j.rarg, right_nullable)] {
                match side {
                    Some(n) => from_item(ctx, ctes, n, nullable || side_nullable, items),
                    None => items.push(Item::Opaque { refname: None }),
                }
            }
        }
        Some(N::RangeSubselect(s)) => items.push(Item::Opaque {
            refname: s.alias.as_ref().map(|a| a.aliasname.clone()),
        }),
        Some(N::RangeFunction(f)) => items.push(Item::Opaque {
            refname: f.alias.as_ref().map(|a| a.aliasname.clone()),
        }),
        _ => items.push(Item::Opaque { refname: None }),
    }
}

/// Add a relation reference: a base table whose stored rows hold what
/// its columns declare, or an opaque item.
fn range_var(
    ctx: &Ctx<'_>,
    ctes: &HashSet<String>,
    rv: &typedpg_pg_query::protobuf::RangeVar,
    nullable: bool,
    items: &mut Vec<Item>,
) {
    let alias = rv.alias.as_ref();
    let refname = alias.map_or_else(|| rv.relname.clone(), |a| a.aliasname.clone());
    let schema = (!rv.schemaname.is_empty()).then(|| rv.schemaname.clone());
    let opaque = || Item::Opaque {
        refname: Some(refname.clone()),
    };
    if schema.is_none() && ctes.contains(&rv.relname)
        || !rv.catalogname.is_empty()
        || alias.is_some_and(|a| !a.colnames.is_empty())
    {
        items.push(opaque());
        return;
    }
    let Some(class) = ctx.catalog.resolve_table(schema.as_deref(), &rv.relname) else {
        items.push(opaque());
        return;
    };
    if !stored_tree(ctx.catalog, class.oid) {
        items.push(opaque());
        return;
    }
    items.push(Item::Table {
        schema: if alias.is_some() { None } else { schema },
        refname,
        rel: class.oid,
        nullable,
    });
}

/// Every relation a scan of `rel` reads: `rel` and its inheritance
/// children and partitions, recursively.
pub(crate) fn inheritance_tree(catalog: &PgCatalog, rel: PgClassOid) -> Vec<PgClassOid> {
    let mut out = Vec::new();
    let mut pending = vec![rel];
    let mut seen = HashSet::new();
    while let Some(r) = pending.pop() {
        if !seen.insert(r) {
            continue;
        }
        out.push(r);
        pending.extend(
            catalog
                .pg_inherits
                .iter()
                .filter(|i| i.inhparent == r)
                .map(|i| i.inhrelid),
        );
    }
    out
}

/// Whether every relation a scan of `rel` reads ([`inheritance_tree`]) is
/// a table or a partitioned table: what PG stores and checks there is what
/// it declares. A foreign table (which can be an inheritance child or a
/// partition) holds whatever its server returns — PG enforces neither its
/// NOT NULLs nor its other constraints — and a view or anything else isn't
/// stored at all.
pub(crate) fn stored_tree(catalog: &PgCatalog, rel: PgClassOid) -> bool {
    inheritance_tree(catalog, rel).into_iter().all(|r| {
        catalog
            .pg_class
            .get(&r)
            .is_some_and(|c| matches!(c.relkind, RelKind::Table | RelKind::Partitioned))
    })
}

/// Whether column `attnum` of `rel` holds no NULL in any row a scan of
/// `rel` returns: a validated NOT NULL on it and on every inheritance
/// child or partition (a `NO INHERIT` one, or a child's own `NOT VALID`
/// one, would let a child hold a NULL).
pub(crate) fn column_not_null(catalog: &PgCatalog, rel: PgClassOid, attnum: i16) -> bool {
    let Some(attr) = catalog
        .attributes_of(rel)
        .iter()
        .find(|a| a.attnum == attnum)
    else {
        return false;
    };
    let name = attr.attname.clone();
    stored_tree(catalog, rel)
        && inheritance_tree(catalog, rel).into_iter().all(|r| {
            catalog.attribute_by_name(r, &name).is_some_and(|a| {
                // A virtual generated column is computed as it is read:
                // its expression, not a stored value.
                a.attgenerated != Some(crate::pg_catalog::AttGenerated::Virtual)
                    && catalog.attr_proven_not_null(a)
            })
        })
}

/// OIDs below this are the ones PG's catalog files assign by hand
/// (`FirstGenbkiObjectId`): the same objects in every installation.
const FIRST_GENBKI_OBJECT_ID: u32 = 10_000;

/// Whether `t` is a built-in type, or a domain over one.
fn builtin_type(catalog: &PgCatalog, t: crate::oid::PgTypeOid) -> bool {
    catalog.unwrap_domain(t).get() < FIRST_GENBKI_OBJECT_ID
}

/// Whether `left = right`, between non-NULL operands of these types, is
/// never NULL: the operator `=` resolves to is a built-in one whose
/// function returns NULL only for a NULL argument, and both types are
/// built-in (or domains over built-in ones). A user `=` — on a domain or a
/// type of its own, or one the search path finds first — may return NULL
/// for any input, where `x NOT IN (…)` and `NOT EXISTS (… x = y)` part.
pub(crate) fn builtin_strict_equality(
    catalog: &PgCatalog,
    left: crate::oid::PgTypeOid,
    right: crate::oid::PgTypeOid,
) -> bool {
    builtin_type(catalog, left)
        && builtin_type(catalog, right)
        && catalog
            .find_operator("=", Some(left), right)
            .is_some_and(|op| {
                op.oid.get() < FIRST_GENBKI_OBJECT_ID
                    && op.code.is_some_and(|p| null_only_on_null_proc(catalog, p))
            })
}

/// The type of column `attnum` of `rel`.
pub(crate) fn column_type(
    catalog: &PgCatalog,
    rel: PgClassOid,
    attnum: i16,
) -> Option<crate::oid::PgTypeOid> {
    catalog
        .attributes_of(rel)
        .iter()
        .find(|a| a.attnum == attnum)
        .map(|a| a.atttypid)
}

/// `sql[range]`, to embed in other SQL: with a newline after it when it
/// ends in a `--` comment (or cut inside one), which would otherwise
/// swallow whatever is written after it.
pub(crate) fn embed_source(sql: &str, range: std::ops::Range<usize>) -> String {
    let text = &sql[range];
    let ends_in_line_comment = match typedpg_pg_query::scan(text) {
        Ok(scanned) => scanned.tokens.last().is_some_and(|t| {
            t.token() == typedpg_pg_query::protobuf::Token::SqlComment
                || usize::try_from(t.end).is_ok_and(|end| text[end..].contains("--"))
        }),
        // Unscannable (cut inside a string or comment): be safe.
        Err(_) => true,
    };
    if ends_in_line_comment {
        format!("{text}\n")
    } else {
        text.to_owned()
    }
}

/// Whether `expr`, evaluated in `scope`, can never be NULL by what PG
/// enforces: a column of a base table with a validated NOT NULL (on its
/// inheritance children too) that no outer join or grouping set makes
/// nullable; a non-NULL constant (cast from a string literal, too); an
/// AND / OR / NOT of such; or a built-in operator or function every
/// overload of which returns NULL only for a NULL argument, over such.
pub(crate) fn strictly_not_null(ctx: &Ctx<'_>, expr: &Node, scope: &FromScope<'_>) -> bool {
    match expr.node.as_ref() {
        Some(N::ColumnRef(c)) => scope
            .resolve(ctx, c)
            .is_some_and(|r| !r.nullable_side && column_not_null(ctx.catalog, r.rel, r.attnum)),
        Some(N::AConst(c)) => !c.isnull && c.val.is_some(),
        Some(N::TypeCast(t)) => matches!(
            t.arg.as_deref().and_then(|a| a.node.as_ref()),
            Some(N::AConst(c)) if !c.isnull
                && matches!(c.val, Some(typedpg_pg_query::protobuf::a_const::Val::Sval(_)))
        ),
        Some(N::BoolExpr(b)) => {
            matches!(
                b.boolop(),
                BoolExprType::AndExpr | BoolExprType::OrExpr | BoolExprType::NotExpr
            ) && b.args.iter().all(|a| strictly_not_null(ctx, a, scope))
        }
        Some(N::AExpr(e)) => {
            e.kind() == AExprKind::AexprOp
                && null_only_on_null_operator(ctx.catalog, &e.name, e.lexpr.is_some())
                && [&e.lexpr, &e.rexpr]
                    .into_iter()
                    .flatten()
                    .all(|a| strictly_not_null(ctx, a, scope))
        }
        Some(N::FuncCall(f)) => {
            f.agg_order.is_empty()
                && f.agg_filter.is_none()
                && f.over.is_none()
                && !f.agg_within_group
                && !f.agg_star
                && !f.agg_distinct
                && !f.func_variadic
                && null_only_on_null_function(ctx.catalog, &f.funcname)
                && f.args.iter().all(|a| {
                    !matches!(a.node, Some(N::NamedArgExpr(_))) && strictly_not_null(ctx, a, scope)
                })
        }
        _ => false,
    }
}

/// The `[schema, name]` / `[name]` strings of a qualified name.
fn name_parts(name: &[Node]) -> Option<(Option<&str>, &str)> {
    let parts: Vec<&str> = name
        .iter()
        .map(|n| match n.node.as_ref() {
            Some(N::String(s)) => Some(s.sval.as_str()),
            _ => None,
        })
        .collect::<Option<_>>()?;
    match parts.as_slice() {
        [name] => Some((None, *name)),
        [schema, name] => Some((Some(*schema), *name)),
        _ => None,
    }
}

/// Whether the built-in routine `proc` is a plain function returning NULL
/// only when an argument is NULL.
fn null_only_on_null_proc(catalog: &PgCatalog, proc: PgProcOid) -> bool {
    catalog.pg_proc.get(&proc).is_some_and(|p| {
        p.oid.get() < FIRST_NORMAL_OBJECT_ID
            && p.prokind == ProKind::Function
            && p.proisstrict
            && !p.proretset
            && !crate::builtin_nullability::NULLABLE_STRICT
                .contains(&crate::functions::proc_signature(p, catalog).as_str())
    })
}

/// Whether every operator named `name` of the arity (prefix or binary)
/// is a built-in one returning NULL only for a NULL operand — whichever
/// one the expression resolved to. Operators of every schema count, not
/// just the search path's.
fn null_only_on_null_operator(catalog: &PgCatalog, name: &[Node], binary: bool) -> bool {
    let Some((_, op)) = name_parts(name) else {
        return false;
    };
    let mut any = false;
    for o in catalog.pg_operator.values() {
        if o.oprname != op || o.oprleft.is_some() != binary {
            continue;
        }
        any = true;
        if o.oid.get() >= FIRST_NORMAL_OBJECT_ID
            || !o
                .oprcode
                .is_some_and(|p| null_only_on_null_proc(catalog, p))
        {
            return false;
        }
    }
    any
}

/// [`null_only_on_null_operator`] for a function call: every function of
/// that name on the search path (or in the schema it names).
fn null_only_on_null_function(catalog: &PgCatalog, name: &[Node]) -> bool {
    let Some((schema, func)) = name_parts(name) else {
        return false;
    };
    let candidates = catalog.find_functions(schema, func);
    !candidates.is_empty()
        && candidates
            .iter()
            .all(|p| null_only_on_null_proc(catalog, p.oid))
}

/// Whether the schema has a volatile function PG may call where the
/// parse tree shows no call: a cast function (an implicit or assignment
/// cast runs unwritten, `WHERE v = 1` over a type with one), a type's
/// support function (its input or output function runs on an I/O
/// conversion), or one a domain's CHECK calls (it runs on every coercion
/// to the domain). Built-in ones never are; when the schema adds one,
/// every expression is taken to be volatile.
fn hidden_volatile_calls(catalog: &PgCatalog) -> bool {
    const PG_TYPE: u32 = 1247;
    const PG_PROC: u32 = 1255;
    const PG_CONSTRAINT: u32 = 2606;
    let volatile = |p: PgProcOid| {
        catalog
            .pg_proc
            .get(&p)
            .is_none_or(|p| p.provolatile == ProVolatile::Volatile)
    };
    catalog
        .pg_cast
        .values()
        .any(|c| c.castfunc.is_some_and(volatile))
        || catalog.iter_pg_depend().any(|d| {
            matches!(d.classid.get(), PG_TYPE | PG_CONSTRAINT)
                && d.refclassid.get() == PG_PROC
                && volatile(PgProcOid::from_raw(d.refobjid.get()))
        })
}

/// Whether `node` may call a volatile function or read something
/// nondeterministic: a function or operator any same-named overload of
/// which is VOLATILE (unknown ones included), a cast to a type that isn't
/// built-in or through a volatile cast function, a view (its body is not
/// looked into) or foreign table, TABLESAMPLE, a locking clause. Errs
/// towards `true`.
pub(crate) fn has_volatile(ctx: &Ctx<'_>, node: &Node) -> bool {
    let catalog = ctx.catalog;
    let ctes = cte_names(ctx);
    let volatile_proc = |p: PgProcOid| {
        catalog
            .pg_proc
            .get(&p)
            .is_none_or(|p| p.provolatile == ProVolatile::Volatile)
    };
    let volatile_operator = |name: &str| {
        catalog
            .pg_operator
            .values()
            .any(|o| o.oprname == name && o.oprcode.is_none_or(volatile_proc))
    };
    if hidden_volatile_calls(catalog) {
        return true;
    }
    let Some(root) = node.node.as_ref() else {
        return false;
    };
    root.nodes().iter().any(|(n, _)| match n {
        NodeRef::FuncCall(f) => match name_parts(&f.funcname) {
            Some((schema, name)) => {
                let candidates = catalog.find_functions(schema, name);
                candidates.is_empty()
                    || candidates
                        .iter()
                        .any(|p| p.provolatile == ProVolatile::Volatile)
            }
            None => true,
        },
        NodeRef::AExpr(e) => {
            let names: Vec<&str> = match e.kind() {
                AExprKind::AexprBetween
                | AExprKind::AexprNotBetween
                | AExprKind::AexprBetweenSym
                | AExprKind::AexprNotBetweenSym => vec![">=", "<=", "<", ">"],
                _ => match name_parts(&e.name) {
                    Some((_, op)) => vec![op],
                    None => return true,
                },
            };
            names.into_iter().any(volatile_operator)
        }
        NodeRef::SubLink(s) => match name_parts(&s.oper_name) {
            Some((_, op)) => volatile_operator(op),
            None => !s.oper_name.is_empty() || volatile_operator("="),
        },
        NodeRef::TypeCast(t) => {
            let Some(target) = t.type_name.as_ref().and_then(|tn| {
                let (schema, name) = name_parts(&tn.names)?;
                catalog.resolve_type_by_name(schema, name)
            }) else {
                return true;
            };
            target.oid.get() >= FIRST_NORMAL_OBJECT_ID
                || catalog
                    .pg_cast
                    .values()
                    .any(|c| c.casttarget == target.oid && c.castfunc.is_some_and(volatile_proc))
        }
        NodeRef::RangeVar(rv) => {
            if rv.schemaname.is_empty() && ctes.contains(&rv.relname) {
                return false;
            }
            let schema = (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str());
            catalog.resolve_table(schema, &rv.relname).is_none_or(|c| {
                c.relkind != RelKind::MaterializedView && !stored_tree(catalog, c.oid)
            })
        }
        NodeRef::RangeTableSample(_) | NodeRef::LockingClause(_) => true,
        _ => false,
    })
}
