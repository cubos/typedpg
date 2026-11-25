//! The catalog oracle's stored-expression layer: the expressions PG keeps
//! cooked in its catalogs (`pg_node_tree` columns) must mean what the
//! analyzer keeps of them.
//!
//! PG's side is read deparsed (`pg_get_expr`, `pg_get_indexdef`,
//! `pg_get_triggerdef`, `pg_get_partkeydef`, `pg_get_statisticsobjdef_
//! expressions`, a domain's `typdefaultbin`) and parsed back — `VALUE` of a
//! domain CHECK too; the analyzer keeps
//! the parse tree as written. The two trees are compared as PG compares
//! stored expressions, not as text ([`crate::ddl::cooked`]): each is
//! transformed over the relation it belongs to — operators and routines by
//! OID, literals as typed constants, implicit coercions made explicit,
//! casts that change nothing dropped — so `x > '0'` is `(x > 0)` and
//! `a IN (1, 2)` is `(a = ANY (ARRAY[1, 2]))`. Two transformed trees made
//! only of what `cooked` models must be equal. Where either keeps a part
//! as written (a construct `cooked` doesn't model, a constant of a type
//! whose canonical form it doesn't know: `'today'::date` is a date), or
//! doesn't analyze over a relation (a trigger's `WHEN` reads `NEW` /
//! `OLD`, a domain's default none), the two must have the same
//! dependencies — the objects they refer to that aren't built in — and
//! produce the same type. A partition bound is compared as the values the
//! analyzer keeps of it.
//!
//! Queries the analyzer keeps no transformed tree of are compared by what
//! the analyzer makes of PG's deparse: a view's query must resolve as the
//! analyzer's view does ([`views`]), a rule's definition must make the rule
//! the analyzer keeps ([`rules`]). Every deparse is read back as the
//! migrations' session, whose deparse it is, reads it.

use postgres::Client;
use typedpg_pg_query::ast;

use crate::ddl::depend::{self, Reference};
use crate::ddl::tables::check_inherit::StoredExpr;
use crate::oid::{PgClassOid, PgTypeOid};
use crate::pg_catalog::PgCatalog;

/// The schemas whose objects are compared: not another session's
/// temporary schema (the query session's helper functions), nor TOAST's.
const VISIBLE: &str = "(n.nspname !~ '^pg_(toast_)?temp_' OR n.oid = pg_catalog.pg_my_temp_schema()) \
     AND n.nspname <> 'pg_toast'";

/// Every stored expression of the server that the analyzer keeps
/// otherwise, one line each.
pub(crate) fn differences(
    client: &mut Client,
    db: &PgCatalog,
) -> Result<Vec<String>, postgres::Error> {
    // The server deparses in the migrations' session, under its search
    // path and with its temporary schema; between migrations the analyzer
    // resolves names as a new session would. The text is read back as the
    // migrations' session reads it.
    let mut session = db.clone();
    session.in_migration = true;
    session.refresh_search_path();
    expression_differences(client, &session)
}

fn expression_differences(
    client: &mut Client,
    db: &PgCatalog,
) -> Result<Vec<String>, postgres::Error> {
    let mut out = Vec::new();
    defaults(client, db, &mut out)?;
    checks(client, db, &mut out)?;
    indexes(client, db, &mut out)?;
    policies(client, db, &mut out)?;
    triggers(client, db, &mut out)?;
    partitions(client, db, &mut out)?;
    statistics(client, db, &mut out)?;
    domain_defaults(client, db, &mut out)?;
    domain_checks(client, db, &mut out)?;
    views(client, db, &mut out)?;
    rules(client, db, &mut out)?;
    arg_defaults(client, db, &mut out)?;
    Ok(out)
}

/// Routines' parameter defaults (`pg_proc.proargdefaults`, deparsed and
/// parsed back as a list), compared as domains' defaults are: the same
/// tree, or the same objects referred to — over no relation, coerced to
/// the parameter's type (a polymorphic parameter's default keeps its own,
/// `proargdefaulttypes`, compared with `pg_proc`).
fn arg_defaults(
    client: &mut Client,
    db: &PgCatalog,
    out: &mut Vec<String>,
) -> Result<(), postgres::Error> {
    let type_name = |t: PgTypeOid| -> Option<String> {
        let ty = db.pg_type.get(&t)?;
        Some(format!(
            "{}.{}",
            db.namespace_name(ty.typnamespace)?,
            ty.typname
        ))
    };
    for r in client.query(
        &format!(
            "SELECT n.nspname::text, p.proname::text, \
                    ARRAY(SELECT tn.nspname || '.' || t.typname \
                          FROM unnest(p.proargtypes::oid[]) WITH ORDINALITY AS u(typ, i) \
                          JOIN pg_catalog.pg_type t ON t.oid = u.typ \
                          JOIN pg_catalog.pg_namespace tn ON tn.oid = t.typnamespace \
                          ORDER BY u.i), \
                    pg_catalog.pg_get_expr(p.proargdefaults, 0) \
             FROM pg_catalog.pg_proc p \
             JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
             WHERE p.oid >= 16384 AND p.proargdefaults IS NOT NULL AND {VISIBLE}"
        ),
        &[],
    )? {
        let (nsp, name, args, text): (String, String, Vec<String>, String) =
            (r.get(0), r.get(1), r.get(2), r.get(3));
        let what = format!("pg_proc {nsp}.{name}({}) defaults", args.join(", "));
        let ours = db.pg_proc.values().find(|p| {
            p.proname == name
                && db.namespace_name(p.pronamespace) == Some(nsp.as_str())
                && p.proargtypes.len() == args.len()
                && p.proargtypes
                    .iter()
                    .zip(&args)
                    .all(|(&t, a)| type_name(t).as_deref() == Some(a.as_str()))
        });
        let Some(ours) = ours else {
            continue;
        };
        let Some(pg) = parse_exprs(&text) else {
            out.push(format!("{what}: PostgreSQL's `{text}` doesn't parse"));
            continue;
        };
        let defaults = db.routine_defaults.get(&ours.oid);
        let defaults = defaults.map_or(&[][..], |d| d.as_slice());
        if pg.len() != defaults.len() {
            out.push(format!(
                "{what}: {} in PostgreSQL ({text}), {} in the analyzer",
                pg.len(),
                defaults.len()
            ));
            continue;
        }
        for ((pg, ours_default), i) in pg.iter().zip(defaults).zip(0..) {
            let Some(written) = ours_default.expr.as_ref() else {
                out.push(format!("{what}: the analyzer keeps no default {}", i + 1));
                continue;
            };
            let declared = ours.proargtypes.get(ours_default.param).copied();
            let target = match declared {
                Some(t) if crate::polymorphic::is_polymorphic(t) => {
                    ours.proargdefaulttypes.get(i).copied()
                }
                other => other,
            };
            use typedpg_pg_query::Equal;
            if !pg.equal(written)
                && shape(db, None, pg, target) != as_stored(|| shape(db, None, written, target))
            {
                out.push(format!(
                    "{what}: PostgreSQL's default {} is `{text}`, the analyzer's differs \
                     ({written:?})",
                    i + 1
                ));
            }
        }
    }
    Ok(())
}

