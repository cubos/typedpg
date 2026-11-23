//! The migration oracle (the `pg_sanity` feature): every migration rule's
//! claim, and its silence, checked against what a real PostgreSQL does.
//!
//! [`crate::lint_migrations`] hands its findings here. The files are
//! replayed, in a scratch database, one statement at a time and in the
//! transactions the migration runner gives them (a statement outside one
//! runs in a transaction of its own, so its locks can still be seen; one
//! that refuses a transaction block runs bare, and only its rewrites are
//! observed). Around each statement the oracle records:
//!
//! - the relation locks the session gained (`pg_locks`, before the
//!   transaction ends), and the ones it holds after;
//! - the tables and indexes whose storage changed (`pg_class.relfilenode`):
//!   a rewrite, or an index built;
//! - the tables scanned (`pg_stat_xact_all_tables`' sequential and index
//!   scans, or PostgreSQL's `DEBUG1` "verifying table" message);
//! - whether `lock_timeout` was set when it started.
//!
//! Each finding of a migration rule is then checked: the table it names is
//! locked as strongly as it says (`missing_lock_timeout`'s "every query"
//! is `ACCESS EXCLUSIVE`, "every write" a weaker mode that still conflicts
//! with `ROW EXCLUSIVE`), rewritten, given a new index, or scanned. And the
//! rules' silence is checked: on a table that existed when the file
//! started,
//!
//! - the first statement waiting for a write-blocking lock with no
//!   `lock_timeout` is the one `missing_lock_timeout` reports;
//! - an `ALTER TABLE` that rewrites it is reported by `table_rewrite`
//!   (`VACUUM FULL`, `CLUSTER`, `REPACK` by `blocking_rebuild`);
//! - an index built on it outside `CONCURRENTLY` is reported by
//!   `create_index_blocking` (or the rewrite / rebuild rule);
//! - a scan of it under a write-blocking lock is reported by one of the
//!   scan rules (`constraint_validating`, `set_not_null_scan`,
//!   `attach_partition_scan`, `validate_in_same_transaction`).
//!
//! The schema rules (`missing_primary_key`, `rls_*`,
//! `security_definer_search_path`, `prefer_*`, `fk_type_mismatch`,
//! `fk_without_index`) are checked against PostgreSQL's catalog once every
//! file is replayed: each finding names an object the catalog shows as the
//! rule describes, and each such object is reported. `redundant_index` is
//! not checked.
//!
//! What is deliberately not held against a rule: a `SET TABLESPACE`
//! reported as a copy (tablespaces are not tracked), an `ATTACH PARTITION`
//! scan claimed under a locale collation (the lint can't order its strings),
//! a lock claim after a `set_config('lock_timeout', …)` the lint doesn't
//! evaluate, an `ADD COLUMN … NOT NULL` without a default (it fails on a
//! table with rows), and an index built on a materialized view (it blocks
//! only `REFRESH`).
//!
//! A rule turned off, or allowed by a comment on the statement, is not
//! expected to report. Migrations that write the cluster's shared catalogs
//! (roles, tablespaces, databases) or that PostgreSQL rejects are not
//! replayed. Broken claims go through [`super::rules::oracle`]'s
//! collector: they panic unless the corpus run (`tests/migration_oracle.rs`)
//! collects them.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use postgres::{Client, Config, NoTls};
use typedpg_core::QualifiedName;
use typedpg_pg_query::ast::{Node as N, ObjectType, TransactionStmtKind};

use crate::migrations::{MigrationFile, MigrationFinding, MigrationLint};
use crate::rules::oracle::{count, record_stats, report_broken};
use crate::{LintOptions, Rule};

/// The statement rules the oracle checks.
const STATEMENT_RULES: [Rule; 10] = [
    Rule::MissingLockTimeout,
    Rule::CreateIndexBlocking,
    Rule::DropIndexBlocking,
    Rule::ConstraintValidating,
    Rule::SetNotNullScan,
    Rule::TableRewrite,
    Rule::BlockingRebuild,
    Rule::AttachPartitionScan,
    Rule::DetachPartitionBlocking,
    Rule::ValidateInSameTransaction,
];

/// The rules that report a scan under a lock.
const SCAN_RULES: [Rule; 4] = [
    Rule::ConstraintValidating,
    Rule::SetNotNullScan,
    Rule::AttachPartitionScan,
    Rule::ValidateInSameTransaction,
];

const SHARE: u8 = 5;
const ACCESS_EXCLUSIVE: u8 = 8;

/// A `pg_locks` mode's strength, `AccessShareLock` (1) to
/// `AccessExclusiveLock` (8), as `lockdefs.h` numbers them: a mode from
/// `ShareLock` (5) up conflicts with a writer's `RowExclusiveLock`, and only
/// `AccessExclusiveLock` with a reader's `AccessShareLock`.
fn strength(mode: &str) -> u8 {
    match mode {
        "AccessShareLock" => 1,
        "RowShareLock" => 2,
        "RowExclusiveLock" => 3,
        "ShareUpdateExclusiveLock" => 4,
        "ShareLock" => 5,
        "ShareRowExclusiveLock" => 6,
        "ExclusiveLock" => 7,
        "AccessExclusiveLock" => 8,
        _ => 0,
    }
}

/// One relation (table, materialized view, foreign table or index) of a
/// user schema.
#[derive(Clone)]
struct Rel {
    relfilenode: u32,
    kind: char,
    /// Rendered as the lint renders it (`QualifiedName`).
    name: String,
    relname: String,
    /// The table of an index.
    indrelid: u32,
    /// Sequential plus index scans in the open transaction's pending
    /// statistics.
    scans: i64,
}

fn is_table(kind: char) -> bool {
    matches!(kind, 'r' | 'p' | 'f' | 'm')
}

#[derive(Clone, Default)]
struct Snapshot {
    rels: HashMap<u32, Rel>,
    /// Each relation's inheritance parents (partitions included).
    parents: HashMap<u32, Vec<u32>>,
    /// The partitioned tables with a key column whose collation orders
    /// strings by a locale (anything but `C`, `POSIX`, `ucs_basic`,
    /// `pg_c_utf8`, `pg_unicode_fast`): `attach_partition_scan` can't tell
    /// whether a CHECK implies their bounds, and reports.
    locale_keys: HashSet<u32>,
}

impl Snapshot {
    /// Whether `oid` is `ancestor` or inherits from it.
    fn descends(&self, oid: u32, ancestor: u32) -> bool {
        let mut stack = vec![oid];
        let mut seen = HashSet::new();
        while let Some(current) = stack.pop() {
            if current == ancestor {
                return true;
            }
            if seen.insert(current) {
                stack.extend(self.parents.get(&current).into_iter().flatten());
            }
        }
        false
    }

    fn by_name(&self, name: &str) -> Option<u32> {
        self.rels
            .iter()
            .find(|(_, rel)| rel.name == name)
            .map(|(oid, _)| *oid)
    }
}

/// What PostgreSQL did running one statement.
struct Observation {
    /// The statement ran in a transaction the oracle could inspect before
    /// it ended: its locks and scans are known.
    observable: bool,
    lock_timeout: bool,
    /// The strongest lock the statement gained on each relation.
    gained: HashMap<u32, u8>,
    /// The strongest lock held after it.
    held_after: HashMap<u32, u8>,
    /// Tables whose storage changed.
    rewritten: HashSet<u32>,
    /// Tables an index was built (or rebuilt) on.
    indexed: HashSet<u32>,
    /// Tables scanned.
    scanned: HashSet<u32>,
    before: Snapshot,
    after: Snapshot,
}

