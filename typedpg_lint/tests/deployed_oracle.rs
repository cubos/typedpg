//! The `breaks_deployed_queries` oracle's corpus run: generated migration
//! changes × deployed query shapes (and the docs page's example), each
//! linted with the rule and checked by running the deployed queries on a
//! real PostgreSQL before and after the change (see
//! `src/deployed_oracle.rs`).
//!
//! Each change is one case over a base schema: every deployed query, as no
//! role and as the role `app`, against the change's new migration.
//! `$DEPLOYED_ORACLE_FILTER` keeps the changes whose name contains it.
//!
//! Missed breaks and false claims go to `$DEPLOYED_ORACLE_OUT` (default
//! `target/deployed-oracle-findings`), one file per case, and fail the test;
//! unjustified findings are written there too (`unjustified-*.txt`) and
//! counted, not failed on. Run it with `scripts/run-lint-oracle.sh
//! deployed_oracle_corpus`.
#![cfg(feature = "pg_sanity")]

use std::sync::Mutex;

use typedpg_analyzer::PgCatalog;
use typedpg_lint::oracle::{DeployedCase, DeployedReport, run_deployed};
use typedpg_lint::{
    Decoding, DeployedKind, DeployedQuery, Deployment, LintOptions, MigrationFile, MigrationLint,
    NewSchema, breaks_deployed_queries,
};

/// One case: the deployed schema's migrations, the new ones, the deployed
/// queries (`(sql, kind)`), as no role and as `app`.
struct Case {
    name: String,
    old: Vec<String>,
    new: Vec<String>,
    queries: Vec<(String, DeployedKind)>,
}

const BASE: &str = "
CREATE ROLE app;
CREATE TYPE status AS ENUM ('active', 'blocked');
CREATE DOMAIN posint AS integer CHECK (VALUE > 0);
CREATE TABLE accounts (
    id integer PRIMARY KEY,
    email text NOT NULL UNIQUE,
    name varchar(20) NOT NULL,
    nick varchar(10),
    status status NOT NULL DEFAULT 'active',
    kind text NOT NULL DEFAULT 'a' CHECK (kind IN ('a', 'b')),
    balance numeric(8,2) NOT NULL DEFAULT 0,
    age integer CHECK (age >= 0),
    score posint,
    note text,
    created date NOT NULL DEFAULT '2020-01-01'
);
CREATE TABLE posts (
    id integer PRIMARY KEY,
    account_id integer REFERENCES accounts,
    title text NOT NULL,
    body text,
    flag bit(3)
);
CREATE VIEW active_accounts AS
    SELECT id, email, name, balance, status FROM accounts WHERE status = 'active';
CREATE FUNCTION account_name(i integer) RETURNS text LANGUAGE sql STABLE
    AS $$ SELECT name FROM accounts WHERE id = i $$;
GRANT SELECT, INSERT, UPDATE, DELETE ON accounts, posts, active_accounts TO app;
";

