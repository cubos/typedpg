//! The lint oracle's corpus run: lints a corpus of (schema, query) pairs
//! with every finding's fix and claim checked against a real PostgreSQL
//! (see `src/rules/oracle.rs`), collecting the broken ones instead of
//! stopping at the first.
//!
//! The corpus:
//!
//! - the examples of `docs/lint/*.md` (each page's schema and queries);
//! - the queries the analyzer's test suite analyzes, with the migrations
//!   before each, recorded by its `record_corpus` feature into the
//!   directory `$LINT_ORACLE_CORPUS` names (`scripts/run-lint-oracle.sh`
//!   records them first);
//! - `$LINT_ORACLE_ITERS` (default 3000) queries generated over a schema
//!   built to make the rules report: CHECK constraints, enums, domains,
//!   nullable and NOT NULL columns, foreign keys, unique keys, outer joins;
//! - `$LINT_ORACLE_FUZZ_ITERS` (default 3000) queries of the analyzer
//!   fuzzer's generators (`typedpg_analyzer/tests/fuzz/`), over its schema;
//! - `$LINT_ORACLE_GEN_CASES` (default 75) random catalogs of
//!   `oracle_support/generator.rs`, `$LINT_ORACLE_GEN_QUERIES` (default 40)
//!   queries over each: keys of every kind, partitioned and inherited
//!   tables, views, row security; LATERAL, set operations, grouping sets,
//!   window functions, data-modifying CTEs, ON CONFLICT, MERGE….
//!   `LINT_ORACLE_GEN_CASE=<n>[,<m>…]` runs those cases alone (the findings
//!   name them),
//!   `LINT_ORACLE_ONLY=catalogs` these cases alone.
//!
//! `$LINT_ORACLE_SEED` seeds the generators. The generated queries (and the
//! generated schemas) also go through the analyzer's own comparison with
//! PostgreSQL, as its fuzzer's do: an analyzer divergence there is a
//! finding too, but for its documented choices (`deliberate_divergence`).
//! The ones both reject go to `invalid-generated.txt`, the generator's own
//! mistakes.
//!
//! Every rule is on, the ones off by default too. Every SELECT is linted
//! as called with `fetch_all` (so `fetch_all_single_row` reports), and a
//! statement with `RETURNING` also as called with `execute()`
//! (`unused_returning`'s fix). A fix the shape check withheld (an
//! `internal_error` finding: the rule proposed a fix changing the query's
//! columns, parameters or validity) is a broken claim too. A panic — an
//! analyzer divergence the analyzer's own mirror reports while replaying a
//! schema, or analyzing a fixed query — is counted apart.
//!
//! Findings go to `$LINT_ORACLE_OUT` (default `target/lint-oracle-findings`),
//! one file per distinct finding; the test fails when there is any.
//! Run it with `scripts/run-lint-oracle.sh`.
#![cfg(feature = "pg_sanity")]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;

use typedpg_analyzer::PgCatalog;
use typedpg_lint::{Call, CallMethod, Language, LintOptions, lint_query_called};

/// The analyzer fuzzer's generators (`typedpg_analyzer/tests/fuzz/`), over
/// its own schema.
#[allow(dead_code, unused_imports)]
#[path = "oracle_support/fuzz.rs"]
mod fuzz;

/// The generator of random catalogs and queries over them.
#[path = "oracle_support/generator.rs"]
mod generator;

/// One schema and the queries linted against it.
struct Case {
    origin: String,
    ddl: Vec<String>,
    queries: Vec<String>,
    /// Generated: the analyzer's outcome on each query (and on the schema)
    /// is compared with PostgreSQL's too, as the analyzer's fuzzer does.
    checked: bool,
}

#[derive(Default)]
struct Totals {
    queries: u64,
    rejected: u64,
    panics: BTreeMap<String, String>,
    broken: BTreeMap<String, String>,
    /// The analyzer disagreeing with PostgreSQL on a generated query (or
    /// rejecting a generated schema's statement).
    divergences: BTreeMap<String, String>,
    /// Divergences on a query with two simultaneous errors, each side
    /// reporting one (see `multi_error`): tagged and written out, not
    /// failures (CLAUDE.md, "multiple simultaneous errors").
    multi_errors: BTreeMap<String, String>,
    /// Generated queries both the analyzer and PostgreSQL reject: the
    /// generator's own mistakes, written out to tune it.
    invalid: Vec<String>,
    stats: BTreeMap<(String, String), u64>,
}

