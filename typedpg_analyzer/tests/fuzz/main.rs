//! Differential fuzzer for the static analyzer.
//!
//! The oracle is [`PgCatalog::analyze_checked`]: it analyzes a query *and*
//! cross-checks the result against a real PostgreSQL server (the same
//! `pg_sanity` mirror the assertion-based tests use), returning a
//! [`Divergence`] instead of panicking. So the fuzzer's only job is to *feed
//! it interesting queries* — any query that makes the analyzer and PG
//! disagree is, by construction, a confirmed bug. Two families of bugs fall
//! out automatically:
//!
//!   * **valid queries with a divergent type** — `Ok`/`Ok` where a column or
//!     parameter type (or count, or name) differs, and
//!   * **invalid queries with a divergent error** — `Err`/`Err` where our
//!     message doesn't start with PG's, plus the `Ok`/`Err` and `Err`/`Ok`
//!     asymmetries, and
//!   * **unsound nullability** — the oracle executes every accepted query
//!     over adversarial rows (NULL in every nullable column, empty tables,
//!     unmatched outer joins; see `pg_sanity/soundness.rs`) and reports a
//!     value the analyzer inferred NOT NULL that comes back NULL.
//!
//! Query generation uses four strategies:
//!   1. a schema-driven **template generator** (`gen_statement`) that emits
//!      SELECTs (with joins, subquery predicates, scalar subqueries, GROUP BY,
//!      DISTINCT [ON], …), set operations, CTEs, and DML
//!      (INSERT/UPDATE/DELETE with RETURNING) — mistypes deliberately allowed,
//!      and `$pN` parameters threaded through typed contexts;
//!   2. an **AST mutator** that parses a seed with `typedpg_pg_query`, tweaks leaf
//!      nodes in the protobuf tree (constants, column refs, operators,
//!      function names, cast targets), and deparses back to SQL — which keeps
//!      the output syntactically plausible while perturbing types; and
//!   3. a **catalog-mined, type-directed generator** (`gen_typed_select`) that
//!      indexes every real function/operator by result type and builds
//!      expressions of a requested type bottom-up, so the query is *valid by
//!      construction* — this is what lets the oracle compare column/param
//!      *types* across the whole builtin surface (see `build_typed_cat`); and
//!   4. a **metamorphic** check (`metamorphic_check`) that wraps a valid query
//!      in a pass-through subquery/CTE and asserts the analyzer reports the same
//!      column type/nullability shape — a self-consistency check that also
//!      catches inference lost (not just made unsound) across the wrap; and
//!   5. a **literal-content probe** (`gen_literal_probe`) that pushes a pool of
//!      valid/invalid literal strings (`'0x1F'`, `'1_'`, `'NaN'`, `'[1,]'`, …)
//!      through every coercion context (cast, operator, COALESCE, INSERT)
//!      against a wide type surface — stressing the analyzer's parse-time
//!      input validation (`literal_input`) in both directions.
//!
//! It also fuzzes the **schema**: each run extends the fixed base with a seeded
//! random schema (`gen_random_schema`) — domains, composites, multi-dim arrays,
//! typmod'd / generated columns, a view, a second schema — applied via the
//! non-panicking `apply_sql_checked`, so a DDL disagreement is itself a finding.
//!
//! Findings are deduplicated by a signature (kind + the message with the SQL
//! body stripped), minimized via structural reductions, written to
//! `$FUZZ_OUT` (default `target/fuzz-findings`), and summarized at the end.
//!
//! Only compiled under the `pg_sanity` feature (it needs the live mirror).
//! Run it via:
//!
//! ```bash
//! scripts/run-pg-sanity.sh -p typedpg_analyzer --run-ignored all fuzz
//! ```
//!
//! Knobs (env vars): `FUZZ_ITERS` (default 2000), `FUZZ_SEED` (default
//! 0xC0FFEE), `FUZZ_OUT` (output dir), `FUZZ_STRICT` (panic at the end if any
//! finding was discovered; `FUZZ_STRICT=single` only for the high-signal ones —
//! single-fault findings and unsound nullability, what CI runs),
//! `FUZZ_DUMP=N` (print N generated statements and a
//! shape histogram, then exit — no DB needed; for inspecting what the template
//! generator produces) and `FUZZ_TYPED_DUMP=N` (print N type-directed samples
//! and the valid-by-construction rate, then exit — needs the DB).
#![cfg(feature = "pg_sanity")]