/// The deployed queries.
fn queries() -> Vec<(String, DeployedKind)> {
    let q = |s: &str| (s.to_owned(), DeployedKind::Query);
    let c = |s: &str| (s.to_owned(), DeployedKind::CopyIn);
    vec![
        q("SELECT * FROM accounts WHERE id = $id"),
        q(
            "SELECT id, email, name, nick, status, kind, balance, age, score, note, created FROM accounts",
        ),
        q("SELECT id, status FROM accounts WHERE status = $status"),
        q(
            "SELECT a.name, p.title, p.flag FROM accounts a LEFT JOIN posts p ON p.account_id = a.id",
        ),
        q("SELECT * FROM active_accounts"),
        q("SELECT account_name($id) AS n"),
        q("SELECT count(*) AS n FROM accounts WHERE kind = $kind"),
        q("SELECT id FROM accounts WHERE name = $name AND balance > $min"),
        q("INSERT INTO accounts (id, email, name) VALUES ($id, $email, $name)"),
        q(
            "INSERT INTO accounts (id, email, name, nick, status, kind, balance, age, score, note) \
           VALUES ($id, $email, $name, $nick, $status, $kind, $balance, $age, $score, $note)",
        ),
        q("INSERT INTO accounts VALUES ($id, $email, $name)"),
        q(
            "INSERT INTO accounts (id, email, name, kind, balance) VALUES ($id, $email, 'constant name', 'b', 12.5)",
        ),
        q(
            "INSERT INTO accounts (id, email, name) SELECT id + 1000, email || 'x', name FROM accounts WHERE id = $id",
        ),
        q("INSERT INTO accounts (id, email, name) VALUES ($id, $email, $name) RETURNING *"),
        q(
            "INSERT INTO posts (id, account_id, title, flag) VALUES ($id, $account_id, $title, $flag)",
        ),
        q("UPDATE accounts SET name = $name WHERE id = $id"),
        q("UPDATE accounts SET nick = $nick, age = $age WHERE id = $id"),
        q("UPDATE accounts SET status = 'blocked' WHERE id = $id"),
        q("UPDATE accounts SET balance = balance + $amount WHERE id = $id"),
        q("UPDATE accounts SET note = DEFAULT, kind = DEFAULT WHERE id = $id"),
        q("UPDATE accounts SET score = $score, created = $created WHERE id = $id"),
        q("UPDATE active_accounts SET balance = $balance WHERE id = $id"),
        q("INSERT INTO active_accounts (id, email, name) VALUES ($id, $email, $name)"),
        q(
            "INSERT INTO accounts (id, email, name) VALUES ($id, $email, $name) \
           ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name, nick = $nick",
        ),
        q("DELETE FROM accounts WHERE id = $id"),
        q(
            "MERGE INTO accounts a USING (SELECT $id::integer AS id, $name::varchar AS name) v ON a.id = v.id \
           WHEN MATCHED THEN UPDATE SET name = v.name \
           WHEN NOT MATCHED THEN INSERT (id, email, name) VALUES (v.id, 'merge@example.com', v.name)",
        ),
        q("UPDATE posts SET title = $title, body = $body WHERE account_id = $account_id"),
        c("accounts (id, email, name)"),
        c("accounts (id, email, name, nick, status, kind, balance, age, score, note, created)"),
        c("posts"),
    ]
}