impl Observation {
    /// Whether `table` (or, for a partitioned or inheritance parent, one of
    /// its children) is in `set`.
    fn touches(&self, set: &HashSet<u32>, table: u32) -> bool {
        set.iter()
            .any(|&oid| self.after.descends(oid, table) || self.before.descends(oid, table))
    }
}

/// A statement of a file, as replayed.
struct Statement<'a> {
    file: usize,
    index: usize,
    /// Its byte range in the file's text.
    range: Range<usize>,
    /// The text whose allow comments apply to it: from the end of the
    /// previous statement's line to the end of its own (a comment after a
    /// statement on its line is its own).
    lead: usize,
    line_end: usize,
    node: &'a N,
    text: String,
    /// The tables that existed when the file started.
    pre_existing: Arc<HashSet<u32>>,
    observation: Option<Observation>,
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A scratch database, dropped with the value.
struct Scratch {
    admin_url: String,
    name: String,
    client: Option<Client>,
    notices: Arc<Mutex<Vec<String>>>,
}

impl Scratch {
    fn new() -> Option<Result<Scratch, String>> {
        let admin_url = std::env::var("POSTGRES_URL").ok()?;
        Some(Scratch::create(admin_url))
    }

    fn create(admin_url: String) -> Result<Scratch, String> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let name = format!(
            "typedpg_migration_oracle_{}_{}_{nanos}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let mut admin = Client::connect(&admin_url, NoTls)
            .map_err(|e| format!("migration oracle: connect failed: {e}"))?;
        admin
            .batch_execute(&format!("CREATE DATABASE \"{name}\""))
            .map_err(|e| format!("migration oracle: CREATE DATABASE failed: {e}"))?;
        let mut config: Config = admin_url
            .parse()
            .map_err(|e| format!("migration oracle: bad POSTGRES_URL: {e}"))?;
        config.dbname(&name);
        let notices = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&notices);
        config.notice_callback(move |notice| {
            sink.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(notice.message().to_owned());
        });
        let mut scratch = Scratch {
            admin_url,
            name,
            client: None,
            notices,
        };
        let mut client = config
            .connect(NoTls)
            .map_err(|e| format!("migration oracle: connect to scratch DB failed: {e}"))?;
        // "verifying table" and "rewriting table" are DEBUG1 messages.
        client
            .batch_execute("SET client_min_messages = debug1")
            .map_err(|e| e.to_string())?;
        scratch.client = Some(client);
        Ok(scratch)
    }

    fn client(&mut self) -> &mut Client {
        self.client.as_mut().expect("connected")
    }

    fn take_notices(&self) -> Vec<String> {
        std::mem::take(&mut *self.notices.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn snapshot(&mut self) -> Result<Snapshot, postgres::Error> {
        let rows = self.client().query(
            "SELECT c.oid::int8, c.relfilenode::int8, c.relkind::text, n.nspname::text, \
                    c.relname::text, coalesce(i.indrelid, 0)::int8, \
                    (coalesce(s.seq_scan, 0) + coalesce(s.idx_scan, 0))::int8 \
             FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             LEFT JOIN pg_catalog.pg_index i ON i.indexrelid = c.oid \
             LEFT JOIN pg_catalog.pg_stat_xact_all_tables s ON s.relid = c.oid \
             WHERE c.relkind IN ('r', 'p', 'f', 'm', 'i', 'I') \
               AND n.nspname <> 'information_schema' AND n.nspname NOT LIKE 'pg\\_%'",
            &[],
        )?;
        let mut snapshot = Snapshot::default();
        for row in rows {
            let oid = row.get::<_, i64>(0) as u32;
            let schema: String = row.get(3);
            let relname: String = row.get(4);
            snapshot.rels.insert(
                oid,
                Rel {
                    relfilenode: row.get::<_, i64>(1) as u32,
                    kind: row.get::<_, String>(2).chars().next().unwrap_or('?'),
                    name: QualifiedName::new(&schema, &relname).to_string(),
                    relname,
                    indrelid: row.get::<_, i64>(5) as u32,
                    scans: row.get(6),
                },
            );
        }
        for row in self.client().query(
            "SELECT inhrelid::int8, inhparent::int8 FROM pg_catalog.pg_inherits",
            &[],
        )? {
            snapshot
                .parents
                .entry(row.get::<_, i64>(0) as u32)
                .or_default()
                .push(row.get::<_, i64>(1) as u32);
        }
        for row in self.client().query(
            "SELECT p.partrelid::int8 FROM pg_catalog.pg_partitioned_table p \
             WHERE EXISTS (SELECT FROM unnest(p.partcollation::oid[]) k \
                           JOIN pg_catalog.pg_collation c ON c.oid = k \
                           WHERE c.collname NOT IN ('C', 'POSIX', 'ucs_basic', \
                                                    'pg_c_utf8', 'pg_unicode_fast'))",
            &[],
        )? {
            snapshot.locale_keys.insert(row.get::<_, i64>(0) as u32);
        }
        Ok(snapshot)
    }

    fn locks(&mut self) -> Result<HashMap<u32, u8>, postgres::Error> {
        let mut out: HashMap<u32, u8> = HashMap::new();
        for row in self.client().query(
            "SELECT relation::int8, mode FROM pg_catalog.pg_locks \
             WHERE pid = pg_backend_pid() AND locktype = 'relation' AND granted \
               AND database = (SELECT oid FROM pg_catalog.pg_database \
                               WHERE datname = current_database())",
            &[],
        )? {
            let oid = row.get::<_, i64>(0) as u32;
            let mode = strength(row.get::<_, &str>(1));
            let entry = out.entry(oid).or_default();
            *entry = (*entry).max(mode);
        }
        Ok(out)
    }

    fn lock_timeout(&mut self) -> Result<bool, postgres::Error> {
        let row = self
            .client()
            .query_one("SELECT current_setting('lock_timeout')", &[])?;
        Ok(row.get::<_, &str>(0) != "0")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        drop(self.client.take());
        if let Ok(mut admin) = Client::connect(&self.admin_url, NoTls) {
            let _ = admin.batch_execute(&format!(
                "DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)",
                self.name
            ));
        }
    }
}

/// Whether a statement writes a catalog the whole cluster shares: replaying
/// it in a scratch database would collide with the other tests'.
fn writes_shared_catalog(node: &N) -> bool {
    let shared = |objtype: ObjectType| {
        matches!(
            objtype,
            ObjectType::ObjectParameterAcl
                | ObjectType::ObjectDatabase
                | ObjectType::ObjectTablespace
                | ObjectType::ObjectRole
        )
    };
    match node {
        N::GrantStmt(g) => shared(g.objtype),
        N::CommentStmt(c) => shared(c.objtype),
        N::SecLabelStmt(l) => shared(l.objtype),
        // COPY … FROM STDIN would wait for data.
        N::CopyStmt(copy) => copy.is_from && copy.filename.is_empty(),
        N::DoStmt(_) => true,
        N::CreateRoleStmt(_)
        | N::AlterRoleStmt(_)
        | N::AlterRoleSetStmt(_)
        | N::DropRoleStmt(_)
        | N::GrantRoleStmt(_)
        | N::DropOwnedStmt(_)
        | N::ReassignOwnedStmt(_)
        | N::AlterSystemStmt(_)
        | N::CreatedbStmt(_)
        | N::DropdbStmt(_)
        | N::AlterDatabaseStmt(_)
        | N::AlterDatabaseSetStmt(_)
        | N::AlterDatabaseRefreshCollStmt(_)
        | N::CreateTableSpaceStmt(_)
        | N::DropTableSpaceStmt(_)
        | N::AlterTableSpaceOptionsStmt(_) => true,
        _ => false,
    }
}

/// Whether a scan this statement makes is its own business, not a
/// validation: queries, data changes and the statements whose rebuild the
/// other checks cover.
fn scan_is_expected(node: &N) -> bool {
    matches!(
        node,
        N::SelectStmt(_)
            | N::InsertStmt(_)
            | N::UpdateStmt(_)
            | N::DeleteStmt(_)
            | N::MergeStmt(_)
            | N::CopyStmt(_)
            | N::CallStmt(_)
            | N::ExplainStmt(_)
            | N::CreateTableAsStmt(_)
            | N::RefreshMatViewStmt(_)
            | N::VacuumStmt(_)
            | N::RepackStmt(_)
            | N::TruncateStmt(_)
            | N::IndexStmt(_)
            | N::ReindexStmt(_)
    )
}

/// Whether an `ALTER TABLE` adds a NOT NULL (or PRIMARY KEY) column with
/// nothing to fill it: PostgreSQL scans the table to check it, but on a
/// table with rows the statement fails, so the scan is no hazard.
fn adds_unfilled_not_null(node: &N) -> bool {
    use typedpg_pg_query::ast::{AlterTableType, ConstrType};
    let N::AlterTableStmt(alter) = node else {
        return false;
    };
    alter.cmds.iter().any(|command| {
        let Some(N::AlterTableCmd(command)) = command.as_ref() else {
            return false;
        };
        let (AlterTableType::AtAddColumn, Some(N::ColumnDef(column))) =
            (command.subtype, command.def.as_ref())
        else {
            return false;
        };
        let kinds: Vec<ConstrType> = column
            .constraints
            .iter()
            .filter_map(|c| match c.as_ref() {
                Some(N::Constraint(c)) => Some(c.contype),
                _ => None,
            })
            .collect();
        let not_null = column.is_not_null
            || kinds
                .iter()
                .any(|k| matches!(k, ConstrType::ConstrNotnull | ConstrType::ConstrPrimary));
        // `DEFAULT NULL` fills nothing.
        let filled = column.constraints.iter().any(|c| match c.as_ref() {
            Some(N::Constraint(c)) => match c.contype {
                ConstrType::ConstrDefault => {
                    !matches!(c.raw_expr.as_ref(), Some(N::AConst(constant)) if constant.isnull)
                }
                ConstrType::ConstrIdentity | ConstrType::ConstrGenerated => true,
                _ => false,
            },
            _ => false,
        });
        not_null && !filled
    })
}

/// Whether a rule is expected to report on `statement`: on, and not allowed
/// by a comment in or before it.
fn expects(
    options: &LintOptions,
    file: &MigrationFile,
    statement: &Statement<'_>,
    rule: Rule,
) -> bool {
    if file.lint != MigrationLint::All || options.level(rule).is_none() {
        return false;
    }
    let text = file
        .sql
        .get(statement.lead..statement.line_end)
        .unwrap_or_default();
    !text.match_indices("allow(").any(|(at, _)| {
        text[at + "allow(".len()..]
            .split(')')
            .next()
            .is_some_and(|names| names.split(',').any(|name| name.trim() == rule.name()))
    })
}

/// Whether a `REINDEX` / `REPACK` option list says `CONCURRENTLY`.
fn concurrently(params: &[Option<N>]) -> bool {
    params.iter().any(|p| {
        matches!(
            p.as_ref(),
            Some(N::DefElem(d)) if d.defname.eq_ignore_ascii_case("concurrently")
                && !matches!(d.arg.as_ref(), Some(N::Boolean(b)) if !b.boolval)
        )
    })
}

/// Whether `node` is a `SELECT set_config('lock_timeout', …)` whose value
/// is not a string literal (optionally cast to `text`) or NULL: a value the
/// lint does not evaluate, and takes as no timeout.
fn unevaluated_lock_timeout(node: &N, text: &str) -> bool {
    let N::SelectStmt(_) = node else {
        return false;
    };
    let lower = text.to_ascii_lowercase();
    let Some(at) = lower.find("set_config") else {
        return false;
    };
    let Some(args) = lower[at..].split_once(',').map(|(_, rest)| rest) else {
        return false;
    };
    let value = args
        .rsplit_once(',')
        .map_or(args, |(value, _)| value)
        .trim();
    let literal =
        value.starts_with('\'') && value.ends_with('\'') && value.matches('\'').count() == 2;
    let cast = value
        .strip_suffix("::text")
        .is_some_and(|v| v.starts_with('\'') && v.ends_with('\'') && v.matches('\'').count() == 2);
    !(literal || cast || value == "null")
}

/// The text between `start` and `end` in `message`.
fn between<'a>(message: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let from = message.find(start)? + start.len();
    let rest = &message[from..];
    Some(&rest[..rest.find(end)?])
}