#[test]
#[ignore = "needs a PostgreSQL (scripts/run-lint-oracle.sh)"]
fn lint_oracle_corpus() {
    // `LINT_ORACLE_ONLY=catalogs` runs the catalog generator's cases alone;
    // `LINT_ORACLE_GEN_CASE=<n>[,<m>…]` only those (to reproduce them).
    let only_catalogs = std::env::var("LINT_ORACLE_ONLY").is_ok_and(|v| v == "catalogs");
    let env = |name: &str, default: usize| -> usize {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let seed: u64 = std::env::var("LINT_ORACLE_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0x5eed);
    let mut cases = Vec::new();
    if !only_catalogs {
        cases = docs_cases();
    }
    let docs = cases.len();
    if !only_catalogs && let Some(dir) = std::env::var_os("LINT_ORACLE_CORPUS") {
        cases.extend(recorded_cases(std::path::Path::new(&dir)));
    }
    let recorded = cases.len() - docs;
    // Generated queries, in chunks of 50 per catalog.
    let iters = if only_catalogs {
        0
    } else {
        env("LINT_ORACLE_ITERS", 3000)
    };
    let mut rng = Rng(seed);
    let generated: Vec<String> = (0..iters).map(|_| generate(&mut rng)).collect();
    for (i, chunk) in generated.chunks(50).enumerate() {
        cases.push(Case {
            origin: format!("generated #{i}"),
            ddl: vec![GENERATED_SCHEMA.to_owned()],
            queries: chunk.to_vec(),
            checked: true,
        });
    }
    // The analyzer fuzzer's statements, narrowing and constraint queries,
    // over its schema.
    let fuzz_iters = if only_catalogs {
        0
    } else {
        env("LINT_ORACLE_FUZZ_ITERS", 3000)
    };
    {
        use fuzz::SeedableRng;
        let mut rng = fuzz::StdRng::seed_from_u64(seed);
        let queries: Vec<String> = (0..fuzz_iters)
            .map(|i| match i % 3 {
                0 => fuzz::gen_statement(&mut rng),
                1 => fuzz::gen_narrowing_select(&mut rng),
                _ => fuzz::gen_constraint_select(&mut rng),
            })
            .collect();
        for (i, chunk) in queries.chunks(50).enumerate() {
            cases.push(Case {
                origin: format!("fuzzer #{i}"),
                ddl: vec![fuzz::SETUP_SQL.to_owned()],
                queries: chunk.to_vec(),
                checked: false,
            });
        }
    }
    // Random catalogs (`oracle_support/generator.rs`): `$LINT_ORACLE_GEN_CASES`
    // schemas, `$LINT_ORACLE_GEN_QUERIES` queries over each.
    let per_case = env("LINT_ORACLE_GEN_QUERIES", 40);
    let gen_cases: Vec<u64> = match std::env::var("LINT_ORACLE_GEN_CASE") {
        Ok(list) => list
            .split(',')
            .map(|n| {
                n.trim()
                    .parse()
                    .expect("LINT_ORACLE_GEN_CASE lists case numbers")
            })
            .collect(),
        Err(_) => (0..env("LINT_ORACLE_GEN_CASES", 75) as u64).collect(),
    };
    for index in gen_cases {
        let generated = generator::case(seed, index, per_case);
        cases.push(Case {
            origin: format!("catalog seed {seed} case {index}"),
            ddl: generated.ddl,
            queries: generated.queries,
            checked: true,
        });
    }
    let total_queries: usize = cases.iter().map(|c| c.queries.len()).sum();
    eprintln!(
        "lint oracle: {} schemas ({docs} docs pages, {recorded} recorded, {} generated), \
         {total_queries} queries",
        cases.len(),
        cases.len() - docs - recorded
    );

    let threads: usize = std::env::var("LINT_ORACLE_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    let totals = Mutex::new(Totals::default());
    let next = Mutex::new(0usize);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let index = {
                        let mut n = next.lock().unwrap();
                        let i = *n;
                        *n += 1;
                        i
                    };
                    let Some(case) = cases.get(index) else {
                        break;
                    };
                    run_case(case, &totals);
                    if index % 200 == 0 {
                        eprintln!("lint oracle: {index}/{} schemas", cases.len());
                    }
                }
            });
        }
    });
    let totals = totals.into_inner().unwrap();

    let out_dir = std::env::var("LINT_ORACLE_OUT").unwrap_or_else(|_| {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../target/lint-oracle-findings"
        )
        .to_owned()
    });
    let _ = std::fs::remove_dir_all(&out_dir);
    std::fs::create_dir_all(&out_dir).unwrap();
    for (i, (key, message)) in totals.broken.iter().enumerate() {
        std::fs::write(
            format!("{out_dir}/finding-{i:03}.txt"),
            format!("{key}\n\n{message}\n"),
        )
        .unwrap();
    }
    for (i, (key, message)) in totals.panics.iter().enumerate() {
        std::fs::write(
            format!("{out_dir}/panic-{i:03}.txt"),
            format!("{key}\n\n{message}\n"),
        )
        .unwrap();
    }
    for (i, (key, message)) in totals.divergences.iter().enumerate() {
        std::fs::write(
            format!("{out_dir}/divergence-{i:03}.txt"),
            format!("{key}\n\n{message}\n"),
        )
        .unwrap();
    }
    for (i, (key, message)) in totals.multi_errors.iter().enumerate() {
        std::fs::write(
            format!("{out_dir}/multi-error-{i:03}.txt"),
            format!("{key}\n\n{message}\n"),
        )
        .unwrap();
    }
    std::fs::write(
        format!("{out_dir}/invalid-generated.txt"),
        totals.invalid.join("\n"),
    )
    .unwrap();
    eprintln!(
        "lint oracle: {} queries linted ({} the analyzer rejects), {} distinct broken claims, \
         {} distinct panics, {} analyzer divergences on generated queries ({} more on \
         queries with two simultaneous errors, tagged multi-error); written to {out_dir}",
        totals.queries,
        totals.rejected,
        totals.broken.len(),
        totals.panics.len(),
        totals.divergences.len(),
        totals.multi_errors.len()
    );
    let mut by_rule: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for ((rule, what), n) in &totals.stats {
        by_rule.entry(rule).or_default().push(format!("{what}={n}"));
    }
    for (rule, counts) in by_rule {
        eprintln!("  {rule:<32} {}", counts.join("  "));
    }
    for key in totals.broken.keys() {
        eprintln!("  BROKEN {key}");
    }
    for key in totals.divergences.keys() {
        eprintln!("  DIVERGENCE {key}");
    }
    for key in totals.multi_errors.keys() {
        eprintln!("  MULTI-ERROR {key}");
    }
    assert!(
        totals.broken.is_empty() && totals.divergences.is_empty(),
        "the lint oracle found {} broken claims and {} analyzer divergences (see {out_dir})",
        totals.broken.len(),
        totals.divergences.len()
    );
}

