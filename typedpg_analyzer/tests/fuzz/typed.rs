use super::*;

// ──────────────────────────────────────────────────────────────────────────
// Strategy 3 — catalog-mined, type-directed generation (valid by construction).
//
// Build a "result type → producers" index from the *live* catalog — every
// builtin (and user) function/operator, not a hardcoded list — then generate
// an expression of a requested type by picking a producer and recursing on its
// declared argument types. Every sub-expression has exactly the type its
// parent expects, so the whole query type-checks; that is what lets the oracle
// compare *column types* (the highest-value divergence on valid queries), not
// just error wording. Leaves are NOT-NULL columns when available (to exercise
// nullability) else a typed `NULL::T` / `$pN::T` (param-type coverage).
//
// Concrete-only for now: producers whose result *or* any argument is a
// pseudo-type (polymorphic `anyelement`/`anyarray`, `record`, `cstring`, …) are
// skipped — polymorphic resolution is already covered by the template
// generator, and the concrete surface is the gap. Set-returning, variadic, and
// aggregate/window procs are skipped too (special placement rules).
// ──────────────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub(crate) enum Producer {
    /// `call(arg0, arg1, …)` — `call` is the (possibly schema-qualified) name.
    Func { call: String, args: Vec<PgTypeOid> },
    /// `(left <op> right)` — binary operator, rendered with its bare name
    /// (pg_catalog only, so search-path resolution is unambiguous).
    Op {
        name: String,
        left: PgTypeOid,
        right: PgTypeOid,
    },
}

pub(crate) struct TypedCat {
    /// result type → producers that yield it.
    by_result: std::collections::HashMap<PgTypeOid, Vec<Producer>>,
    /// type → SQL-renderable name (`pg_catalog.int4`, `public.email`, …).
    type_name: std::collections::HashMap<PgTypeOid, String>,
    /// result types that have at least one producer (for goal selection).
    pub(crate) producible: Vec<PgTypeOid>,
    /// user relations: `(relname, [(col, type, not_null)])`.
    pub(crate) relations: Vec<(String, Vec<RelationColumn>)>,
}

/// One column of a user relation: `(name, type, not_null)`.
pub(crate) type RelationColumn = (String, PgTypeOid, bool);

/// Build the type-directed generation index from the live catalog.
pub(crate) fn build_typed_cat(db: &PgCatalog) -> TypedCat {
    let is_pseudo = |t: PgTypeOid| {
        db.type_row(t)
            .map(|r| r.typtype == TypType::Pseudo)
            .unwrap_or(true)
    };
    let mut type_name = std::collections::HashMap::new();
    for ty in db.iter_types() {
        if let Some(ns) = db.namespace_name(ty.typnamespace) {
            type_name.insert(ty.oid, QualifiedName::new(ns, &ty.typname).to_string());
        }
    }

    let mut by_result: std::collections::HashMap<PgTypeOid, Vec<Producer>> =
        std::collections::HashMap::new();

    for p in db.iter_procs() {
        if p.prokind != ProKind::Function || p.proretset || p.provariadic.is_some() {
            continue;
        }
        if is_pseudo(p.prorettype) || p.proargtypes.iter().any(|&a| is_pseudo(a)) {
            continue;
        }
        if p.proargtypes.len() > 4 {
            continue; // keep call sizes (and recursion fan-out) sane
        }
        let ns = match db.namespace_name(p.pronamespace) {
            Some(n @ ("pg_catalog" | "public")) => n,
            _ => continue,
        };
        by_result
            .entry(p.prorettype)
            .or_default()
            .push(Producer::Func {
                call: QualifiedName::new(ns, &p.proname).to_string(),
                args: p.proargtypes.clone(),
            });
    }

    for o in db.iter_operators() {
        let (Some(left), Some(result)) = (o.oprleft, o.oprresult) else {
            continue; // prefix/postfix ops (no left) — skip for v1
        };
        let right = o.oprright;
        if is_pseudo(left) || is_pseudo(right) || is_pseudo(result) {
            continue;
        }
        // Only pg_catalog operators so the bare `a <op> b` rendering resolves
        // unambiguously (user operators would need `OPERATOR(schema.op)`).
        if db.namespace_name(o.oprnamespace) != Some("pg_catalog") {
            continue;
        }
        by_result.entry(result).or_default().push(Producer::Op {
            name: o.oprname.clone(),
            left,
            right,
        });
    }

    let producible = by_result.keys().copied().collect();
    TypedCat {
        by_result,
        type_name,
        producible,
        relations: db.iter_relations(),
    }
}