/// Check `findings`, the lint of `files`, against PostgreSQL.
pub(crate) fn check(
    files: &[MigrationFile],
    use_transaction: bool,
    options: &LintOptions,
    findings: &[MigrationFinding],
) {
    if files.iter().all(|file| file.lint != MigrationLint::All) {
        return;
    }
    let mut parsed = Vec::new();
    for file in files {
        let stripped = typedpg_core::migration_marker::strip_bom(&file.sql);
        let Ok(tree) = typedpg_pg_query::parse(stripped) else {
            return;
        };
        parsed.push((file.sql.len() - stripped.len(), tree));
    }
    if parsed.iter().any(|(_, tree)| {
        tree.tree
            .stmts
            .iter()
            .any(|raw| raw.stmt.as_ref().is_some_and(writes_shared_catalog))
    }) {
        return;
    }
    let Some(scratch) = Scratch::new() else {
        return;
    };
    let mut scratch = match scratch {
        Ok(scratch) => scratch,
        Err(error) => panic!("{error}"),
    };
    let mut statements = Vec::new();
    let mut stats = Vec::new();
    let complete = replay(
        &mut scratch,
        files,
        &parsed,
        use_transaction,
        &mut statements,
    );
    let mut broken = Vec::new();
    let context = |statement: &Statement<'_>| {
        let mut out = format!(
            "statement {} of {}:\n---\n{}\n---\nmigrations (use_transaction = {use_transaction}):",
            statement.index + 1,
            files[statement.file].path,
            statement.text
        );
        for file in files {
            out.push_str(&format!("\n-- {}\n{}", file.path, file.sql));
        }
        out
    };

    // Each finding's claim.
    for finding in findings {
        let rule = finding.finding.rule;
        if !STATEMENT_RULES.contains(&rule) {
            continue;
        }
        let Some(statement) = statements.iter().find(|statement| {
            statement.file == finding.file
                && statement.range.start <= finding.finding.span.start
                && finding.finding.span.start <= statement.range.end
        }) else {
            count(&mut stats, rule, "claim inconclusive");
            continue;
        };
        let Some(observation) = statement.observation.as_ref() else {
            count(&mut stats, rule, "claim inconclusive");
            continue;
        };
        // A timeout set from a value the lint does not evaluate.
        if rule == Rule::MissingLockTimeout
            && observation.lock_timeout
            && statements.iter().any(|earlier| {
                earlier.file == statement.file
                    && earlier.index < statement.index
                    && unevaluated_lock_timeout(earlier.node, &earlier.text)
            })
        {
            count(&mut stats, rule, "claim inconclusive");
            continue;
        }
        match claim(rule, &finding.finding.message, observation) {
            Some(Ok(())) => count(&mut stats, rule, "claim held"),
            Some(Err(how)) => {
                count(&mut stats, rule, "claim broken");
                broken.push(format!(
                    "migration oracle: the {rule} claim is wrong: {how}\nmessage: {}\n{}",
                    finding.finding.message,
                    context(statement)
                ));
            }
            None => count(&mut stats, rule, "claim inconclusive"),
        }
    }

    // The rules' silence.
    let reported = |statement: &Statement<'_>, rule: Rule| {
        findings.iter().any(|finding| {
            finding.file == statement.file
                && finding.finding.rule == rule
                && statement.range.start <= finding.finding.span.start
                && finding.finding.span.start <= statement.range.end
        })
    };
    let mut lock_reported_in: HashSet<usize> = HashSet::new();
    for statement in &statements {
        let file = &files[statement.file];
        let Some(observation) = statement.observation.as_ref() else {
            continue;
        };
        let pre = |oid: &u32| {
            statement.pre_existing.contains(oid)
                && observation
                    .before
                    .rels
                    .get(oid)
                    .is_some_and(|rel| is_table(rel.kind))
        };
        let name = |oid: &u32| {
            observation
                .before
                .rels
                .get(oid)
                .map(|rel| rel.name.clone())
                .unwrap_or_default()
        };
        let mut missed = |rule: Rule, what: String, broken: &mut Vec<String>| {
            count(&mut stats, rule, "missed");
            broken.push(format!(
                "migration oracle: {rule} is silent, but {what}\n{}",
                context(statement)
            ));
        };

        // missing_lock_timeout: the file's first statement waiting for a
        // write-blocking lock with no timeout.
        if !lock_reported_in.contains(&statement.file) {
            if reported(statement, Rule::MissingLockTimeout) {
                lock_reported_in.insert(statement.file);
            } else if observation.observable
                && !observation.lock_timeout
                && expects(options, file, statement, Rule::MissingLockTimeout)
            {
                // A materialized view is written by nobody but REFRESH: only
                // ACCESS EXCLUSIVE, which blocks its readers, matters.
                let matview = |oid: &u32| {
                    observation
                        .before
                        .rels
                        .get(oid)
                        .is_some_and(|rel| rel.kind == 'm')
                };
                let mut waits: Vec<(&u32, &u8)> = observation
                    .gained
                    .iter()
                    .filter(|(oid, mode)| {
                        pre(oid)
                            && (**mode == ACCESS_EXCLUSIVE || (**mode >= SHARE && !matview(oid)))
                    })
                    .collect();
                waits.sort();
                if let Some((oid, mode)) = waits.first() {
                    lock_reported_in.insert(statement.file);
                    missed(
                        Rule::MissingLockTimeout,
                        format!(
                            "the statement waits for a lock of strength {mode} on {} with no \
                             lock_timeout (the file's first such statement)",
                            name(oid)
                        ),
                        &mut broken,
                    );
                }
            }
        }

        let concurrent = match statement.node {
            N::IndexStmt(index) => index.concurrent,
            N::ReindexStmt(reindex) => concurrently(&reindex.params),
            N::RepackStmt(repack) => concurrently(&repack.params),
            _ => false,
        };
        // A rewrite.
        let rewritten: Vec<&u32> = observation
            .rewritten
            .iter()
            .filter(|oid| pre(oid))
            .collect();
        let rebuild_rule = match statement.node {
            N::AlterTableStmt(_) => Some(Rule::TableRewrite),
            N::VacuumStmt(_) | N::RepackStmt(_) => Some(Rule::BlockingRebuild),
            // TRUNCATE and REFRESH replace the storage by design.
            N::TruncateStmt(_) | N::RefreshMatViewStmt(_) => None,
            _ => Some(Rule::TableRewrite),
        };
        if let (Some(oid), Some(rule)) = (rewritten.first(), rebuild_rule)
            && !concurrent
            && !reported(statement, rule)
            && expects(options, file, statement, rule)
        {
            missed(
                rule,
                format!("PostgreSQL rewrote {}", name(oid)),
                &mut broken,
            );
        }
        // An index built.
        // A materialized view's index build blocks only REFRESH.
        let indexed: Vec<&u32> = observation
            .indexed
            .iter()
            .filter(|oid| {
                pre(oid)
                    && !observation.rewritten.contains(oid)
                    && observation
                        .before
                        .rels
                        .get(oid)
                        .is_some_and(|rel| rel.kind != 'm')
            })
            .collect();
        if let Some(oid) = indexed.first()
            && !concurrent
            && rewritten.is_empty()
        {
            let covering = [
                Rule::CreateIndexBlocking,
                Rule::TableRewrite,
                Rule::BlockingRebuild,
            ];
            if !covering.iter().any(|rule| reported(statement, *rule))
                && covering
                    .iter()
                    .all(|rule| expects(options, file, statement, *rule))
            {
                missed(
                    Rule::CreateIndexBlocking,
                    format!("PostgreSQL built an index on {}", name(oid)),
                    &mut broken,
                );
            }
        }
        // A scan under a write-blocking lock.
        if observation.observable
            && !scan_is_expected(statement.node)
            && !adds_unfilled_not_null(statement.node)
        {
            let scanned: Vec<&u32> = observation
                .scanned
                .iter()
                .filter(|oid| {
                    pre(oid)
                        && observation.held_after.get(oid).copied().unwrap_or(0) >= SHARE
                        && !observation.rewritten.contains(oid)
                        && !observation.indexed.contains(oid)
                })
                .collect();
            if let Some(oid) = scanned.first()
                && rewritten.is_empty()
                && !SCAN_RULES.iter().any(|rule| reported(statement, *rule))
                && SCAN_RULES
                    .iter()
                    .all(|rule| expects(options, file, statement, *rule))
            {
                missed(
                    Rule::ConstraintValidating,
                    format!(
                        "PostgreSQL scanned {} under a lock of strength {} (no scan rule \
                         reports it)",
                        name(oid),
                        observation.held_after.get(oid).copied().unwrap_or(0)
                    ),
                    &mut broken,
                );
            }
        }
    }
    if complete {
        check_schema(
            &mut scratch,
            files,
            options,
            findings,
            &mut stats,
            &mut broken,
        );
    } else {
        count(&mut stats, Rule::MissingLockTimeout, "replay incomplete");
    }
    record_stats(&stats);
    report_broken(broken);
}