fn run_case(case: &Case, totals: &Mutex<Totals>) {
    let built = catch_unwind(AssertUnwindSafe(|| {
        let mut catalog = PgCatalog::new().unwrap();
        let mut rejected = Vec::new();
        for ddl in &case.ddl {
            if let Err(e) = catalog.apply_sql(ddl)
                && case.checked
            {
                rejected.push(format!("{e}\nstatement:\n{ddl}"));
            }
        }
        (catalog, rejected)
    }));
    let built = match built {
        Ok((catalog, rejected)) => {
            // A generated schema is valid: the analyzer rejecting one of
            // its statements is a divergence (or a generator bug).
            let mut t = totals.lock().unwrap();
            for message in rejected {
                t.divergences.insert(
                    format!("schema of {}: {}", case.origin, first_line(&message)),
                    format!("{message}\nschema:\n{};", case.ddl.join(";\n")),
                );
            }
            Ok(catalog)
        }
        Err(panic) => Err(panic),
    };
    let catalog = match built {
        Ok(catalog) => catalog,
        Err(panic) => {
            let message = panic_message(&panic);
            totals.lock().unwrap().panics.insert(
                format!("schema of {}: {}", case.origin, first_line(&message)),
                message,
            );
            return;
        }
    };
    // Every rule, the ones off by default too.
    let options = {
        let mut config = typedpg_core::config::LintConfig::default();
        for rule in typedpg_lint::Rule::ALL {
            if rule.default_level().is_none() {
                config.levels.insert(
                    rule.name().to_owned(),
                    typedpg_core::config::LintLevel::Info,
                );
            }
        }
        LintOptions::from_config(&config)
    };
    let fetch_all = [Call {
        method: CallMethod::FetchAll,
        name: "fetch_all".to_owned(),
        language: Language::Rust,
        stringified: Vec::new(),
    }];
    let execute = [Call {
        method: CallMethod::Execute,
        name: "execute".to_owned(),
        language: Language::Rust,
        stringified: Vec::new(),
    }];
    for query in &case.queries {
        if case.checked {
            // The analyzer's outcome against PostgreSQL's: acceptance,
            // error, columns, nullability over the seeded rows.
            let checked = catch_unwind(AssertUnwindSafe(|| catalog.analyze_checked(query)));
            let mut t = totals.lock().unwrap();
            match checked {
                Ok((_, Some(divergence))) if deliberate_divergence(&divergence) => {}
                Ok((_, Some(divergence))) if multi_error(&divergence) => {
                    let key = format!("{}: {}", case.origin, first_line(&divergence.message));
                    t.multi_errors.entry(key).or_insert(format!(
                        "{}\nschema:\n{};",
                        divergence.message,
                        case.ddl.join(";\n")
                    ));
                }
                Ok((_, Some(divergence))) => {
                    let key = format!("{}: {}", case.origin, first_line(&divergence.message));
                    t.divergences.entry(key).or_insert(format!(
                        "{}\nschema:\n{};",
                        divergence.message,
                        case.ddl.join(";\n")
                    ));
                }
                Ok((Err(e), None)) => t.invalid.push(format!("{e}\n{query}\n")),
                Ok(_) => {}
                Err(panic) => {
                    let message = panic_message(&panic);
                    t.panics.insert(
                        format!("{}: {}", case.origin, first_line(&message)),
                        format!("{message}\nquery:\n{query}"),
                    );
                    return;
                }
            }
        }
        let upper = query.to_ascii_uppercase();
        let mut calls: Vec<&[Call]> = vec![&fetch_all];
        if upper.contains("RETURNING") {
            calls.push(&execute);
        }
        for calls in calls {
            let run = catch_unwind(AssertUnwindSafe(|| {
                typedpg_lint::oracle::collect(|| {
                    lint_query_called(&catalog, query, calls, &options).ok()
                })
            }));
            let stats = typedpg_lint::oracle::take_stats();
            let mut t = totals.lock().unwrap();
            for (key, n) in stats {
                *t.stats.entry(key).or_default() += n;
            }
            match run {
                Ok((findings, mut broken)) => {
                    t.queries += 1;
                    let Some(findings) = findings else {
                        t.rejected += 1;
                        continue;
                    };
                    // How often each rule reports, rules without a fix or
                    // a claim included: which ones the corpus exercises.
                    for finding in &findings {
                        let finding = &finding.finding;
                        *t.stats
                            .entry((finding.rule.name().to_owned(), "reported".to_owned()))
                            .or_default() += 1;
                        // A fix `fix_keeps_shape` withheld: the rule
                        // proposed a fix changing the query's shape.
                        if finding.rule == typedpg_lint::Rule::InternalError {
                            broken.push(format!(
                                "lint oracle: {}\nquery:\n---\n{query}\n---",
                                finding.message
                            ));
                        }
                    }
                    for message in broken {
                        let key = first_line(&message).to_owned();
                        let key = format!("{key} [{}]", case.origin);
                        t.broken.entry(key).or_insert_with(|| {
                            format!("{message}\nschema:\n{};", case.ddl.join(";\n"))
                        });
                    }
                }
                Err(panic) => {
                    let message = panic_message(&panic);
                    t.panics.insert(
                        format!("{}: {}", case.origin, first_line(&message)),
                        format!("{message}\nquery:\n{query}"),
                    );
                    // The mirror's mutex may be poisoned now.
                    return;
                }
            }
        }
    }
}

