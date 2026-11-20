/*
 * parse_plpgsql.c — compile DO blocks and CREATE FUNCTION / PROCEDURE ...
 * LANGUAGE plpgsql statements with PostgreSQL's PL/pgSQL compiler, returning
 * the compiled functions as JSON.
 *
 * Derived from libpg_query's src/pg_query_parse_plpgsql.c (BSD-3-Clause; see
 * LICENSE in this directory), adapted to typedpg:
 *
 *  - PostgreSQL's sources are the real, unmodified ones (postgres/), so the
 *    compiler runs through plpgsql_compile() exactly as a validator does,
 *    against a fake pg_proc row registered in the syscache;
 *  - memory and errors follow csrc/pg/shim.c: one context per call, errors
 *    caught with PG_TRY and copied out as malloc'd strings;
 *  - the catalog is whatever typedpg_catalog_install() installed (the
 *    caller's Catalog, or the built-in pg_type/pg_namespace rows).
 */
#include "postgres.h"

#include <stdlib.h>
#include <string.h>

#include "catalog/objectaddress.h"
#include "catalog/pg_language_d.h"
#include "catalog/pg_proc.h"
#include "catalog/pg_type.h"
#include "commands/defrem.h"
#include "lib/stringinfo.h"
#include "mb/pg_wchar.h"
#include "nodes/nodeFuncs.h"
#include "nodes/parsenodes.h"
#include "parser/parse_type.h"
#include "parser/parser.h"
#include "utils/builtins.h"
#include "utils/lsyscache.h"
#include "utils/memutils.h"
#include "utils/syscache.h"

#include "shim_internal.h"
#include "typedpg_catalog.h"

#include "plpgsql_json.h"

/* One compilation at a time: the fake pg_proc row keeps this identity. */
static TransactionId next_proc_xmin = FirstNormalTransactionId;

typedef struct ShimPlpgsqlResult
{
	char	   *json;			/* malloc'd, NULL on error */
	char	   *notices;		/* malloc'd, WARNING/NOTICE text */
	ShimError  *error;			/* malloc'd, NULL on success */
} ShimPlpgsqlResult;

/*
 * Examine the RETURNS clause of the CREATE FUNCTION statement and return
 * information about it as *prorettype_p and *returnsSet_p.
 *
 * This is more complex than the average typename lookup because we want to
 * allow a shell type to be used, or even created if the specified return type
 * doesn't exist yet.  (Without this, there's no way to define the I/O procs
 * for a new type.)  But SQL function creation won't cope, so error out if the
 * target language is SQL.  Only C-coded functions can be I/O functions, which
 * is why a missing type is an error for every other language — the shell type
 * this function creates in a server is not created here.
 */
static void
compute_return_type(TypeName *returnType, Oid languageOid,
					Oid *prorettype_p, bool *returnsSet_p)
{
	Oid			rettype;
	Type		typtup;

	typtup = LookupTypeName(NULL, returnType, NULL, false);

	if (typtup)
	{
		if (!((Form_pg_type) GETSTRUCT(typtup))->typisdefined)
		{
			if (languageOid == SQLlanguageId)
				ereport(ERROR,
						(errcode(ERRCODE_INVALID_FUNCTION_DEFINITION),
						 errmsg("SQL function cannot return shell type %s",
								TypeNameToString(returnType))));
			else
				ereport(NOTICE,
						(errcode(ERRCODE_WRONG_OBJECT_TYPE),
						 errmsg("return type %s is only a shell",
								TypeNameToString(returnType))));
		}
		rettype = typeTypeId(typtup);
		ReleaseSysCache(typtup);
	}
	else
	{
		char	   *typnam = TypeNameToString(returnType);

		/*
		 * Only C-coded functions can be I/O functions.  We enforce this
		 * restriction here mainly to prevent littering the catalogs with
		 * shell types due to simple typos in user-defined function
		 * definitions.
		 */
		if (languageOid != INTERNALlanguageId &&
			languageOid != ClanguageId)
			ereport(ERROR,
					(errcode(ERRCODE_UNDEFINED_OBJECT),
					 errmsg("type \"%s\" does not exist", typnam)));

		/* Reject if there's typmod decoration, too */
		if (returnType->typmods != NIL)
			ereport(ERROR,
					(errcode(ERRCODE_SYNTAX_ERROR),
					 errmsg("type modifier cannot be specified for shell type \"%s\"",
							typnam)));

		/*
		 * PostgreSQL creates the shell type here (TypeShellMake) — a catalog
		 * write, which typedpg does not do; only C functions reach this.
		 */
		elog(ERROR, "typedpg: creating shell types is not supported");
	}

	*prorettype_p = rettype;
	*returnsSet_p = returnType->setof;
}

