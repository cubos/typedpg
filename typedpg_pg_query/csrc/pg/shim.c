/*
 * shim.c — the crate's C entry points into PostgreSQL's own parser.
 *
 * PostgreSQL's parser keeps process-wide state (memory contexts, the error
 * machinery, the scanner's settings), so callers serialize every call.  Each
 * call runs PostgreSQL on a private memory context, with the stack base and
 * budget libpg_query used to set (see src/stack.rs), and catches ERRORs with
 * PG_TRY instead of letting them abort the process.
 *
 * The raw parse tree lives in the call's context: the caller converts it to
 * Rust before calling shim_finish(), which deletes the context.  Errors and
 * notices are copied into malloc'd strings that survive it.
 */
#include "postgres.h"

#include "catalog/namespace.h"
#include "common/keywords.h"
#include "gramparse.h"
#include "mb/pg_wchar.h"
#include "miscadmin.h"
#include "nodes/pg_list.h"
#include "parser/parser.h"
#include "parser/scanner.h"
#include "utils/elog.h"
#include "utils/guc.h"
#include "utils/guc_hooks.h"
#include "utils/memutils.h"

#include <stdlib.h>
#include <string.h>

/* src/stack.rs: the stack left below the caller. */
extern size_t typedpg_remaining_stack(void);

/* The flex entry points scan.c exposes but scanner.h does not declare. */
extern char *core_yyget_text(core_yyscan_t scanner);
extern int	core_yyget_leng(core_yyscan_t scanner);

typedef struct ShimError
{
	char	   *message;
	char	   *funcname;
	char	   *filename;
	int			lineno;
	int			cursorpos;
} ShimError;

typedef struct ShimParseResult
{
	List	   *tree;			/* in the call's context */
	char	   *notices;		/* malloc'd, WARNING/NOTICE text */
	ShimError  *error;			/* malloc'd, NULL on success */
	MemoryContext context;		/* freed by shim_finish() */
} ShimParseResult;

typedef struct ShimToken
{
	int			start;
	int			end;
	int			token;
	int			keyword_kind;	/* 0 = no keyword, else category + 1 */
} ShimToken;

typedef struct ShimScanResult
{
	ShimToken  *tokens;			/* malloc'd */
	int			ntokens;
	ShimError  *error;			/* malloc'd, NULL on success */
} ShimScanResult;

static bool initialized = false;

/* Notices of the call in progress, collected by capture_notices. */
static StringInfoData notices;
static bool capturing = false;

static char *
copy_string(const char *s)
{
	return s ? strdup(s) : NULL;
}

/* Keep what PostgreSQL would print below ERROR, and print nothing. */
static void
capture_notices(ErrorData *edata)
{
	if (!capturing)
		return;
	if (edata->elevel < ERROR)
	{
		appendStringInfo(&notices, "%s:  %s\n",
						 error_severity(edata->elevel),
						 edata->message ? edata->message : "");
		if (edata->detail)
			appendStringInfo(&notices, "DETAIL:  %s\n", edata->detail);
		if (edata->hint)
			appendStringInfo(&notices, "HINT:  %s\n", edata->hint);
		if (edata->context)
			appendStringInfo(&notices, "CONTEXT:  %s\n", edata->context);
	}
	/* Neither the caller's stderr nor a server log: the caller gets it. */
	edata->output_to_server = false;
}

/*
 * The one-time initialization a server's main() does for the parser:
 * PostgreSQL's memory contexts, UTF-8 as both the database and the message
 * encoding, the log hook that collects notices, and the search path the
 * catalog services expect.
 */
static void
shim_init(void)
{
	if (initialized)
		return;
	MemoryContextInit();
	SetDatabaseEncoding(PG_UTF8);
	SetMessageEncoding(PG_UTF8);
	emit_log_hook = capture_notices;
	namespace_search_path = "\"$user\", public";
	InitializeSearchPath();
	initialized = true;
}

/*
 * Size PostgreSQL's stack-depth budget from the stack the caller left below
 * this call, the way libpg_query's set_max_stack_depth() does: the headroom
 * minus STACK_DEPTH_SLOP, in kB, clamped to [100, 2048] (100 is the default
 * and the minimum, 2048 is what a server auto-detects).
 */
static void
shim_set_stack_depth(void)
{
	size_t		remaining = typedpg_remaining_stack();
	ssize_t		new_limit;

	if (remaining == 0)
		return;
	new_limit = ((ssize_t) remaining - STACK_DEPTH_SLOP) / 1024;
	if (new_limit <= 100)
		return;
	if (new_limit > 2048)
		new_limit = 2048;
	max_stack_depth = (int) new_limit;
	assign_max_stack_depth((int) new_limit, NULL);
}