/// A divergence that is the analyzer's documented choice, or noise, not a
/// bug (as the analyzer's fuzzer judges, `deliberate_divergence` in
/// `typedpg_analyzer/tests/fuzz/main.rs`):
///
/// - a literal NULL written into a NOT NULL column (or domain) is rejected
///   in UPDATE and MERGE too, although PG fails only the executions that
///   touch a row (CLAUDE.md, "Errors that every execution raises");
/// - that NULL next to another error (an ambiguous column, a FULL JOIN
///   PG can't plan): two errors, and PG reports the other;
/// - the execute fallback (all-NULL parameters) failing on another
///   constraint than the analyzer reports: a statement writing several
///   NULLs or values out of a partition's bounds breaks several
///   constraints at once, and which one PG reports first is its order of
///   checks (CLAUDE.md, "multiple simultaneous errors").
fn deliberate_divergence(div: &typedpg_analyzer::Divergence) -> bool {
    use typedpg_analyzer::DivergenceKind;
    let line = |prefix: &str| {
        div.message
            .lines()
            .find_map(|l| l.trim_start().strip_prefix(prefix).map(str::to_owned))
    };
    match div.kind {
        DivergenceKind::AnalyzeAcceptedExecuted => line("analyzer error: ").is_some_and(|e| {
            e.starts_with("null value in column ")
                || (e.starts_with("domain ") && e.contains("does not allow null values"))
        }),
        DivergenceKind::ErrorPrefix => {
            // Another error next to the NULL the analyzer rejects for
            // its own reason (above): PG reports the other one.
            line("analyzer:").is_some_and(|e| {
                let e = e.trim_start();
                e.starts_with("null value in column ")
                    || (e.starts_with("domain ") && e.contains("does not allow null values"))
            }) || (div.message.contains("(execute fallback)")
                && line("PG (with SQLSTATE):").is_some_and(|e| e.contains("(SQLSTATE 23")))
        }
        _ => false,
    }
}

/// A divergence on a statement with two errors at once, where each side
/// reports one of them:
///
/// - an INSERT … ON CONFLICT whose arbiter is a deferrable constraint fails
///   as its first row is stored (`ExecCheckIndexConstraints`), and a value
///   one of its rows can't hold (a domain's CHECK, a NOT NULL or CHECK
///   constraint) fails as that row is built or stored: the executor reports
///   whichever row comes first, the analyzer the value's error. Which one
///   depends on the row the value is in — the executor's row-by-row order,
///   which the analyzer doesn't model.
fn multi_error(div: &typedpg_analyzer::Divergence) -> bool {
    use typedpg_analyzer::DivergenceKind;
    let line = |prefix: &str| {
        div.message
            .lines()
            .find_map(|l| l.trim_start().strip_prefix(prefix).map(str::to_owned))
    };
    div.kind == DivergenceKind::ErrorPrefix
        && div.message.contains("(execute fallback)")
        && line("PG (expected prefix):").is_some_and(|e| {
            e.trim_start().starts_with(
                "ON CONFLICT does not support deferrable unique constraints/exclusion \
                 constraints as arbiters",
            )
        })
        && line("analyzer:").is_some_and(|e| {
            let e = e.trim_start();
            (e.starts_with("value for domain ") && e.contains("violates check constraint"))
                || e.starts_with("null value in column ")
                || (e.starts_with("new row for relation ")
                    && e.contains("violates check constraint"))
        })
}

fn panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
    panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_else(|| "(no message)".to_owned())
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or_default()
}

// ── docs/lint/*.md ─────────────────────────────────────────────────────────