/// Replay the files, observing each statement. `false` when PostgreSQL
/// failed a statement (the rest is not replayed).
fn replay<'a>(
    scratch: &mut Scratch,
    files: &'a [MigrationFile],
    parsed: &'a [(usize, typedpg_pg_query::ParseResult)],
    use_transaction: bool,
    out: &mut Vec<Statement<'a>>,
) -> bool {
    for (index, (file, (bom, tree))) in files.iter().zip(parsed).enumerate() {
        let Ok(start) = scratch.snapshot() else {
            return false;
        };
        let pre_existing: Arc<HashSet<u32>> = Arc::new(
            start
                .rels
                .iter()
                .filter(|(_, rel)| is_table(rel.kind))
                .map(|(oid, _)| *oid)
                .collect(),
        );
        // The lint takes every file to start without a timeout.
        if scratch
            .client()
            .batch_execute("SET lock_timeout = 0")
            .is_err()
        {
            return false;
        }
        let mut in_transaction = use_transaction
            && !typedpg_core::migration_marker::has_no_transaction_marker(&file.sql);
        if in_transaction && scratch.client().batch_execute("BEGIN").is_err() {
            return false;
        }
        let stripped = &file.sql[*bom..];
        let mut lead = *bom;
        for (number, raw) in tree.tree.stmts.iter().enumerate() {
            let Some(node) = raw.stmt.as_ref() else {
                continue;
            };
            let location = raw.stmt_location as usize;
            let end = if raw.stmt_len == 0 {
                stripped.len()
            } else {
                location + raw.stmt_len as usize
            };
            let text = stripped[location..end].to_owned();
            let line_end = bom
                + stripped[end..]
                    .find('\n')
                    .map_or(stripped.len(), |at| end + at);
            let mut statement = Statement {
                file: index,
                index: number,
                range: bom + location..bom + end,
                lead,
                line_end,
                node,
                text,
                pre_existing: Arc::clone(&pre_existing),
                observation: None,
            };
            lead = line_end;
            if let N::TransactionStmt(transaction) = node {
                if scratch.client().batch_execute(&statement.text).is_err() {
                    let _ = scratch.client().batch_execute("ROLLBACK");
                    return false;
                }
                in_transaction = match transaction.kind {
                    TransactionStmtKind::TransStmtBegin | TransactionStmtKind::TransStmtStart => {
                        true
                    }
                    TransactionStmtKind::TransStmtCommit
                    | TransactionStmtKind::TransStmtRollback => transaction.chain,
                    TransactionStmtKind::TransStmtPrepare => false,
                    _ => in_transaction,
                };
                out.push(statement);
                continue;
            }
            match observe(scratch, &statement.text, in_transaction) {
                Ok(observation) => {
                    statement.observation = Some(observation);
                    out.push(statement);
                }
                Err(_) => {
                    let _ = scratch.client().batch_execute("ROLLBACK");
                    return false;
                }
            }
        }
        if in_transaction && scratch.client().batch_execute("COMMIT").is_err() {
            let _ = scratch.client().batch_execute("ROLLBACK");
            return false;
        }
    }
    true
}