/// `text`, a comma-separated list of expressions, parsed.
fn parse_exprs(text: &str) -> Option<Vec<ast::Node>> {
    let parsed = typedpg_pg_query::parse(&format!("SELECT {text}")).ok()?;
    let stmt = parsed.tree.stmts.into_iter().next()?.stmt?;
    let ast::Node::SelectStmt(sel) = stmt else {
        return None;
    };
    sel.target_list
        .iter()
        .map(|t| match t.as_ref()? {
            ast::Node::ResTarget(rt) => rt.val.clone(),
            _ => None,
        })
        .collect()
}

/// What the rewriter makes of a rule (see [`crate::ddl::rules::Rule`]),
/// relations by name: its event, whether it is INSTEAD, conditional or
/// returns rows, each action's command, the statements it rewrites and
/// how, and what planning its actions raises. Not whether a value is
/// written as the bare `NULL` literal (`Assign::null`, for typedpg's
/// literal-NULL rule): that is syntax PG doesn't keep — its rule holds a
/// NULL constant either way, and ruleutils prints it typed (`NULL::integer`).
fn rule_summary(db: &PgCatalog, rule: &crate::ddl::rules::Rule) -> String {
    let rel = |relid: PgClassOid| {
        db.pg_class
            .get(&relid)
            .map_or_else(|| format!("{relid:?}"), |c| c.relname.clone())
    };
    let actions: Vec<String> = rule
        .actions
        .iter()
        .map(|(relid, rewrite)| {
            let mut rewrite = rewrite.clone();
            for assign in rewrite
                .actions
                .iter_mut()
                .flat_map(|a| a.assigns.iter_mut())
                .chain(rewrite.on_conflict.iter_mut().flatten().flatten())
            {
                assign.null = false;
            }
            format!("{} {rewrite:?}", rel(*relid))
        })
        .collect();
    let plan_errors: Vec<String> = rule
        .plan_errors
        .iter()
        .map(|e| {
            format!(
                "{:?}: {}",
                e.target.map(|(relid, cmd)| (rel(relid), cmd)),
                e.error
            )
        })
        .collect();
    format!(
        "event {:?}, instead {}, conditional {}, returning {}, actions {} {:?} {actions:?}, \
         plan errors {plan_errors:?}",
        rule.event,
        rule.instead,
        rule.conditional,
        rule.returning,
        rule.has_actions,
        rule.action_cmds
    )
}

/// Rules (`pg_rewrite` but views' `_RETURN`: `ev_qual`, `ev_action`, read
/// through `pg_get_ruledef`). The analyzer keeps what the rewriter makes of
/// a rule ([`crate::ddl::rules::Rule`]), not its statements: PG's rule
/// definition, analyzed by the analyzer as CREATE RULE is, must make the
/// same — the same event, INSTEAD, condition, RETURNING, actions and
/// rewrites, the same planning errors. What its condition and actions
/// refer to is compared as the rule's `pg_depend` rows.
fn rules(
    client: &mut Client,
    db: &PgCatalog,
    out: &mut Vec<String>,
) -> Result<(), postgres::Error> {
    for r in client.query(
        &format!(
            "SELECT n.nspname::text, c.relname::text, r.rulename::text, \
                    pg_catalog.pg_get_ruledef(r.oid) \
             FROM pg_catalog.pg_rewrite r \
             JOIN pg_catalog.pg_class c ON c.oid = r.ev_class \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE r.rulename <> '_RETURN' AND c.oid >= 16384 AND {VISIBLE}"
        ),
        &[],
    )? {
        let (nsp, rel, name, def): (String, String, String, String) =
            (r.get(0), r.get(1), r.get(2), r.get(3));
        let what = format!("pg_rewrite {name} on {}", display(&nsp, &rel));
        let Some(relid) = our_rel(db, &nsp, &rel) else {
            continue;
        };
        let Some(ours) = db
            .rules
            .get(&relid)
            .and_then(|rules| rules.iter().find(|r| r.name == name))
        else {
            continue;
        };
        let stmt = typedpg_pg_query::parse(&def)
            .ok()
            .and_then(|p| p.tree.stmts.into_iter().next())
            .and_then(|s| s.stmt);
        let Some(ast::Node::RuleStmt(stmt)) = stmt else {
            out.push(format!("{what}: PostgreSQL's `{def}` doesn't parse"));
            continue;
        };
        let theirs =
            crate::ddl::rules::analyze_rule(db, relid, &stmt, (&mut Vec::new(), &mut Vec::new()));
        match theirs {
            Ok(theirs) => {
                let (theirs, ours) = (rule_summary(db, &theirs), rule_summary(db, ours));
                if theirs != ours {
                    out.push(format!(
                        "{what}: PostgreSQL's (`{def}`) is {theirs}, the analyzer's {ours}"
                    ));
                }
            }
            Err(e) => out.push(format!(
                "{what}: PostgreSQL's `{def}` is rejected by the analyzer: {e}"
            )),
        }
    }
    Ok(())
}