use std::collections::BTreeMap;

use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use typedpg_analyzer::{
    AnalyzedQuery, Divergence, DivergenceKind, PgCatalog, PgTypeOid, ProKind, QualifiedName,
    TypType, Type,
};
use typedpg_pg_query::NodeEnum;
use typedpg_pg_query::protobuf::{self, a_const};

mod generators;
mod minimize;
mod mutate;
mod report;
mod schema;
mod typed;

use generators::*;
use minimize::*;
use mutate::*;
use report::*;
use schema::*;
use typed::*;

#[test]
#[ignore = "differential fuzzer: long-running, needs Docker PG via run-pg-sanity.sh"]
fn fuzz_analyze_against_pg() {
    let iters: u32 = std::env::var("FUZZ_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2000);
    let seed: u64 = std::env::var("FUZZ_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0xC0FFEE);
    let out_dir = std::env::var("FUZZ_OUT").unwrap_or_else(|_| "target/fuzz-findings".to_string());

    let mut rng = StdRng::seed_from_u64(seed);

    // Diagnostic: `FUZZ_DUMP=N` prints N generated statements (and a shape
    // histogram) without touching the DB, so you can see what the generators
    // actually produce. Returns before any Postgres connection is needed.
    if let Ok(n) = std::env::var("FUZZ_DUMP").map(|s| s.parse::<u32>().unwrap_or(20)) {
        let mut shapes: BTreeMap<&str, u32> = BTreeMap::new();
        for _ in 0..n {
            let sql = gen_statement(&mut rng);
            let shape = if sql.starts_with("INSERT") {
                "INSERT"
            } else if sql.starts_with("UPDATE") {
                "UPDATE"
            } else if sql.starts_with("DELETE") {
                "DELETE"
            } else if sql.starts_with("WITH") {
                "CTE"
            } else if sql.contains(" UNION ")
                || sql.contains(" INTERSECT ")
                || sql.contains(" EXCEPT ")
            {
                "SET-OP"
            } else {
                "SELECT"
            };
            *shapes.entry(shape).or_default() += 1;
            eprintln!("[{shape}] {sql}");
        }
        eprintln!("\n── shape histogram ({n}) ──");
        for (s, c) in &shapes {
            eprintln!("  {s:<8} {c}");
        }
        return;
    }

    let mut db = PgCatalog::new().expect("PgCatalog::new (pg_sanity must spawn the mirror)");
    db.apply_sql(SETUP_SQL)
        .expect("setup schema must apply cleanly");

    // Schema fuzzing: extend the base with a seeded random schema, applied via
    // the non-panicking checked path. A DDL disagreement is itself a finding;
    // on any non-clean outcome we rebuild a fresh base-only catalog so a
    // possible analyzer/mirror desync can't poison the query phase.
    let mut schema_divergence: Option<Divergence> = {
        let random_schema = gen_random_schema(&mut rng);
        let (res, div) = db.apply_sql_checked(&random_schema);
        if res.is_ok() && div.is_none() {
            eprintln!("fuzz: random schema applied cleanly (extends base)");
            None
        } else {
            eprintln!("fuzz: random schema reverted (DDL divergence or invalid); base only");
            db = PgCatalog::new().expect("PgCatalog::new");
            db.apply_sql(SETUP_SQL).expect("base schema reapply");
            div
        }
    };

    // Catalog-mined, type-directed generator index (Strategy 3). Built once
    // from the live catalog so it reflects the full builtin surface plus the
    // (possibly randomized) user schema above.
    let typed_cat = build_typed_cat(&db);
    eprintln!(
        "fuzz: type-directed index — {} producible types, {} relations",
        typed_cat.producible.len(),
        typed_cat.relations.len(),
    );

    // `FUZZ_TYPED_DUMP=N` prints N type-directed samples and reports the
    // valid-by-construction rate (how many analyze cleanly *and* agree with PG),
    // then exits — a quick check that the Strategy-3 generator is healthy.
    if let Ok(n) = std::env::var("FUZZ_TYPED_DUMP").map(|s| s.parse::<u32>().unwrap_or(300)) {
        let (mut okok, mut ok_div, mut rej) = (0u32, 0u32, 0u32);
        for k in 0..n {
            let Some(q) = gen_typed_select(&typed_cat, &mut rng, &mut 0u32) else {
                continue;
            };
            if k < 8 {
                eprintln!("  [sample] {q}");
            }
            match db.analyze_checked(&q) {
                (Ok(_), None) => okok += 1,
                (Ok(_), Some(_)) => ok_div += 1,
                (Err(_), _) => rej += 1,
            }
        }
        eprintln!(
            "FUZZ_TYPED_DUMP({n}): valid+agree={okok} valid+divergent={ok_div} rejected/diverged={rej}"
        );
        return;
    }

    let mut findings: BTreeMap<String, Finding> = BTreeMap::new();
    // A DDL disagreement from the random schema is a high-signal finding in its
    // own right — record it before the query loop.
    if let Some(div) = schema_divergence.take() {
        eprintln!(
            "\n[fuzz schema] NEW {:?}\n  {}",
            div.kind,
            div.message.lines().next().unwrap_or("")
        );
        findings.insert(
            signature(&div),
            Finding {
                kind: div.kind,
                example_sql: "-- random schema DDL (see message)".to_string(),
                message: div.message,
                single_fault: true,
            },
        );
    }
    // Reusable pool of parseable queries to seed the AST mutator (the static
    // seeds plus generated ones that round-tripped through typedpg_pg_query).
    let mut live_seeds: Vec<String> = SEEDS.iter().map(|s| s.to_string()).collect();
    // Pool of queries known to analyze cleanly (Ok, no divergence). The
    // single-fault mode mutates these with exactly one edit, so any resulting
    // divergence has a single root cause — much higher signal than a query
    // riddled with simultaneous errors, where PG and the analyzer merely
    // disagree on which one to report first.
    let mut valid_seeds: Vec<String> = SEEDS
        .iter()
        .filter(|s| {
            let (r, d) = db.analyze_checked(s);
            r.is_ok() && d.is_none()
        })
        // Pools hold the positional (`$N`) form typedpg_pg_query can parse.
        .map(|s| named_to_positional(s))
        .collect();

    let mut metamorphic_checks = 0u32;
    for i in 0..iters {
        let roll = rng.random_range(0..100);
        // `single_fault` marks the high-signal path: exactly one perturbation
        // of a valid query, so any divergence reflects a single error whose
        // report should match PG verbatim. Multi-fault queries can diverge
        // merely on *which* simultaneous error each side reports first — by
        // design we don't treat that ordering as a bug (see CLAUDE.md), so
        // those findings are kept separate and de-prioritized.
        // Literal probes (32..40) are single-coercion by construction, so
        // they share the high-signal tier with the single-edit mutations.
        let single_fault =
            (32..40).contains(&roll) || ((40..80).contains(&roll) && !valid_seeds.is_empty());
        let sql = if roll < 15 {
            // Type-directed generator (Strategy 3): valid by construction, so
            // the oracle can compare column types — the highest-value
            // divergence on accepted queries. Falls back to the template
            // generator if the index is empty.
            match gen_typed_select(&typed_cat, &mut rng, &mut 0u32) {
                Some(q) => q,
                None => gen_statement(&mut rng),
            }
        } else if roll < 32 {
            // Template generator (SELECT / set-op / CTE / VALUES / DML).
            // Pool the *positional* form (`$N`) — typedpg_pg_query can parse it, so
            // parametrized statements feed the mutation pipeline too.
            let q = gen_statement(&mut rng);
            let positional = named_to_positional(&q);
            if typedpg_pg_query::parse(&positional).is_ok() && live_seeds.len() < 400 {
                live_seeds.push(positional);
            }
            q
        } else if roll < 40 {
            // Literal-content probe (Strategy 5): single coercion of a
            // string literal into a typed context. By construction these
            // have at most one fault, so the report must match PG verbatim
            // — treated as high-signal below via `single_fault`'s sibling
            // branch in the recording (probe shapes are single-coercion).
            gen_literal_probe(&mut rng)
        } else if single_fault {
            // Single-fault: one edit over a known-valid base (pooled in
            // positional form; the analyzer wants named `$pN` back).
            let base = valid_seeds[rng.random_range(0..valid_seeds.len())].clone();
            match mutate(&base, &mut rng, 1) {
                Some(m) => positional_to_named(&m),
                None => continue,
            }
        } else {
            // Multi-fault: 1..=3 edits over any parseable seed.
            let base = live_seeds[rng.random_range(0..live_seeds.len())].clone();
            let n_edits = rng.random_range(1..=3);
            match mutate(&base, &mut rng, n_edits) {
                Some(m) => positional_to_named(&m),
                None => continue,
            }
        };

        let (result, divergence) = db.analyze_checked(&sql);
        if let (Ok(q), None) = (&result, &divergence) {
            // Grow the valid-base pool with cleanly-analyzing queries we
            // generate, so single-fault has fresh material beyond the static
            // seeds. Pool the *positional* (`$N`) form: typedpg_pg_query parses it,
            // so parametrized queries join the mutation pipeline — mutating
            // *around* a bare param is exactly the single-fault shape that
            // stresses parameter-type inference.
            let positional = named_to_positional(&sql);
            if valid_seeds.len() < 400 && typedpg_pg_query::parse(&positional).is_ok() {
                valid_seeds.push(positional);
            }
            // Metamorphic self-consistency (Strategy 4): a pass-through wrap
            // must preserve the column type/nullability shape.
            if q.can_run_as_subquery && rng.random_bool(0.35) {
                metamorphic_checks += 1;
                metamorphic_check(&db, &sql, q, &mut rng, &mut findings, i);
            }
        }
        if let Some(div) = divergence.filter(|d| !deliberate_divergence(d)) {
            let sig = signature(&div);
            // Only the first query per signature is minimized + recorded
            // (minimize is expensive — gate it behind the vacant slot).
            if let std::collections::btree_map::Entry::Vacant(slot) = findings.entry(sig) {
                let minimized = minimize(&db, &sql, div.kind);
                eprintln!(
                    "\n[fuzz iter {i}] NEW {}{:?}\n  query: {minimized}\n  {}",
                    if single_fault { "[single-fault] " } else { "" },
                    div.kind,
                    div.message.lines().next().unwrap_or("")
                );
                slot.insert(Finding {
                    kind: div.kind,
                    example_sql: minimized,
                    message: div.message.clone(),
                    single_fault,
                });
            }
        }
    }

    eprintln!("fuzz: ran {metamorphic_checks} metamorphic pass-through checks");
    write_findings(&out_dir, &findings);
    print_summary(iters, &findings);

    match std::env::var("FUZZ_STRICT").as_deref() {
        Ok("single") => {
            let high: Vec<&Finding> = findings.values().filter(|f| f.high_signal()).collect();
            if !high.is_empty() {
                panic!(
                    "fuzz: {} high-signal divergence(s) found (FUZZ_STRICT=single):\n{}\nSee {out_dir}/",
                    high.len(),
                    high.iter()
                        .map(|f| format!("  {:?}: {}", f.kind, f.example_sql))
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
            }
        }
        Ok(_) if !findings.is_empty() => panic!(
            "fuzz: {} unique divergence(s) found (FUZZ_STRICT). See {out_dir}/",
            findings.len()
        ),
        _ => {}
    }
}

/// An `Err`/`Ok` divergence that is the analyzer's documented choice, not a
/// bug: a literal NULL written into a NOT NULL column (or domain) is
/// rejected in UPDATE and MERGE too, although PG fails only the executions
/// that touch a row — and the execute fallback runs against empty tables
/// (see CLAUDE.md, "Errors that every execution raises").
fn deliberate_divergence(div: &Divergence) -> bool {
    div.kind == DivergenceKind::AnalyzeAcceptedExecuted
        && div.message.lines().any(|l| {
            l.starts_with("analyzer error: null value in column ")
                || l.starts_with("analyzer error: domain ")
                    && l.contains("does not allow null values")
        })
}

// ──────────────────────────────────────────────────────────────────────────
// Strategy 4 — metamorphic self-consistency.
//
// Wrapping a subquery-able query in a *pass-through* subquery / CTE
// (`SELECT * FROM (<q>) m`, `WITH m AS (<q>) SELECT * FROM m`) preserves every
// output column's type AND nullability — that's a semantic identity in PG. The
// wire protocol doesn't expose nullability, so the differential oracle can't
// check nullability propagation directly; this metamorphic relation is the only
// way to test it. Any shape change the analyzer reports across the wrap is a
// genuine self-inconsistency (e.g. losing a PK functional-dependency NOT NULL
// through a subquery), independent of whether PG agrees.
// ──────────────────────────────────────────────────────────────────────────

/// The positional (type, nullable) shape of a query's output columns — the
/// invariant a pass-through wrap must preserve.
fn col_shape(q: &AnalyzedQuery) -> Vec<(Type, bool)> {
    q.columns
        .iter()
        .map(|c| (c.pg_type.clone(), c.nullable))
        .collect()
}

/// Check that wrapping `base` (a query that analyzed cleanly and is
/// subquery-able) in a pass-through subquery/CTE preserves its column shape.
/// Records a finding on any divergence.
fn metamorphic_check(
    db: &PgCatalog,
    base_sql: &str,
    base: &AnalyzedQuery,
    rng: &mut StdRng,
    findings: &mut BTreeMap<String, Finding>,
    iter: u32,
) {
    let wrapped = if rng.random_bool(0.5) {
        format!("SELECT * FROM ({base_sql}) AS _m")
    } else {
        format!("WITH _m AS ({base_sql}) SELECT * FROM _m")
    };
    // Only the analyzer's *own* result matters here (self-consistency); the
    // PG cross-check on generated queries is handled by the main loop.
    let (wrapped_result, _) = db.analyze_checked(&wrapped);

    let base_shape = col_shape(base);
    let wrapped_shape = wrapped_result.as_ref().ok().map(col_shape);
    if wrapped_shape.as_ref() == Some(&base_shape) {
        return; // shape preserved — good
    }

    let wrapped_desc = match &wrapped_result {
        Ok(q) => format!("{:?}", col_shape(q)),
        Err(e) => format!("Err({e})"),
    };
    let sig = format!("metamorphic:{base_shape:?}=>{wrapped_desc}");
    if let std::collections::btree_map::Entry::Vacant(slot) = findings.entry(sig) {
        eprintln!(
            "\n[fuzz iter {iter}] NEW [metamorphic] column shape changed under pass-through wrap\n  base: {base_sql}"
        );
        slot.insert(Finding {
            kind: DivergenceKind::ColumnType,
            example_sql: base_sql.to_string(),
            message: format!(
                "metamorphic: wrapping a valid query in a pass-through subquery/CTE changed its \
                 column type/nullability shape (analyzer self-inconsistency).\n\
                 base SQL:\n---\n{base_sql}\n---\nwrapped SQL:\n---\n{wrapped}\n---\n\
                 base shape:    {base_shape:?}\nwrapped shape: {wrapped_desc}"
            ),
            single_fault: true,
        });
    }
}
