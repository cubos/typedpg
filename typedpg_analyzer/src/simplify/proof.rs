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

    /// The scope of a list of FROM items alone (a join's two sides, for
    /// its ON condition), within `parent`.
    pub(crate) fn of_items(
        ctx: &Ctx<'_>,
        from: &[Node],
        parent: Option<&'a FromScope<'a>>,
    ) -> FromScope<'a> {
        let ctes = cte_names(ctx);
        let mut items = Vec::new();
        for f in from {
            from_item(ctx, &ctes, f, false, &mut items);
        }
        FromScope {
            items,
            grouping_sets: false,
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

    /// The `(schema, refname)` of the table an unqualified `column` binds
    /// to, as [`Self::resolve`] finds it: the schema only when written and
    /// the table has no alias.
    pub(crate) fn binding_item(
        &self,
        ctx: &Ctx<'_>,
        column: &str,
    ) -> Option<(Option<String>, String)> {
        let mut level = Some(self);
        while let Some(scope) = level {
            let mut found = None;
            for item in &scope.items {
                match item {
                    Item::Opaque { .. } => return None,
                    Item::Table {
                        schema,
                        refname,
                        rel,
                        ..
                    } => {
                        if ctx.catalog.attribute_by_name(*rel, column).is_some()
                            || is_system_column(column)
                        {
                            if found.is_some() {
                                return None;
                            }
                            found = Some((schema.clone(), refname.clone()));
                        }
                    }
                }
            }
            if found.is_some() {
                return found;
            }
            level = scope.parent;
        }
        None
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
/// function returns NULL only for a NULL argument, both types are
/// built-in (or domains over built-in ones), and the operator takes
/// exactly those types (domains unwrapped), so PG coerces neither operand
/// first. A user `=` — on a domain or a type of its own, or one the
/// search path finds first — may return NULL for any input, and so may a
/// user cast PG inserts before a built-in one: there `x NOT IN (…)` and
/// `NOT EXISTS (… x = y)` part.
pub(crate) fn builtin_strict_equality(
    catalog: &PgCatalog,
    left: crate::oid::PgTypeOid,
    right: crate::oid::PgTypeOid,
) -> bool {
    builtin_operator_exact(catalog, "=", left, right)
        .is_some_and(|op| op.code.is_some_and(|p| null_only_on_null_proc(catalog, p)))
}

/// The built-in operator `name` resolves to between `left` and `right`
/// (both built-in types or domains over them), when it takes exactly
/// those types once domains are unwrapped: no coercion runs before it.
fn builtin_operator_exact(
    catalog: &PgCatalog,
    name: &str,
    left: crate::oid::PgTypeOid,
    right: crate::oid::PgTypeOid,
) -> Option<crate::lookup::ResolvedOperator> {
    if !builtin_type(catalog, left) || !builtin_type(catalog, right) {
        return None;
    }
    let op = catalog.find_operator(name, Some(left), right)?;
    let declared = catalog.pg_operator.get(&op.oid)?;
    (op.oid.get() < FIRST_GENBKI_OBJECT_ID
        && declared.oprleft == Some(catalog.unwrap_domain(left))
        && declared.oprright == catalog.unwrap_domain(right))
    .then_some(op)
}

/// `unknown`: an untyped literal, which takes the other operand's type.
const UNKNOWN: u32 = 705;

/// The comparison operators [`cannot_error`] accepts.
const COMPARISONS: &[&str] = &["=", "<>", "!=", "<", "<=", ">", ">="];

/// Where an operand's collation comes from.
#[derive(Clone, Copy)]
enum OperandCollation {
    /// A column: its own (`attcollation`), with implicit derivation.
    Column(Option<crate::oid::PgCollationOid>),
    /// A constant or parameter: its type's (`typcollation`) once coerced,
    /// with implicit derivation too.
    OfType,
}

/// `DEFAULT_COLLATION_OID`: the database default, which an implicit
/// collation of the other side overrides without a conflict.
const DEFAULT_COLLATION: u32 = 100;

/// What an operand of a comparison is, for [`cannot_error`]: its type, and
/// where its collation comes from.
fn operand(
    ctx: &Ctx<'_>,
    node: &Node,
    scope: &FromScope<'_>,
) -> Option<(crate::oid::PgTypeOid, OperandCollation)> {
    use crate::oid::PgTypeOid;
    use typedpg_pg_query::protobuf::a_const::Val;
    match node.node.as_ref()? {
        N::ColumnRef(c) => {
            let r = scope.resolve(ctx, c)?;
            let attr = ctx
                .catalog
                .attributes_of(r.rel)
                .iter()
                .find(|a| a.attnum == r.attnum)?;
            Some((attr.atttypid, OperandCollation::Column(attr.attcollation)))
        }
        N::AConst(c) if c.isnull => Some((PgTypeOid::from_raw(UNKNOWN), OperandCollation::OfType)),
        N::AConst(c) => Some((
            PgTypeOid::from_raw(match c.val.as_ref()? {
                Val::Ival(_) => 23,
                Val::Fval(_) => 1700,
                Val::Boolval(_) => 16,
                Val::Sval(_) => UNKNOWN,
                Val::Bsval(_) => return None,
            }),
            OperandCollation::OfType,
        )),
        N::ParamRef(p) => {
            let param = ctx
                .analyzed
                .params
                .get(usize::try_from(p.number).ok()?.checked_sub(1)?)?;
            let (schema, name) = match &param.pg_type {
                crate::types::Type::Basic { schema, name, .. }
                | crate::types::Type::Domain { schema, name, .. } => (schema, name),
                _ => return None,
            };
            Some((
                ctx.catalog.resolve_type_by_name(Some(schema), name)?.oid,
                OperandCollation::OfType,
            ))
        }
        _ => None,
    }
}

/// Whether comparing operands of these (coerced) types and collation
/// sources can't hit a collation conflict at run time. Every side's
/// derivation is implicit — a column's, and a constant's or parameter's,
/// which is its type's collation (`name`'s is "C") — so PG merges them
/// without complaint only when they are equal or one is the database
/// default (which the other then overrides). Two columns must agree
/// exactly; a collatable type whose collation can't be read is refused.
fn collations_agree(
    catalog: &PgCatalog,
    left: (crate::oid::PgTypeOid, OperandCollation),
    right: (crate::oid::PgTypeOid, OperandCollation),
) -> bool {
    let collatable = |t: crate::oid::PgTypeOid| catalog.get_type(t).map(|t| t.typcollation);
    let (Some(lcoll), Some(rcoll)) = (collatable(left.0), collatable(right.0)) else {
        return false;
    };
    if lcoll.is_none() && rcoll.is_none() {
        return true;
    }
    let effective =
        |(coll, source): (Option<crate::oid::PgCollationOid>, OperandCollation)| match source {
            OperandCollation::Column(c) => c,
            OperandCollation::OfType => coll,
        };
    match (left.1, right.1) {
        (OperandCollation::Column(a), OperandCollation::Column(b)) => a.is_some() && a == b,
        _ => {
            let (Some(a), Some(b)) = (effective((lcoll, left.1)), effective((rcoll, right.1)))
            else {
                return false;
            };
            a == b || a.get() == DEFAULT_COLLATION || b.get() == DEFAULT_COLLATION
        }
    }
}

/// Whether evaluating `expr` in `scope` can never raise an error, whatever
/// the rows: column references (not virtual generated ones), constants,
/// parameters; AND / OR / NOT, IS [NOT] NULL, IS [NOT] TRUE / FALSE /
/// UNKNOWN of such; and `= <> < <= > >=` between them through a built-in
/// operator that takes exactly their types (no cast runs first), between
/// columns of one collation. Anything else — a cast, arithmetic, a function
/// call, a pattern match — may fail on some row.
pub(crate) fn cannot_error(ctx: &Ctx<'_>, expr: &Node, scope: &FromScope<'_>) -> bool {
    match expr.node.as_ref() {
        Some(N::ColumnRef(c)) => scope.resolve(ctx, c).is_some_and(|r| {
            ctx.catalog
                .attributes_of(r.rel)
                .iter()
                .find(|a| a.attnum == r.attnum)
                .is_some_and(|a| a.attgenerated != Some(crate::pg_catalog::AttGenerated::Virtual))
        }),
        Some(N::AConst(_) | N::ParamRef(_)) => true,
        Some(N::BoolExpr(b)) => b.args.iter().all(|a| cannot_error(ctx, a, scope)),
        Some(N::NullTest(t)) => t
            .arg
            .as_deref()
            .is_some_and(|a| cannot_error(ctx, a, scope)),
        Some(N::BooleanTest(t)) => t
            .arg
            .as_deref()
            .is_some_and(|a| cannot_error(ctx, a, scope)),
        Some(N::AExpr(e)) => {
            let (Some(l), Some(r)) = (e.lexpr.as_deref(), e.rexpr.as_deref()) else {
                return false;
            };
            let Some((None, op)) = name_parts(&e.name) else {
                return false;
            };
            if e.kind() != AExprKind::AexprOp
                || !COMPARISONS.contains(&op)
                || !cannot_error(ctx, l, scope)
                || !cannot_error(ctx, r, scope)
            {
                return false;
            }
            let (Some((lt, lc)), Some((rt, rc))) = (operand(ctx, l, scope), operand(ctx, r, scope))
            else {
                return false;
            };
            // An untyped literal takes the other side's type; its input
            // function runs as the query is parsed, for both forms alike.
            let (lt, rt) = match (lt.get() == UNKNOWN, rt.get() == UNKNOWN) {
                (true, true) => return false,
                (true, false) => (rt, rt),
                (false, true) => (lt, lt),
                (false, false) => (lt, rt),
            };
            collations_agree(ctx.catalog, (lt, lc), (rt, rc))
                && builtin_operator_exact(ctx.catalog, op, lt, rt).is_some()
        }
        _ => false,
    }
}

/// Whether running `select` (a subquery, in `scope`'s level: [`FromScope::of_select`]
/// of it) more or fewer times, or over rows it didn't reach before, can
/// never raise an error: its FROM only reads stored tables (joined with ON
/// conditions [`cannot_error`] accepts, in a single FROM item), and every
/// expression of it — target list, WHERE, GROUP BY, HAVING, DISTINCT ON,
/// ORDER BY — is one [`cannot_error`] accepts; LIMIT / OFFSET are
/// non-negative integer constants. No WITH, window or set operation.
pub(crate) fn error_free_select(ctx: &Ctx<'_>, select: &SelectStmt, scope: &FromScope<'_>) -> bool {
    use typedpg_pg_query::protobuf::a_const::Val;
    if select.op != typedpg_pg_query::protobuf::SetOperation::SetopNone as i32
        || select.with_clause.is_some()
        || !select.window_clause.is_empty()
        || !select.values_lists.is_empty()
        || !select.locking_clause.is_empty()
    {
        return false;
    }
    let ctes = cte_names(ctx);
    let mut quals = Vec::new();
    if !select
        .from_clause
        .iter()
        .all(|f| reads_stored_tables(ctx, &ctes, f, &mut quals))
        || (!quals.is_empty() && select.from_clause.len() != 1)
    {
        return false;
    }
    let ok = |n: &Node| cannot_error(ctx, n, scope);
    let limit_ok = |n: Option<&Node>| match n.and_then(|n| n.node.as_ref()) {
        None => true,
        Some(N::AConst(c)) => c.isnull || matches!(&c.val, Some(Val::Ival(i)) if i.ival >= 0),
        _ => false,
    };
    // An ON condition sees its own join's items and the levels around the
    // subquery's, not the subquery's other FROM items.
    quals.into_iter().all(|(q, join)| {
        let sides: Vec<Node> = [&join.larg, &join.rarg]
            .into_iter()
            .flatten()
            .map(|n| (**n).clone())
            .collect();
        cannot_error(ctx, q, &FromScope::of_items(ctx, &sides, scope.parent))
    }) && select.target_list.iter().all(|t| match t.node.as_ref() {
        Some(N::ResTarget(r)) => r.indirection.is_empty() && r.val.as_deref().is_some_and(ok),
        _ => false,
    }) && select.where_clause.as_deref().is_none_or(ok)
        && select.group_clause.iter().all(ok)
        && select.having_clause.as_deref().is_none_or(ok)
        && select
            .distinct_clause
            .iter()
            .all(|d| d.node.is_none() || ok(d))
        && select.sort_clause.iter().all(|s| match s.node.as_ref() {
            Some(N::SortBy(by)) => by.use_op.is_empty() && by.node.as_deref().is_some_and(ok),
            _ => false,
        })
        && limit_ok(select.limit_count.as_deref())
        && limit_ok(select.limit_offset.as_deref())
}

/// Whether a FROM item reads stored tables only (no function, subquery,
/// CTE or view, no USING / NATURAL join, whose implicit equality may
/// coerce), collecting its ON conditions.
fn reads_stored_tables<'n>(
    ctx: &Ctx<'_>,
    ctes: &HashSet<String>,
    from: &'n Node,
    quals: &mut Vec<(&'n Node, &'n typedpg_pg_query::protobuf::JoinExpr)>,
) -> bool {
    match from.node.as_ref() {
        Some(N::RangeVar(rv)) => {
            let schema = (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str());
            !(schema.is_none() && ctes.contains(&rv.relname))
                && ctx
                    .catalog
                    .resolve_table(schema, &rv.relname)
                    .is_some_and(|c| stored_tree(ctx.catalog, c.oid))
        }
        Some(N::JoinExpr(j)) => {
            if j.is_natural || !j.using_clause.is_empty() {
                return false;
            }
            if let Some(q) = j.quals.as_deref() {
                quals.push((q, j));
            }
            [&j.larg, &j.rarg].into_iter().all(|side| {
                side.as_deref()
                    .is_some_and(|s| reads_stored_tables(ctx, ctes, s, quals))
            })
        }
        _ => false,
    }
}

/// `col` with the qualifier PG resolved it by: the alias (or name, with
/// its schema when written) of the table it reads, so that, moved into
/// another level, it can't be captured by a name that level adds. `None`
/// when [`FromScope::resolve`] can't place it.
pub(crate) fn qualified_column(
    ctx: &Ctx<'_>,
    col: &ColumnRef,
    scope: &FromScope<'_>,
) -> Option<ColumnRef> {
    scope.resolve(ctx, col)?;
    let fields: Vec<&str> = col
        .fields
        .iter()
        .map(|f| match f.node.as_ref() {
            Some(N::String(s)) => Some(s.sval.as_str()),
            _ => None,
        })
        .collect::<Option<_>>()?;
    if fields.len() > 1 {
        return Some(col.clone());
    }
    let string = |s: &str| Node {
        node: Some(N::String(typedpg_pg_query::protobuf::String {
            sval: s.to_owned(),
        })),
    };
    let (schema, refname) = scope.binding_item(ctx, fields[0])?;
    let mut out: Vec<Node> = schema.iter().map(|s| string(s)).collect();
    out.push(string(&refname));
    out.push(string(fields[0]));
    Some(ColumnRef {
        fields: out,
        location: col.location,
    })
}

/// Whether the SQL as written uses a name starting with `prefix` (case
/// insensitively): any identifier or string of its parse tree, decoded
/// (`U&"\005F…"` is `_…`), and its raw text.
pub(crate) fn written_names_use(ctx: &Ctx<'_>, prefix: &str) -> bool {
    let sql = &ctx.analyzed.sql;
    if sql.to_ascii_lowercase().contains(prefix) {
        return true;
    }
    let Ok(parsed) = typedpg_pg_query::parse(sql) else {
        return true;
    };
    let hit = |s: &str| s.to_ascii_lowercase().starts_with(prefix);
    let alias_hit = |a: &Option<typedpg_pg_query::protobuf::Alias>| {
        a.as_ref().is_some_and(|a| {
            hit(&a.aliasname)
                || a.colnames
                    .iter()
                    .any(|c| matches!(&c.node, Some(N::String(s)) if hit(&s.sval)))
        })
    };
    parsed.protobuf.nodes().iter().any(|(n, _)| match n {
        NodeRef::String(s) => hit(&s.sval),
        NodeRef::RangeVar(r) => hit(&r.relname) || hit(&r.schemaname) || alias_hit(&r.alias),
        NodeRef::RangeSubselect(r) => alias_hit(&r.alias),
        NodeRef::RangeFunction(r) => alias_hit(&r.alias),
        NodeRef::JoinExpr(j) => alias_hit(&j.alias) || alias_hit(&j.join_using_alias),
        NodeRef::ResTarget(r) => hit(&r.name),
        NodeRef::CommonTableExpr(c) => hit(&c.ctename),
        _ => false,
    })
}

/// The collation of column `attnum` of `rel` (`None`: not collatable),
/// `Err` when the column doesn't exist.
pub(crate) fn column_collation(
    catalog: &PgCatalog,
    rel: PgClassOid,
    attnum: i16,
) -> Result<Option<crate::oid::PgCollationOid>, ()> {
    catalog
        .attributes_of(rel)
        .iter()
        .find(|a| a.attnum == attnum)
        .map(|a| a.attcollation)
        .ok_or(())
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

/// Whether a semi- or anti-join rewrite of the site at `site_location`
/// (the rewritten expression's own node: a NOT IN's `NOT`) can't make
/// something at its level run on rows the site used to filter out first.
///
/// A sublink qual is applied where the planner puts it — on a scan, below
/// a join whose ON (or a pulled-up derived table's, view's or LATERAL
/// item's qual, or a FROM function's argument) runs after it — and the
/// written order wins cost ties. As a join, the site moves above all of
/// those, and anything that can fail there now sees every row (`JOIN u ON
/// t.id = u.k AND 10 / (u.z * t.a) > 1` dividing by zero on the row the
/// NOT IN vetoed). The same holds for any level the site's query is pulled
/// up into. So the site qualifies in two places only, of the top-level
/// statement (the outermost SELECT, not a set operation, or the top-level
/// UPDATE / DELETE), and not inside any sublink or subquery there:
///
/// - the WHERE of a SELECT without any locking clause (`FOR UPDATE`, `FOR
///   NO KEY UPDATE`, `FOR SHARE`, `FOR KEY SHARE`, at any level), when
///   every FROM item of the level is a stored table or a join of them (no
///   subquery, function, LATERAL, VALUES, CTE, view, USING / NATURAL
///   join), every JOIN ON condition passes [`cannot_error`] in its own
///   join's scope, and [`siblings_cannot_error`] holds for the WHERE;
/// - its target list (a SELECT's, locking or not, or an UPDATE's SET
///   values): evaluated only on the rows every qual let through, so
///   nothing is reordered around it, whatever the FROM. A correlated NOT
///   EXISTS there still gains an index probe over an unhashed NOT IN's
///   rescans.
///
/// Never an UPDATE's, DELETE's (or MERGE's) WHERE, nor a locking SELECT's:
/// in READ COMMITTED a row a concurrent transaction updated is rechecked
/// by EvalPlanQual, which re-evaluates a SubPlan qual on the new version
/// but re-joins the row with the anti-join's inner side as fetched — it
/// doesn't exclude it. With `t(id, v)` = (1, 0), (5, 0), `bn(id)` = (2),
/// and a concurrent `UPDATE t SET id = 2 WHERE id = 1` committing while
/// `UPDATE t SET v = 7 WHERE id NOT IN (SELECT id FROM bn) RETURNING id`
/// waits, the written statement updates 5 only; `… WHERE NOT EXISTS
/// (SELECT 1 FROM bn WHERE t.id = bn.id)` updates 5 and 2 (PG 18.6).
/// A target-list or SET-value site is a SubPlan in both forms, evaluated
/// per row: the same experiment gives the same rows and values for both.
///
/// Anywhere else — HAVING (where a NOT EXISTS isn't pulled up anyway), a
/// nested query, a CTE — it doesn't.
pub(crate) fn rewrite_site_is_safe(ctx: &Ctx<'_>, site_location: i32) -> bool {
    fn reaches(node: NodeRef<'_>, at: i32) -> bool {
        if node.location() == Some(at) {
            return true;
        }
        if matches!(node, NodeRef::SubLink(_) | NodeRef::SelectStmt(_)) {
            return false;
        }
        node.children().into_iter().any(|c| reaches(c, at))
    }
    let reaches_in = |n: &Node| {
        n.node
            .as_ref()
            .is_some_and(|n| reaches(n.to_ref(), site_location))
    };
    let Some(top) = ctx
        .tree
        .stmts
        .first()
        .and_then(|s| s.stmt.as_deref())
        .and_then(|s| s.node.as_ref())
    else {
        return false;
    };
    let ctes = cte_names(ctx);
    // The WHERE of a level whose FROM is `from`.
    let where_safe = |from: &[Node], where_clause: &Node, scope: &FromScope<'_>| {
        let mut quals = Vec::new();
        from.iter()
            .all(|f| reads_stored_tables(ctx, &ctes, f, &mut quals))
            && quals.into_iter().all(|(q, join)| {
                let sides: Vec<Node> = [&join.larg, &join.rarg]
                    .into_iter()
                    .flatten()
                    .map(|n| (**n).clone())
                    .collect();
                cannot_error(ctx, q, &FromScope::of_items(ctx, &sides, None))
            })
            && siblings_cannot_error(ctx, where_clause, site_location, scope)
    };
    // Any row locking: EvalPlanQual rechecks a locked row it re-fetched.
    let locks = ctx
        .tree
        .nodes()
        .iter()
        .any(|(n, _)| matches!(n, NodeRef::LockingClause(_)));
    match top {
        N::SelectStmt(s) => {
            if s.op != typedpg_pg_query::protobuf::SetOperation::SetopNone as i32 {
                return false;
            }
            if s.target_list.iter().any(reaches_in) {
                return true;
            }
            match s.where_clause.as_deref() {
                Some(w) if !locks && reaches_in(w) => {
                    let scope = FromScope::of_select(ctx, s, None);
                    where_safe(&s.from_clause, w, &scope)
                }
                _ => false,
            }
        }
        // SET values are evaluated per row to update; the WHERE is
        // rechecked by EvalPlanQual.
        N::UpdateStmt(u) => u.target_list.iter().any(reaches_in),
        _ => false,
    }
}

/// Whether everything evaluated next to a rewrite site can't fail: every
/// expression of the site's `clause` (the WHERE, JOIN ON or HAVING it sits
/// in) other than the site itself — the other conjuncts, and the OR / NOT
/// arms around the site — passes [`cannot_error`] in `scope` (the clause's
/// level) and runs only built-in code. `site_location` is the location of
/// the rewritten expression's own node (a NOT IN's `NOT`).
///
/// Cost order can't be relied on to run those siblings after the site.
/// A hashed SubPlan costs one `cpu_operator_cost` per tuple, the same as a
/// built-in function of procost 1 (most of them, `int4div` and `int4in`
/// included), and `order_qual_clauses` sorts the qual list stably by
/// per-tuple cost: on such a tie the written order wins, and a site
/// written first vetoes the row before `10 / a > 1` divides by zero. As a
/// semi- or anti-join the site leaves the qual list, and the sibling runs
/// on every row. An OR evaluates its arms as written, so an arm after the
/// site was guarded by it too. So the siblings must be unable to fail at
/// all, not merely cheap — which refuses `created_at > now() - interval
/// '1 day'` next to a site, among many harmless ones.
///
/// A schema with a user cast function or a user function in a domain
/// CHECK (calls the parse tree doesn't show) refuses every sibling; so does
/// an operator name some extension shares with a built-in one.
///
/// `false` when the site isn't found in `clause`, or is nested in
/// anything but AND / OR / NOT.
pub(crate) fn siblings_cannot_error(
    ctx: &Ctx<'_>,
    clause: &Node,
    site_location: i32,
    scope: &FromScope<'_>,
) -> bool {
    let holds_site = |n: &Node| {
        n.node.as_ref().is_some_and(|n| {
            n.nodes()
                .iter()
                .any(|(n, _)| n.location() == Some(site_location))
        })
    };
    if !holds_site(clause) {
        return false;
    }
    let hidden = hidden_user_calls(ctx.catalog);
    fn check(
        ctx: &Ctx<'_>,
        node: &Node,
        site_location: i32,
        scope: &FromScope<'_>,
        hidden: bool,
        holds_site: &dyn Fn(&Node) -> bool,
    ) -> bool {
        if node
            .node
            .as_ref()
            .is_some_and(|n| n.to_ref().location() == Some(site_location))
        {
            return true;
        }
        if holds_site(node) {
            return match node.node.as_ref() {
                Some(N::BoolExpr(b)) => b
                    .args
                    .iter()
                    .all(|a| check(ctx, a, site_location, scope, hidden, holds_site)),
                _ => false,
            };
        }
        !hidden && cheap(ctx, node) && cannot_error(ctx, node, scope)
    }
    check(ctx, clause, site_location, scope, hidden, &holds_site)
}

/// Whether evaluating `node` runs only built-in code and no sublink: see
/// [`siblings_cannot_error`]. Every same-named candidate counts, being unable to
/// tell which one PG resolves.
fn cheap(ctx: &Ctx<'_>, node: &Node) -> bool {
    let catalog = ctx.catalog;
    let builtin = |oid: u32| oid < FIRST_GENBKI_OBJECT_ID;
    let operator_builtin = |name: &str| {
        catalog
            .pg_operator
            .values()
            .filter(|o| o.oprname == name)
            .all(|o| builtin(o.oid.get()) && o.oprcode.is_none_or(|p| builtin(p.get())))
    };
    let Some(root) = node.node.as_ref() else {
        return true;
    };
    root.nodes().iter().all(|(n, _)| match n {
        NodeRef::SubLink(_) => false,
        NodeRef::FuncCall(f) => name_parts(&f.funcname).is_some_and(|(schema, name)| {
            let candidates = catalog.find_functions(schema, name);
            !candidates.is_empty() && candidates.iter().all(|p| builtin(p.oid.get()))
        }),
        NodeRef::AExpr(e) => match e.kind() {
            AExprKind::AexprBetween
            | AExprKind::AexprNotBetween
            | AExprKind::AexprBetweenSym
            | AExprKind::AexprNotBetweenSym => {
                [">=", "<=", "<", ">"].into_iter().all(operator_builtin)
            }
            _ => name_parts(&e.name).is_some_and(|(_, op)| operator_builtin(op)),
        },
        NodeRef::TypeCast(t) => t
            .type_name
            .as_ref()
            .and_then(|tn| {
                let (schema, name) = name_parts(&tn.names)?;
                catalog.resolve_type_by_name(schema, name)
            })
            .is_some_and(|target| builtin(target.oid.get())),
        _ => true,
    })
}

/// Whether the schema has a user function PG may call where the parse
/// tree shows none: a cast function, or a function a domain's CHECK calls.
fn hidden_user_calls(catalog: &PgCatalog) -> bool {
    const PG_PROC: u32 = 1255;
    const PG_CONSTRAINT: u32 = 2606;
    let user = |p: PgProcOid| p.get() >= FIRST_GENBKI_OBJECT_ID;
    catalog
        .pg_cast
        .values()
        .any(|c| c.castfunc.is_some_and(user))
        || catalog.iter_pg_depend().any(|d| {
            d.classid.get() == PG_CONSTRAINT
                && d.refclassid.get() == PG_PROC
                && user(PgProcOid::from_raw(d.refobjid.get()))
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
