//! Transaction-block rules (PreventInTransactionBlock /
//! RequireTransactionBlock, xact.c). The migration runner wraps a
//! migration in a transaction unless its first line is `-- no-transaction`
//! (or `use_transaction = false`); a multi-statement migration sent
//! without one still runs as the implicit transaction block of a single
//! query. So a migration of several statements is always inside a block,
//! a lone `-- no-transaction` statement never is, and a lone statement
//! otherwise depends on the runner's setting — left unchecked.

use pg_query::protobuf::{DiscardMode, ReindexObjectType, TransactionStmtKind, node};

use super::DdlError;

/// Where a migration's statements run.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TxContext {
    /// `Some(true)`: inside a transaction block; `Some(false)`: outside
    /// one; `None`: unknown.
    in_block: Option<bool>,
    /// The migration opted out of the runner's transaction.
    no_transaction: bool,
}

impl TxContext {
    pub(crate) fn of(sql: &str, statements: &[pg_query::protobuf::RawStmt]) -> TxContext {
        // MigrationSource: the first line, trimmed.
        let no_transaction = sql
            .lines()
            .next()
            .is_some_and(|line| line.trim() == "-- no-transaction");
        // Explicit BEGIN / COMMIT inside the file move the block
        // boundaries; don't guess.
        let explicit = statements.iter().any(|s| {
            matches!(
                s.stmt.as_ref().and_then(|n| n.node.as_ref()),
                Some(node::Node::TransactionStmt(t))
                    if matches!(
                        TransactionStmtKind::try_from(t.kind),
                        Ok(TransactionStmtKind::TransStmtBegin
                            | TransactionStmtKind::TransStmtStart
                            | TransactionStmtKind::TransStmtCommit
                            | TransactionStmtKind::TransStmtRollback)
                    )
            )
        });
        let in_block = if explicit {
            None
        } else if statements.len() > 1 {
            Some(true)
        } else if no_transaction {
            Some(false)
        } else {
            None
        };
        TxContext {
            in_block,
            no_transaction,
        }
    }

    pub(crate) fn check(&self, stmt: &node::Node) -> Result<(), DdlError> {
        if self.in_block == Some(true)
            && let Some(what) = forbidden_in_block(stmt)
        {
            return Err(DdlError::UnsupportedDdl(format!(
                "{what} cannot run inside a transaction block"
            )));
        }
        if let Some((what, implicit_ok)) = needs_block(stmt) {
            // Savepoints need an explicit block; LOCK / DECLARE accept the
            // implicit one.
            let outside = if implicit_ok {
                self.in_block == Some(false)
            } else {
                self.no_transaction && self.in_block.is_some()
            };
            if outside {
                return Err(DdlError::UnsupportedDdl(format!(
                    "{what} can only be used in transaction blocks"
                )));
            }
        }
        Ok(())
    }
}

/// PreventInTransactionBlock callers.
fn forbidden_in_block(stmt: &node::Node) -> Option<&'static str> {
    let concurrently = |params: &[pg_query::protobuf::Node]| {
        params.iter().any(|p| {
            matches!(p.node.as_ref(), Some(node::Node::DefElem(de))
                if de.defname == "concurrently"
                    && !matches!(de.arg.as_deref().and_then(|a| a.node.as_ref()),
                        Some(node::Node::Boolean(b)) if !b.boolval))
        })
    };
    match stmt {
        node::Node::VacuumStmt(v) if v.is_vacuumcmd => Some("VACUUM"),
        node::Node::IndexStmt(i) if i.concurrent => Some("CREATE INDEX CONCURRENTLY"),
        node::Node::DropStmt(d) if d.concurrent => Some("DROP INDEX CONCURRENTLY"),
        node::Node::ReindexStmt(r) => {
            if concurrently(&r.params) {
                return Some("REINDEX CONCURRENTLY");
            }
            match ReindexObjectType::try_from(r.kind) {
                Ok(ReindexObjectType::ReindexObjectSchema) => Some("REINDEX SCHEMA"),
                Ok(ReindexObjectType::ReindexObjectSystem) => Some("REINDEX SYSTEM"),
                Ok(ReindexObjectType::ReindexObjectDatabase) => Some("REINDEX DATABASE"),
                _ => None,
            }
        }
        node::Node::ClusterStmt(c) if c.relation.is_none() => Some("CLUSTER"),
        node::Node::AlterTableStmt(a) => a.cmds.iter().find_map(|c| match c.node.as_ref() {
            Some(node::Node::AlterTableCmd(cmd)) => {
                match cmd.def.as_deref().and_then(|d| d.node.as_ref()) {
                    Some(node::Node::PartitionCmd(pc)) if pc.concurrent => {
                        Some("ALTER TABLE ... DETACH CONCURRENTLY")
                    }
                    _ => None,
                }
            }
            _ => None,
        }),
        node::Node::DiscardStmt(d) if d.target == DiscardMode::DiscardAll as i32 => {
            Some("DISCARD ALL")
        }
        node::Node::CreatedbStmt(_) => Some("CREATE DATABASE"),
        node::Node::DropdbStmt(_) => Some("DROP DATABASE"),
        node::Node::CreateTableSpaceStmt(_) => Some("CREATE TABLESPACE"),
        node::Node::DropTableSpaceStmt(_) => Some("DROP TABLESPACE"),
        node::Node::AlterSystemStmt(_) => Some("ALTER SYSTEM"),
        _ => None,
    }
}

/// RequireTransactionBlock callers: `(name, the implicit block of a
/// multi-statement query suffices)`.
fn needs_block(stmt: &node::Node) -> Option<(&'static str, bool)> {
    /// CURSOR_OPT_HOLD (parsenodes.h).
    const CURSOR_OPT_HOLD: i32 = 0x0020;
    match stmt {
        node::Node::LockStmt(_) => Some(("LOCK TABLE", true)),
        node::Node::DeclareCursorStmt(d) if d.options & CURSOR_OPT_HOLD == 0 => {
            Some(("DECLARE CURSOR", true))
        }
        node::Node::TransactionStmt(t) => match TransactionStmtKind::try_from(t.kind) {
            Ok(TransactionStmtKind::TransStmtSavepoint) => Some(("SAVEPOINT", false)),
            Ok(TransactionStmtKind::TransStmtRelease) => Some(("RELEASE SAVEPOINT", false)),
            Ok(TransactionStmtKind::TransStmtRollbackTo) => Some(("ROLLBACK TO SAVEPOINT", false)),
            _ => None,
        },
        _ => None,
    }
}
