//! Strategy 2 — AST mutation: parse a seed with pg_query, tweak leaf
//! nodes, deparse back to SQL.

use super::*;

// ──────────────────────────────────────────────────────────────────────────
// Strategy 2 — AST mutation via pg_query parse → tweak → deparse.
// ──────────────────────────────────────────────────────────────────────────

/// Seed queries for the AST mutator. Named `$pN` params are welcome: the
/// pools store the positional (`$N`) form pg_query understands, and the
/// mutation boundary converts back — mutating *around* a bare param is the
/// canonical single-fault probe of parameter-type inference.
pub(crate) const SEEDS: &[&str] = &[
    "SELECT id, name FROM users WHERE age > 18",
    "SELECT count(*), max(score) FROM users GROUP BY st",
    "SELECT u.name, p.title FROM users u JOIN posts p ON p.user_id = u.id",
    "SELECT COALESCE(age, 0) + 1 FROM users",
    "SELECT upper(name) || '!' FROM users WHERE active",
    "SELECT id FROM users ORDER BY created_at DESC LIMIT 10",
    "SELECT tags[1], nums[2] FROM users",
    "SELECT prefs->>'theme' FROM users WHERE prefs ? 'theme'",
    "SELECT avg(rating) FROM posts WHERE views > 100",
    "SELECT CASE WHEN age < 18 THEN 'minor' ELSE 'adult' END FROM users",
    "SELECT id::text, score::int FROM users",
    "SELECT extract(year FROM created_at) FROM users",
    // Window functions / FILTER / HAVING / set-returning shapes.
    "SELECT row_number() OVER (ORDER BY id) FROM users",
    "SELECT sum(views) OVER (PARTITION BY user_id ORDER BY id) FROM posts",
    "SELECT lag(title) OVER (ORDER BY published_at) FROM posts",
    "SELECT count(*) FILTER (WHERE active) FROM users",
    "SELECT st, count(*) FROM users GROUP BY st HAVING count(*) > 1",
    // Predicate special forms.
    "SELECT id FROM users WHERE age BETWEEN 18 AND 65",
    "SELECT id FROM users WHERE st IN ('draft', 'published')",
    "SELECT id FROM users WHERE id = ANY(ARRAY[1, 2, 3])",
    "SELECT name FROM users WHERE addr IS DISTINCT FROM 'x'",
    "SELECT id FROM posts WHERE published_at IS NOT NULL",
    // Conditional / minmax functions.
    "SELECT GREATEST(age, 5), LEAST(views, 10) FROM users, posts",
    "SELECT NULLIF(score, 0) FROM users",
    "SELECT COALESCE(body, title, 'untitled') FROM posts",
    // DML beyond single-row VALUES.
    "INSERT INTO posts (user_id, title) SELECT id, name FROM users RETURNING id",
    "INSERT INTO users (name) VALUES ('x') ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name",
    // Parametrized shapes — params in operator, function-argument,
    // conditional, special-form, and set-op positions.
    "SELECT id FROM users WHERE id = $p1 AND name = $p2",
    "SELECT age + $p1 FROM users",
    "SELECT name || $p1 FROM users",
    "SELECT coalesce($p1, age) FROM users",
    "SELECT substr(name, $p1) FROM users",
    "SELECT id FROM users WHERE age BETWEEN $p1 AND $p2",
    "SELECT id FROM users WHERE id = ANY($p1)",
    "SELECT CASE WHEN active THEN $p1 ELSE age END FROM users",
    "SELECT $p1 UNION ALL SELECT age FROM users",
    "SELECT prefs -> $p1 FROM users",
    // MERGE / recursive CTE / data-modifying CTE / window frames.
    "MERGE INTO posts p USING users u ON p.user_id = u.id \
     WHEN MATCHED THEN UPDATE SET views = 0 \
     WHEN NOT MATCHED THEN INSERT (user_id, title) VALUES (u.id, u.name)",
    "WITH RECURSIVE r AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM r WHERE n < 5) \
     SELECT * FROM r",
    "WITH moved AS (DELETE FROM posts WHERE views = 0 RETURNING id) \
     SELECT count(*) FROM moved",
    "SELECT sum(views) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM posts",
    "SELECT st, count(*) FROM users GROUP BY ROLLUP (st)",
    "SELECT name COLLATE \"C\" FROM users ORDER BY name COLLATE \"C\"",
    // Derived tables.
    "SELECT v.a FROM (VALUES (1, 'x'), (2, 'y')) AS v(a, b)",
    "SELECT u.name FROM users u WHERE EXISTS (SELECT 1 FROM posts p WHERE p.user_id = u.id)",
];

