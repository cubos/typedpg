/*
 * vars.c — server globals the copied PostgreSQL sources reference but whose
 * home files (GUC tables, the postmaster, the frontend protocol) typedpg does
 * not carry.  The values are the server defaults that make sense outside a
 * server; `typedpg_unavailable` is the error hook the generated stubs raise
 * when a path reaches code the file selection does not include.
 */
#include "postgres.h"
#include "miscadmin.h"
#include "postmaster/bgworker.h"
#include "storage/proc.h"
#include "pgstat.h"
#include "libpq/libpq.h"
#include "tcop/dest.h"
#include "pgtime.h"
#include "utils/guc.h"

#include <stdlib.h>

char	   *application_name = NULL;
char	   *backtrace_functions = NULL;
/* Read by elog.c's event-log writer on Windows only (guc_tables.c is not
 * carried). */
char	   *event_source = NULL;
bool		ClientAuthInProgress = false;
int			client_min_messages = NOTICE;
const char *debug_query_string = NULL;
int			log_min_error_statement = ERROR;
/* Indexed by BackendType since PostgreSQL 19; guc_tables.c's initializer. */
int			log_min_messages[] = {
#define PG_PROCTYPE(bktype, bkcategory, description, main_func, shmem_attach) \
	[bktype] = WARNING,
#include "postmaster/proctypelist.h"
#undef PG_PROCTYPE
};
/*
 * elog.c's check_log_min_messages (PostgreSQL 19) parses with it;
 * guc_tables.c's table.  A stub can't stand in: its incomplete-array
 * tentative definition is a definition to gcc but only a declaration to
 * MSVC, which then fails to link.
 */
const struct config_enum_entry server_message_level_options[] = {
	{"debug5", DEBUG5, false},
	{"debug4", DEBUG4, false},
	{"debug3", DEBUG3, false},
	{"debug2", DEBUG2, false},
	{"debug1", DEBUG1, false},
	{"debug", DEBUG2, true},
	{"info", INFO, false},
	{"notice", NOTICE, false},
	{"warning", WARNING, false},
	{"error", ERROR, false},
	{"log", LOG, false},
	{"fatal", FATAL, false},
	{"panic", PANIC, false},
	{NULL, 0, false}
};
pg_tz	   *log_timezone = NULL;
BackendType MyBackendType = B_INVALID;
BackgroundWorker *MyBgworkerEntry = NULL;
PGPROC	   *MyProc = NULL;
SessionEndType pgStatSessionEndCause = DISCONNECT_NORMAL;
const PQcommMethods *PqCommMethods = NULL;
bool		proc_exit_inprogress = false;
bool		redirection_done = false;
CommandDest whereToSendOutput = DestNone;
char	   *GUC_check_errdetail_string = NULL;
char	   *GUC_check_errhint_string = NULL;

void
typedpg_unavailable(const char *name)
{
	elog(ERROR, "typedpg: %s is not available outside a server", name);
	pg_unreachable();
}

/* Non-zero defaults the generated stubs would get wrong. */
#include "access/parallel.h"
bool		check_function_bodies = true;
ProcessingMode Mode = NormalProcessing;
int			ParallelWorkerNumber = -1;

/*
 * proc_exit — what PostgreSQL's error machinery calls for a FATAL error.
 * Without a server there is nothing to clean up, and re-entering the error
 * machinery (what the generated stub would do with its ERROR) would recurse
 * until the stack is gone.  Leave the process instead.
 */
void
proc_exit(int code)
{
	exit(code);
}