static void
shim_enter(void)
{
	shim_init();
	set_stack_base();
	shim_set_stack_depth();
}

static void
shim_leave(void)
{
	restore_stack_base(NULL);
}

/* Copy the caught error out and clear PostgreSQL's error state. */
static ShimError *
take_error(void)
{
	ErrorData  *edata = CopyErrorData();
	ShimError  *error = calloc(1, sizeof(ShimError));

	error->message = copy_string(edata->message);
	error->funcname = copy_string(edata->funcname);
	error->filename = copy_string(edata->filename);
	error->lineno = edata->lineno;
	error->cursorpos = edata->cursorpos;
	FreeErrorData(edata);
	FlushErrorState();
	return error;
}

static void
free_error(ShimError *error)
{
	if (!error)
		return;
	free(error->message);
	free(error->funcname);
	free(error->filename);
	free(error);
}

/*
 * Run raw_parser() on `sql`.  `mode` is PostgreSQL's RawParseMode:
 * RAW_PARSE_DEFAULT (0) or RAW_PARSE_TYPE_NAME (1).
 */
ShimParseResult
shim_parse(const char *sql, int mode)
{
	ShimParseResult result = {0};
	MemoryContext ctx;

	shim_enter();
	ctx = AllocSetContextCreate(TopMemoryContext, "typedpg parse",
								ALLOCSET_DEFAULT_SIZES);
	result.context = ctx;
	MemoryContextSwitchTo(ctx);
	initStringInfo(&notices);
	capturing = true;
	PG_TRY();
	{
		result.tree = raw_parser(sql, (RawParseMode) mode);
	}
	PG_CATCH();
	{
		MemoryContextSwitchTo(ctx);
		result.error = take_error();
	}
	PG_END_TRY();
	capturing = false;
	if (notices.len > 0)
		result.notices = copy_string(notices.data);
	MemoryContextSwitchTo(TopMemoryContext);
	shim_leave();
	return result;
}

/* Release a parse result, including the context its tree lives in. */
void
shim_finish(ShimParseResult result)
{
	if (result.context)
		MemoryContextDelete(result.context);
	free(result.notices);
	free_error(result.error);
}

/* The keyword whose grammar token is `tok`, or -1. */
static int
keyword_index(int tok)
{
	for (int i = 0; i < ScanKeywords.num_keywords; i++)
		if (ScanKeywordTokens[i] == tok)
			return i;
	return -1;
}

/* Lex `sql`, returning its tokens; NULL tokens with an error set on failure. */
ShimScanResult
shim_scan(const char *sql)
{
	ShimScanResult result = {0};
	MemoryContext ctx;
	ShimToken  *tokens = NULL;

	shim_enter();
	ctx = AllocSetContextCreate(TopMemoryContext, "typedpg scan",
								ALLOCSET_DEFAULT_SIZES);
	MemoryContextSwitchTo(ctx);
	PG_TRY();
	{
		core_yy_extra_type extra;
		core_yyscan_t scanner = scanner_init(sql, &extra, &ScanKeywords,
											 ScanKeywordTokens);
		int			capacity = 64;
		int			n = 0;
		core_YYSTYPE yylval;
		YYLTYPE		yylloc;

		tokens = malloc(sizeof(ShimToken) * capacity);

		PG_TRY();
		{
			for (;;)
			{
				int			tok = core_yylex(&yylval, &yylloc, scanner);
				int			kw;

				if (tok == 0)
					break;
				if (n == capacity)
				{
					capacity *= 2;
					tokens = realloc(tokens, sizeof(ShimToken) * capacity);
				}
				kw = keyword_index(tok);
				tokens[n].start = yylloc;
				tokens[n].token = tok;
				tokens[n].keyword_kind = kw >= 0 ? ScanKeywordCategories[kw] + 1 : 0;
				tokens[n].end = (int) (core_yyget_text(scanner) - extra.scanbuf)
					+ core_yyget_leng(scanner);
				n++;
			}
			result.tokens = tokens;
			result.ntokens = n;
		}
		PG_FINALLY();
		{
			scanner_finish(scanner);
		}
		PG_END_TRY();
	}
	PG_CATCH();
	{
		MemoryContextSwitchTo(ctx);
		free(tokens);
		result.tokens = NULL;
		result.ntokens = 0;
		result.error = take_error();
	}
	PG_END_TRY();
	MemoryContextSwitchTo(TopMemoryContext);
	MemoryContextDelete(ctx);
	shim_leave();
	return result;
}

void
shim_scan_free(ShimScanResult result)
{
	free(result.tokens);
	free_error(result.error);
}