/// The migration changes: a name, extra deployed migrations (after the
/// base), and the new migration.
fn changes() -> Vec<(&'static str, &'static str, String)> {
    // A column the view reads changes type only with the view recreated.
    let with_view = |sql: &str| {
        format!(
            "DROP VIEW active_accounts; {sql} CREATE VIEW active_accounts AS SELECT id, email, \
             name, balance, status FROM accounts WHERE status = 'active'; GRANT SELECT, INSERT, \
             UPDATE, DELETE ON active_accounts TO app;"
        )
    };
    let changes: Vec<(&str, &str, &str)> = vec![
        (
            "rename column",
            "",
            "ALTER TABLE accounts RENAME COLUMN email TO email_address;",
        ),
        ("drop column", "", "ALTER TABLE accounts DROP COLUMN note;"),
        (
            "drop view column",
            "",
            "CREATE OR REPLACE VIEW active_accounts AS SELECT id, email, name, balance, status FROM accounts WHERE status = 'active' AND kind = 'a';",
        ),
        ("rename table", "", "ALTER TABLE accounts RENAME TO users;"),
        (
            "narrow varchar",
            "",
            "VIEW:ALTER TABLE accounts ALTER COLUMN name TYPE varchar(10);",
        ),
        (
            "narrow nullable varchar",
            "",
            "ALTER TABLE accounts ALTER COLUMN nick TYPE varchar(5);",
        ),
        (
            "widen varchar",
            "",
            "VIEW:ALTER TABLE accounts ALTER COLUMN name TYPE varchar(40);",
        ),
        (
            "varchar to text",
            "",
            "VIEW:ALTER TABLE accounts ALTER COLUMN name TYPE text;",
        ),
        (
            "int to bigint",
            "",
            "VIEW:ALTER TABLE accounts ALTER COLUMN id TYPE bigint;",
        ),
        (
            "int to smallint",
            "",
            "ALTER TABLE accounts ALTER COLUMN age TYPE smallint;",
        ),
        (
            "narrow numeric digits",
            "",
            "VIEW:ALTER TABLE accounts ALTER COLUMN balance TYPE numeric(6,2);",
        ),
        (
            "narrow numeric scale",
            "",
            "VIEW:ALTER TABLE accounts ALTER COLUMN balance TYPE numeric(8,1);",
        ),
        (
            "numeric to integer",
            "",
            "VIEW:ALTER TABLE accounts ALTER COLUMN balance TYPE integer USING balance::integer;",
        ),
        (
            "date to timestamp",
            "",
            "ALTER TABLE accounts ALTER COLUMN created TYPE timestamp;",
        ),
        (
            "drop domain",
            "",
            "ALTER TABLE accounts ALTER COLUMN score TYPE integer;",
        ),
        (
            "bit length",
            "",
            "ALTER TABLE posts ALTER COLUMN flag TYPE bit(4) USING flag::bit(4);",
        ),
        (
            "kind to varchar(1)",
            "",
            "ALTER TABLE accounts ALTER COLUMN kind TYPE varchar(1);",
        ),
        (
            "set not null",
            "",
            "ALTER TABLE accounts ALTER COLUMN nick SET NOT NULL;",
        ),
        (
            "set not null on age",
            "",
            "UPDATE accounts SET age = 0 WHERE age IS NULL; ALTER TABLE accounts ALTER COLUMN age SET NOT NULL;",
        ),
        (
            "drop not null",
            "",
            "ALTER TABLE accounts ALTER COLUMN name DROP NOT NULL;",
        ),
        (
            "drop not null on fk",
            "",
            "ALTER TABLE posts ALTER COLUMN title DROP NOT NULL;",
        ),
        (
            "add nullable column",
            "",
            "ALTER TABLE accounts ADD COLUMN tenant integer;",
        ),
        (
            "add not null default column",
            "",
            "ALTER TABLE accounts ADD COLUMN tenant integer NOT NULL DEFAULT 0;",
        ),
        (
            "add not null column then drop default",
            "",
            "ALTER TABLE accounts ADD COLUMN tenant integer NOT NULL DEFAULT 0; ALTER TABLE accounts ALTER COLUMN tenant DROP DEFAULT;",
        ),
        (
            "add not null column",
            "",
            "ALTER TABLE accounts ADD COLUMN tenant integer NOT NULL;",
        ),
        (
            "drop default of not null",
            "",
            "ALTER TABLE accounts ALTER COLUMN kind DROP DEFAULT;",
        ),
        (
            "drop enum default",
            "",
            "ALTER TABLE accounts ALTER COLUMN status DROP DEFAULT;",
        ),
        (
            "set default",
            "",
            "ALTER TABLE accounts ALTER COLUMN nick SET DEFAULT 'x';",
        ),
        (
            "add check",
            "",
            "ALTER TABLE accounts ADD CONSTRAINT age_max CHECK (age < 150);",
        ),
        (
            "add check not valid",
            "",
            "ALTER TABLE accounts ADD CONSTRAINT kind_a CHECK (kind = 'a') NOT VALID;",
        ),
        (
            "add multi column check",
            "",
            "ALTER TABLE accounts ADD CONSTRAINT name_len CHECK (length(name) > 2 AND (nick IS NULL OR nick <> name)) NOT VALID;",
        ),
        (
            "add balance check",
            "",
            "ALTER TABLE accounts ADD CONSTRAINT bal CHECK (balance >= 0) NOT VALID;",
        ),
        (
            "add not null check",
            "",
            "ALTER TABLE accounts ADD CONSTRAINT nick_nn CHECK (nick IS NOT NULL) NOT VALID;",
        ),
        (
            "widen check",
            "",
            "ALTER TABLE accounts DROP CONSTRAINT accounts_kind_check; ALTER TABLE accounts ADD CONSTRAINT accounts_kind_check CHECK (kind IN ('a', 'b', 'c'));",
        ),
        (
            "narrow check",
            "",
            "ALTER TABLE accounts DROP CONSTRAINT accounts_kind_check; ALTER TABLE accounts ADD CONSTRAINT accounts_kind_check CHECK (kind IN ('a')) NOT VALID;",
        ),
        (
            "drop check",
            "",
            "ALTER TABLE accounts DROP CONSTRAINT accounts_age_check;",
        ),
        (
            "add unique",
            "",
            "ALTER TABLE accounts ADD CONSTRAINT name_unique UNIQUE (name);",
        ),
        (
            "add foreign key",
            "CREATE TABLE tenants (id integer PRIMARY KEY);",
            "ALTER TABLE posts ADD CONSTRAINT posts_title_fk FOREIGN KEY (id) REFERENCES tenants NOT VALID;",
        ),
        (
            "domain check",
            "",
            "ALTER DOMAIN posint ADD CONSTRAINT lt100 CHECK (VALUE < 100) NOT VALID;",
        ),
        (
            "domain not null",
            "",
            "UPDATE accounts SET score = 1 WHERE score IS NULL; ALTER DOMAIN posint SET NOT NULL;",
        ),
        (
            "enum add label",
            "",
            "ALTER TYPE status ADD VALUE 'deleted';",
        ),
        (
            "enum rename label",
            "",
            "ALTER TYPE status RENAME VALUE 'blocked' TO 'suspended';",
        ),
        ("revoke update", "", "REVOKE UPDATE ON accounts FROM app;"),
        ("revoke select", "", "REVOKE SELECT ON accounts FROM app;"),
        (
            "revoke insert",
            "",
            "REVOKE INSERT ON accounts, posts FROM app;",
        ),
        ("revoke view", "", "REVOKE ALL ON active_accounts FROM app;"),
        (
            "revoke function",
            "",
            "REVOKE EXECUTE ON FUNCTION account_name(integer) FROM PUBLIC;",
        ),
        (
            "column privileges",
            "",
            "REVOKE ALL ON accounts FROM app; GRANT SELECT (id, email, name), INSERT (id, email, name), UPDATE (name) ON accounts TO app;",
        ),
        (
            "enable rls",
            "",
            "ALTER TABLE accounts ENABLE ROW LEVEL SECURITY;",
        ),
        (
            "rls select policy",
            "",
            "ALTER TABLE accounts ENABLE ROW LEVEL SECURITY; CREATE POLICY p ON accounts FOR SELECT TO app USING (true);",
        ),
        (
            "rls permissive policy",
            "",
            "ALTER TABLE accounts ENABLE ROW LEVEL SECURITY; CREATE POLICY p ON accounts TO app USING (status = 'active') WITH CHECK (kind = 'a');",
        ),
        (
            "rls false policy",
            "",
            "ALTER TABLE accounts ENABLE ROW LEVEL SECURITY; CREATE POLICY p ON accounts TO app USING (false);",
        ),
        (
            "rls drop policy",
            "ALTER TABLE accounts ENABLE ROW LEVEL SECURITY; CREATE POLICY p ON accounts TO app USING (true);",
            "DROP POLICY p ON accounts;",
        ),
        (
            "rls restrictive policy",
            "ALTER TABLE accounts ENABLE ROW LEVEL SECURITY; CREATE POLICY p ON accounts TO app USING (true);",
            "CREATE POLICY r ON accounts AS RESTRICTIVE FOR INSERT TO app WITH CHECK (kind = 'b');",
        ),
        (
            "rls posts",
            "",
            "ALTER TABLE posts ENABLE ROW LEVEL SECURITY; CREATE POLICY p ON posts FOR SELECT TO app USING (true);",
        ),
        (
            "view adds column",
            "",
            "CREATE OR REPLACE VIEW active_accounts AS SELECT id, email, name, balance, status, kind FROM accounts WHERE status = 'active';",
        ),
        (
            "view not updatable",
            "",
            "DROP VIEW active_accounts; CREATE VIEW active_accounts AS SELECT DISTINCT id, email, name, balance, status FROM accounts WHERE status = 'active'; GRANT ALL ON active_accounts TO app;",
        ),
        (
            "view check option",
            "",
            "DROP VIEW active_accounts; CREATE VIEW active_accounts AS SELECT id, email, name, balance, status FROM accounts WHERE status = 'active' AND balance < 1000 WITH CHECK OPTION; GRANT ALL ON active_accounts TO app;",
        ),
        (
            "view instead of trigger",
            "",
            "CREATE FUNCTION ignore_row() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END $$; CREATE TRIGGER ignore INSTEAD OF INSERT OR UPDATE ON active_accounts FOR EACH ROW EXECUTE FUNCTION ignore_row();",
        ),
        (
            "view security invoker",
            "",
            "ALTER VIEW active_accounts SET (security_invoker = true); REVOKE SELECT ON accounts FROM app;",
        ),
        (
            "function signature",
            "",
            "DROP FUNCTION account_name(integer); CREATE FUNCTION account_name(i bigint) RETURNS text LANGUAGE sql STABLE AS $$ SELECT name FROM accounts WHERE id = i $$;",
        ),
        (
            "function return type",
            "",
            "DROP FUNCTION account_name(integer); CREATE FUNCTION account_name(i integer) RETURNS varchar LANGUAGE sql STABLE AS $$ SELECT name FROM accounts WHERE id = i $$;",
        ),
        (
            "function dropped",
            "",
            "DROP FUNCTION account_name(integer);",
        ),
        (
            "trigger rejects",
            "",
            "CREATE FUNCTION no_long() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF length(NEW.name) > 15 THEN RAISE EXCEPTION 'too long'; END IF; RETURN NEW; END $$; CREATE TRIGGER no_long BEFORE INSERT OR UPDATE ON accounts FOR EACH ROW EXECUTE FUNCTION no_long();",
        ),
        (
            "generated column",
            "",
            "ALTER TABLE accounts ADD COLUMN upper_name text GENERATED ALWAYS AS (upper(name)) STORED;",
        ),
        (
            "identity column",
            "",
            "ALTER TABLE accounts ADD COLUMN seq integer GENERATED ALWAYS AS IDENTITY;",
        ),
        (
            "column type to enum",
            "CREATE TYPE kind AS ENUM ('a', 'b');",
            "ALTER TABLE accounts ALTER COLUMN kind DROP DEFAULT; ALTER TABLE accounts DROP CONSTRAINT accounts_kind_check; ALTER TABLE accounts ALTER COLUMN kind TYPE kind USING kind::kind; ALTER TABLE accounts ALTER COLUMN kind SET DEFAULT 'a';",
        ),
        (
            "column to domain",
            "CREATE DOMAIN shortname AS varchar(20) CHECK (length(VALUE) <= 12);",
            "VIEW:ALTER TABLE accounts ALTER COLUMN name TYPE shortname;",
        ),
        ("drop table", "", "DROP TABLE posts;"),
        (
            "move schema",
            "CREATE SCHEMA archive;",
            "ALTER TABLE posts SET SCHEMA archive;",
        ),
        (
            "check on defaulted column",
            "",
            "ALTER TABLE accounts ADD CONSTRAINT kind_b CHECK (kind = 'b') NOT VALID;",
        ),
        (
            "not null not valid",
            "",
            "ALTER TABLE accounts ADD CONSTRAINT nick_nn NOT NULL nick NOT VALID;",
        ),
        (
            "domain default",
            "",
            "ALTER DOMAIN posint SET DEFAULT 1; UPDATE accounts SET score = 1 WHERE score IS NULL; ALTER DOMAIN posint SET NOT NULL;",
        ),
        (
            "set default violating check",
            "",
            "ALTER TABLE accounts ALTER COLUMN kind SET DEFAULT 'c';",
        ),
        (
            "rls with check on posts",
            "",
            "ALTER TABLE posts ENABLE ROW LEVEL SECURITY; CREATE POLICY p ON posts TO app USING (true) WITH CHECK (body IS NOT NULL);",
        ),
    ];
    changes
        .into_iter()
        .map(|(name, extra, new)| {
            let new = match new.strip_prefix("VIEW:") {
                Some(sql) => with_view(sql),
                None => new.to_owned(),
            };
            (name, extra, new)
        })
        .collect()
}