/// Run one statement, observing it.
fn observe(
    scratch: &mut Scratch,
    text: &str,
    in_transaction: bool,
) -> Result<Observation, postgres::Error> {
    let lock_timeout = scratch.lock_timeout()?;
    let wrapped = !in_transaction;
    if wrapped {
        scratch.client().batch_execute("BEGIN")?;
    }
    let before = scratch.snapshot()?;
    let held_before = scratch.locks()?;
    scratch.take_notices();
    let mut observable = true;
    if let Err(error) = scratch.client().batch_execute(text) {
        // `cannot run inside a transaction block`: run it bare.
        let refuses_block =
            error.code() == Some(&postgres::error::SqlState::ACTIVE_SQL_TRANSACTION);
        if !(wrapped && refuses_block) {
            return Err(error);
        }
        scratch.client().batch_execute("ROLLBACK")?;
        scratch.client().batch_execute(text)?;
        observable = false;
    }
    let notices = scratch.take_notices();
    let after = scratch.snapshot()?;
    let held_after = if observable {
        scratch.locks()?
    } else {
        HashMap::new()
    };
    if wrapped && observable {
        scratch.client().batch_execute("COMMIT")?;
    }
    let mut gained = HashMap::new();
    for (oid, mode) in &held_after {
        if held_before.get(oid).is_none_or(|before| before < mode) {
            gained.insert(*oid, *mode);
        }
    }
    let old_files: HashSet<u32> = before.rels.values().map(|rel| rel.relfilenode).collect();
    let mut rewritten = HashSet::new();
    let mut indexed = HashSet::new();
    let mut scanned = HashSet::new();
    let verified: HashSet<&str> = notices
        .iter()
        .filter_map(|notice| {
            notice
                .strip_prefix("verifying table \"")
                .and_then(|rest| rest.strip_suffix('"'))
        })
        .collect();
    for (oid, rel) in &after.rels {
        let old = before.rels.get(oid);
        match rel.kind {
            'r' | 'm' => {
                if old.is_some_and(|old| old.relfilenode != rel.relfilenode && rel.relfilenode != 0)
                {
                    rewritten.insert(*oid);
                }
                let scans = rel.scans - old.map_or(0, |old| old.scans);
                if observable
                    && old.is_some()
                    && (scans > 0 || verified.contains(rel.relname.as_str()))
                {
                    scanned.insert(*oid);
                }
            }
            'i' if !old_files.contains(&rel.relfilenode)
                && before.rels.contains_key(&rel.indrelid) =>
            {
                indexed.insert(rel.indrelid);
            }
            _ => {}
        }
    }
    Ok(Observation {
        observable,
        lock_timeout,
        gained,
        held_after,
        rewritten,
        indexed,
        scanned,
        before,
        after,
    })
}

