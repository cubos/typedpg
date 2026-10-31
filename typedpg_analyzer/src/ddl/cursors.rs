//! The migration session's cursors (portalcmds.c, portalmem.c): `DECLARE`
//! analyzes and plans its query and opens a cursor of a name no open one
//! has, `FETCH` / `MOVE` / `CLOSE` need an open one, `CLOSE ALL` (and
//! `DISCARD ALL`) close them all. The transaction ending closes those it
//! opened, but a `WITH HOLD` one its `COMMIT` keeps open for the rest of
//! the session (PreCommit_Portals); `ROLLBACK` closes even those, and
//! `ROLLBACK TO SAVEPOINT` the ones opened since the savepoint
//! (AtSubCleanup_Portals). Closing is never undone.
//!
//! User code — a function, a procedure, a `DO` block — can open and close
//! cursors too (`OPEN`, an `EXECUTE`d or SQL-function `DECLARE`), which
//! the analyzer can't follow. Once code that may is defined or run, the
//! names of the open cursors are no longer certain, and neither error is
//! reported for the rest of the session.

use std::collections::HashMap;

use typedpg_pg_query::protobuf::{ClosePortalStmt, DeclareCursorStmt, FetchStmt};

use super::DdlError;
use crate::pg_catalog::PgCatalog;

/// `CURSOR_OPT_*` (parsenodes.h).
const CURSOR_OPT_SCROLL: i32 = 0x0002;
const CURSOR_OPT_NO_SCROLL: i32 = 0x0004;
const CURSOR_OPT_INSENSITIVE: i32 = 0x0008;
const CURSOR_OPT_ASENSITIVE: i32 = 0x0010;
const CURSOR_OPT_HOLD: i32 = 0x0020;

/// The session's open cursors (the portal hash table).
#[derive(Clone, Debug, Default)]
pub(crate) struct Cursors {
    open: HashMap<String, Cursor>,
    /// The number the next `DECLARE` takes: a savepoint's snapshot of the
    /// catalog holds the one it saw, which tells the cursors opened since.
    next: u64,
    /// User code that may open or close cursors was defined or run.
    uncertain: bool,
}

#[derive(Clone, Debug)]
struct Cursor {
    hold: bool,
    /// The `DECLARE`'s number, for a cursor its transaction opened
    /// (`createSubid` set); `None` once held over a commit.
    opened: Option<u64>,
}

impl Cursors {
    /// The transaction commits (PreCommit_Portals): its `WITH HOLD` cursors
    /// stay open, its other ones close.
    pub(crate) fn commit(&mut self) {
        self.open.retain(|_, c| c.hold || c.opened.is_none());
        for c in self.open.values_mut() {
            c.opened = None;
        }
    }

    /// The transaction aborts (AtCleanup_Portals): the cursors it opened
    /// close, `WITH HOLD` or not.
    pub(crate) fn abort(&mut self) {
        self.open.retain(|_, c| c.opened.is_none());
    }

    /// `ROLLBACK TO SAVEPOINT` of a savepoint whose catalog is `savepoint`
    /// (AtSubCleanup_Portals): the cursors opened since close.
    pub(crate) fn roll_back_to(&mut self, savepoint: &Cursors) {
        self.open
            .retain(|_, c| c.opened.is_none_or(|n| n < savepoint.next));
    }

    /// `CLOSE ALL`, `DISCARD ALL` (PortalHashTableDeleteAll).
    pub(crate) fn close_all(&mut self) {
        self.open.clear();
    }

    /// `text`, a function's definition or a `DO` block, has run or may
    /// run: when it may open or close a cursor, the open ones are no
    /// longer certain. Its words decide, over-cautiously: `OPEN`,
    /// `CLOSE`, a `DECLARE ... CURSOR`, or an `EXECUTE` of a string.
    pub(crate) fn note_user_code(&mut self, text: &str) {
        if self.uncertain {
            return;
        }
        let text = text.to_ascii_lowercase();
        self.uncertain = ["open", "close", "cursor", "execute"]
            .iter()
            .any(|word| text.contains(word));
    }
}