fn cases() -> Vec<Case> {
    let mut out = Vec::new();
    let filter = std::env::var("DEPLOYED_ORACLE_FILTER").ok();
    for (name, extra, new) in changes() {
        if filter.as_deref().is_some_and(|f| !name.contains(f)) {
            continue;
        }
        let mut old = vec![BASE.to_owned()];
        if !extra.is_empty() {
            old.push(extra.to_owned());
        }
        out.push(Case {
            name: name.to_owned(),
            old,
            new: vec![new],
            queries: queries(),
        });
    }
    if filter.is_none() {
        out.push(docs_case());
    }
    out
}

/// The docs page's example (`docs/lint/breaks_deployed_queries.md`): its
/// first block's `migrations/0001`, queries and `migrations/0002`.
fn docs_case() -> Case {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../docs/lint/breaks_deployed_queries.md"
    ))
    .unwrap();
    let block = text
        .split("```sql\n")
        .nth(1)
        .and_then(|b| b.split_once("```"))
        .map(|(b, _)| b)
        .unwrap();
    let section = |start: &str, end: Option<&str>| -> String {
        let from = block.find(start).unwrap() + start.len();
        let rest = &block[from..];
        let to = end.and_then(|e| rest.find(e)).unwrap_or(rest.len());
        rest[..to].lines().skip(1).collect::<Vec<_>>().join("\n")
    };
    let old = section("-- migrations/0001_init.sql", Some("-- queries.sql"));
    let queries = section("-- queries.sql", Some("-- migrations/0002"));
    let new = section("-- migrations/0002", None);
    let queries = queries
        .split("/* @name")
        .skip(1)
        .map(|q| {
            let sql = q.split_once("*/").unwrap().1.trim().to_owned();
            (sql, DeployedKind::Query)
        })
        .collect();
    Case {
        name: "docs example".to_owned(),
        old: vec![old],
        new: vec![new],
        queries,
    }
}

