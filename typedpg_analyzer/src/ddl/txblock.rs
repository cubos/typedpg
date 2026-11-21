//! Transaction state of a migration (xact.c): the transaction block a
//! statement runs in, what `COMMIT` / `ROLLBACK` / savepoints do to the
//! catalog, and the rules that depend on the block
//! (`PreventInTransactionBlock`, `RequireTransactionBlock`, read-only
//! transactions).
//!
//! How a migration's statements are sent ([`PgCatalog::migration_runner`]):
//! by default the file is one simple query (`batch_execute`), where several
//! statements outside a transaction run as an implicit transaction block
//! (`exec_simple_query`) and a single statement as a transaction of its
//! own. The migration runner instead sends the file inside a transaction it
//! opened (`use_transaction`, unless the file's first line is
//! `-- no-transaction`) or else one statement at a time, each a query — and
//! so a transaction — of its own. Sent that way, a migration must close
//! every block it opens: the runner fails one that ends inside its own
//! `BEGIN`, and so does [`Transaction::finish`].
//! A statement that fails aborts the transaction it runs in: the catalog
//! goes back to how that transaction found it — the start of the migration,
//! or its last `COMMIT` / `ROLLBACK`. The session's cursors follow the
//! transaction's end too ([`super::cursors`]).

use typedpg_pg_query::ast::{
    self, DiscardMode, ReindexObjectType, TransactionStmt, TransactionStmtKind, VariableSetKind,
};

use super::DdlError;
use crate::pg_catalog::PgCatalog;

/// `TBlockState`, as far as a migration can tell them apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Block {
    /// A single-statement query outside a transaction block
    /// (`TBLOCK_STARTED`).
    Started,
    /// A multi-statement query outside a transaction block
    /// (`TBLOCK_IMPLICIT_INPROGRESS`).
    Implicit,
    /// Inside `BEGIN` ... `COMMIT` (`TBLOCK_INPROGRESS` /
    /// `TBLOCK_SUBINPROGRESS`), including the runner's own transaction.
    Explicit,
}

/// The transaction a migration's statements run in.
pub(crate) struct Transaction {
    block: Block,
    /// The query has more than one statement: after a `COMMIT` the rest runs
    /// in a new implicit block.
    multi: bool,
    /// The catalog as the current transaction found it.
    start: Box<PgCatalog>,
    /// Open savepoints, innermost last, with the catalog each one saw.
    savepoints: Vec<(String, Box<PgCatalog>)>,
    /// The statements are sent one at a time (the runner's unwrapped
    /// migrations): one outside a block commits once it has run.
    per_statement: bool,
    /// `XactReadOnly`.
    read_only: bool,
    /// A statement that takes a snapshot ran (`FirstSnapshotSet`): the
    /// transaction can no longer become read-write.
    queried: bool,
}

impl Transaction {
    /// The transaction the first statement of migration `sql` runs in.
    pub(crate) fn start(
        db: &PgCatalog,
        sql: &str,
        statements: &[typedpg_pg_query::ast::RawStmt],
    ) -> Transaction {
        // The runner's rule, shared (`typedpg_core::migration_marker`).
        let no_transaction = typedpg_core::migration_marker::has_no_transaction_marker(sql);
        let (block, per_statement) = match db.migration_runner {
            Some(true) if !no_transaction => (Block::Explicit, false),
            // The runner's `execute_each`: each statement a query of its own.
            Some(_) => (Block::Started, true),
            None if statements.len() > 1 => (Block::Implicit, false),
            None => (Block::Started, false),
        };
        Transaction {
            block,
            multi: !per_statement && statements.len() > 1,
            per_statement,
            start: db.snapshot(),
            savepoints: Vec::new(),
            read_only: false,
            queried: false,
        }
    }

    /// Whether the next statement runs inside a transaction block
    /// (PreventInTransactionBlock's test).
    pub(crate) fn in_block(&self) -> bool {
        self.block != Block::Started
    }

