#ifndef TYPEDPG_PLPGSQL_JSON_H
#define TYPEDPG_PLPGSQL_JSON_H

#include "postgres.h"
#include "plpgsql.h"

char	   *plpgsqlToJSON(PLpgSQL_function *func);

#endif							/* TYPEDPG_PLPGSQL_JSON_H */