/// The checks of a `DECLARE CURSOR` before the cursor opens: its options
/// and query (transformDeclareCursorStmt), which is planned
/// (PerformCursorOpen) — with the checks of a query that runs — and its
/// executor started (PortalStart). `EXPLAIN DECLARE` runs these too.
pub(crate) fn check_declare(interp: &PgCatalog, stmt: &DeclareCursorStmt) -> Result<(), DdlError> {
    let options = stmt.options;
    for (a, b, a_name, b_name) in [
        (
            CURSOR_OPT_SCROLL,
            CURSOR_OPT_NO_SCROLL,
            "SCROLL",
            "NO SCROLL",
        ),
        (
            CURSOR_OPT_ASENSITIVE,
            CURSOR_OPT_INSENSITIVE,
            "ASENSITIVE",
            "INSENSITIVE",
        ),
    ] {
        if options & a != 0 && options & b != 0 {
            return Err(DdlError::Parse(format!(
                "cannot specify both {a_name} and {b_name}"
            )));
        }
    }
    let Some(query) = stmt.query.as_deref() else {
        return Ok(());
    };
    let Some(inner) = query.node.as_ref() else {
        return Ok(());
    };
    super::dml::check_executed(interp, inner)?;
    if super::views::has_modifying_cte(query) {
        return Err(DdlError::UnsupportedDdl(
            "DECLARE CURSOR must not contain data-modifying statements in WITH".into(),
        ));
    }
    if let Some(strength) = super::session::first_row_mark_strength(inner) {
        if options & CURSOR_OPT_HOLD != 0 {
            return Err(DdlError::UnsupportedDdl(format!(
                "DECLARE CURSOR WITH HOLD ... {strength} is not supported (Holdable cursors must \
                 be READ ONLY.)"
            )));
        }
        if options & CURSOR_OPT_SCROLL != 0 {
            return Err(DdlError::UnsupportedDdl(format!(
                "DECLARE SCROLL CURSOR ... {strength} is not supported (Scrollable cursors must \
                 be READ ONLY.)"
            )));
        }
        if options & CURSOR_OPT_INSENSITIVE != 0 {
            return Err(DdlError::UnsupportedDdl(format!(
                "DECLARE INSENSITIVE CURSOR ... {strength} is not valid (Insensitive cursors \
                 must be READ ONLY.)"
            )));
        }
    }
    Ok(())
}

/// `DECLARE name CURSOR ... FOR query`: checked ([`check_declare`]), then
/// opened under a name no open cursor has (CreatePortal).
pub(crate) fn declare(interp: &mut PgCatalog, stmt: &DeclareCursorStmt) -> Result<(), DdlError> {
    check_declare(interp, stmt)?;
    let cursors = &mut interp.cursors;
    if cursors.open.contains_key(&stmt.portalname) && !cursors.uncertain {
        return Err(DdlError::DuplicateObject(format!(
            "cursor \"{}\" already exists",
            stmt.portalname
        )));
    }
    let opened = cursors.next;
    cursors.next += 1;
    cursors.open.insert(
        stmt.portalname.clone(),
        Cursor {
            hold: stmt.options & CURSOR_OPT_HOLD != 0,
            opened: Some(opened),
        },
    );
    Ok(())
}

/// `FETCH` / `MOVE` (PerformPortalFetch): the cursor must be open.
pub(crate) fn fetch(interp: &PgCatalog, stmt: &FetchStmt) -> Result<(), DdlError> {
    require_open(interp, &stmt.portalname)
}

/// `CLOSE name` / `CLOSE ALL` (PerformPortalClose).
pub(crate) fn close(interp: &mut PgCatalog, stmt: &ClosePortalStmt) -> Result<(), DdlError> {
    // The grammar leaves CLOSE ALL's name NULL — empty here; a cursor's
    // name can't be empty.
    if stmt.portalname.is_empty() {
        interp.cursors.close_all();
        return Ok(());
    }
    require_open(interp, &stmt.portalname)?;
    interp.cursors.open.remove(&stmt.portalname);
    Ok(())
}

fn require_open(interp: &PgCatalog, name: &str) -> Result<(), DdlError> {
    let cursors = &interp.cursors;
    if cursors.uncertain || cursors.open.contains_key(name) {
        return Ok(());
    }
    Err(DdlError::TableNotFound(format!(
        "cursor \"{name}\" does not exist"
    )))
}
