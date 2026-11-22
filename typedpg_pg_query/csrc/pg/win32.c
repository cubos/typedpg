/*
 * win32.c — the pieces of PostgreSQL's Windows backend the imported sources
 * call but typedpg does not import.
 *
 * On Windows, miscadmin.h's CHECK_FOR_INTERRUPTS() reads PostgreSQL's signal
 * queue and dispatches what the signal thread queued (port/win32_port.h
 * declares both the queue and the dispatcher), and elog.c asks whether the
 * process runs as a Windows service.  typedpg runs inside an ordinary Rust
 * process and installs no signal handlers, so nothing ever enqueues a signal
 * and the process is never a service: the queue stays empty, dispatching it
 * is a no-op, and is_service() answers no.
 *
 * pg_usleep: on a Windows backend PostgreSQL answers it from
 * src/backend/port/win32/signal.c (the backend-only, signal-aware version);
 * src/port/pgsleep.c deliberately compiles that function only for frontend
 * or non-Windows builds.  The imported sources need it on Windows because
 * src/port/open.c (import/FILES.windows) sleeps before retrying a file
 * sharing violation.  This mirrors pgsleep.c's Windows branch.
 *
 * This file is compiled on every target and is empty everywhere but Windows;
 * gen_stubs.sh subtracts nothing from it elsewhere.
 */
#if defined(_WIN32)

#include "postgres.h"
#include "storage/ipc.h"
#include "utils/pg_locale.h"

/* The error hook the generated stubs raise (csrc/pg/vars.c defines it). */
extern void typedpg_unavailable(const char *name);

/* port/win32_port.h and miscadmin.h declare these. */
volatile int pg_signal_queue;
int pg_signal_mask;

void
pgwin32_dispatch_queued_signals(void)
{
	/* No signal handlers are installed, so the queue is always empty. */
}

/*
 * elog.c's set_backtrace (PostgreSQL 19) has a Windows branch, invisible to
 * the generated stubs, that registers a cleanup callback and converts the
 * DbgHelp symbol names.  It runs only for the functions backtrace_functions
 * names, which typedpg never sets: raise as the generated stubs do.
 */
void
on_proc_exit(pg_on_exit_callback function, Datum arg)
{
	typedpg_unavailable("on_proc_exit");
	pg_unreachable();
}

size_t
wchar2char(char *to, const wchar_t *from, size_t tolen, locale_t loc)
{
	typedpg_unavailable("wchar2char");
	pg_unreachable();
}

/* typedpg is a library, never a service. */
int
pgwin32_is_service(void)
{
	return 0;
}

/*
 * pg_codepage_to_encoding: only elog.c's event-log writer asks for it (to
 * compare the ANSI code page with the message encoding), and typedpg never
 * configures the event log as a destination.  Its home, src/port/chklocale.c,
 * is not imported for that one call; raise as the generated stubs do.
 */
int
pg_codepage_to_encoding(UINT cp)
{
	typedpg_unavailable("pg_codepage_to_encoding");
	pg_unreachable();
}

void
pg_usleep(long microsec)
{
	if (microsec > 0)
		SleepEx((microsec < 500 ? 1 : (microsec + 500) / 1000), FALSE);
}

#endif /* _WIN32 */