/// References as a view's are bound (`view_references`), rendered.
fn bound(refs: &[Reference]) -> Vec<String> {
    let mut out: Vec<String> = refs.iter().map(|r| format!("{r:?}")).collect();
    out.sort();
    out.dedup();
    out
}

/// Views' and materialized views' queries (`pg_rewrite.ev_action`, read
/// through `pg_get_viewdef`). The analyzer keeps a view's query as
/// written; PG's query, resolved by the analyzer as CREATE VIEW resolves a
/// query ([`crate::ddl::views::view_shape`]), must produce columns of the
/// view's types, typmods and collations, and make the view updatable as
/// the analyzer's query does (`view_updatability`). What the query binds
/// is compared as the `_RETURN` rule's `pg_depend` rows: a deparse spells
/// the implicit coercions as casts, naming types the written query only
/// implies, so what a re-analysis of it collects isn't the written
/// query's. A deparsed query isn't the query as written, so the trees
/// aren't compared either.
///
/// A view column's NOT NULL is the analyzer's inference, no PG state: what
/// it claims, PG's query must support; and where the analyzer's own query
/// still resolves to what it bound (a rename can leave its text naming
/// nothing, and the view's columns nullable), the two infer alike.
fn views(
    client: &mut Client,
    db: &PgCatalog,
    out: &mut Vec<String>,
) -> Result<(), postgres::Error> {
    for r in client.query(
        &format!(
            "SELECT n.nspname::text, c.relname::text, c.relkind::text, \
                    pg_catalog.pg_get_viewdef(c.oid) \
             FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relkind IN ('v', 'm') AND c.oid >= 16384 AND {VISIBLE}"
        ),
        &[],
    )? {
        let (nsp, rel, relkind, def): (String, String, String, String) =
            (r.get(0), r.get(1), r.get(2), r.get(3));
        let what = format!("pg_rewrite _RETURN of {}", display(&nsp, &rel));
        let Some(relid) = our_rel(db, &nsp, &rel) else {
            continue;
        };
        let pg_query = typedpg_pg_query::parse(&def)
            .ok()
            .and_then(|p| p.tree.stmts.into_iter().next())
            .and_then(|s| s.stmt);
        let Some(pg_query) = pg_query else {
            out.push(format!("{what}: PostgreSQL's `{def}` doesn't parse"));
            continue;
        };
        let (pg_columns, _) = match crate::ddl::views::view_shape(db, &pg_query) {
            Ok(shape) => shape,
            Err(e) => {
                out.push(format!(
                    "{what}: PostgreSQL's `{def}` doesn't analyze in the analyzer: {e}"
                ));
                continue;
            }
        };
        let Some(stored) = db.view_references.get(&relid) else {
            out.push(format!("{what}: the analyzer keeps no query"));
            continue;
        };
        let stored = bound(stored);
        let attrs: Vec<_> = db
            .attributes_of(relid)
            .iter()
            .filter(|a| a.attnum > 0)
            .collect();
        let types = |cols: &[(
            PgTypeOid,
            Option<i32>,
            Option<crate::oid::PgCollationOid>,
            bool,
        )]| { cols.iter().map(|c| (c.0, c.1, c.2)).collect::<Vec<_>>() };
        let ours: Vec<_> = attrs
            .iter()
            .map(|a| (a.atttypid, a.atttypmod, a.attcollation))
            .collect();
        if types(&pg_columns) != ours {
            out.push(format!(
                "{what}: PostgreSQL's query's columns are {:?}, the analyzer's view's {ours:?}",
                types(&pg_columns)
            ));
        }
        let claimed: Vec<bool> = attrs.iter().map(|a| a.attnotnull).collect();
        let supported: Vec<bool> = pg_columns.iter().map(|c| c.3).collect();
        if claimed.iter().zip(&supported).any(|(&c, &s)| c && !s) {
            out.push(format!(
                "{what}: the analyzer infers NOT NULL {claimed:?}, PostgreSQL's query \
                 {supported:?}"
            ));
        }
        if let Some(query) = crate::ddl::views::view_query(db, relid)
            && let Ok((columns, refs)) = crate::ddl::views::view_shape(db, &query)
            && bound(&refs) == stored
        {
            let ours: Vec<bool> = columns.iter().map(|c| c.3).collect();
            if ours != supported {
                out.push(format!(
                    "{what}: the analyzer's query infers NOT NULL {ours:?}, PostgreSQL's \
                     {supported:?}"
                ));
            }
        }
        if relkind == "v"
            && let Some(ours) = db.view_updatability.get(&relid)
        {
            let theirs = crate::ddl::views::view_updatability(db, &pg_query);
            if format!("{theirs:?}") != format!("{ours:?}") {
                out.push(format!(
                    "{what}: updatable as {theirs:?} by PostgreSQL's query, {ours:?} by the \
                     analyzer's"
                ));
            }
        }
    }
    Ok(())
}

/// The analyzer's relation `nspname.relname` (a session's temporary schema
/// is the analyzer's `pg_temp`).
fn our_rel(db: &PgCatalog, nspname: &str, relname: &str) -> Option<PgClassOid> {
    let ns = if nspname.starts_with("pg_temp_") {
        db.temp_namespace?
    } else {
        *db.namespace_by_name.get(nspname)?
    };
    db.class_by_qname.get(&(ns, relname.to_owned())).copied()
}

fn display(nspname: &str, relname: &str) -> String {
    let nsp = if nspname.starts_with("pg_temp_") {
        "pg_temp"
    } else {
        nspname
    };
    typedpg_core::QualifiedName::new(nsp, relname).to_string()
}