pub(crate) fn str_node(s: &str) -> protobuf::Node {
    protobuf::Node {
        node: Some(NodeEnum::String(protobuf::String {
            sval: s.to_string(),
        })),
    }
}

/// Parse `seed`, mutate exactly `n_edits` leaf nodes in the protobuf tree, and
/// deparse back to SQL. Returns `None` if parse or deparse fails (some
/// mutations produce un-deparseable trees — we just skip those).
///
/// With `n_edits == 1` over a *valid* seed this is the single-fault mode: the
/// one perturbation is usually the sole reason the query diverges, which
/// isolates genuine wording / coverage bugs from the error-ordering noise that
/// multi-fault queries produce (PG and the analyzer picking different "first"
/// errors).
pub(crate) fn mutate(seed: &str, rng: &mut StdRng, n_edits: u32) -> Option<String> {
    let mut parsed = pg_query::parse(seed).ok()?;

    // SAFETY: `nodes_mut` hands back raw pointers into `parsed`'s owned
    // protobuf tree. They stay valid because `parsed` outlives this block and
    // we don't move it; we mutate through them and then deparse. The pointers
    // carry no lifetime, so the `&mut parsed` borrow ends here and `deparse`
    // (an immutable borrow) is free to read the mutated tree afterwards.
    let nodes: Vec<pg_query::NodeMut> = unsafe { parsed.protobuf.nodes_mut() }
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    if nodes.is_empty() {
        return None;
    }

    for _ in 0..n_edits {
        let idx = rng.random_range(0..nodes.len());
        apply_mutation(nodes[idx], rng);
    }

    parsed.deparse().ok()
}

/// One mutation on a single node, dispatched on its kind.
pub(crate) fn apply_mutation(node: pg_query::NodeMut, rng: &mut StdRng) {
    use pg_query::NodeMut;
    unsafe {
        match node {
            NodeMut::AConst(p) if !p.is_null() => {
                let c = &mut *p;
                c.isnull = false;
                c.val = Some(match rng.random_range(0..5) {
                    0 => a_const::Val::Ival(protobuf::Integer {
                        ival: rng.random_range(-5..1000),
                    }),
                    1 => a_const::Val::Sval(protobuf::String {
                        // Drawn from the literal-probe pool so constant
                        // mutations stress the input-syntax validators too
                        // (radix/underscore ints, float specials, array /
                        // range / json shapes, datetime keywords, …).
                        sval: LITERAL_PROBES[rng.random_range(0..LITERAL_PROBES.len())].to_string(),
                    }),
                    2 => a_const::Val::Boolval(protobuf::Boolean {
                        boolval: rng.random_bool(0.5),
                    }),
                    3 => a_const::Val::Fval(protobuf::Float {
                        fval: "3.14".to_string(),
                    }),
                    _ => {
                        c.isnull = true;
                        c.val = None;
                        return;
                    }
                });
            }
            NodeMut::ColumnRef(p) if !p.is_null() => {
                let cr = &mut *p;
                if let Some(last) = cr.fields.last_mut()
                    && matches!(last.node, Some(NodeEnum::String(_)))
                {
                    let name = ALL_COLUMN_NAMES[rng.random_range(0..ALL_COLUMN_NAMES.len())];
                    *last = str_node(name);
                }
            }
            NodeMut::AExpr(p) if !p.is_null() => {
                let e = &mut *p;
                // kind 0 == AEXPR_OP (a plain binary/unary operator).
                if e.kind == 0 && e.name.len() == 1 {
                    let op = OPERATORS[rng.random_range(0..OPERATORS.len())];
                    e.name[0] = str_node(op);
                }
            }
            NodeMut::FuncCall(p) if !p.is_null() => {
                let fc = &mut *p;
                if !fc.agg_star {
                    let f = FUNCTIONS[rng.random_range(0..FUNCTIONS.len())];
                    fc.funcname = vec![str_node(f)];
                }
            }
            NodeMut::TypeName(p) if !p.is_null() => {
                let tn = &mut *p;
                let t = BASE_TYPE_NAMES[rng.random_range(0..BASE_TYPE_NAMES.len())];
                tn.names = vec![str_node(t)];
                tn.typmods.clear();
                tn.array_bounds.clear();
                tn.typemod = -1;
            }
            _ => {}
        }
    }
}
