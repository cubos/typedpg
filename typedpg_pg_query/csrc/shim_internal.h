/*
 * shim_internal.h — what csrc/pg/shim.c and csrc/plpgsql/parse_plpgsql.c
 * share: the error a caught PostgreSQL ERROR becomes, and the notice
 * collector.  Both are malloc'd so they survive the call's memory context.
 */
#ifndef TYPEDPG_SHIM_INTERNAL_H
#define TYPEDPG_SHIM_INTERNAL_H

#include "postgres.h"

typedef struct ShimError
{
	char	   *message;
	char	   *funcname;
	char	   *filename;
	int			lineno;
	int			cursorpos;
} ShimError;

/* Copy the current error out with CopyErrorData, then clear the error state. */
ShimError  *typedpg_take_error(void);

/* Start / stop collecting WARNING and NOTICE output for the call. */
void		typedpg_notices_begin(void);
char	   *typedpg_notices_end(void);

/* The per-call entry points: stack base and budget, and the log hook. */
void		shim_enter(void);
void		shim_leave(void);

#endif							/* TYPEDPG_SHIM_INTERNAL_H */