/// Each page's first `sql` block: its statements that are queries are the
/// queries, the rest the schema.
fn docs_cases() -> Vec<Case> {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../docs/lint");
    let mut out = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "md"))
        .collect();
    entries.sort();
    for path in entries {
        let text = std::fs::read_to_string(&path).unwrap();
        let mut ddl = Vec::new();
        let mut queries = Vec::new();
        for block in text.split("```sql\n").skip(1) {
            let Some((block, _)) = block.split_once("```") else {
                continue;
            };
            let Ok(parsed) = typedpg_pg_query::parse(block) else {
                continue;
            };
            for raw in &parsed.tree.stmts {
                let start = raw.stmt_location as usize;
                let end = if raw.stmt_len == 0 {
                    block.len()
                } else {
                    start + raw.stmt_len as usize
                };
                let stmt = block[start..end].trim().to_owned();
                use typedpg_pg_query::ast::Node as N;
                match raw.stmt.as_ref() {
                    Some(
                        N::SelectStmt(_)
                        | N::InsertStmt(_)
                        | N::UpdateStmt(_)
                        | N::DeleteStmt(_)
                        | N::MergeStmt(_),
                    ) => queries.push(stmt),
                    _ => ddl.push(stmt),
                }
            }
        }
        if !queries.is_empty() {
            out.push(Case {
                origin: format!("docs/lint/{}", path.file_name().unwrap().to_string_lossy()),
                ddl: vec![ddl.join(";\n")],
                queries,
                checked: false,
            });
        }
    }
    out
}

// ── the recorded analyzer corpus ───────────────────────────────────────────

fn recorded_cases(dir: &std::path::Path) -> Vec<Case> {
    let mut schemas: HashMap<String, Vec<String>> = HashMap::new();
    let mut queries: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    files.sort();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        for line in text.lines() {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let Some(id) = value["schema"].as_str() else {
                continue;
            };
            if let Some(ddl) = value["ddl"].as_array() {
                schemas.insert(
                    id.to_owned(),
                    ddl.iter()
                        .filter_map(|d| d.as_str().map(str::to_owned))
                        // A `COPY … FROM STDIN` leaves the mirror's
                        // session waiting for the data.
                        .filter(|d| !d.to_ascii_uppercase().contains("FROM STDIN"))
                        .collect(),
                );
            } else if let Some(query) = value["query"].as_str() {
                queries
                    .entry(id.to_owned())
                    .or_default()
                    .insert(query.to_owned());
            }
        }
    }
    queries
        .into_iter()
        .filter_map(|(id, queries)| {
            Some(Case {
                origin: format!("recorded schema {id}"),
                ddl: schemas.get(&id)?.clone(),
                queries: queries.into_iter().collect(),
                checked: false,
            })
        })
        .collect()
}

// ── the generator ──────────────────────────────────────────────────────────

const GENERATED_SCHEMA: &str = "
CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy');
CREATE DOMAIN posint AS integer CHECK (VALUE > 0);
CREATE TABLE users (
    id bigint PRIMARY KEY,
    name text,
    email text NOT NULL UNIQUE,
    age integer CHECK (age >= 0 AND age < 200),
    status text NOT NULL CHECK (status IN ('active', 'inactive')),
    mood mood,
    score numeric,
    born date
);
CREATE TABLE posts (
    id bigint PRIMARY KEY,
    user_id bigint NOT NULL REFERENCES users,
    editor_id bigint REFERENCES users,
    title text NOT NULL,
    body text,
    views integer NOT NULL DEFAULT 0 CHECK (views >= 0),
    kind text CHECK (kind IN ('a', 'b')),
    rank posint,
    UNIQUE (user_id, title)
);
CREATE TABLE tags (
    post_id bigint NOT NULL REFERENCES posts,
    tag text NOT NULL,
    weight integer,
    PRIMARY KEY (post_id, tag)
);
CREATE TABLE events (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id bigint REFERENCES users,
    kind text NOT NULL,
    note text,
    CHECK (kind <> 'login' OR user_id IS NOT NULL)
);
";

/// A splitmix64 generator: deterministic per seed, no dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

#[derive(Clone, Copy)]
enum Ty {
    Int,
    Text,
    Status,
    Kind,
    Mood,
    Num,
    Date,
}

struct Col {
    name: &'static str,
    ty: Ty,
}

struct Table {
    name: &'static str,
    cols: &'static [Col],
}

const fn col(name: &'static str, ty: Ty) -> Col {
    Col { name, ty }
}

const TABLES: &[Table] = &[
    Table {
        name: "users",
        cols: &[
            col("id", Ty::Int),
            col("name", Ty::Text),
            col("email", Ty::Text),
            col("age", Ty::Int),
            col("status", Ty::Status),
            col("mood", Ty::Mood),
            col("score", Ty::Num),
            col("born", Ty::Date),
        ],
    },
    Table {
        name: "posts",
        cols: &[
            col("id", Ty::Int),
            col("user_id", Ty::Int),
            col("editor_id", Ty::Int),
            col("title", Ty::Text),
            col("body", Ty::Text),
            col("views", Ty::Int),
            col("kind", Ty::Kind),
            col("rank", Ty::Int),
        ],
    },
    Table {
        name: "tags",
        cols: &[
            col("post_id", Ty::Int),
            col("tag", Ty::Text),
            col("weight", Ty::Int),
        ],
    },
    Table {
        name: "events",
        cols: &[
            col("id", Ty::Int),
            col("user_id", Ty::Int),
            col("kind", Ty::Text),
            col("note", Ty::Text),
        ],
    },
];