static PLpgSQL_function *
compile_do_stmt(DoStmt *stmt)
{
	char	   *proc_source = NULL;
	const ListCell *lc;
	char	   *language = "plpgsql";

	Assert(IsA(stmt, DoStmt));

	foreach(lc, stmt->args)
	{
		DefElem    *elem = (DefElem *) lfirst(lc);

		if (strcmp(elem->defname, "as") == 0)
		{
			Assert(IsA(elem->arg, String));
			proc_source = strVal(elem->arg);
		}
		else if (strcmp(elem->defname, "language") == 0)
			language = strVal(elem->arg);
	}

	if (proc_source == NULL)
		ereport(ERROR,
				(errcode(ERRCODE_SYNTAX_ERROR),
				 errmsg("no inline code specified")));

	if (strcmp(language, "plpgsql") != 0)
		return (PLpgSQL_function *) palloc0(sizeof(PLpgSQL_function));
	return plpgsql_compile_inline(proc_source);
}

/*
 * Build the fake pg_proc row: the fixed FormData_pg_proc payload, with room
 * for the proargtypes oidvector's values after it (the struct's last field),
 * plus the variable-length attributes SysCacheGetAttr hands out.
 *
 * This mirrors what upstream ProcedureCreate fills in, minus everything that
 * writes catalogs, checks ACLs or records dependencies.
 */
static TypedpgTuple *
pg_query_procedure_create(const char *procedureName,
						  bool returnsSet,
						  Oid returnType,
						  char prokind,
						  char volatility,
						  oidvector *parameterTypes,
						  Datum allParameterTypes,
						  Datum parameterModes,
						  Datum parameterNames,
						  const char *prosrc)
{
	TypedpgTuple *wrapper;
	FormData_pg_proc procStruct = {0};
	Size		size;
	FormData_pg_proc *row;
	int			parameterCount;

	Assert(PointerIsValid(prosrc));

	parameterCount = parameterTypes->dim1;

	/* Fixed-size attributes that GETSTRUCT reads back. */
	namestrcpy(&procStruct.proname, procedureName);
	procStruct.prokind = prokind;
	procStruct.provolatile = volatility;
	procStruct.pronargs = parameterCount;
	procStruct.prorettype = returnType;
	procStruct.proretset = returnsSet;
	procStruct.proargtypes = *parameterTypes;

	/* The oidvector's values follow the row struct. */
	size = sizeof(FormData_pg_proc) + parameterCount * sizeof(Oid);
	row = palloc0(size);
	memcpy(row, &procStruct, sizeof(FormData_pg_proc));
	if (parameterCount > 0)
		memcpy(row->proargtypes.values, parameterTypes->values,
			   parameterCount * sizeof(Oid));

	wrapper = typedpg_tuple_create(row, size, Natts_pg_proc);

	/*
	 * Anum-indexed attributes that get_func_arg_info reads via
	 * SysCacheGetAttr.  These mirror the assignments in upstream
	 * ProcedureCreate.
	 */
	if (allParameterTypes != PointerGetDatum(NULL))
		typedpg_tuple_set_attr(wrapper, Anum_pg_proc_proallargtypes,
							   allParameterTypes);
	if (parameterModes != PointerGetDatum(NULL))
		typedpg_tuple_set_attr(wrapper, Anum_pg_proc_proargmodes,
							   parameterModes);
	if (parameterNames != PointerGetDatum(NULL))
		typedpg_tuple_set_attr(wrapper, Anum_pg_proc_proargnames,
							   parameterNames);
	typedpg_tuple_set_attr(wrapper, Anum_pg_proc_prosrc,
						   CStringGetTextDatum(prosrc));

	return wrapper;
}

/*
 * Build the fake pg_proc row for a CREATE FUNCTION statement: pick the
 * language and source out of stmt->options, run
 * interpret_function_parameter_list and compute_return_type the way upstream
 * CreateFunction does, then hand back the row.  is_dml_trigger /
 * is_event_trigger are inferred from the RETURNS clause so the caller can
 * attach a TriggerData / EventTriggerData node to fcinfo->context, which is
 * what plpgsql_compile_callback inspects via CALLED_AS_TRIGGER /
 * CALLED_AS_EVENT_TRIGGER.
 */