/// Check one finding's message against the observation: `None` when the
/// message names nothing the oracle can check.
fn claim(rule: Rule, message: &str, o: &Observation) -> Option<Result<(), String>> {
    let resolve = |name: &str| o.before.by_name(name).or_else(|| o.after.by_name(name));
    let held = |oid: u32| o.held_after.get(&oid).copied().unwrap_or(0);
    // "blocks writes" (not reads): the lock the statement takes is SHARE
    // up to EXCLUSIVE — or it gained none, holding one from earlier in the
    // transaction (whose own statement answers for a stronger one).
    let writes_only = |oid: u32| match o.gained.get(&oid) {
        Some(mode) => (SHARE..ACCESS_EXCLUSIVE).contains(mode),
        None => held(oid) >= SHARE,
    };
    let scanned = |name: &str| -> Option<Result<(), String>> {
        if !o.observable {
            return None;
        }
        let oid = resolve(name)?;
        Some(if o.touches(&o.scanned, oid) {
            Ok(())
        } else {
            Err(format!("PostgreSQL did not scan {name}"))
        })
    };
    let locked = |name: &str, at_least: u8| -> Option<Result<(), String>> {
        if !o.observable {
            return None;
        }
        let oid = resolve(name)?;
        Some(if held(oid) >= at_least {
            Ok(())
        } else {
            Err(format!(
                "PostgreSQL holds a lock of strength {} on {name}, not {at_least}",
                held(oid)
            ))
        })
    };
    let rewritten = |name: &str| -> Option<Result<(), String>> {
        let oid = resolve(name)?;
        Some(if o.touches(&o.rewritten, oid) {
            Ok(())
        } else {
            Err(format!("PostgreSQL did not rewrite {name}"))
        })
    };
    let and = |a: Option<Result<(), String>>, b: Option<Result<(), String>>| match (a, b) {
        (Some(Err(e)), _) | (_, Some(Err(e))) => Some(Err(e)),
        (Some(Ok(())), _) | (_, Some(Ok(()))) => Some(Ok(())),
        _ => None,
    };
    match rule {
        Rule::MissingLockTimeout => {
            if !o.observable {
                return None;
            }
            if o.lock_timeout {
                return Some(Err("lock_timeout is set".to_owned()));
            }
            let table = between(message, " waits for a lock on ", " with no lock_timeout")?;
            let table = table.split(" (the table of ").next()?;
            let oid = resolve(table)?;
            let mode = o.gained.get(&oid).copied().unwrap_or(0);
            let reads = message.ends_with("it blocks every query on the table");
            Some(if mode < SHARE {
                Err(format!(
                    "the statement gains a lock of strength {mode} on {table}, which blocks no \
                     write"
                ))
            } else if reads != (mode == ACCESS_EXCLUSIVE) {
                Err(format!(
                    "the statement gains a lock of strength {mode} on {table}: it {} reads",
                    if mode == ACCESS_EXCLUSIVE {
                        "blocks"
                    } else {
                        "does not block"
                    }
                ))
            } else {
                Ok(())
            })
        }
        Rule::CreateIndexBlocking => {
            if let Some(table) = between(message, "blocks writes on ", " for the whole build") {
                let oid = resolve(table)?;
                let built = Some(if o.touches(&o.indexed, oid) {
                    Ok(())
                } else {
                    Err(format!("PostgreSQL built no index on {table}"))
                });
                let mode = if o.observable {
                    let mode = held(oid);
                    Some(if writes_only(oid) {
                        Ok(())
                    } else {
                        Err(format!(
                            "PostgreSQL holds a lock of strength {mode} on {table}"
                        ))
                    })
                } else {
                    None
                };
                and(built, mode)
            } else {
                let table = between(
                    message,
                    "blocking reads and writes on ",
                    " for the whole build",
                )?;
                let oid = resolve(table)?;
                let built = Some(if o.touches(&o.indexed, oid) {
                    Ok(())
                } else {
                    Err(format!("PostgreSQL built no index on {table}"))
                });
                and(built, locked(table, ACCESS_EXCLUSIVE))
            }
        }
        Rule::DropIndexBlocking => locked(
            between(
                message,
                "takes an ACCESS EXCLUSIVE lock on ",
                ", blocking every query",
            )?,
            ACCESS_EXCLUSIVE,
        ),
        Rule::DetachPartitionBlocking => locked(
            between(
                message,
                "takes an ACCESS EXCLUSIVE lock on ",
                ", blocking every query",
            )?,
            ACCESS_EXCLUSIVE,
        ),
        Rule::ConstraintValidating => {
            scanned(between(message, "validates every row of ", " under ")?)
        }
        Rule::SetNotNullScan => scanned(between(message, "scans every row of ", " under a lock")?),
        Rule::AttachPartitionScan => {
            let table = match between(
                message,
                "scans every row of the default partition ",
                " under ",
            ) {
                Some(table) => table,
                None => {
                    let table = between(message, "scans every row of ", " under ")?;
                    let oid = resolve(table)?;
                    if o.after
                        .locale_keys
                        .iter()
                        .any(|&parent| parent != oid && o.after.descends(oid, parent))
                    {
                        return None;
                    }
                    table
                }
            };
            and(scanned(table), locked(table, ACCESS_EXCLUSIVE))
        }
        Rule::ValidateInSameTransaction => {
            let table = between(message, "ADD CONSTRAINT's lock on ", " is still held")?;
            let oid = resolve(table)?;
            // The ADD may be an earlier command of the same statement.
            let mode = held(oid);
            let lock = if !o.observable {
                None
            } else if mode >= SHARE {
                Some(Ok(()))
            } else {
                Some(Err(format!(
                    "the transaction holds a lock of strength {mode} on {table}, which blocks no \
                     write"
                )))
            };
            and(lock, scanned(table))
        }
        Rule::TableRewrite => {
            // SET TABLESPACE is reported even to the tablespace the table is
            // in (the catalog doesn't track tablespaces), a documented
            // over-report: only its lock is checked.
            if let Some(table) = between(message, "copies every block of ", " under an ACCESS") {
                return locked(table, ACCESS_EXCLUSIVE);
            }
            let table = between(message, "rewrites every row of ", " under an ACCESS")?;
            and(rewritten(table), locked(table, ACCESS_EXCLUSIVE))
        }
        Rule::BlockingRebuild => {
            // REINDEX: the lock (it may rebuild only the TOAST table's
            // index, which the oracle doesn't follow).
            if let Some(table) = between(message, "blocks writes on ", ", and reads that") {
                let oid = resolve(table)?;
                let rebuilt = None;
                let mode = if o.observable {
                    let mode = held(oid);
                    Some(if writes_only(oid) {
                        Ok(())
                    } else {
                        Err(format!(
                            "PostgreSQL holds a lock of strength {mode} on {table}"
                        ))
                    })
                } else {
                    None
                };
                and(rebuilt, mode)
            } else {
                let target = between(message, " rewrites ", " under an ACCESS EXCLUSIVE lock")?;
                if target.starts_with("every table") {
                    return None;
                }
                and(rewritten(target), locked(target, ACCESS_EXCLUSIVE))
            }
        }
        _ => None,
    }
}

// ── the schema rules ───────────────────────────────────────────────────────

/// A user table that is no inheritance child (`is_reportable_table`).
const REPORTABLE: &str = "c.relkind IN ('r', 'p') \
     AND n.nspname <> 'information_schema' AND n.nspname NOT LIKE 'pg\\_%' \
     AND NOT EXISTS (SELECT FROM pg_catalog.pg_inherits WHERE inhrelid = c.oid)";

/// A column's type through domains, then an array's element through
/// domains (`inner_type`).
const INNER: &str = "(WITH RECURSIVE d(oid, depth) AS ( \
         SELECT a.atttypid, 0 \
         UNION ALL SELECT y.typbasetype, d.depth + 1 FROM d \
             JOIN pg_catalog.pg_type y ON y.oid = d.oid WHERE y.typbasetype <> 0), \
       b AS (SELECT oid FROM d ORDER BY depth DESC LIMIT 1), \
       e(oid, depth) AS ( \
         SELECT CASE WHEN y.typcategory = 'A' THEN y.typelem ELSE y.oid END, 0 \
             FROM b JOIN pg_catalog.pg_type y ON y.oid = b.oid \
         UNION ALL SELECT y.typbasetype, e.depth + 1 FROM e \
             JOIN pg_catalog.pg_type y ON y.oid = e.oid WHERE y.typbasetype <> 0) \
     SELECT oid FROM e ORDER BY depth DESC LIMIT 1)";