    /// The rules the transaction state imposes on `stmt`, before it runs.
    pub(crate) fn check(&self, db: &PgCatalog, stmt: &ast::Node) -> Result<(), DdlError> {
        if self.block != Block::Started
            && let Some(what) = forbidden_in_block(stmt)
        {
            return Err(DdlError::UnsupportedDdl(format!(
                "{what} cannot run inside a transaction block"
            )));
        }
        if let Some((what, implicit_ok)) = needs_block(stmt) {
            // Savepoints need an explicit block (DefineSavepoint & co.
            // reject TBLOCK_IMPLICIT_INPROGRESS); LOCK / DECLARE accept the
            // implicit one (RequireTransactionBlock).
            let outside = match self.block {
                Block::Started => true,
                Block::Implicit => !implicit_ok,
                Block::Explicit => false,
            };
            if outside {
                return Err(DdlError::UnsupportedDdl(format!(
                    "{what} can only be used in transaction blocks"
                )));
            }
        }
        if self.read_only
            && let Some(what) = writes(db, stmt)
        {
            return Err(DdlError::UnsupportedDdl(format!(
                "cannot execute {what} in a read-only transaction"
            )));
        }
        Ok(())
    }

    /// Bookkeeping after `stmt` ran: sent on its own, outside a block, it
    /// was a transaction of its own, which commits.
    pub(crate) fn after(&mut self, db: &mut PgCatalog, stmt: &ast::Node) {
        self.track(stmt);
        if self.per_statement && self.block == Block::Started {
            self.end(db, false, true);
        }
    }

    fn track(&mut self, stmt: &ast::Node) {
        match stmt {
            ast::Node::VariableSetStmt(v) => {
                if let Some(read_only) = read_only_setting(v) {
                    self.read_only = read_only;
                }
            }
            ast::Node::TransactionStmt(_) | ast::Node::VariableShowStmt(_) => {}
            _ => self.queried = true,
        }
    }

    /// `SET TRANSACTION READ WRITE` / `SET transaction_read_only = off`
    /// after the transaction took its snapshot (check_transaction_read_only).
    pub(crate) fn check_setting(&self, stmt: &ast::Node) -> Result<(), DdlError> {
        if let ast::Node::VariableSetStmt(v) = stmt
            && read_only_setting(v) == Some(false)
            && self.read_only
            && self.queried
        {
            return Err(DdlError::UnsupportedDdl(
                "transaction read-write mode must be set before any query".into(),
            ));
        }
        Ok(())
    }

    /// Transaction control: `BEGIN`, `COMMIT`, `ROLLBACK`, savepoints and
    /// two-phase commit.
    pub(crate) fn control(
        &mut self,
        db: &mut PgCatalog,
        stmt: &TransactionStmt,
    ) -> Result<(), DdlError> {
        use TransactionStmtKind as K;
        let kind = stmt.kind;
        match kind {
            K::TransStmtBegin | K::TransStmtStart => {
                // BeginTransactionBlock: inside a block, only a warning; an
                // implicit block becomes an explicit one.
                self.block = Block::Explicit;
                for option in &stmt.options {
                    if let Some(ast::Node::DefElem(de)) = option.as_ref()
                        && de.defname == "transaction_read_only"
                    {
                        self.read_only = def_elem_true(de);
                    }
                }
            }
            K::TransStmtCommit | K::TransStmtRollback => {
                // EndTransactionBlock / UserAbortTransactionBlock: AND
                // CHAIN needs an explicit block.
                if stmt.chain && self.block != Block::Explicit {
                    return Err(DdlError::UnsupportedDdl(format!(
                        "{} AND CHAIN can only be used in transaction blocks",
                        if kind == K::TransStmtCommit {
                            "COMMIT"
                        } else {
                            "ROLLBACK"
                        }
                    )));
                }
                // Outside a block there is nothing to end (a warning).
                if self.block == Block::Started {
                    return Ok(());
                }
                if kind == K::TransStmtRollback {
                    db.roll_back_to(&self.start);
                }
                self.end(db, stmt.chain, kind == K::TransStmtCommit);
            }
            K::TransStmtSavepoint => {
                self.savepoints
                    .push((stmt.savepoint_name.clone(), db.snapshot()));
            }
            K::TransStmtRelease | K::TransStmtRollbackTo => {
                // Innermost savepoint of that name (ReleaseSavepoint /
                // RollbackToSavepoint).
                let Some(at) = self
                    .savepoints
                    .iter()
                    .rposition(|(name, _)| *name == stmt.savepoint_name)
                else {
                    return Err(DdlError::UnsupportedDdl(format!(
                        "savepoint \"{}\" does not exist",
                        stmt.savepoint_name
                    )));
                };
                if kind == K::TransStmtRelease {
                    self.savepoints.truncate(at);
                } else {
                    // The savepoint stays, the ones after it go.
                    self.savepoints.truncate(at + 1);
                    let savepoint = &self.savepoints[at].1;
                    db.roll_back_to(savepoint);
                    db.cursors.roll_back_to(&savepoint.cursors);
                }
            }
            K::TransStmtPrepare => {
                // PrepareTransactionBlock ends the block like COMMIT, then
                // MarkAsPreparing needs max_prepared_transactions > 0 —
                // 0 on a stock server. Outside a block: a warning only.
                if self.block != Block::Started {
                    return Err(DdlError::UnsupportedDdl(
                        "prepared transactions are disabled".into(),
                    ));
                }
            }
            K::TransStmtCommitPrepared | K::TransStmtRollbackPrepared => {
                // FinishPreparedTransaction: nothing can have been
                // prepared (see above).
                return Err(DdlError::TypeNotFound(format!(
                    "prepared transaction with identifier \"{}\" does not exist",
                    stmt.gid
                )));
            }
        }
        Ok(())
    }