/// `text`, an expression, parsed.
fn parse_expr(text: &str) -> Option<ast::Node> {
    let parsed = typedpg_pg_query::parse(&format!("SELECT {text}")).ok()?;
    let stmt = parsed.tree.stmts.into_iter().next()?.stmt?;
    let ast::Node::SelectStmt(sel) = stmt else {
        return None;
    };
    let target = sel.target_list.first()?.clone()?;
    let ast::Node::ResTarget(rt) = target else {
        return None;
    };
    rt.val.clone()
}

/// The objects an expression refers to, resolved, as its dependencies
/// record them — built-in (pinned) objects aside: PG's deparse spells out
/// coercions and constants' types (`NULL::text`, `(0)::bigint`) that the
/// written form leaves implicit.
fn referenced(db: &PgCatalog, refs: &[Reference]) -> Vec<String> {
    let mut out: Vec<String> = depend::reference_addresses(db, refs)
        .into_iter()
        .filter(|a| !depend::is_pinned(*a))
        .map(|a| format!("{:?}", (a.classid, a.objid, a.objsubid)))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// What the weaker comparison looks at: what an expression refers to and
/// the type it produces.
fn shape(
    db: &PgCatalog,
    relid: Option<PgClassOid>,
    expr: &ast::Node,
    target: Option<PgTypeOid>,
) -> (Vec<String>, Option<PgTypeOid>) {
    let refs = depend::coerced_expression_references(db, Some(expr), relid, target);
    let typ = match (target, relid) {
        (Some(t), _) => Some(t),
        (None, Some(rel)) => crate::ddl::volatile::infer_over_relation(db, rel, Some(expr), None)
            .and_then(Result::ok)
            .map(|t| t.type_oid),
        (None, None) => None,
    };
    (referenced(db, &refs), typ)
}

/// Run `f`, an analysis of one of the analyzer's stored expressions, as the
/// analyzer reads them: bound, their unqualified names are `pg_catalog`'s
/// ([`crate::lookup::catalog_first`]).
fn as_stored<R>(f: impl FnOnce() -> R) -> R {
    crate::lookup::catalog_first(f)
}

/// Whether PG's expression `pg` (deparsed and parsed back) and the
/// analyzer's `ours`, both over relation `relid` (and coerced to `target`
/// when given), are the same (see the module's documentation).
fn same(
    db: &PgCatalog,
    relid: PgClassOid,
    pg: &ast::Node,
    ours: &ast::Node,
    target: Option<PgTypeOid>,
) -> bool {
    use crate::ddl::cooked;
    // PG's text names objects as the search path finds them (ruleutils);
    // the analyzer's, bound, names every object outside `pg_catalog`
    // qualified (`as_stored`).
    let cooked_pg = cooked::cook_deparsed(db, relid, Some(pg), target);
    let cooked_ours = cooked::cook(db, relid, Some(ours), target);
    if let (Some(a), Some(b)) = (&cooked_pg, &cooked_ours) {
        // Each as a deparse parses back (PG's is one already).
        if a.round_tripped(db) == b.round_tripped(db) {
            return true;
        }
        if a.fully_cooked() && b.fully_cooked() {
            return false;
        }
    }
    shape(db, Some(relid), pg, target) == as_stored(|| shape(db, Some(relid), ours, target))
}

#[allow(clippy::too_many_arguments)]
fn compare(
    out: &mut Vec<String>,
    what: String,
    db: &PgCatalog,
    relid: PgClassOid,
    pg: &str,
    ours: Option<&ast::Node>,
    target: Option<PgTypeOid>,
) {
    let Some(pg_node) = parse_expr(pg) else {
        out.push(format!("{what}: PostgreSQL's `{pg}` doesn't parse"));
        return;
    };
    match ours {
        None => out.push(format!(
            "{what}: PostgreSQL's is `{pg}`, the analyzer keeps none"
        )),
        Some(ours) if !same(db, relid, &pg_node, ours, target) => out.push(format!(
            "{what}: PostgreSQL's is `{pg}`, the analyzer's differs ({ours:?})"
        )),
        Some(_) => {}
    }
}

fn stored(e: &StoredExpr, db: &PgCatalog) -> Option<ast::Node> {
    match e {
        StoredExpr::Written(node) => (**node).clone(),
        // A serial column's: `nextval('seq'::regclass)` of the sequence
        // its default depends on.
        StoredExpr::Serial(relid, attnum) => {
            use crate::pg_catalog::{DepType, PG_CLASS_RELID, RelKind};
            let class = db
                .pg_depend
                .iter()
                .filter(|d| {
                    d.classid == PG_CLASS_RELID
                        && d.objid.get() == relid.get()
                        && d.objsubid == *attnum
                        && d.refclassid == PG_CLASS_RELID
                        && d.deptype == DepType::Normal
                })
                .find_map(|d| {
                    db.pg_class
                        .get(&PgClassOid::from_raw(d.refobjid.get()))
                        .filter(|c| c.relkind == RelKind::Sequence)
                })?;
            let name = typedpg_core::QualifiedName::new(
                db.namespace_name(class.relnamespace)?,
                class.relname.clone(),
            );
            let literal = format!("'{}'", name.to_string().replace('\'', "''"));
            parse_expr(&format!("nextval({literal}::regclass)"))
        }
    }
}

/// Column defaults and generation expressions (`pg_attrdef`).
fn defaults(
    client: &mut Client,
    db: &PgCatalog,
    out: &mut Vec<String>,
) -> Result<(), postgres::Error> {
    for r in client.query(
        &format!(
            "SELECT n.nspname::text, c.relname::text, a.attname::text, \
                    pg_catalog.pg_get_expr(d.adbin, d.adrelid) \
             FROM pg_catalog.pg_attrdef d \
             JOIN pg_catalog.pg_attribute a ON a.attrelid = d.adrelid AND a.attnum = d.adnum \
             JOIN pg_catalog.pg_class c ON c.oid = d.adrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.oid >= 16384 AND {VISIBLE}"
        ),
        &[],
    )? {
        let (nsp, rel, col, pg): (String, String, String, String) =
            (r.get(0), r.get(1), r.get(2), r.get(3));
        let Some(relid) = our_rel(db, &nsp, &rel) else {
            continue;
        };
        let Some(attr) = db.attribute_by_name(relid, &col) else {
            continue;
        };
        let ours = db
            .attr_default_exprs
            .get(&(relid, attr.attnum))
            .and_then(|e| stored(e, db));
        compare(
            out,
            format!("pg_attrdef {}.{col}", display(&nsp, &rel)),
            db,
            relid,
            &pg,
            ours.as_ref(),
            Some(attr.atttypid),
        );
    }
    Ok(())
}

/// CHECK constraints (`pg_constraint.conbin`).
fn checks(
    client: &mut Client,
    db: &PgCatalog,
    out: &mut Vec<String>,
) -> Result<(), postgres::Error> {
    for r in client.query(
        &format!(
            "SELECT n.nspname::text, c.relname::text, k.conname::text, \
                    pg_catalog.pg_get_expr(k.conbin, k.conrelid) \
             FROM pg_catalog.pg_constraint k \
             JOIN pg_catalog.pg_class c ON c.oid = k.conrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE k.contype = 'c' AND c.oid >= 16384 AND {VISIBLE}"
        ),
        &[],
    )? {
        let (nsp, rel, name, pg): (String, String, String, String) =
            (r.get(0), r.get(1), r.get(2), r.get(3));
        let Some(relid) = our_rel(db, &nsp, &rel) else {
            continue;
        };
        let Some(con) = db
            .pg_constraint
            .values()
            .find(|c| c.conrelid == relid && c.conname == name)
        else {
            continue;
        };
        let ours = db
            .check_defs
            .get(&con.oid)
            .and_then(|d| stored(&d.expr, db));
        compare(
            out,
            format!("pg_constraint {name} on {}", display(&nsp, &rel)),
            db,
            relid,
            &pg,
            ours.as_ref(),
            Some(crate::pg_catalog::oid::BOOL),
        );
    }
    Ok(())
}

/// Index expressions and predicates (`pg_index.indexprs`, `indpred`).
fn indexes(
    client: &mut Client,
    db: &PgCatalog,
    out: &mut Vec<String>,
) -> Result<(), postgres::Error> {
    for r in client.query(
        &format!(
            "SELECT n.nspname::text, t.relname::text, c.relname::text, \
                    ARRAY(SELECT pg_catalog.pg_get_indexdef(i.indexrelid, k.i::int, false) \
                          FROM unnest(i.indkey::int2[]) WITH ORDINALITY k(a, i) \
                          WHERE k.a = 0 ORDER BY k.i), \
                    pg_catalog.pg_get_expr(i.indpred, i.indrelid) \
             FROM pg_catalog.pg_index i \
             JOIN pg_catalog.pg_class c ON c.oid = i.indexrelid \
             JOIN pg_catalog.pg_class t ON t.oid = i.indrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE i.indexrelid >= 16384 AND i.indrelid >= 16384 \
               AND (i.indexprs IS NOT NULL OR i.indpred IS NOT NULL) AND {VISIBLE}"
        ),
        &[],
    )? {
        let (nsp, table, index, exprs, pred): (
            String,
            String,
            String,
            Vec<String>,
            Option<String>,
        ) = (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4));
        let (Some(relid), Some(indexrelid)) =
            (our_rel(db, &nsp, &table), our_rel(db, &nsp, &index))
        else {
            continue;
        };
        let Some(row) = db.pg_index.get(&indexrelid) else {
            continue;
        };
        let what = format!("pg_index {}", display(&nsp, &index));
        if exprs.len() != row.indexprs.len() {
            out.push(format!(
                "{what}: {} expressions in PostgreSQL, {} in the analyzer",
                exprs.len(),
                row.indexprs.len()
            ));
        } else {
            for (i, (pg, ours)) in exprs.iter().zip(&row.indexprs).enumerate() {
                compare(
                    out,
                    format!("{what} expression {}", i + 1),
                    db,
                    relid,
                    pg,
                    ours.ast.as_ref().map(strip_top_collate).as_ref(),
                    None,
                );
            }
        }
        match (pred, row.indpred.as_ref()) {
            (Some(pg), ours) => compare(
                out,
                format!("{what} predicate"),
                db,
                relid,
                &pg,
                ours.and_then(|p| p.ast.as_ref()),
                Some(crate::pg_catalog::oid::BOOL),
            ),
            (None, Some(_)) => out.push(format!("{what}: a predicate only in the analyzer")),
            (None, None) => {}
        }
    }
    Ok(())
}

