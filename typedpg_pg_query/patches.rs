// Patches applied to libpg_query's sources at build time (see build.rs).
//
// Each fixes a gap in the pinned libpg_query release that typedpg depends on.
// `find` must occur exactly once in `file`: when libpg_query changes a patched
// spot, the build fails and points here. Re-derive the patch then, or drop it
// once the fix has landed upstream.

struct Patch {
    file: &'static str,
    why: &'static str,
    find: &'static str,
    replace: &'static str,
}

const PATCHES: &[Patch] = &[
    Patch {
        file: "src/pg_query_json_plpgsql.c",
        why: "A block's DECLAREd variables (initvarnos: the datums exec_stmt_block initializes on entry) were not dumped, so a reader couldn't tell which block declares which variable.",
        find: r#"	WRITE_NODE_TYPE("PLpgSQL_stmt_block");

	WRITE_INT_FIELD(lineno, lineno, lineno);
	WRITE_STRING_FIELD(label, label, label);
"#,
        replace: r#"	WRITE_NODE_TYPE("PLpgSQL_stmt_block");

	WRITE_INT_FIELD(lineno, lineno, lineno);
	WRITE_STRING_FIELD(label, label, label);
	if (node->n_initvars > 0)
	{
		appendStringInfoString(out, "\"initvarnos\":[");
		for (int i = 0; i < node->n_initvars; i++)
			appendStringInfo(out, "%d,", node->initvarnos[i]);
		removeTrailingDelimiter(out);
		appendStringInfoString(out, "],");
	}
"#,
    },
    Patch {
        file: "src/pg_query_json_plpgsql.c",
        why: "A trigger function's TG_* variables are promise datums (a PLpgSQL_var filled in at call time); the dump wrote them as empty objects, making the whole output invalid JSON.",
        find: r#"			case PLPGSQL_DTYPE_RECFIELD:
				dump_record_field(out, (PLpgSQL_recfield *) d);
				break;
"#,
        replace: r#"			case PLPGSQL_DTYPE_RECFIELD:
				dump_record_field(out, (PLpgSQL_recfield *) d);
				break;
			case PLPGSQL_DTYPE_PROMISE:
				dump_var(out, (PLpgSQL_var *) d);
				break;
"#,
    },
    Patch {
        file: "src/pg_query_json_plpgsql.c",
        why: "RETURN of a variable in a function returning a tuple or set stores the variable in retvarno instead of expr; the dump left retvarno out (PLpgSQL_stmt_return).",
        find: r#"WRITE_NODE_TYPE("PLpgSQL_stmt_return");

	WRITE_INT_FIELD(lineno, lineno, lineno);
	WRITE_EXPR_FIELD(expr);
	//WRITE_INT_FIELD(retvarno);"#,
        replace: r#"WRITE_NODE_TYPE("PLpgSQL_stmt_return");

	WRITE_INT_FIELD(lineno, lineno, lineno);
	WRITE_EXPR_FIELD(expr);
	WRITE_INT_FIELD(retvarno, retvarno, retvarno);"#,
    },
    Patch {
        file: "src/pg_query_json_plpgsql.c",
        why: "RETURN of a variable in a function returning a tuple or set stores the variable in retvarno instead of expr; the dump left retvarno out (PLpgSQL_stmt_return_next).",
        find: r#"WRITE_NODE_TYPE("PLpgSQL_stmt_return_next");

	WRITE_INT_FIELD(lineno, lineno, lineno);
	WRITE_EXPR_FIELD(expr);
	//WRITE_INT_FIELD(retvarno);"#,
        replace: r#"WRITE_NODE_TYPE("PLpgSQL_stmt_return_next");

	WRITE_INT_FIELD(lineno, lineno, lineno);
	WRITE_EXPR_FIELD(expr);
	WRITE_INT_FIELD(retvarno, retvarno, retvarno);"#,
    },
    Patch {
        file: "src/pg_query_json_plpgsql.c",
        why: "The type name as written (origtypname) was not dumped; it is all that is left of a type the catalog-less parser couldn't resolve (compiled as a record).",
        find: r#"	WRITE_NODE_TYPE("PLpgSQL_type");

	WRITE_STRING_FIELD(typname, typname, typname);
"#,
        replace: r#"	WRITE_NODE_TYPE("PLpgSQL_type");

	WRITE_STRING_FIELD(typname, typname, typname);
	if (node->origtypname != NULL && node->origtypname->names != NIL)
	{
		ListCell   *lc;

		appendStringInfoString(out, "\"origtypname\":[");
		foreach(lc, node->origtypname->names)
		{
			_outToken(out, strVal(lfirst(lc)));
			appendStringInfoChar(out, ',');
		}
		removeTrailingDelimiter(out);
		appendStringInfoString(out, "],");
		if (node->origtypname->arrayBounds != NIL)
			appendStringInfo(out, "\"origtypname_array_bounds\":%d,",
							 list_length(node->origtypname->arrayBounds));
	}
"#,
    },
    Patch {
        file: "src/pg_query_json_plpgsql.c",
        why: "A record variable's declared type (NULL for a plain RECORD) was not dumped.",
        find: r#"	WRITE_NODE_TYPE("PLpgSQL_rec");

	WRITE_STRING_FIELD(refname, refname, refname);
	WRITE_INT_FIELD(dno, dno, dno);
	WRITE_INT_FIELD(lineno, lineno, lineno);
"#,
        replace: r#"	WRITE_NODE_TYPE("PLpgSQL_rec");

	WRITE_STRING_FIELD(refname, refname, refname);
	WRITE_INT_FIELD(dno, dno, dno);
	WRITE_INT_FIELD(lineno, lineno, lineno);
	WRITE_OBJ_FIELD(datatype, dump_type);
"#,
    },
    Patch {
        file: "src/postgres/src_backend_utils_cache_syscache.c",
        why: "The mock pg_type tuple had no typelem, so get_element_type() failed for every array and any VARIADIC function was rejected (VARIADIC parameter must be an array). An array's element is the built-in type whose typarray it is, and a true array has the array subscript handler (IsTrueArrayType).",
        find: r#"    t->typarray = bt->typarray;
"#,
        replace: r#"    t->typarray = bt->typarray;
    for (int i = 0; i < lengthof(pg_query_builtin_types); i++)
    {
        if (pg_query_builtin_types[i].typarray == DatumGetObjectId(key1))
        {
            t->typelem = pg_query_builtin_types[i].oid;
            t->typsubscript = F_ARRAY_SUBSCRIPT_HANDLER;
            break;
        }
    }
"#,
    },
    Patch {
        file: "src/postgres/src_backend_catalog_namespace.c",
        why: "Only pg_catalog and public were supported, so a type qualified by any other schema failed with Not implemented. Treat other schemas like public, whose unknown types the mocks assume to be row types.",
        find: r#"    if (strcmp(nspname, "public") == 0)
        return PG_PUBLIC_NAMESPACE;
"#,
        replace: r#"    if (strcmp(nspname, "public") == 0)
        return PG_PUBLIC_NAMESPACE;

    return PG_PUBLIC_NAMESPACE;
"#,
    },
    Patch {
        file: "src/postgres/src_backend_parser_parse_type.c",
        why: "The mocks resolve every user-defined type to RECORD (an assumed row type), so an array of one became the pseudo-type record[] and PL/pgSQL rejected the variable. Keep the assumed row type, except for a literal record[].",
        find: r#"		/* If an array reference, return the array type instead */
		if (typeName->arrayBounds != NIL)
			typoid = get_array_type(typoid);
"#,
        replace: r#"		/* If an array reference, return the array type instead */
		if (typeName->arrayBounds != NIL &&
			(typoid != RECORDOID || strcmp(typname, "record") == 0))
			typoid = get_array_type(typoid);
"#,
    },
    Patch {
        file: "src/postgres/src_backend_utils_cache_syscache.c",
        why: "F_ARRAY_SUBSCRIPT_HANDLER, used by the typelem patch below.",
        find: r#"#include "pg_query_pg_type.c"
"#,
        replace: r#"#include "pg_query_pg_type.c"
#include "utils/fmgroids.h"
"#,
    },
    Patch {
        file: "src/postgres/src_pl_plpgsql_src_pl_comp.c",
        why: "build_datatype never kept the type name as written (origtypname): the composite branch that sets it is commented out for the catalog-less build. Keep it, so the dump can report a type the mocks couldn't resolve.",
        find: r#"	else*/
	{
		typ->origtypname = NULL;
"#,
        replace: r#"	else*/
	{
		typ->origtypname = origtypname;
"#,
    },
    Patch {
        file: "src/postgres/src_backend_utils_cache_syscache.c",
        why: "The catalog hooks' declarations.",
        find: r#"#include "postgres.h"
"#,
        replace: r#"#include "postgres.h"
#include "typedpg_catalog_internal.h"
"#,
    },
    Patch {
        file: "src/postgres/src_backend_utils_cache_syscache.c",
        why: "Route pg_type lookups by OID to the installed catalog.",
        find: r#"    if (cacheId != TYPEOID)
        elog(ERROR, "Not implemented (SearchSysCache1 only supports TYPEOID cache (%d), got cache %d)", TYPEOID, cacheId);
"#,
        replace: r#"    if (typedpg_catalog_active() && cacheId == TYPEOID)
        return typedpg_search_type(DatumGetObjectId(key1));

    if (cacheId != TYPEOID)
        elog(ERROR, "Not implemented (SearchSysCache1 only supports TYPEOID cache (%d), got cache %d)", TYPEOID, cacheId);
"#,
    },
    Patch {
        file: "src/postgres/src_backend_utils_cache_syscache.c",
        why: "Route pg_type lookups by name and schema to the installed catalog.",
        find: r#"	if (cacheId != TYPENAMENSP)
        elog(ERROR, "Not implemented (GetSysCacheOid only supports TYPENAMENSP cache (%d), got cache %d)", TYPENAMENSP, cacheId);
"#,
        replace: r#"    if (typedpg_catalog_active() && cacheId == TYPENAMENSP)
        return typedpg_type_by_name(DatumGetObjectId(key2), DatumGetPointer(key1));

	if (cacheId != TYPENAMENSP)
        elog(ERROR, "Not implemented (GetSysCacheOid only supports TYPENAMENSP cache (%d), got cache %d)", TYPENAMENSP, cacheId);
"#,
    },
    Patch {
        file: "src/postgres/src_backend_catalog_namespace.c",
        why: "The catalog hooks' declarations.",
        find: r#"#include "postgres.h"
"#,
        replace: r#"#include "postgres.h"
#include "typedpg_catalog_internal.h"
"#,
    },
    Patch {
        file: "src/postgres/src_backend_catalog_namespace.c",
        why: "Resolve schemas with the installed catalog (PG's missing-schema error).",
        find: r#"	// CHANGED: Only support pg_catalog and public namespace
"#,
        replace: r#"	if (typedpg_catalog_active())
		return typedpg_lookup_namespace(nspname, missing_ok);

	// CHANGED: Only support pg_catalog and public namespace
"#,
    },
    Patch {
        file: "src/postgres/src_backend_catalog_namespace.c",
        why: "Take the search path from the installed catalog.",
        find: r#"activeSearchPath = list_make2_oid(PG_CATALOG_NAMESPACE, PG_PUBLIC_NAMESPACE);}"#,
        replace: r#"if (typedpg_catalog_active())
	activeSearchPath = typedpg_search_path();
else
	activeSearchPath = list_make2_oid(PG_CATALOG_NAMESPACE, PG_PUBLIC_NAMESPACE);}"#,
    },
    Patch {
        file: "src/postgres/src_backend_catalog_namespace.c",
        why: "TypeIsVisible (it decides whether messages schema-qualify a type) from the installed catalog.",
        find: r#"TypeIsVisible(Oid typid)
{
return true;}"#,
        replace: r#"TypeIsVisible(Oid typid)
{
if (typedpg_catalog_active())
	return typedpg_type_is_visible(typid);
return true;}"#,
    },
    Patch {
        file: "src/postgres/src_backend_utils_cache_lsyscache.c",
        why: "The catalog hooks' declarations.",
        find: r#"#include "postgres.h"
"#,
        replace: r#"#include "postgres.h"
#include "typedpg_catalog_internal.h"
"#,
    },
    Patch {
        file: "src/postgres/src_backend_utils_cache_lsyscache.c",
        why: "Schema names from the installed catalog (the NAMESPACEOID cache isn't mocked).",
        find: r#"get_namespace_name(Oid nspid)
{
	HeapTuple	tp;
"#,
        replace: r#"get_namespace_name(Oid nspid)
{
	HeapTuple	tp;

	if (typedpg_catalog_active())
		return typedpg_namespace_name(nspid);
"#,
    },
    Patch {
        file: "src/postgres/src_backend_parser_parse_type.c",
        why: "The catalog hooks' declarations.",
        find: r#"#include "postgres.h"
"#,
        replace: r#"#include "postgres.h"
#include "typedpg_catalog_internal.h"
"#,
    },
    Patch {
        file: "src/postgres/src_backend_parser_parse_type.c",
        why: "Restore PostgreSQL 18.4's %TYPE lookup (table.column%TYPE in a function signature) over the installed catalog; the mock only has Not implemented.",
        find: r#"	else if (typeName->pct_type)
	{
        // CHANGED: Not currently implemented, requires us to get type mappings from caller
        elog(ERROR, "Not implemented");
		/* Handle %TYPE reference to type of an existing field */
		//RangeVar   *rel = makeRangeVar(NULL, NULL, typeName->location);
		//char	   *field = NULL;
		//Oid			relid;
		//AttrNumber	attnum;

		/* deconstruct the name list */
		//switch (list_length(typeName->names))
		//{
		//	case 1:
		//		ereport(ERROR,
		//				(errcode(ERRCODE_SYNTAX_ERROR),
		//				 errmsg("improper %%TYPE reference (too few dotted names): %s",
		//						NameListToString(typeName->names)),
		//				 parser_errposition(pstate, typeName->location)));
		//		break;
		//	case 2:
		//		rel->relname = strVal(linitial(typeName->names));
		//		field = strVal(lsecond(typeName->names));
		//		break;
		//	case 3:
		//		rel->schemaname = strVal(linitial(typeName->names));
		//		rel->relname = strVal(lsecond(typeName->names));
		//		field = strVal(lthird(typeName->names));
		//		break;
		//	case 4:
		//		rel->catalogname = strVal(linitial(typeName->names));
		//		rel->schemaname = strVal(lsecond(typeName->names));
		//		rel->relname = strVal(lthird(typeName->names));
		//		field = strVal(lfourth(typeName->names));
		//		break;
		//	default:
		//		ereport(ERROR,
		//				(errcode(ERRCODE_SYNTAX_ERROR),
		//				 errmsg("improper %%TYPE reference (too many dotted names): %s",
		//						NameListToString(typeName->names)),
		//				 parser_errposition(pstate, typeName->location)));
		//		break;
		//}

		/*
		 * Look up the field.
		 *
		 * XXX: As no lock is taken here, this might fail in the presence of
		 * concurrent DDL.  But taking a lock would carry a performance
		 * penalty and would also require a permissions check.
		 */
		//relid = RangeVarGetRelid(rel, NoLock, missing_ok);
		//attnum = get_attnum(relid, field);
		//if (attnum == InvalidAttrNumber)
		//{
		//	if (missing_ok)
		//		typoid = InvalidOid;
		//	else
		//		ereport(ERROR,
		//				(errcode(ERRCODE_UNDEFINED_COLUMN),
		//				 errmsg("column \"%s\" of relation \"%s\" does not exist",
		//						field, rel->relname),
		//				 parser_errposition(pstate, typeName->location)));
		//}
		//else
		//{
		//	typoid = get_atttype(relid, attnum);
        //
		//	/* this construct should never have an array indicator */
		//	Assert(typeName->arrayBounds == NIL);
        //
		//	/* emit nuisance notice (intentionally not errposition'd) */
		//	ereport(NOTICE,
		//			(errmsg("type reference %s converted to %s",
		//					TypeNameToString(typeName),
		//					format_type_be(typoid))));
		//}
	}
"#,
        replace: r#"	else if (typeName->pct_type && typedpg_catalog_active())
	{
		/* Handle %TYPE reference to type of an existing field */
		RangeVar   *rel = makeRangeVar(NULL, NULL, typeName->location);
		char	   *field = NULL;
		Oid			relid;
		AttrNumber	attnum;

		/* deconstruct the name list */
		switch (list_length(typeName->names))
		{
			case 1:
				ereport(ERROR,
						(errcode(ERRCODE_SYNTAX_ERROR),
						 errmsg("improper %%TYPE reference (too few dotted names): %s",
								NameListToString(typeName->names)),
						 parser_errposition(pstate, typeName->location)));
				break;
			case 2:
				rel->relname = strVal(linitial(typeName->names));
				field = strVal(lsecond(typeName->names));
				break;
			case 3:
				rel->schemaname = strVal(linitial(typeName->names));
				rel->relname = strVal(lsecond(typeName->names));
				field = strVal(lthird(typeName->names));
				break;
			case 4:
				rel->catalogname = strVal(linitial(typeName->names));
				rel->schemaname = strVal(lsecond(typeName->names));
				rel->relname = strVal(lthird(typeName->names));
				field = strVal(lfourth(typeName->names));
				break;
			default:
				ereport(ERROR,
						(errcode(ERRCODE_SYNTAX_ERROR),
						 errmsg("improper %%TYPE reference (too many dotted names): %s",
								NameListToString(typeName->names)),
						 parser_errposition(pstate, typeName->location)));
				break;
		}

		/*
		 * Look up the field.
		 *
		 * XXX: As no lock is taken here, this might fail in the presence of
		 * concurrent DDL.  But taking a lock would carry a performance
		 * penalty and would also require a permissions check.
		 */
		relid = RangeVarGetRelid(rel, NoLock, missing_ok);
		attnum = get_attnum(relid, field);
		if (attnum == InvalidAttrNumber)
		{
			if (missing_ok)
				typoid = InvalidOid;
			else
				ereport(ERROR,
						(errcode(ERRCODE_UNDEFINED_COLUMN),
						 errmsg("column \"%s\" of relation \"%s\" does not exist",
								field, rel->relname),
						 parser_errposition(pstate, typeName->location)));
		}
		else
		{
			typoid = get_atttype(relid, attnum);

			/* this construct should never have an array indicator */
			Assert(typeName->arrayBounds == NIL);

			/* emit nuisance notice (intentionally not errposition'd) */
			ereport(NOTICE,
					(errmsg("type reference %s converted to %s",
							TypeNameToString(typeName),
							format_type_be(typoid))));
		}
	}
	else if (typeName->pct_type)
	{
        // CHANGED: Not currently implemented, requires us to get type mappings from caller
        elog(ERROR, "Not implemented");
		/* Handle %TYPE reference to type of an existing field */
		//RangeVar   *rel = makeRangeVar(NULL, NULL, typeName->location);
		//char	   *field = NULL;
		//Oid			relid;
		//AttrNumber	attnum;

		/* deconstruct the name list */
		//switch (list_length(typeName->names))
		//{
		//	case 1:
		//		ereport(ERROR,
		//				(errcode(ERRCODE_SYNTAX_ERROR),
		//				 errmsg("improper %%TYPE reference (too few dotted names): %s",
		//						NameListToString(typeName->names)),
		//				 parser_errposition(pstate, typeName->location)));
		//		break;
		//	case 2:
		//		rel->relname = strVal(linitial(typeName->names));
		//		field = strVal(lsecond(typeName->names));
		//		break;
		//	case 3:
		//		rel->schemaname = strVal(linitial(typeName->names));
		//		rel->relname = strVal(lsecond(typeName->names));
		//		field = strVal(lthird(typeName->names));
		//		break;
		//	case 4:
		//		rel->catalogname = strVal(linitial(typeName->names));
		//		rel->schemaname = strVal(lsecond(typeName->names));
		//		rel->relname = strVal(lthird(typeName->names));
		//		field = strVal(lfourth(typeName->names));
		//		break;
		//	default:
		//		ereport(ERROR,
		//				(errcode(ERRCODE_SYNTAX_ERROR),
		//				 errmsg("improper %%TYPE reference (too many dotted names): %s",
		//						NameListToString(typeName->names)),
		//				 parser_errposition(pstate, typeName->location)));
		//		break;
		//}

		/*
		 * Look up the field.
		 *
		 * XXX: As no lock is taken here, this might fail in the presence of
		 * concurrent DDL.  But taking a lock would carry a performance
		 * penalty and would also require a permissions check.
		 */
		//relid = RangeVarGetRelid(rel, NoLock, missing_ok);
		//attnum = get_attnum(relid, field);
		//if (attnum == InvalidAttrNumber)
		//{
		//	if (missing_ok)
		//		typoid = InvalidOid;
		//	else
		//		ereport(ERROR,
		//				(errcode(ERRCODE_UNDEFINED_COLUMN),
		//				 errmsg("column \"%s\" of relation \"%s\" does not exist",
		//						field, rel->relname),
		//				 parser_errposition(pstate, typeName->location)));
		//}
		//else
		//{
		//	typoid = get_atttype(relid, attnum);
        //
		//	/* this construct should never have an array indicator */
		//	Assert(typeName->arrayBounds == NIL);
        //
		//	/* emit nuisance notice (intentionally not errposition'd) */
		//	ereport(NOTICE,
		//			(errmsg("type reference %s converted to %s",
		//					TypeNameToString(typeName),
		//					format_type_be(typoid))));
		//}
	}
"#,
    },
    Patch {
        file: "src/postgres/src_pl_plpgsql_src_pl_comp.c",
        why: "The catalog hooks' declarations.",
        find: r#"#include "postgres.h"
"#,
        replace: r#"#include "postgres.h"
#include "typedpg_catalog_internal.h"
"#,
    },
    Patch {
        file: "src/postgres/src_pl_plpgsql_src_pl_comp.c",
        why: "Restore PostgreSQL 18.4's plpgsql_parse_wordtype (variable, table and column lookups for %TYPE / %ROWTYPE) over the installed catalog; the mock only echoes the text.",
        find: r#"PLpgSQL_type *
plpgsql_parse_wordtype(char *ident)
{
	PLpgSQL_type *typ;

	typ = (PLpgSQL_type *) palloc0(sizeof(PLpgSQL_type));
	typ->typname = psprintf("%s%%TYPE", ident);
	typ->ttype = PLPGSQL_TTYPE_SCALAR;
	return typ;
}
"#,
        replace: r#"static PLpgSQL_type *
typedpg_pg18_plpgsql_parse_wordtype(char *ident)
{
	PLpgSQL_nsitem *nse;

	/*
	 * Do a lookup in the current namespace stack
	 */
	nse = plpgsql_ns_lookup(plpgsql_ns_top(), false,
							ident, NULL, NULL,
							NULL);

	if (nse != NULL)
	{
		switch (nse->itemtype)
		{
			case PLPGSQL_NSTYPE_VAR:
				return ((PLpgSQL_var *) (plpgsql_Datums[nse->itemno]))->datatype;
			case PLPGSQL_NSTYPE_REC:
				return ((PLpgSQL_rec *) (plpgsql_Datums[nse->itemno]))->datatype;
			default:
				break;
		}
	}

	/* No match, complain */
	ereport(ERROR,
			(errcode(ERRCODE_UNDEFINED_OBJECT),
			 errmsg("variable \"%s\" does not exist", ident)));
	return NULL;				/* keep compiler quiet */
}

PLpgSQL_type *
plpgsql_parse_wordtype(char *ident)
{
	if (typedpg_catalog_active())
		return typedpg_pg18_plpgsql_parse_wordtype(ident);
	PLpgSQL_type *typ;

	typ = (PLpgSQL_type *) palloc0(sizeof(PLpgSQL_type));
	typ->typname = psprintf("%s%%TYPE", ident);
	typ->ttype = PLPGSQL_TTYPE_SCALAR;
	return typ;
}
"#,
    },
    Patch {
        file: "src/postgres/src_pl_plpgsql_src_pl_comp.c",
        why: "Restore PostgreSQL 18.4's plpgsql_parse_cwordtype (variable, table and column lookups for %TYPE / %ROWTYPE) over the installed catalog; the mock only echoes the text.",
        find: r#"PLpgSQL_type *
plpgsql_parse_cwordtype(List *idents)
{
	PLpgSQL_type *typ;

	typ = (PLpgSQL_type *) palloc0(sizeof(PLpgSQL_type));
	typ->typname = psprintf("%s%%TYPE", NameListToString(idents));
	typ->ttype = PLPGSQL_TTYPE_SCALAR;
	return typ;
}
"#,
        replace: r#"static PLpgSQL_type *
typedpg_pg18_plpgsql_parse_cwordtype(List *idents)
{
	PLpgSQL_type *dtype = NULL;
	PLpgSQL_nsitem *nse;
	int			nnames;
	RangeVar   *relvar = NULL;
	const char *fldname = NULL;
	Oid			classOid;
	HeapTuple	attrtup = NULL;
	HeapTuple	typetup = NULL;
	Form_pg_attribute attrStruct;
	MemoryContext oldCxt;

	/* Avoid memory leaks in the long-term function context */
	oldCxt = MemoryContextSwitchTo(plpgsql_compile_tmp_cxt);

	if (list_length(idents) == 2)
	{
		/*
		 * Do a lookup in the current namespace stack
		 */
		nse = plpgsql_ns_lookup(plpgsql_ns_top(), false,
								strVal(linitial(idents)),
								strVal(lsecond(idents)),
								NULL,
								&nnames);

		if (nse != NULL && nse->itemtype == PLPGSQL_NSTYPE_VAR)
		{
			/* Block-qualified reference to scalar variable. */
			dtype = ((PLpgSQL_var *) (plpgsql_Datums[nse->itemno]))->datatype;
			goto done;
		}
		else if (nse != NULL && nse->itemtype == PLPGSQL_NSTYPE_REC &&
				 nnames == 2)
		{
			/* Block-qualified reference to record variable. */
			dtype = ((PLpgSQL_rec *) (plpgsql_Datums[nse->itemno]))->datatype;
			goto done;
		}

		/*
		 * First word could also be a table name
		 */
		relvar = makeRangeVar(NULL,
							  strVal(linitial(idents)),
							  -1);
		fldname = strVal(lsecond(idents));
	}
	else
	{
		/*
		 * We could check for a block-qualified reference to a field of a
		 * record variable, but %TYPE is documented as applying to variables,
		 * not fields of variables.  Things would get rather ambiguous if we
		 * allowed either interpretation.
		 */
		List	   *rvnames;

		Assert(list_length(idents) > 2);
		rvnames = list_delete_last(list_copy(idents));
		relvar = makeRangeVarFromNameList(rvnames);
		fldname = strVal(llast(idents));
	}

	/* Look up relation name.  Can't lock it - we might not have privileges. */
	classOid = RangeVarGetRelid(relvar, NoLock, false);

	/*
	 * Fetch the named table field and its type
	 */
	attrtup = SearchSysCacheAttName(classOid, fldname);
	if (!HeapTupleIsValid(attrtup))
		ereport(ERROR,
				(errcode(ERRCODE_UNDEFINED_COLUMN),
				 errmsg("column \"%s\" of relation \"%s\" does not exist",
						fldname, relvar->relname)));
	attrStruct = (Form_pg_attribute) GETSTRUCT(attrtup);

	typetup = SearchSysCache1(TYPEOID,
							  ObjectIdGetDatum(attrStruct->atttypid));
	if (!HeapTupleIsValid(typetup))
		elog(ERROR, "cache lookup failed for type %u", attrStruct->atttypid);

	/*
	 * Found that - build a compiler type struct in the caller's cxt and
	 * return it.  Note that we treat the type as being found-by-OID; no
	 * attempt to re-look-up the type name will happen during invalidations.
	 */
	MemoryContextSwitchTo(oldCxt);
	dtype = build_datatype(typetup,
						   attrStruct->atttypmod,
						   attrStruct->attcollation,
						   NULL);
	MemoryContextSwitchTo(plpgsql_compile_tmp_cxt);

done:
	if (HeapTupleIsValid(attrtup))
		ReleaseSysCache(attrtup);
	if (HeapTupleIsValid(typetup))
		ReleaseSysCache(typetup);

	MemoryContextSwitchTo(oldCxt);
	return dtype;
}

PLpgSQL_type *
plpgsql_parse_cwordtype(List *idents)
{
	if (typedpg_catalog_active())
		return typedpg_pg18_plpgsql_parse_cwordtype(idents);
	PLpgSQL_type *typ;

	typ = (PLpgSQL_type *) palloc0(sizeof(PLpgSQL_type));
	typ->typname = psprintf("%s%%TYPE", NameListToString(idents));
	typ->ttype = PLPGSQL_TTYPE_SCALAR;
	return typ;
}
"#,
    },
    Patch {
        file: "src/postgres/src_pl_plpgsql_src_pl_comp.c",
        why: "Restore PostgreSQL 18.4's plpgsql_parse_wordrowtype (variable, table and column lookups for %TYPE / %ROWTYPE) over the installed catalog; the mock only echoes the text.",
        find: r#"PLpgSQL_type *
plpgsql_parse_wordrowtype(char *ident)
{
	PLpgSQL_type *typ;

	typ = (PLpgSQL_type *) palloc0(sizeof(PLpgSQL_type));
	typ->typname = psprintf("%s%%rowtype", ident);
	typ->ttype = PLPGSQL_TTYPE_SCALAR;
	return typ;
}
"#,
        replace: r#"static PLpgSQL_type *
typedpg_pg18_plpgsql_parse_wordrowtype(char *ident)
{
	Oid			classOid;
	Oid			typOid;

	/*
	 * Look up the relation.  Note that because relation rowtypes have the
	 * same names as their relations, this could be handled as a type lookup
	 * equally well; we use the relation lookup code path only because the
	 * errors thrown here have traditionally referred to relations not types.
	 * But we'll make a TypeName in case we have to do re-look-up of the type.
	 */
	classOid = RelnameGetRelid(ident);
	if (!OidIsValid(classOid))
		ereport(ERROR,
				(errcode(ERRCODE_UNDEFINED_TABLE),
				 errmsg("relation \"%s\" does not exist", ident)));

	/* Some relkinds lack type OIDs */
	typOid = get_rel_type_id(classOid);
	if (!OidIsValid(typOid))
		ereport(ERROR,
				(errcode(ERRCODE_WRONG_OBJECT_TYPE),
				 errmsg("relation \"%s\" does not have a composite type",
						ident)));

	/* Build and return the row type struct */
	return plpgsql_build_datatype(typOid, -1, InvalidOid,
								  makeTypeName(ident));
}

PLpgSQL_type *
plpgsql_parse_wordrowtype(char *ident)
{
	if (typedpg_catalog_active())
		return typedpg_pg18_plpgsql_parse_wordrowtype(ident);
	PLpgSQL_type *typ;

	typ = (PLpgSQL_type *) palloc0(sizeof(PLpgSQL_type));
	typ->typname = psprintf("%s%%rowtype", ident);
	typ->ttype = PLPGSQL_TTYPE_SCALAR;
	return typ;
}
"#,
    },
    Patch {
        file: "src/postgres/src_pl_plpgsql_src_pl_comp.c",
        why: "Restore PostgreSQL 18.4's plpgsql_parse_cwordrowtype (variable, table and column lookups for %TYPE / %ROWTYPE) over the installed catalog; the mock only echoes the text.",
        find: r#"PLpgSQL_type *
plpgsql_parse_cwordrowtype(List *idents)
{
	PLpgSQL_type *typ;

	typ = (PLpgSQL_type *) palloc0(sizeof(PLpgSQL_type));
	typ->typname = psprintf("%s%%rowtype", NameListToString(idents));
	typ->ttype = PLPGSQL_TTYPE_SCALAR;
	return typ;
}
"#,
        replace: r#"static PLpgSQL_type *
typedpg_pg18_plpgsql_parse_cwordrowtype(List *idents)
{
	Oid			classOid;
	Oid			typOid;
	RangeVar   *relvar;
	MemoryContext oldCxt;

	/*
	 * As above, this is a relation lookup but could be a type lookup if we
	 * weren't being backwards-compatible about error wording.
	 */

	/* Avoid memory leaks in long-term function context */
	oldCxt = MemoryContextSwitchTo(plpgsql_compile_tmp_cxt);

	/* Look up relation name.  Can't lock it - we might not have privileges. */
	relvar = makeRangeVarFromNameList(idents);
	classOid = RangeVarGetRelid(relvar, NoLock, false);

	/* Some relkinds lack type OIDs */
	typOid = get_rel_type_id(classOid);
	if (!OidIsValid(typOid))
		ereport(ERROR,
				(errcode(ERRCODE_WRONG_OBJECT_TYPE),
				 errmsg("relation \"%s\" does not have a composite type",
						relvar->relname)));

	MemoryContextSwitchTo(oldCxt);

	/* Build and return the row type struct */
	return plpgsql_build_datatype(typOid, -1, InvalidOid,
								  makeTypeNameFromNameList(idents));
}

PLpgSQL_type *
plpgsql_parse_cwordrowtype(List *idents)
{
	if (typedpg_catalog_active())
		return typedpg_pg18_plpgsql_parse_cwordrowtype(idents);
	PLpgSQL_type *typ;

	typ = (PLpgSQL_type *) palloc0(sizeof(PLpgSQL_type));
	typ->typname = psprintf("%s%%rowtype", NameListToString(idents));
	typ->ttype = PLPGSQL_TTYPE_SCALAR;
	return typ;
}
"#,
    },
    Patch {
        file: "src/pg_query_parse_plpgsql.c",
        why: "A CREATE FUNCTION with no string body (none at all, or a BEGIN ATOMIC / RETURN body) failed a C assert and aborted the process; raise interpret_AS_clause's errors (functioncmds.c) instead. Without LANGUAGE, an inline body is SQL, not PL/pgSQL.",
        find: r#"	assert(proc_source != NULL);

	if (strcmp(language, "plpgsql") != 0)
		return (PLpgSQL_function *) palloc0(sizeof(PLpgSQL_function));
"#,
        replace: r#"	{
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

	/* interpret_AS_clause (functioncmds.c) */
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
"#,
    },
];