/// The joins the schema's foreign keys (and a few others) suggest:
/// (left table, right table, left column, right column).
const JOINS: &[(&str, &str, &str, &str)] = &[
    ("posts", "users", "user_id", "id"),
    ("posts", "users", "editor_id", "id"),
    ("tags", "posts", "post_id", "id"),
    ("events", "users", "user_id", "id"),
    ("users", "posts", "id", "user_id"),
    ("users", "events", "id", "user_id"),
    ("posts", "tags", "id", "post_id"),
    ("users", "posts", "id", "editor_id"),
];

struct From {
    /// (alias, table) in scope.
    items: Vec<(String, &'static Table)>,
    text: String,
}

fn table(name: &str) -> &'static Table {
    TABLES.iter().find(|t| t.name == name).unwrap()
}

fn literal(rng: &mut Rng, ty: Ty) -> String {
    match ty {
        Ty::Int => rng
            .pick(&["0", "1", "2", "-1", "199", "200", "5", "100"])
            .to_string(),
        Ty::Text => rng
            .pick(&["'a'", "'b'", "'x'", "''", "'login'"])
            .to_string(),
        Ty::Status => rng
            .pick(&["'active'", "'inactive'", "'actve'", "'x'"])
            .to_string(),
        Ty::Kind => rng.pick(&["'a'", "'b'", "'c'"]).to_string(),
        Ty::Mood => rng.pick(&["'sad'", "'ok'", "'happy'"]).to_string(),
        Ty::Num => rng.pick(&["0", "1.5", "-2", "0.001"]).to_string(),
        Ty::Date => rng
            .pick(&["'2020-01-01'", "'infinity'", "'1999-12-31'"])
            .to_string(),
    }
}

fn param(rng: &mut Rng, ty: Ty) -> String {
    let name = ["p", "q", "r"][rng.below(3)];
    let cast = match ty {
        Ty::Int => "::int",
        Ty::Text | Ty::Status | Ty::Kind => "::text",
        Ty::Mood => "::mood",
        Ty::Num => "::numeric",
        Ty::Date => "::date",
    };
    format!("${name}_{}{cast}", ty as u8)
}

fn column(rng: &mut Rng, from: &From) -> (String, &'static Col) {
    let (alias, table) = &from.items[rng.below(from.items.len())];
    let col = &table.cols[rng.below(table.cols.len())];
    (format!("{alias}.{}", col.name), col)
}

fn value(rng: &mut Rng, ty: Ty) -> String {
    if rng.chance(15) {
        param(rng, ty)
    } else {
        literal(rng, ty)
    }
}

fn compatible(a: Ty, b: Ty) -> bool {
    matches!(
        (a, b),
        (Ty::Int, Ty::Int)
            | (
                Ty::Text | Ty::Status | Ty::Kind,
                Ty::Text | Ty::Status | Ty::Kind
            )
            | (Ty::Mood, Ty::Mood)
            | (Ty::Num, Ty::Num | Ty::Int)
            | (Ty::Int, Ty::Num)
            | (Ty::Date, Ty::Date)
    )
}