/// Row security policies' USING and WITH CHECK (`polqual`, `polwithcheck`).
fn policies(
    client: &mut Client,
    db: &PgCatalog,
    out: &mut Vec<String>,
) -> Result<(), postgres::Error> {
    for r in client.query(
        &format!(
            "SELECT n.nspname::text, c.relname::text, p.polname::text, \
                    pg_catalog.pg_get_expr(p.polqual, p.polrelid), \
                    pg_catalog.pg_get_expr(p.polwithcheck, p.polrelid) \
             FROM pg_catalog.pg_policy p \
             JOIN pg_catalog.pg_class c ON c.oid = p.polrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.oid >= 16384 AND {VISIBLE}"
        ),
        &[],
    )? {
        let (nsp, rel, name): (String, String, String) = (r.get(0), r.get(1), r.get(2));
        let quals: [Option<String>; 2] = [r.get(3), r.get(4)];
        let Some(relid) = our_rel(db, &nsp, &rel) else {
            continue;
        };
        let Some(ours) = db.column_deps.policy_quals.get(&(relid, name.clone())) else {
            if quals.iter().any(Option::is_some) {
                out.push(format!(
                    "pg_policy {name} on {}: the analyzer keeps no expressions",
                    display(&nsp, &rel)
                ));
            }
            continue;
        };
        // A clause is read again as the analyzer reads a stored one: its
        // column-notation calls resolve to the routines they found, by OID
        // (`policies::with_indirection_bindings`).
        let ours = [ours.0.as_ref(), ours.1.as_ref()].map(|c| {
            (
                c.and_then(|c| c.expr.clone()),
                c.map(|c| c.refs.indirections.clone()).unwrap_or_default(),
            )
        });
        for ((clause, pg), (ours, bindings)) in ["USING", "WITH CHECK"].iter().zip(quals).zip(ours)
        {
            let what = format!("pg_policy {name} on {} {clause}", display(&nsp, &rel));
            match (pg, ours) {
                (Some(pg), ours) => {
                    crate::ddl::policies::with_indirection_bindings(bindings, || {
                        compare(
                            out,
                            what,
                            db,
                            relid,
                            &pg,
                            ours.as_ref(),
                            Some(crate::pg_catalog::oid::BOOL),
                        )
                    })
                }
                (None, Some(_)) => out.push(format!("{what}: only in the analyzer")),
                (None, None) => {}
            }
        }
    }
    Ok(())
}

