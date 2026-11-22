/*
 * plpgsql_dump.c — what `#option dump` in a PL/pgSQL body does here.
 *
 * In the server the option makes the compiler print the function's tree to
 * stdout (pl_comp.c calls plpgsql_dumptree, pl_funcs.c), a debugging aid.
 * Here stdout belongs to the process running the analyzer — a `cargo
 * build`, an editor — so build.rs compiles pl_comp.c with that call renamed
 * to this function, which prints nothing: the option is accepted and has no
 * effect.
 */
#include "postgres.h"

#include "plpgsql.h"

void		typedpg_plpgsql_dumptree(PLpgSQL_function *func);

void
typedpg_plpgsql_dumptree(PLpgSQL_function *func)
{
	(void) func;
}