    /// End of the current transaction, `committed` or rolled back: the next
    /// statement runs in a new one — an explicit block for AND CHAIN (with
    /// the same characteristics), else the implicit block of the rest of
    /// the query.
    fn end(&mut self, db: &mut PgCatalog, chain: bool, committed: bool) {
        if committed {
            db.cursors.commit();
        } else {
            db.cursors.abort();
        }
        db.end_transaction_scope();
        self.savepoints.clear();
        self.queried = false;
        if chain {
            self.block = Block::Explicit;
        } else {
            self.read_only = false;
            self.block = if self.multi {
                Block::Implicit
            } else {
                Block::Started
            };
        }
        self.start = db.snapshot();
    }

    /// A statement failed: the transaction aborts, and the catalog goes back
    /// to how it found it.
    pub(crate) fn abort(&self, db: &mut PgCatalog) {
        db.roll_back_to(&self.start);
        db.cursors.abort();
        db.end_transaction_scope();
    }

    /// Whether the next statement runs inside `BEGIN` ... `COMMIT`.
    pub(crate) fn in_explicit_block(&self) -> bool {
        self.block == Block::Explicit
    }

    /// The end of the migration: whatever transaction is open commits — the
    /// runner commits its own, and a bare simple query leaves the session in
    /// the block its `BEGIN` opened. A migration the runner sends one
    /// statement at a time must not end inside a block it opened: nothing
    /// after it would commit, so the runner rolls that block back and fails
    /// the migration (`typedpg_core::migrations::machine`), and so does this.
    pub(crate) fn finish(mut self, db: &mut PgCatalog) -> Result<(), DdlError> {
        if self.per_statement && self.block == Block::Explicit {
            self.abort(db);
            return Err(DdlError::UnsupportedDdl(
                typedpg_core::migrations::machine::left_transaction_open_check_message(),
            ));
        }
        self.end(db, false, true);
        Ok(())
    }
}

/// The value `BEGIN` / `SET TRANSACTION` options carry as an integer
/// constant (`transaction_read_only` = 1).
fn def_elem_true(de: &typedpg_pg_query::ast::DefElem) -> bool {
    match de.arg.as_ref() {
        Some(ast::Node::AConst(c)) => match c.val.as_ref() {
            Some(ast::AConstValue::Integer(i)) => i.ival != 0,
            Some(ast::AConstValue::Boolean(b)) => b.boolval,
            _ => true,
        },
        Some(ast::Node::Integer(i)) => i.ival != 0,
        _ => true,
    }
}