/// Triggers' WHEN conditions (`pg_trigger.tgqual`).
fn triggers(
    client: &mut Client,
    db: &PgCatalog,
    out: &mut Vec<String>,
) -> Result<(), postgres::Error> {
    for r in client.query(
        &format!(
            "SELECT n.nspname::text, c.relname::text, t.tgname::text, \
                    pg_catalog.pg_get_triggerdef(t.oid, false) \
             FROM pg_catalog.pg_trigger t \
             JOIN pg_catalog.pg_class c ON c.oid = t.tgrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE NOT t.tgisinternal AND t.tgqual IS NOT NULL AND c.oid >= 16384 AND {VISIBLE}"
        ),
        &[],
    )? {
        let (nsp, rel, name, def): (String, String, String, String) =
            (r.get(0), r.get(1), r.get(2), r.get(3));
        let Some(relid) = our_rel(db, &nsp, &rel) else {
            continue;
        };
        let what = format!("pg_trigger {name} on {} WHEN", display(&nsp, &rel));
        let pg = typedpg_pg_query::parse(&def).ok().and_then(|p| {
            match p.tree.stmts.into_iter().next()?.stmt? {
                ast::Node::CreateTrigStmt(t) => t.when_clause.clone(),
                _ => None,
            }
        });
        let Some(pg) = pg else {
            out.push(format!("{what}: PostgreSQL's `{def}` doesn't parse"));
            continue;
        };
        let ours = db
            .triggers
            .get(&relid)
            .and_then(|ts| ts.iter().find(|t| t.name == name))
            .and_then(|t| t.when.clone());
        let Some(ours) = ours else {
            out.push(format!("{what}: the analyzer keeps none"));
            continue;
        };
        use typedpg_pg_query::Equal;
        if pg.equal(&ours) {
            continue;
        }
        // Over NEW and OLD: what each refers to.
        let refs =
            |e: &ast::Node| referenced(db, &depend::trigger_when_references(db, Some(e), relid));
        if refs(&pg) != as_stored(|| refs(&ours)) {
            out.push(format!(
                "{what}: PostgreSQL's ({def}) refers to other objects than the analyzer's ({ours:?})"
            ));
        }
    }
    Ok(())
}

/// An index or partition key expression without its top-level `COLLATE`:
/// transformIndexStmt / transformPartitionSpec make that the column's
/// collation (`indcollation`, `partcollation`, compared with `pg_index` /
/// `pg_partitioned_table`), the expression stored being the clause's
/// argument — which the analyzer keeps the clause around.
fn strip_top_collate(expr: &ast::Node) -> ast::Node {
    let mut expr = expr;
    while let ast::Node::CollateClause(c) = expr {
        let Some(arg) = c.arg.as_ref() else {
            break;
        };
        expr = arg;
    }
    expr.clone()
}

/// Partition bounds (`relpartbound`) and partition key expressions
/// (`partexprs`).
fn partitions(
    client: &mut Client,
    db: &PgCatalog,
    out: &mut Vec<String>,
) -> Result<(), postgres::Error> {
    for r in client.query(
        &format!(
            "SELECT n.nspname::text, c.relname::text, pn.nspname::text, p.relname::text, \
                    pg_catalog.pg_get_expr(c.relpartbound, c.oid) \
             FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_catalog.pg_inherits i ON i.inhrelid = c.oid \
             JOIN pg_catalog.pg_class p ON p.oid = i.inhparent \
             JOIN pg_catalog.pg_namespace pn ON pn.oid = p.relnamespace \
             WHERE c.relispartition AND c.relkind IN ('r', 'p', 'f') AND c.oid >= 16384 \
               AND {VISIBLE}"
        ),
        &[],
    )? {
        let (nsp, rel, pnsp, prel, bound): (String, String, String, String, String) =
            (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4));
        let (Some(relid), Some(parent)) = (our_rel(db, &nsp, &rel), our_rel(db, &pnsp, &prel))
        else {
            continue;
        };
        let what = format!("relpartbound of {}", display(&nsp, &rel));
        let spec = typedpg_pg_query::parse(&format!("CREATE TABLE x PARTITION OF y {bound}"))
            .ok()
            .and_then(|p| match p.tree.stmts.into_iter().next()?.stmt? {
                ast::Node::CreateStmt(c) => c.partbound.clone(),
                _ => None,
            });
        let Some(spec) = spec else {
            out.push(format!("{what}: PostgreSQL's `{bound}` doesn't parse"));
            continue;
        };
        let pg = crate::ddl::tables::partbound::bound_of(db, parent, &spec);
        match (pg, db.partition_bounds.get(&relid)) {
            (Some(pg), Some(ours)) if crate::ddl::tables::partbound::same_bound(&pg, ours) => {}
            (pg, ours) => out.push(format!(
                "{what}: PostgreSQL's is `{bound}` ({pg:?}), the analyzer's {ours:?}"
            )),
        }
    }
    for r in client.query(
        &format!(
            "SELECT n.nspname::text, c.relname::text, pg_catalog.pg_get_partkeydef(c.oid) \
             FROM pg_catalog.pg_partitioned_table t \
             JOIN pg_catalog.pg_class c ON c.oid = t.partrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE t.partexprs IS NOT NULL AND c.oid >= 16384 AND {VISIBLE}"
        ),
        &[],
    )? {
        let (nsp, rel, def): (String, String, String) = (r.get(0), r.get(1), r.get(2));
        let Some(relid) = our_rel(db, &nsp, &rel) else {
            continue;
        };
        let what = format!("partexprs of {}", display(&nsp, &rel));
        let spec = typedpg_pg_query::parse(&format!("CREATE TABLE x () PARTITION BY {def}"))
            .ok()
            .and_then(|p| match p.tree.stmts.into_iter().next()?.stmt? {
                ast::Node::CreateStmt(c) => c.partspec.clone(),
                _ => None,
            });
        let Some(spec) = spec else {
            out.push(format!("{what}: PostgreSQL's `{def}` doesn't parse"));
            continue;
        };
        let pg: Vec<ast::Node> = spec
            .part_params
            .iter()
            .filter_map(|p| match p.as_ref()? {
                ast::Node::PartitionElem(e) if e.name.is_empty() => {
                    e.expr.as_ref().map(strip_top_collate)
                }
                _ => None,
            })
            .collect();
        let ours: Vec<Option<ast::Node>> = db
            .partition_key_exprs
            .get(&relid)
            .map(|v| {
                v.iter()
                    .map(|e| e.ast.as_ref().map(strip_top_collate))
                    .collect()
            })
            .unwrap_or_default();
        if pg.len() != ours.len() {
            out.push(format!(
                "{what}: {} in PostgreSQL ({def}), {} in the analyzer",
                pg.len(),
                ours.len()
            ));
            continue;
        }
        for (pg, ours) in pg.iter().zip(&ours) {
            match ours {
                Some(ours) if same(db, relid, pg, ours, None) => {}
                _ => out.push(format!(
                    "{what}: PostgreSQL's is {def}, the analyzer's differs ({ours:?})"
                )),
            }
        }
    }
    Ok(())
}