/// The schema rules checked against the final catalog, each with the SQL
/// listing (from PostgreSQL's catalog) the objects it must report — schema,
/// relation or routine, column — as [`schema_key`] reads them from its
/// message.
fn schema_queries() -> Vec<(Rule, String)> {
    let columns = |condition: &str| {
        format!(
            "SELECT n.nspname::text, c.relname::text, a.attname::text FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid \
             WHERE {REPORTABLE} AND a.attnum > 0 AND NOT a.attisdropped AND ({condition})"
        )
    };
    let inner_is = |name: &str| format!("{INNER} = 'pg_catalog.{name}'::pg_catalog.regtype");
    vec![
        (
            Rule::MissingPrimaryKey,
            "SELECT n.nspname::text, c.relname::text, '' FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relkind IN ('r', 'p') AND c.relpersistence <> 't' \
               AND n.nspname <> 'information_schema' AND n.nspname NOT LIKE 'pg\\_%' \
               AND NOT c.relispartition \
               AND NOT EXISTS (SELECT FROM pg_catalog.pg_constraint \
                               WHERE conrelid = c.oid AND contype = 'p')"
                .to_owned(),
        ),
        (
            Rule::RlsNoPolicy,
            "SELECT n.nspname::text, c.relname::text, '' FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relkind IN ('r', 'p') AND c.relrowsecurity \
               AND NOT EXISTS (SELECT FROM pg_catalog.pg_policy WHERE polrelid = c.oid)"
                .to_owned(),
        ),
        (
            Rule::RlsPolicyNotEnforced,
            "SELECT n.nspname::text, c.relname::text, '' FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relkind IN ('r', 'p') AND NOT c.relrowsecurity \
               AND EXISTS (SELECT FROM pg_catalog.pg_policy WHERE polrelid = c.oid)"
                .to_owned(),
        ),
        // A BEGIN ATOMIC body resolves its names when it is created; an
        // extension's routines are not the migrations' to fix.
        (
            Rule::SecurityDefinerSearchPath,
            "SELECT n.nspname::text, p.proname::text, '' FROM pg_catalog.pg_proc p \
             JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
             WHERE p.prosecdef AND p.prosqlbody IS NULL AND n.nspname <> 'information_schema' \
               AND NOT EXISTS (SELECT FROM pg_catalog.pg_depend \
                               WHERE classid = 'pg_catalog.pg_proc'::regclass \
                                 AND objid = p.oid AND deptype = 'e') \
               AND n.nspname NOT LIKE 'pg\\_%' \
               AND NOT EXISTS (SELECT FROM unnest(p.proconfig) s \
                               WHERE s LIKE 'search\\_path=%')"
                .to_owned(),
        ),
        (Rule::PreferTimestamptz, columns(&inner_is("timestamp"))),
        (Rule::PreferJsonb, columns(&inner_is("json"))),
        (
            Rule::PreferText,
            columns(&format!(
                // The column's typmod, else the first one along its domain
                // chain (a domain over `varchar(10)` limits as it does).
                "{} OR ({} AND coalesce(nullif(a.atttypmod, -1), \
                   (WITH RECURSIVE d(oid, typmod, depth) AS ( \
                      SELECT y.typbasetype, y.typtypmod, 0 FROM pg_catalog.pg_type y \
                      WHERE y.oid = a.atttypid \
                      UNION ALL SELECT y.typbasetype, y.typtypmod, d.depth + 1 FROM d \
                      JOIN pg_catalog.pg_type y ON y.oid = d.oid WHERE d.oid <> 0) \
                    SELECT typmod FROM d WHERE typmod <> -1 ORDER BY depth LIMIT 1)) > 4)",
                inner_is("bpchar"),
                inner_is("varchar")
            )),
        ),
        (
            Rule::PreferIdentity,
            columns(
                "a.attidentity = '' AND EXISTS ( \
                   SELECT FROM pg_catalog.pg_depend dep \
                   JOIN pg_catalog.pg_class s ON s.oid = dep.objid AND s.relkind = 'S' \
                   WHERE dep.refobjid = c.oid AND dep.refobjsubid = a.attnum \
                     AND dep.deptype = 'a' AND dep.classid = 'pg_catalog.pg_class'::regclass) \
                 AND pg_catalog.pg_get_expr( \
                   (SELECT adbin FROM pg_catalog.pg_attrdef \
                    WHERE adrelid = c.oid AND adnum = a.attnum), c.oid) LIKE 'nextval(%'",
            ),
        ),
        (
            Rule::PreferBigintPk,
            columns(&format!(
                "a.attnum = ANY ((SELECT conkey FROM pg_catalog.pg_constraint \
                                  WHERE conrelid = c.oid AND contype = 'p')::int2[]) \
                 AND ({0} OR {1})",
                inner_is("int4"),
                inner_is("int2")
            )),
        ),
    ]
}

/// The object a schema finding's message names, as [`schema_queries`]
/// lists it.
fn schema_key(rule: Rule, message: &str) -> Option<String> {
    match rule {
        Rule::MissingPrimaryKey => message
            .strip_suffix(" has no primary key")
            .map(str::to_owned),
        Rule::RlsNoPolicy => between(
            message,
            "row-level security is enabled on ",
            ", which has no policy",
        )
        .map(str::to_owned),
        Rule::RlsPolicyNotEnforced => {
            let (subject, _) = message.split_once(": row-level security is not enabled")?;
            let subject = subject
                .strip_suffix(" does nothing")
                .or_else(|| subject.strip_suffix(" do nothing"))?;
            subject
                .rsplit_once(" on ")
                .map(|(_, table)| table.to_owned())
        }
        Rule::SecurityDefinerSearchPath => {
            let rest = message.strip_prefix("SECURITY DEFINER ")?;
            let rest = rest
                .strip_prefix("function ")
                .or_else(|| rest.strip_prefix("procedure "))?;
            rest.split_once(" has no SET search_path")
                .map(|(name, _)| name.to_owned())
        }
        Rule::PreferTimestamptz
        | Rule::PreferJsonb
        | Rule::PreferText
        | Rule::PreferIdentity
        | Rule::PreferBigintPk => message.split_once(" is ").map(|(key, _)| key.to_owned()),
        Rule::FkTypeMismatch => {
            let name = between(message, "foreign key ", " compares ")?;
            let column = between(message, " compares ", " (")?;
            Some(format!("{name} {column}"))
        }
        Rule::FkWithoutIndex => {
            between(message, "foreign key ", " has no index").map(str::to_owned)
        }
        _ => None,
    }
}

/// A type with its domains stripped (`base_type`), for the column `a`.
const BASE: &str = "(WITH RECURSIVE d(oid, depth) AS ( \
         SELECT a.atttypid, 0 \
         UNION ALL SELECT y.typbasetype, d.depth + 1 FROM d \
             JOIN pg_catalog.pg_type y ON y.oid = d.oid WHERE y.typbasetype <> 0) \
     SELECT oid FROM d ORDER BY depth DESC LIMIT 1)";