/// What a `SET` does to `transaction_read_only`: `SET TRANSACTION [READ
/// ONLY | READ WRITE]` or `SET transaction_read_only = ...`.
fn read_only_setting(v: &typedpg_pg_query::ast::VariableSetStmt) -> Option<bool> {
    let kind = v.kind;
    if kind == VariableSetKind::VarSetMulti && v.name.eq_ignore_ascii_case("TRANSACTION") {
        return v.args.iter().rev().find_map(|arg| match arg.as_ref() {
            Some(ast::Node::DefElem(de)) if de.defname == "transaction_read_only" => {
                Some(def_elem_true(de))
            }
            _ => None,
        });
    }
    if kind == VariableSetKind::VarSetValue && v.name.eq_ignore_ascii_case("transaction_read_only")
    {
        let [arg] = v.args.as_slice() else {
            return None;
        };
        return match arg.as_ref() {
            Some(ast::Node::AConst(c)) => match c.val.as_ref()? {
                ast::AConstValue::Integer(i) => Some(i.ival != 0),
                ast::AConstValue::Boolean(b) => Some(b.boolval),
                ast::AConstValue::String(s) => match s.sval.to_ascii_lowercase().as_str() {
                    "on" | "true" | "yes" | "1" | "t" | "y" => Some(true),
                    "off" | "false" | "no" | "0" | "f" | "n" => Some(false),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        };
    }
    None
}

/// The command a read-only transaction refuses `stmt` as, if it writes:
/// DDL and TRUNCATE (ClassifyUtilityCommandAsReadOnly), and DML on a
/// permanent relation (ExecCheckXactReadOnly).
fn writes(db: &PgCatalog, stmt: &ast::Node) -> Option<&'static str> {
    use ast::Node as N;
    if super::cmdtag::utility_is_not_read_only(stmt) {
        return Some(super::cmdtag::command_tag(stmt));
    }
    let target = match stmt {
        // SELECT INTO is CREATE TABLE AS.
        N::SelectStmt(s) if s.into_clause.is_some() => return Some("SELECT INTO"),
        N::InsertStmt(s) => s.relation.as_ref(),
        N::UpdateStmt(s) => s.relation.as_ref(),
        N::DeleteStmt(s) => s.relation.as_ref(),
        N::MergeStmt(s) => s.relation.as_ref(),
        _ => None,
    }?;
    let (_, relid) = super::util::lookup_relation(db, target).ok()?;
    if super::util::is_temp_relation(db, relid) {
        return None;
    }
    Some(super::cmdtag::command_tag(stmt))
}

/// REINDEX's options as ExecReindex reads them, before anything else:
/// CONCURRENTLY and VERBOSE booleans (defGetBoolean), TABLESPACE, and no
/// other. Whether CONCURRENTLY is on (the last one given wins).
pub(crate) fn reindex_options(
    params: &[Option<typedpg_pg_query::ast::Node>],
) -> Result<bool, DdlError> {
    let mut concurrently = false;
    for p in params {
        let Some(ast::Node::DefElem(de)) = p.as_ref() else {
            continue;
        };
        match de.defname.as_str() {
            "concurrently" => concurrently = super::session::def_get_boolean(de)?,
            "verbose" => {
                super::session::def_get_boolean(de)?;
            }
            // defGetString needs a value. The tablespace itself, a cluster
            // object the analyzer doesn't see, is taken to exist (see
            // check_tablespace_placement): PG looks only the last one up.
            "tablespace" => {
                if de.arg.is_none() {
                    return Err(DdlError::Parse("tablespace requires a parameter".into()));
                }
            }
            other => {
                return Err(DdlError::Parse(format!(
                    "unrecognized REINDEX option \"{other}\""
                )));
            }
        }
    }
    Ok(concurrently)
}

/// REINDEX's TABLESPACE: the last one given, as defGetString renders it.
pub(crate) fn reindex_tablespace(params: &[Option<typedpg_pg_query::ast::Node>]) -> Option<String> {
    params.iter().rev().find_map(|p| match p.as_ref() {
        Some(ast::Node::DefElem(de)) if de.defname == "tablespace" => match de.arg.as_ref()? {
            ast::Node::String(s) => Some(s.sval.clone()),
            ast::Node::Integer(i) => Some(i.ival.to_string()),
            ast::Node::Float(f) => Some(f.fval.clone()),
            ast::Node::Boolean(b) => Some(if b.boolval { "true" } else { "false" }.into()),
            _ => None,
        },
        _ => None,
    })
}

/// PreventInTransactionBlock callers. (A REINDEX whose options are invalid
/// fails on them first, in [`super::maintenance::reindex`].)
fn forbidden_in_block(stmt: &ast::Node) -> Option<&'static str> {
    match stmt {
        ast::Node::VacuumStmt(v) if v.is_vacuumcmd => Some("VACUUM"),
        ast::Node::IndexStmt(i) if i.concurrent => Some("CREATE INDEX CONCURRENTLY"),
        ast::Node::DropStmt(d) if d.concurrent => Some("DROP INDEX CONCURRENTLY"),
        ast::Node::ReindexStmt(r) => {
            let Ok(concurrently) = reindex_options(&r.params) else {
                return None;
            };
            if concurrently {
                return Some("REINDEX CONCURRENTLY");
            }
            match r.kind {
                ReindexObjectType::ReindexObjectSchema => Some("REINDEX SCHEMA"),
                ReindexObjectType::ReindexObjectSystem => Some("REINDEX SYSTEM"),
                ReindexObjectType::ReindexObjectDatabase => Some("REINDEX DATABASE"),
                _ => None,
            }
        }
        ast::Node::AlterTableStmt(a) => a.cmds.iter().find_map(|c| match c.as_ref() {
            Some(ast::Node::AlterTableCmd(cmd)) => match cmd.def.as_ref() {
                Some(ast::Node::PartitionCmd(pc)) if pc.concurrent => {
                    Some("ALTER TABLE ... DETACH CONCURRENTLY")
                }
                _ => None,
            },
            _ => None,
        }),
        ast::Node::DiscardStmt(d) if d.target == DiscardMode::DiscardAll => Some("DISCARD ALL"),
        ast::Node::CreatedbStmt(_) => Some("CREATE DATABASE"),
        ast::Node::DropdbStmt(_) => Some("DROP DATABASE"),
        ast::Node::CreateTableSpaceStmt(_) => Some("CREATE TABLESPACE"),
        ast::Node::DropTableSpaceStmt(_) => Some("DROP TABLESPACE"),
        ast::Node::AlterSystemStmt(_) => Some("ALTER SYSTEM"),
        ast::Node::TransactionStmt(t) => match t.kind {
            TransactionStmtKind::TransStmtCommitPrepared => Some("COMMIT PREPARED"),
            TransactionStmtKind::TransStmtRollbackPrepared => Some("ROLLBACK PREPARED"),
            _ => None,
        },
        _ => None,
    }
}

/// RequireTransactionBlock callers: `(name, the implicit block of a
/// multi-statement query suffices)`.
fn needs_block(stmt: &ast::Node) -> Option<(&'static str, bool)> {
    /// CURSOR_OPT_HOLD (parsenodes.h).
    const CURSOR_OPT_HOLD: i32 = 0x0020;
    match stmt {
        ast::Node::LockStmt(_) => Some(("LOCK TABLE", true)),
        ast::Node::DeclareCursorStmt(d) if d.options & CURSOR_OPT_HOLD == 0 => {
            Some(("DECLARE CURSOR", true))
        }
        ast::Node::TransactionStmt(t) => match t.kind {
            TransactionStmtKind::TransStmtSavepoint => Some(("SAVEPOINT", false)),
            TransactionStmtKind::TransStmtRelease => Some(("RELEASE SAVEPOINT", false)),
            TransactionStmtKind::TransStmtRollbackTo => Some(("ROLLBACK TO SAVEPOINT", false)),
            _ => None,
        },
        _ => None,
    }
}