/// Extended statistics expressions (`pg_statistic_ext.stxexprs`).
fn statistics(
    client: &mut Client,
    db: &PgCatalog,
    out: &mut Vec<String>,
) -> Result<(), postgres::Error> {
    for r in client.query(
        &format!(
            "SELECT n.nspname::text, s.stxname::text, \
                    pg_catalog.pg_get_statisticsobjdef_expressions(s.oid) \
             FROM pg_catalog.pg_statistic_ext s \
             JOIN pg_catalog.pg_namespace n ON n.oid = s.stxnamespace \
             WHERE s.stxexprs IS NOT NULL AND s.oid >= 16384 AND {VISIBLE}"
        ),
        &[],
    )? {
        let (nsp, name, exprs): (String, String, Vec<String>) = (r.get(0), r.get(1), r.get(2));
        let ns = if nsp.starts_with("pg_temp_") {
            db.temp_namespace
        } else {
            db.namespace_by_name.get(&nsp).copied()
        };
        let Some(s) = db
            .statistics
            .iter()
            .find(|s| Some(s.namespace) == ns && s.name == name)
        else {
            continue;
        };
        let what = format!("pg_statistic_ext {}", display(&nsp, &name));
        if exprs.len() != s.expressions.len() {
            continue; // the counts are compared with the rows
        }
        for (i, (pg, ours)) in exprs.iter().zip(&s.expressions).enumerate() {
            compare(
                out,
                format!("{what} expression {}", i + 1),
                db,
                s.relid,
                pg,
                stored(ours, db).as_ref(),
                None,
            );
        }
    }
    Ok(())
}

/// Domains' defaults (`typdefaultbin`, deparsed now: `typdefault` is its text
/// as deparsed at CREATE DOMAIN, under that moment's search path).
fn domain_defaults(
    client: &mut Client,
    db: &PgCatalog,
    out: &mut Vec<String>,
) -> Result<(), postgres::Error> {
    for r in client.query(
        &format!(
            "SELECT n.nspname::text, t.typname::text, \
                    pg_catalog.pg_get_expr(t.typdefaultbin, 0) \
             FROM pg_catalog.pg_type t \
             JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace \
             WHERE t.typtype = 'd' AND t.typdefaultbin IS NOT NULL AND t.oid >= 16384 \
               AND {VISIBLE}"
        ),
        &[],
    )? {
        let (nsp, name, pg): (String, String, String) = (r.get(0), r.get(1), r.get(2));
        let ns = if nsp.starts_with("pg_temp_") {
            db.temp_namespace
        } else {
            db.namespace_by_name.get(&nsp).copied()
        };
        let Some(typid) = ns.and_then(|ns| db.type_by_qname.get(&(ns, name.clone())).copied())
        else {
            continue;
        };
        let what = format!("pg_type {} DEFAULT", display(&nsp, &name));
        let Some(pg_node) = parse_expr(&pg) else {
            out.push(format!("{what}: PostgreSQL's `{pg}` doesn't parse"));
            continue;
        };
        let ours = db
            .domain_default_exprs
            .get(&typid)
            .and_then(|e| stored(e, db));
        let Some(ours) = ours else {
            out.push(format!(
                "{what}: PostgreSQL's is `{pg}`, the analyzer keeps none"
            ));
            continue;
        };
        use typedpg_pg_query::Equal;
        if !pg_node.equal(&ours)
            && shape(db, None, &pg_node, Some(typid))
                != as_stored(|| shape(db, None, &ours, Some(typid)))
        {
            out.push(format!(
                "{what}: PostgreSQL's is `{pg}`, the analyzer's differs ({ours:?})"
            ));
        }
    }
    Ok(())
}

