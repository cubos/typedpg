/*
 * syscache.c — the syscache, answered from typedpg's built-in catalog instead
 * of heap scans.  It serves the release's pg_type and pg_namespace rows
 * (generated into postgres/typedpg/builtin_catalog.c); later phases replace
 * that table with the analyzer's catalog.  Every other cache reports "not
 * found".
 */
#include "postgres.h"
#include "access/htup_details.h"
#include "catalog/pg_namespace.h"
#include "catalog/pg_type.h"
#include "utils/catcache.h"
#include "utils/memutils.h"
#include "utils/syscache.h"

extern const FormData_pg_type typedpg_builtin_types[];
extern const int typedpg_builtin_ntypes;
extern const FormData_pg_namespace typedpg_builtin_namespaces[];
extern const int typedpg_builtin_nnamespaces;

/* A heap tuple whose fixed part is `row`; built once, never freed. */
static HeapTuple
tuple(const void *row, Size size)
{
	MemoryContext old = MemoryContextSwitchTo(TopMemoryContext);
	Size		hoff = MAXALIGN(SizeofHeapTupleHeader);
	HeapTuple	tup = palloc0(HEAPTUPLESIZE + hoff + size);

	tup->t_data = (HeapTupleHeader) ((char *) tup + HEAPTUPLESIZE);
	tup->t_len = hoff + size;
	tup->t_data->t_hoff = hoff;
	memcpy((char *) tup->t_data + hoff, row, size);
	MemoryContextSwitchTo(old);
	return tup;
}

static HeapTuple *type_tuples;
static HeapTuple *namespace_tuples;

static void
init(void)
{
	if (type_tuples)
		return;
	type_tuples = MemoryContextAlloc(TopMemoryContext, sizeof(HeapTuple) * typedpg_builtin_ntypes);
	for (int i = 0; i < typedpg_builtin_ntypes; i++)
		type_tuples[i] = tuple(&typedpg_builtin_types[i], sizeof(FormData_pg_type));
	namespace_tuples = MemoryContextAlloc(TopMemoryContext, sizeof(HeapTuple) * typedpg_builtin_nnamespaces);
	for (int i = 0; i < typedpg_builtin_nnamespaces; i++)
		namespace_tuples[i] = tuple(&typedpg_builtin_namespaces[i], sizeof(FormData_pg_namespace));
}

static HeapTuple
lookup(int cacheId, Datum k1, Datum k2)
{
	init();
	switch (cacheId)
	{
		case TYPEOID:
			for (int i = 0; i < typedpg_builtin_ntypes; i++)
				if (typedpg_builtin_types[i].oid == DatumGetObjectId(k1))
					return type_tuples[i];
			return NULL;
		case TYPENAMENSP:
			for (int i = 0; i < typedpg_builtin_ntypes; i++)
				if (typedpg_builtin_types[i].typnamespace == DatumGetObjectId(k2) &&
					strcmp(NameStr(typedpg_builtin_types[i].typname), DatumGetCString(k1)) == 0)
					return type_tuples[i];
			return NULL;
		case NAMESPACEOID:
			for (int i = 0; i < typedpg_builtin_nnamespaces; i++)
				if (typedpg_builtin_namespaces[i].oid == DatumGetObjectId(k1))
					return namespace_tuples[i];
			return NULL;
		case NAMESPACENAME:
			for (int i = 0; i < typedpg_builtin_nnamespaces; i++)
				if (strcmp(NameStr(typedpg_builtin_namespaces[i].nspname), DatumGetCString(k1)) == 0)
					return namespace_tuples[i];
			return NULL;
		default:
			return NULL;
	}
}

HeapTuple
SearchSysCache(int cacheId, Datum k1, Datum k2, Datum k3, Datum k4)
{
	return lookup(cacheId, k1, k2);
}

HeapTuple
SearchSysCache1(int cacheId, Datum k1)
{
	return lookup(cacheId, k1, 0);
}

HeapTuple
SearchSysCache2(int cacheId, Datum k1, Datum k2)
{
	return lookup(cacheId, k1, k2);
}

HeapTuple
SearchSysCache3(int cacheId, Datum k1, Datum k2, Datum k3)
{
	return lookup(cacheId, k1, k2);
}

HeapTuple
SearchSysCache4(int cacheId, Datum k1, Datum k2, Datum k3, Datum k4)
{
	return lookup(cacheId, k1, k2);
}

void
ReleaseSysCache(HeapTuple tuple)
{
}

bool
SearchSysCacheExists(int cacheId, Datum k1, Datum k2, Datum k3, Datum k4)
{
	return lookup(cacheId, k1, k2) != NULL;
}

Oid
GetSysCacheOid(int cacheId, AttrNumber oidcol, Datum k1, Datum k2, Datum k3, Datum k4)
{
	HeapTuple	tup = lookup(cacheId, k1, k2);

	/* Both catalogs served keep their oid first. */
	return tup ? *(Oid *) GETSTRUCT(tup) : InvalidOid;
}

/* The served rows have no variable-length columns: all read as NULL. */
Datum
SysCacheGetAttr(int cacheId, HeapTuple tup, AttrNumber attributeNumber, bool *isNull)
{
	*isNull = true;
	return (Datum) 0;
}

Datum
SysCacheGetAttrNotNull(int cacheId, HeapTuple tup, AttrNumber attributeNumber)
{
	elog(ERROR, "typedpg: column %d of cache %d is not served", attributeNumber, cacheId);
	return (Datum) 0;
}

HeapTuple
SearchSysCacheAttName(Oid relid, const char *attname)
{
	return NULL;
}