fn predicate(rng: &mut Rng, from: &From, depth: u32) -> String {
    let (c, col) = column(rng, from);
    match rng.below(if depth > 1 { 8 } else { 18 }) {
        0 | 1 => {
            let op = *rng.pick(&["=", "<>", "<", ">", "<=", ">="]);
            format!("{c} {op} {}", value(rng, col.ty))
        }
        2 => format!("{c} IS {}NULL", if rng.chance(50) { "NOT " } else { "" }),
        3 => {
            let n = 1 + rng.below(3);
            let items: Vec<String> = (0..n).map(|_| literal(rng, col.ty)).collect();
            format!(
                "{c} {}IN ({})",
                if rng.chance(30) { "NOT " } else { "" },
                items.join(", ")
            )
        }
        4 => {
            let (d, other) = column(rng, from);
            if compatible(col.ty, other.ty) {
                let op = *rng.pick(&["=", "<>", "<", ">="]);
                format!("{c} {op} {d}")
            } else {
                format!("{c} = {c}")
            }
        }
        5 => format!("{c} = {c}"),
        6 => {
            let t = rng.pick(TABLES);
            let inner_col = &t.cols[rng.below(t.cols.len())];
            let corr = if compatible(inner_col.ty, col.ty) && rng.chance(60) {
                format!(" WHERE s.{} = {c}", inner_col.name)
            } else {
                String::new()
            };
            let body = match rng.below(3) {
                0 => format!("SELECT 1 FROM {} s{corr}", t.name),
                1 => format!("SELECT count(*) FROM {} s{corr}", t.name),
                _ => format!("SELECT 1 FROM {} s{corr} LIMIT 1", t.name),
            };
            format!(
                "{}EXISTS ({body})",
                if rng.chance(30) { "NOT " } else { "" }
            )
        }
        7 => {
            let a = predicate(rng, from, depth + 1);
            let b = predicate(rng, from, depth + 1);
            format!("({a} {} {b})", rng.pick(&["AND", "OR"]))
        }
        8 => format!("NOT ({})", predicate(rng, from, depth + 1)),
        9 => {
            let a = predicate(rng, from, depth + 1);
            let b = predicate(rng, from, depth + 1);
            format!("{a} AND {b}")
        }
        10 => format!(
            "COALESCE({c}, {}) = {}",
            value(rng, col.ty),
            literal(rng, col.ty)
        ),
        12 => format!(
            "{c} {}BETWEEN {} AND {}",
            if rng.chance(30) { "NOT " } else { "" },
            value(rng, col.ty),
            literal(rng, col.ty)
        ),
        13 => format!(
            "{c} IS {}DISTINCT FROM {}",
            if rng.chance(50) { "NOT " } else { "" },
            if rng.chance(30) {
                "NULL".to_owned()
            } else {
                value(rng, col.ty)
            }
        ),
        14 => match col.ty {
            Ty::Int | Ty::Num => format!(
                "{c} {} {} {} {}",
                rng.pick(&["+", "-", "*"]),
                rng.pick(&["1", "2", "-1"]),
                rng.pick(&["=", "<", ">", "<>"]),
                literal(rng, col.ty)
            ),
            Ty::Text | Ty::Status | Ty::Kind => format!(
                "{c} {}LIKE {}",
                if rng.chance(30) { "NOT " } else { "" },
                rng.pick(&["'a%'", "'%'", "'x'", "''"])
            ),
            _ => format!("NULLIF({c}, {}) IS NULL", literal(rng, col.ty)),
        },
        15 => {
            // A comparison under an outer join's other side, or a CASE.
            let p = predicate(rng, from, depth + 1);
            format!(
                "CASE WHEN {p} THEN {c} IS NULL ELSE {} END",
                predicate(rng, from, depth + 1)
            )
        }
        16 => {
            let t = rng.pick(TABLES);
            let ic = &t.cols[rng.below(t.cols.len())];
            if compatible(ic.ty, col.ty) {
                format!(
                    "{c} {} (SELECT {}(s.{}) FROM {} s)",
                    rng.pick(&["=", "<", ">="]),
                    rng.pick(&["max", "min", ""]),
                    ic.name,
                    t.name
                )
            } else {
                format!("{c} IS NOT NULL")
            }
        }
        _ => {
            let t = rng.pick(TABLES);
            let inner = t.cols.iter().find(|ic| compatible(ic.ty, col.ty));
            match inner {
                Some(ic) => format!(
                    "{c} {}IN (SELECT s.{} FROM {} s)",
                    if rng.chance(50) { "NOT " } else { "" },
                    ic.name,
                    t.name
                ),
                None => format!("{c} IS NOT NULL"),
            }
        }
    }
}

fn from_clause(rng: &mut Rng) -> From {
    let base = rng.pick(TABLES);
    let mut items = vec![("t0".to_owned(), base)];
    let mut text = format!("{} t0", base.name);
    for i in 1..=rng.below(3) {
        let options: Vec<_> = JOINS
            .iter()
            .filter(|(l, _, _, _)| items.iter().any(|(_, t)| t.name == *l))
            .collect();
        if options.is_empty() {
            break;
        }
        let (l, r, lc, rc) = **rng.pick(&options);
        let left_alias = items
            .iter()
            .filter(|(_, t)| t.name == l)
            .map(|(a, _)| a.clone())
            .next()
            .unwrap();
        let kind = *rng.pick(&["JOIN", "LEFT JOIN", "LEFT JOIN", "RIGHT JOIN", "FULL JOIN"]);
        let alias = format!("t{i}");
        let mut on = format!("{alias}.{rc} = {left_alias}.{lc}");
        if rng.chance(20) {
            let (_, rt) = (alias.clone(), table(r));
            let c = &rt.cols[rng.below(rt.cols.len())];
            on.push_str(&format!(" AND {alias}.{} = {}", c.name, literal(rng, c.ty)));
        }
        text.push_str(&format!(" {kind} {r} {alias} ON {on}"));
        items.push((alias, table(r)));
    }
    From { items, text }
}

fn select_item(rng: &mut Rng, from: &From, grouped: bool) -> String {
    let (c, col) = column(rng, from);
    if grouped {
        return match rng.below(4) {
            0 => format!("count({c})"),
            1 => "count(*)".to_owned(),
            2 => format!("count({})", rng.pick(&["1", "'x'"])),
            _ => format!("max({c})"),
        };
    }
    match rng.below(15) {
        0..=3 => c,
        12 => format!("count({c}) OVER ()"),
        13 => format!("CASE WHEN {c} IS NOT NULL THEN 1 WHEN {c} IS NULL THEN 2 ELSE 3 END"),
        14 => format!("COALESCE({c}, {c}, {})", literal(rng, col.ty)),
        4 => format!("COALESCE({c}, {})", literal(rng, col.ty)),
        5 => format!("{c} IS NULL"),
        6 => {
            let p = predicate(rng, from, 1);
            let else_ = if rng.chance(50) {
                format!(" ELSE {}", literal(rng, Ty::Int))
            } else {
                String::new()
            };
            format!(
                "CASE WHEN {p} THEN 1 WHEN {} THEN 2{else_} END",
                predicate(rng, from, 1)
            )
        }
        7 => format!(
            "CASE {c} WHEN {} THEN 'x' WHEN {} THEN 'y' END",
            literal(rng, col.ty),
            literal(rng, col.ty)
        ),
        8 => {
            let p = predicate(rng, from, 1);
            format!("CASE WHEN {p} THEN true ELSE false END")
        }
        9 => {
            let t = rng.pick(TABLES);
            let ic = &t.cols[rng.below(t.cols.len())];
            let corr = if compatible(ic.ty, col.ty) {
                format!(" WHERE s.{} = {c}", ic.name)
            } else {
                String::new()
            };
            format!("(SELECT s.{} FROM {} s{corr} LIMIT 1)", ic.name, t.name)
        }
        10 => format!(
            "CASE WHEN {c} IS NULL THEN {} ELSE {c} END",
            literal(rng, col.ty)
        ),
        _ => format!("{c}::text"),
    }
}