/// Generate an expression of exactly type `goal`. `depth` bounds recursion;
/// `cols` are the in-scope columns of the chosen relation.
pub(crate) fn gen_typed(
    cat: &TypedCat,
    cols: &[(String, PgTypeOid, bool)],
    goal: PgTypeOid,
    depth: u32,
    rng: &mut StdRng,
    np: &mut u32,
) -> String {
    if depth > 0
        && !rng.random_bool(0.4)
        && let Some(prods) = cat.by_result.get(&goal)
        && !prods.is_empty()
    {
        return match &prods[rng.random_range(0..prods.len())] {
            Producer::Func { call, args } => {
                let a: Vec<String> = args
                    .iter()
                    .map(|&at| gen_typed(cat, cols, at, depth - 1, rng, np))
                    .collect();
                format!("{call}({})", a.join(", "))
            }
            Producer::Op { name, left, right } => format!(
                "({} {} {})",
                gen_typed(cat, cols, *left, depth - 1, rng, np),
                name,
                gen_typed(cat, cols, *right, depth - 1, rng, np),
            ),
        };
    }
    gen_typed_leaf(cat, cols, goal, rng, np)
}

/// A leaf of exactly type `goal`: a NOT-NULL column when one exists (to keep
/// nullability interesting), occasionally a pinned `$pN::T` (param coverage),
/// else a typed `NULL::T` — always a valid value of the goal type.
pub(crate) fn gen_typed_leaf(
    cat: &TypedCat,
    cols: &[(String, PgTypeOid, bool)],
    goal: PgTypeOid,
    rng: &mut StdRng,
    np: &mut u32,
) -> String {
    let matching: Vec<&(String, PgTypeOid, bool)> =
        cols.iter().filter(|(_, t, _)| *t == goal).collect();
    if !matching.is_empty() && rng.random_bool(0.6) {
        return matching[rng.random_range(0..matching.len())].0.clone();
    }
    let tname = match cat.type_name.get(&goal) {
        Some(n) => n.clone(),
        None => return "NULL".to_string(),
    };
    if rng.random_bool(0.15) {
        return format!("(${}::{tname})", next_param(np));
    }
    format!("NULL::{tname}")
}

/// A full type-directed SELECT: 1..4 projections, each an expression of a
/// random producible / column type, over a random relation, with an optional
/// boolean WHERE (also type-directed).
pub(crate) fn gen_typed_select(cat: &TypedCat, rng: &mut StdRng, np: &mut u32) -> Option<String> {
    if cat.relations.is_empty() || cat.producible.is_empty() {
        return None;
    }
    let (tbl, cols) = &cat.relations[rng.random_range(0..cat.relations.len())];

    // Goal types to draw from: the relation's own column types plus the global
    // producible set, so column leaves are reachable and the producer surface
    // is exercised.
    let pick_goal = |rng: &mut StdRng| -> PgTypeOid {
        if !cols.is_empty() && rng.random_bool(0.5) {
            cols[rng.random_range(0..cols.len())].1
        } else {
            cat.producible[rng.random_range(0..cat.producible.len())]
        }
    };

    let n = rng.random_range(1..4);
    let projs: Vec<String> = (0..n)
        .map(|i| {
            let goal = pick_goal(rng);
            format!("{} AS c{i}", gen_typed(cat, cols, goal, 3, rng, np))
        })
        .collect();
    let mut sql = format!("SELECT {} FROM {}", projs.join(", "), tbl);

    // Optional WHERE — a boolean-typed expression (so it's a *valid* filter,
    // exercising nullability/type through predicates rather than syntax).
    if rng.random_bool(0.5) {
        let bool_oid = cat
            .type_name
            .iter()
            .find(|(_, n)| n.as_str() == "pg_catalog.bool")
            .map(|(&o, _)| o);
        if let Some(b) = bool_oid {
            sql.push_str(&format!(" WHERE {}", gen_typed(cat, cols, b, 3, rng, np)));
        }
    }
    Some(sql)
}