/// The foreign keys `fk_type_mismatch` and `fk_without_index` must report,
/// keyed as [`schema_key`] reads their messages; `None` for a foreign key
/// whose index the oracle can't judge (a partial one with the right keys:
/// whether its predicate serves the check is the rule's own proof).
fn foreign_key_expectations(
    scratch: &mut Scratch,
) -> Result<(HashSet<String>, HashMap<String, bool>), postgres::Error> {
    let mut mismatched = HashSet::new();
    // The first column pair, in key order, whose base types differ.
    for row in scratch.client().query(
        &format!(
            "SELECT k.conname::text, n.nspname::text, c.relname::text, \
                    (SELECT a.attname::text FROM unnest(k.conkey, k.confkey) \
                         WITH ORDINALITY AS u(fk, pk, ord) \
                     JOIN pg_catalog.pg_attribute a ON a.attrelid = k.conrelid AND a.attnum = u.fk \
                     JOIN LATERAL (SELECT pa.atttypid FROM pg_catalog.pg_attribute pa \
                                   WHERE pa.attrelid = k.confrelid AND pa.attnum = u.pk) p ON true \
                     WHERE {BASE} <> (WITH RECURSIVE d(oid, depth) AS ( \
                               SELECT p.atttypid, 0 \
                               UNION ALL SELECT y.typbasetype, d.depth + 1 FROM d \
                                   JOIN pg_catalog.pg_type y ON y.oid = d.oid \
                                   WHERE y.typbasetype <> 0) \
                           SELECT oid FROM d ORDER BY depth DESC LIMIT 1) \
                     ORDER BY u.ord LIMIT 1) \
             FROM pg_catalog.pg_constraint k \
             JOIN pg_catalog.pg_class c ON c.oid = k.conrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE k.contype = 'f'"
        ),
        &[],
    )? {
        let Some(column) = row.get::<_, Option<String>>(3) else {
            continue;
        };
        let table = QualifiedName::new(row.get::<_, &str>(1), row.get::<_, &str>(2));
        mismatched.insert(format!(
            "{} {table}.{}",
            typedpg_core::quote_identifier(row.get::<_, &str>(0)),
            typedpg_core::quote_identifier(&column)
        ));
    }
    // An index serves the check when it is a btree whose leading key
    // columns are the foreign key's (in any order), or a hash index on its
    // one column.
    let mut unindexed = HashMap::new();
    for row in scratch.client().query(
        "SELECT k.conname::text, n.nspname::text, c.relname::text, \
                ARRAY(SELECT a.attname::text FROM unnest(k.conkey) WITH ORDINALITY u(attnum, ord) \
                      JOIN pg_catalog.pg_attribute a \
                        ON a.attrelid = k.conrelid AND a.attnum = u.attnum ORDER BY u.ord), \
                (SELECT bool_or(i.indpred IS NULL) FROM pg_catalog.pg_index i \
                 JOIN pg_catalog.pg_class ic ON ic.oid = i.indexrelid \
                 JOIN pg_catalog.pg_am am ON am.oid = ic.relam \
                 WHERE i.indrelid = k.conrelid \
                   AND (am.amname = 'btree' \
                        OR (am.amname = 'hash' AND cardinality(k.conkey) = 1)) \
                   AND i.indnkeyatts >= cardinality(k.conkey) \
                   AND (SELECT array_agg(x ORDER BY x) FROM unnest(k.conkey) x) \
                     = (SELECT array_agg(x ORDER BY x) FROM unnest( \
                          (i.indkey::int2[])[0:cardinality(k.conkey) - 1]) x)) \
         FROM pg_catalog.pg_constraint k \
         JOIN pg_catalog.pg_class c ON c.oid = k.conrelid \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE k.contype = 'f'",
        &[],
    )? {
        let served: Option<bool> = row.get(4);
        let table = QualifiedName::new(row.get::<_, &str>(1), row.get::<_, &str>(2));
        let columns: Vec<String> = row.get(3);
        let columns = columns
            .iter()
            .map(|column| typedpg_core::quote_identifier(column))
            .collect::<Vec<_>>()
            .join(", ");
        let key = format!(
            "{} on {table} ({columns})",
            typedpg_core::quote_identifier(row.get::<_, &str>(0))
        );
        match served {
            Some(true) => {}
            // Only partial indexes: the rule decides from their predicates.
            Some(false) => {
                unindexed.insert(key, false);
            }
            None => {
                unindexed.insert(key, true);
            }
        }
    }
    Ok((mismatched, unindexed))
}

/// Check the schema rules' findings (claims and silence) against the
/// catalog the replay left.
fn check_schema(
    scratch: &mut Scratch,
    files: &[MigrationFile],
    options: &LintOptions,
    findings: &[MigrationFinding],
    stats: &mut Vec<(String, String)>,
    broken: &mut Vec<String>,
) {
    let all_linted = files.iter().all(|file| file.lint == MigrationLint::All);
    let migrations = || {
        let mut out = String::new();
        for file in files {
            out.push_str(&format!("\n-- {}\n{}", file.path, file.sql));
        }
        out
    };
    let (mismatched, unindexed) = match foreign_key_expectations(scratch) {
        Ok(expectations) => expectations,
        Err(error) => panic!("migration oracle: the foreign key query failed: {error}"),
    };
    let mut checks: Vec<(Rule, HashSet<String>, HashSet<String>)> = vec![
        (Rule::FkTypeMismatch, mismatched, HashSet::new()),
        (
            Rule::FkWithoutIndex,
            unindexed
                .iter()
                .filter(|(_, sure)| **sure)
                .map(|(key, _)| key.clone())
                .collect(),
            unindexed
                .iter()
                .filter(|(_, sure)| !**sure)
                .map(|(key, _)| key.clone())
                .collect(),
        ),
    ];
    for (rule, query) in schema_queries() {
        let rows = match scratch.client().query(query.as_str(), &[]) {
            Ok(rows) => rows,
            Err(error) => panic!("migration oracle: the {rule} query failed: {error}"),
        };
        let expected: HashSet<String> = rows
            .iter()
            .map(|row| {
                let name =
                    QualifiedName::new(row.get::<_, &str>(0), row.get::<_, &str>(1)).to_string();
                let column: &str = row.get(2);
                if column.is_empty() {
                    name
                } else {
                    format!("{name}.{}", typedpg_core::quote_identifier(column))
                }
            })
            .collect();
        checks.push((rule, expected, HashSet::new()));
    }
    for (rule, expected, unsure) in checks {
        if options.level(rule).is_none() {
            continue;
        }
        let mut reported = HashSet::new();
        for finding in findings
            .iter()
            .filter(|finding| finding.finding.rule == rule)
        {
            let Some(key) = schema_key(rule, &finding.finding.message) else {
                count(stats, rule, "claim inconclusive");
                continue;
            };
            if unsure.contains(&key) {
                count(stats, rule, "claim inconclusive");
            } else if expected.contains(&key) {
                count(stats, rule, "claim held");
            } else {
                count(stats, rule, "claim broken");
                broken.push(format!(
                    "migration oracle: the {rule} claim is wrong: PostgreSQL's catalog does not \
                     have {key} as the rule says\nmessage: {}\nmigrations:{}",
                    finding.finding.message,
                    migrations()
                ));
            }
            reported.insert(key);
        }
        // An allow comment anywhere may be the creator's.
        let allowed = files.iter().any(|file| file.sql.contains(rule.name()));
        if !all_linted || allowed {
            continue;
        }
        let mut missing: Vec<&String> = expected.difference(&reported).collect();
        missing.sort();
        for key in missing {
            count(stats, rule, "missed");
            broken.push(format!(
                "migration oracle: {rule} is silent, but PostgreSQL's catalog has {key} as the \
                 rule describes\nmigrations:{}",
                migrations()
            ));
        }
    }
}