fn generate_select(rng: &mut Rng) -> String {
    let from = from_clause(rng);
    let grouped = rng.chance(20);
    let mut sql = String::from("SELECT ");
    if !grouped && rng.chance(15) {
        sql.push_str("DISTINCT ");
    }
    let mut items: Vec<String> = Vec::new();
    let mut keys: Vec<String> = Vec::new();
    let pk = format!("t0.{}", from.items[0].1.cols[0].name);
    if grouped {
        if rng.chance(40) {
            keys.push(pk.clone());
        }
        for _ in 0..1 + rng.below(3) {
            let (c, _) = column(rng, &from);
            if !keys.contains(&c) {
                keys.push(c);
            }
        }
        items.extend(keys.iter().cloned());
        for _ in 0..1 + rng.below(2) {
            items.push(select_item(rng, &from, true));
        }
    } else {
        for _ in 0..1 + rng.below(3) {
            items.push(select_item(rng, &from, false));
        }
    }
    sql.push_str(&items.join(", "));
    sql.push_str(&format!(" FROM {}", from.text));
    if rng.chance(80) {
        sql.push_str(&format!(" WHERE {}", predicate(rng, &from, 0)));
    }
    if grouped {
        sql.push_str(&format!(" GROUP BY {}", keys.join(", ")));
        if rng.chance(30) {
            sql.push_str(&format!(" HAVING count(*) > {}", rng.below(3)));
        }
    }
    if rng.chance(40) {
        let n = 1 + rng.below(3);
        let lead = rng.chance(30).then(|| pk.clone());
        let order: Vec<String> = lead
            .into_iter()
            .chain((0..n).map(|_| {
                if rng.chance(40) {
                    format!("{}", 1 + rng.below(items.len()))
                } else if grouped {
                    keys[rng.below(keys.len())].clone()
                } else {
                    column(rng, &from).0
                }
            }))
            .collect();
        sql.push_str(&format!(" ORDER BY {}", order.join(", ")));
    }
    if rng.chance(30) {
        sql.push_str(&format!(" LIMIT {}", rng.pick(&["1", "2", "5"])));
    }
    sql
}

fn generate(rng: &mut Rng) -> String {
    match rng.below(10) {
        0 => {
            // An UPDATE, maybe storing a constant a CHECK rejects.
            let t = rng.pick(TABLES);
            let cols: Vec<&Col> = t.cols.iter().filter(|c| c.name != "id").collect();
            let c = *rng.pick(&cols);
            let from = From {
                items: vec![(t.name.to_owned(), t)],
                text: String::new(),
            };
            let mut sql = format!("UPDATE {} SET {} = {}", t.name, c.name, value(rng, c.ty));
            if rng.chance(20) {
                // Another column assigned itself (the same one twice is an error).
                let other = cols.iter().find(|o| o.name != c.name).unwrap_or(&c);
                sql.push_str(&format!(", {0} = {0}", other.name));
            }
            sql.push_str(&format!(" WHERE {}", predicate(rng, &from, 1)));
            if rng.chance(50) {
                sql.push_str(&format!(" RETURNING {}", t.cols[0].name));
            }
            sql
        }
        1 => {
            let t = rng.pick(TABLES);
            let from = From {
                items: vec![(t.name.to_owned(), t)],
                text: String::new(),
            };
            let mut sql = format!("DELETE FROM {} WHERE {}", t.name, predicate(rng, &from, 1));
            if rng.chance(50) {
                sql.push_str(&format!(" RETURNING {}", t.cols[0].name));
            }
            sql
        }
        2 => {
            let id = rng.below(1000) + 10;
            let status = literal(rng, Ty::Status);
            let age = literal(rng, Ty::Int);
            let mut sql = format!(
                "INSERT INTO users (id, email, status, age) VALUES ({id}, 'e{id}', {status}, {age})"
            );
            if rng.chance(40) {
                sql.push_str(&format!(
                    " ON CONFLICT (id) DO UPDATE SET status = {}",
                    literal(rng, Ty::Status)
                ));
            }
            if rng.chance(50) {
                sql.push_str(" RETURNING id");
            }
            sql
        }
        3 => {
            let inner = generate_select(rng);
            format!("WITH w AS ({inner}), unused AS (SELECT 1) SELECT * FROM w")
        }
        _ => generate_select(rng),
    }
}