/// `proargdefaulttypes` of the server's routines: the type of each
/// parameter default as PG keeps it (`proargdefaults`, deparsed and
/// analyzed) — the export can only assume the parameter's own type, which
/// a polymorphic parameter's default doesn't have.
pub(crate) fn default_types(
    client: &mut Client,
    db: &PgCatalog,
    seed: &mut crate::pg_catalog::PgCatalogSeed,
) -> Result<(), postgres::Error> {
    // A type the analyzer infers is named in the server's rows by its
    // schema and name.
    let nsp_names: std::collections::HashMap<u32, String> = seed
        .pg_namespace
        .iter()
        .map(|n| (n.oid.get(), n.nspname.clone()))
        .collect();
    let server_types: std::collections::HashMap<(String, String), PgTypeOid> = seed
        .pg_type
        .iter()
        .filter_map(|t| {
            let nsp = nsp_names.get(&t.typnamespace.get())?;
            Some(((nsp.clone(), t.typname.clone()), t.oid))
        })
        .collect();
    let server_type = |t: PgTypeOid| -> Option<PgTypeOid> {
        if t.get() < 16384 {
            return Some(t);
        }
        let ty = db.pg_type.get(&t)?;
        let nsp = db.namespace_name(ty.typnamespace)?;
        server_types
            .get(&(nsp.to_owned(), ty.typname.clone()))
            .copied()
    };
    for r in client.query(
        "SELECT oid, pg_catalog.pg_get_expr(proargdefaults, 0) FROM pg_catalog.pg_proc \
         WHERE oid >= 16384 AND proargdefaults IS NOT NULL",
        &[],
    )? {
        let (oid, text): (u32, String) = (r.get(0), r.get(1));
        let Ok(parsed) = typedpg_pg_query::parse(&format!("SELECT {text}")) else {
            continue;
        };
        let Some(ast::Node::SelectStmt(sel)) =
            parsed.tree.stmts.first().and_then(|s| s.stmt.as_ref())
        else {
            continue;
        };
        let types: Option<Vec<PgTypeOid>> = sel
            .target_list
            .iter()
            .map(|t| match t.as_ref()? {
                ast::Node::ResTarget(rt) => server_type(free_type(db, rt.val.as_ref()?)?),
                _ => None,
            })
            .collect();
        let Some(proc) = seed.pg_proc.iter_mut().find(|p| p.oid.get() == oid) else {
            continue;
        };
        if let Some(types) = types
            && types.len() == proc.proargdefaulttypes.len()
        {
            // A default of a parameter of a concrete type was coerced to
            // it (its deparse may not show it: an `oid` constant prints as
            // a bare number); a polymorphic one's keeps its own type.
            let first = proc.proargtypes.len() - types.len();
            proc.proargdefaulttypes = types
                .into_iter()
                .zip(&proc.proargtypes[first..])
                .map(|(t, &declared)| {
                    let polymorphic = crate::polymorphic::is_polymorphic(declared)
                        || declared == PgTypeOid::from_raw(2276);
                    if polymorphic { t } else { declared }
                })
                .collect();
        }
    }
    Ok(())
}

/// The type of an expression over no relation, as the analyzer infers it.
fn free_type(db: &PgCatalog, expr: &ast::Node) -> Option<PgTypeOid> {
    let scope = crate::scope::Scope::default();
    let null_ctx = crate::nullability::NullabilityContext::default();
    let mut params = crate::param_collector::ParamCollector::default();
    let (result, _) = depend::collect(|| {
        crate::expr::infer_expr(
            Some(expr),
            crate::expr::Ctx::new(&scope, &null_ctx, db),
            &mut params,
            crate::expr::TypeGoal::NONE,
        )
    });
    result.ok().map(|t| t.type_oid)
}

/// Domains' CHECK constraints (`pg_constraint.conbin` of a domain), over
/// `VALUE`.
fn domain_checks(
    client: &mut Client,
    db: &PgCatalog,
    out: &mut Vec<String>,
) -> Result<(), postgres::Error> {
    for r in client.query(
        &format!(
            "SELECT n.nspname::text, t.typname::text, c.conname::text, \
                    pg_catalog.pg_get_expr(c.conbin, 0) \
             FROM pg_catalog.pg_constraint c \
             JOIN pg_catalog.pg_type t ON t.oid = c.contypid \
             JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace \
             WHERE c.contype = 'c' AND t.oid >= 16384 AND {VISIBLE}"
        ),
        &[],
    )? {
        let (nsp, name, con, pg): (String, String, String, String) =
            (r.get(0), r.get(1), r.get(2), r.get(3));
        let ns = if nsp.starts_with("pg_temp_") {
            db.temp_namespace
        } else {
            db.namespace_by_name.get(&nsp).copied()
        };
        let Some(typid) = ns.and_then(|ns| db.type_by_qname.get(&(ns, name.clone())).copied())
        else {
            continue;
        };
        let what = format!("pg_constraint {con} on domain {}", display(&nsp, &name));
        let Some(base) = db.pg_type.get(&typid).and_then(|t| t.typbasetype) else {
            continue;
        };
        let ours = db
            .domain_constraints
            .get(&typid)
            .and_then(|cs| cs.iter().find(|c| c.name == con))
            .and_then(|c| c.expr.as_ref())
            .and_then(|e| stored(e, db));
        let Some(pg_node) = parse_expr(&pg) else {
            out.push(format!("{what}: PostgreSQL's `{pg}` doesn't parse"));
            continue;
        };
        let Some(ours) = ours else {
            out.push(format!(
                "{what}: PostgreSQL's is `{pg}`, the analyzer keeps none"
            ));
            continue;
        };
        use typedpg_pg_query::Equal;
        if pg_node.equal(&ours) {
            continue;
        }
        // Over VALUE: the same dependencies.
        let refs = |e: &ast::Node| {
            crate::ddl::types::domain_check_references(db, base, e).map(|r| referenced(db, &r))
        };
        let (a, b) = (refs(&pg_node), as_stored(|| refs(&ours)));
        if a.is_none() || a != b {
            out.push(format!(
                "{what}: PostgreSQL's is `{pg}`, the analyzer's differs ({ours:?})"
            ));
        }
    }
    Ok(())
}