static TypedpgTuple *
pg_query_create_function(CreateFunctionStmt *stmt,
						 const char *language,
						 const char *proc_source,
						 bool *is_dml_trigger,
						 bool *is_event_trigger)
{
	char	   *funcname;
	Oid			prorettype;
	bool		returnsSet;
	char		prokind;
	oidvector  *parameterTypes;
	List	   *parameterTypes_list = NIL;
	ArrayType  *allParameterTypes;
	ArrayType  *parameterModes;
	ArrayType  *parameterNames;
	List	   *inParameterNames_list = NIL;
	List	   *parameterDefaults;
	Oid			variadicArgType;
	Oid			requiredResultType;

	funcname = strVal(linitial(stmt->funcname));

	*is_dml_trigger = false;
	*is_event_trigger = false;
	if (stmt->returnType != NULL)
	{
		foreach_ptr(String, val, stmt->returnType->names)
		{
			if (strcmp(val->sval, "trigger") == 0)
				*is_dml_trigger = true;
			else if (strcmp(val->sval, "event_trigger") == 0)
				*is_event_trigger = true;
		}
	}

	interpret_function_parameter_list(palloc0(sizeof(ParseState)),
									  stmt->parameters,
									  InvalidOid,
									  stmt->is_procedure ? OBJECT_PROCEDURE : OBJECT_FUNCTION,
									  &parameterTypes,
									  &parameterTypes_list,
									  &allParameterTypes,
									  &parameterModes,
									  &parameterNames,
									  &inParameterNames_list,
									  &parameterDefaults,
									  &variadicArgType,
									  &requiredResultType);

	if (stmt->is_procedure)
	{
		Assert(!stmt->returnType);
		prorettype = requiredResultType ? requiredResultType : VOIDOID;
		returnsSet = false;
	}
	else if (stmt->returnType)
	{
		compute_return_type(stmt->returnType, InvalidOid,
							&prorettype, &returnsSet);
		if (OidIsValid(requiredResultType) && prorettype != requiredResultType)
			ereport(ERROR,
					(errcode(ERRCODE_INVALID_FUNCTION_DEFINITION),
					 errmsg("function result type must be %s because of OUT parameters",
							format_type_be(requiredResultType))));
	}
	else if (OidIsValid(requiredResultType))
	{
		prorettype = requiredResultType;
		returnsSet = false;
	}
	else
	{
		ereport(ERROR,
				(errcode(ERRCODE_INVALID_FUNCTION_DEFINITION),
				 errmsg("function result type must be specified")));
	}

	prokind = stmt->is_procedure ? PROKIND_PROCEDURE : PROKIND_FUNCTION;

	/*
	 * When there are no OUT args, interpret_function_parameter_list returns
	 * allParameterTypes=NULL.  get_func_arg_info then reads proargtypes,
	 * which our fake row carries.  Nothing else needs the array.
	 */
	return pg_query_procedure_create(funcname,
									 returnsSet,
									 prorettype,
									 prokind,
									 PROVOLATILE_VOLATILE,
									 parameterTypes,
									 allParameterTypes ? PointerGetDatum(allParameterTypes) : PointerGetDatum(NULL),
									 parameterModes ? PointerGetDatum(parameterModes) : PointerGetDatum(NULL),
									 parameterNames ? PointerGetDatum(parameterNames) : PointerGetDatum(NULL),
									 proc_source);
}

static PLpgSQL_function *
compile_create_function_stmt(CreateFunctionStmt *stmt)
{
	PLpgSQL_function *function;
	TypedpgTuple *wrapper;
	FmgrInfo	flinfo = {0};
	LOCAL_FCINFO(fcinfo, 0);
	TriggerData triggerData = {0};
	EventTriggerData eventTriggerData = {0};
	char	   *proc_source = NULL;
	char	   *language = "plpgsql";
	bool		is_dml_trigger;
	bool		is_event_trigger;

	Assert(IsA(stmt, CreateFunctionStmt));

	foreach_ptr(DefElem, elem, stmt->options)
	{
		if (strcmp(elem->defname, "as") == 0)
		{
			Assert(IsA(elem->arg, List));
			foreach_ptr(String, proc_source_str, (List *) elem->arg)
				proc_source = proc_source_str->sval;
		}
		else if (strcmp(elem->defname, "language") == 0)
		{
			language = strVal(elem->arg);
		}
	}

	{
		bool		language_given = false;

		foreach_ptr(DefElem, elem, stmt->options)
		{
			if (strcmp(elem->defname, "language") == 0)
				language_given = true;
		}
		/* CreateFunction: without LANGUAGE, an inline body is SQL. */
		if (!language_given && stmt->sql_body != NULL)
			return (PLpgSQL_function *) palloc0(sizeof(PLpgSQL_function));
	}

	if (strcmp(language, "plpgsql") != 0)
		return (PLpgSQL_function *) palloc0(sizeof(PLpgSQL_function));

	/* Mirrors the checks in upstream interpret_AS_clause */
	if (stmt->sql_body == NULL && proc_source == NULL)
		ereport(ERROR,
				(errcode(ERRCODE_INVALID_FUNCTION_DEFINITION),
				 errmsg("no function body specified")));

	if (stmt->sql_body != NULL && proc_source != NULL)
		ereport(ERROR,
				(errcode(ERRCODE_INVALID_FUNCTION_DEFINITION),
				 errmsg("duplicate function body specified")));

	if (stmt->sql_body != NULL)
		ereport(ERROR,
				(errcode(ERRCODE_INVALID_FUNCTION_DEFINITION),
				 errmsg("inline SQL function body only valid for language SQL")));

	wrapper = pg_query_create_function(stmt, language, proc_source,
									   &is_dml_trigger, &is_event_trigger);

	flinfo.fn_oid = InvalidOid;
	flinfo.fn_expr = NULL;
	fcinfo->flinfo = &flinfo;
	fcinfo->fncollation = InvalidOid;
	fcinfo->context = NULL;
	fcinfo->resultinfo = NULL;
	fcinfo->isnull = false;
	fcinfo->nargs = 0;

	if (is_dml_trigger)
	{
		triggerData.type = T_TriggerData;
		fcinfo->context = (Node *) &triggerData;
	}
	else if (is_event_trigger)
	{
		eventTriggerData.type = T_EventTriggerData;
		fcinfo->context = (Node *) &eventTriggerData;
	}

	/*
	 * A fresh xmin identifies this compilation to funccache.c, which keys
	 * its cache on the pg_proc row: a later compilation must not be handed
	 * this one's tree.
	 */
	typedpg_tuple_set_xmin(wrapper, next_proc_xmin++);
	typedpg_proc_install(flinfo.fn_oid, wrapper);

	function = plpgsql_compile(fcinfo, true);
	return function;
}

