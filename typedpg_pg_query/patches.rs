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
];