fn catalog(migrations: &[String]) -> Option<PgCatalog> {
    let mut catalog = PgCatalog::new().ok()?;
    for (i, sql) in migrations.iter().enumerate() {
        catalog.apply_migration(&format!("{i:04}.sql"), sql).ok()?;
    }
    Some(catalog)
}

/// Lint and check one case.
fn run_case(case: &Case) -> Result<DeployedReport, String> {
    let old_catalog = catalog(&case.old).ok_or("the deployed migrations don't apply")?;
    let all: Vec<String> = case.old.iter().chain(&case.new).cloned().collect();
    let new_catalog = catalog(&all).ok_or("the new migrations don't apply")?;
    let files: Vec<MigrationFile> = all
        .iter()
        .enumerate()
        .map(|(i, sql)| MigrationFile {
            path: format!("{i:04}.sql"),
            sql: sql.clone(),
            lint: MigrationLint::All,
        })
        .collect();
    let mut queries = Vec::new();
    for (i, (sql, kind)) in case.queries.iter().enumerate() {
        for role in [None, Some("app")] {
            if role.is_some() && case.name == "docs example" {
                continue;
            }
            queries.push(DeployedQuery {
                sql: sql.clone(),
                locations: vec![format!(
                    "q{}{}",
                    i + 1,
                    role.map_or(String::new(), |r| format!("-{r}"))
                )],
                roles: role.map(str::to_owned).into_iter().collect(),
                kind: *kind,
            });
        }
    }
    let decoding = Decoding::TypeScript;
    let findings = breaks_deployed_queries(
        &Deployment {
            reference: "origin/main",
            catalog: &old_catalog,
            queries: &queries,
            decoding,
        },
        &NewSchema {
            files: &files,
            first_new: case.old.len(),
            use_transaction: true,
            catalog: &new_catalog,
            prepare: &|_| Ok(()),
        },
        &LintOptions::default(),
    );
    Ok(run_deployed(&DeployedCase {
        old: &case.old,
        new: &case.new,
        queries: &queries,
        findings: &findings,
        old_catalog: &old_catalog,
        decoding,
    }))
}