/* Compile every DO block and CREATE FUNCTION / PROCEDURE of `sql`. */
static char *
compile_statements(List *tree)
{
	StringInfoData json;
	int			nfuncs = 0;
	ListCell   *lc;

	initStringInfo(&json);
	appendStringInfoString(&json, "[\n");

	foreach(lc, tree)
	{
		RawStmt    *raw = lfirst_node(RawStmt, lc);
		Node	   *stmt = raw->stmt;
		PLpgSQL_function *func = NULL;
		char	   *func_json;

		if (IsA(stmt, CreateFunctionStmt))
			func = compile_create_function_stmt((CreateFunctionStmt *) stmt);
		else if (IsA(stmt, DoStmt))
			func = compile_do_stmt((DoStmt *) stmt);
		else
			continue;

		if (func == NULL)
			continue;

		PG_TRY();
		{
			func_json = plpgsqlToJSON(func);
		}
		PG_CATCH();
		{
			/* A compiled function lives in CacheMemoryContext: free it. */
			plpgsql_free_function_memory(func);
			typedpg_proc_uninstall();
			PG_RE_THROW();
		}
		PG_END_TRY();
		plpgsql_free_function_memory(func);
		typedpg_proc_uninstall();

		appendStringInfo(&json, "%s,\n", func_json);
		nfuncs++;
	}

	if (nfuncs == 0)
		return strdup("[]");

	json.data[json.len - 2] = '\n';
	json.data[json.len - 1] = ']';
	return strdup(json.data);
}

/*
 * The crate's entry point: parse `sql` and compile its PL/pgSQL.  With a
 * `catalog` installed, the compiler resolves types, schemas, relations and
 * columns against it; NULL means the built-in rows of an empty database.
 */
ShimPlpgsqlResult
shim_plpgsql(const char *sql, const TypedpgCatalog *catalog)
{
	ShimPlpgsqlResult result = {0};
	MemoryContext ctx;

	shim_enter();
	ctx = AllocSetContextCreate(TopMemoryContext, "typedpg plpgsql",
								ALLOCSET_DEFAULT_SIZES);
	MemoryContextSwitchTo(ctx);
	typedpg_notices_begin();
	PG_TRY();
	{
		List	   *tree;

		if (catalog != NULL)
			typedpg_catalog_install(catalog);

		tree = raw_parser(sql, RAW_PARSE_DEFAULT);
		result.json = compile_statements(tree);
	}
	PG_CATCH();
	{
		MemoryContextSwitchTo(ctx);
		result.error = typedpg_take_error();
	}
	PG_END_TRY();

	/* The catalog must not outlive the Rust session it points into. */
	if (catalog != NULL)
		typedpg_catalog_install(NULL);
	typedpg_proc_uninstall();

	result.notices = typedpg_notices_end();
	MemoryContextSwitchTo(TopMemoryContext);
	MemoryContextDelete(ctx);
	shim_leave();
	return result;
}

void
shim_plpgsql_free(ShimPlpgsqlResult result)
{
	free(result.json);
	free(result.notices);
	if (result.error)
	{
		free(result.error->message);
		free(result.error->funcname);
		free(result.error->filename);
		free(result.error);
	}
}