#[test]
#[ignore = "needs a PostgreSQL (scripts/run-lint-oracle.sh)"]
fn deployed_oracle_corpus() {
    let cases = cases();
    let total_queries: usize = cases.iter().map(|c| c.queries.len()).sum();
    eprintln!(
        "deployed oracle: {} cases, {total_queries} deployed queries (each as no role and as app)",
        cases.len()
    );
    let threads: usize = std::env::var("DEPLOYED_ORACLE_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(6);
    let results: Mutex<Vec<(String, Result<DeployedReport, String>)>> = Mutex::new(Vec::new());
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
                    let result =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_case(case)))
                            .unwrap_or_else(|panic| {
                                Err(format!(
                                    "panicked: {}",
                                    panic
                                        .downcast_ref::<String>()
                                        .cloned()
                                        .or_else(|| panic
                                            .downcast_ref::<&str>()
                                            .map(|s| (*s).to_owned()))
                                        .unwrap_or_default()
                                ))
                            });
                    results.lock().unwrap().push((case.name.clone(), result));
                }
            });
        }
    });
    let mut results = results.into_inner().unwrap();
    results.sort_by(|a, b| a.0.cmp(&b.0));

    let out_dir = std::env::var("DEPLOYED_ORACLE_OUT").unwrap_or_else(|_| {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../target/deployed-oracle-findings"
        )
        .to_owned()
    });
    let _ = std::fs::remove_dir_all(&out_dir);
    std::fs::create_dir_all(&out_dir).unwrap();
    let (mut missed, mut false_claims, mut unjustified, mut justified, mut calls, mut queries) =
        (0, 0, 0, 0, 0, 0);
    let mut unobservable = 0;
    let mut failing = Vec::new();
    for (name, result) in &results {
        let file = name.replace(' ', "_");
        match result {
            Err(why) => {
                eprintln!("  {name}: not run: {why}");
                std::fs::write(format!("{out_dir}/error-{file}.txt"), why).unwrap();
                failing.push(name.clone());
            }
            Ok(report) => {
                if let Some(why) = &report.inconclusive {
                    eprintln!("  {name}: inconclusive: {why}");
                }
                missed += report.missed.len();
                false_claims += report.false_claims.len();
                unjustified += report.unjustified.len();
                justified += report.justified;
                unobservable += report.unobservable;
                calls += report.calls;
                queries += report.queries;
                let broken: Vec<String> = report
                    .missed
                    .iter()
                    .chain(&report.false_claims)
                    .cloned()
                    .collect();
                if !broken.is_empty() {
                    eprintln!(
                        "  {name}: {} missed, {} false claims",
                        report.missed.len(),
                        report.false_claims.len()
                    );
                    std::fs::write(format!("{out_dir}/broken-{file}.txt"), broken.join("\n\n"))
                        .unwrap();
                    failing.push(name.clone());
                }
                if !report.unjustified.is_empty() {
                    std::fs::write(
                        format!("{out_dir}/unjustified-{file}.txt"),
                        report.unjustified.join("\n\n"),
                    )
                    .unwrap();
                }
            }
        }
    }
    eprintln!(
        "deployed oracle: {queries} queries checked, {calls} calls; {justified} findings borne \
         out, {unjustified} unjustified, {unobservable} of queries no call ran before, {missed} \
         missed breaks, {false_claims} false claims; written to {out_dir}"
    );
    assert!(
        failing.is_empty(),
        "the deployed oracle found problems in {} cases: {failing:?} (see {out_dir})",
        failing.len()
    );
}
