/*
 * catalog.c — the caller's catalog as PostgreSQL's syscache sees it.
 *
 * See typedpg_catalog.h.  This file installs the callbacks, builds the heap
 * tuples the syscache hands out, and keeps the one fake pg_proc row a
 * compilation registers.
 */
#include "postgres.h"

#include "access/htup_details.h"
#include "catalog/namespace.h"
#include "lib/stringinfo.h"
#include "utils/builtins.h"
#include "utils/guc_hooks.h"
#include "utils/memutils.h"
#include "utils/ruleutils.h"

#include "typedpg_catalog.h"

/* The installed catalog, or NULL when the built-in rows answer. */
static TypedpgCatalog const *current_catalog = NULL;

/* The search path shim_init() sets, the one an empty database has. */
#define TYPEDPG_DEFAULT_SEARCH_PATH "\"$user\", public"

/* The fake pg_proc row: at most one compilation runs at a time. */
static Oid	current_proc_oid = InvalidOid;
static TypedpgTuple *current_proc_tuple = NULL;

/* namespace_search_path's storage, kept in TopMemoryContext. */
static char *search_path_string = NULL;

static void
install_search_path(const char *path)
{
	if (search_path_string != NULL)
		pfree(search_path_string);
	search_path_string = MemoryContextStrdup(TopMemoryContext, path);
	namespace_search_path = search_path_string;
	assign_search_path(namespace_search_path, NULL);
}

void
typedpg_catalog_install(const TypedpgCatalog *catalog)
{
	if (catalog == NULL)
	{
		current_catalog = NULL;
		install_search_path(TYPEDPG_DEFAULT_SEARCH_PATH);
		typedpg_invalidate_syscaches();
		return;
	}

	/*
	 * PostgreSQL's search path is a GUC string of quoted schema names.
	 * Rendering the caller's path here and letting namespace.c parse it back
	 * keeps every consumer (recomputeNamespacePath, preprocessNamespacePath,
	 * the $user and pg_temp cases) on PostgreSQL's own code.
	 */
	{
		size_t		capacity = 16;
		Oid		   *oids = palloc(capacity * sizeof(Oid));
		size_t		n;
		StringInfoData buf;

		n = catalog->search_path(catalog->ctx, oids, capacity);
		if (n > capacity)
		{
			oids = palloc(n * sizeof(Oid));
			capacity = n;
			n = catalog->search_path(catalog->ctx, oids, capacity);
		}
		if (n > capacity)
			n = capacity;

		initStringInfo(&buf);
		for (size_t i = 0; i < n; i++)
		{
			const char *name = catalog->namespace_name(catalog->ctx, oids[i]);

			if (name == NULL)
				continue;
			if (buf.len > 0)
				appendStringInfoString(&buf, ", ");
			appendStringInfoString(&buf, quote_identifier(name));
		}
		pfree(oids);

		install_search_path(buf.data);
		pfree(buf.data);
	}

	current_catalog = catalog;
	typedpg_invalidate_syscaches();
}

bool
typedpg_catalog_active(void)
{
	return current_catalog != NULL;
}

const TypedpgCatalog *
typedpg_catalog(void)
{
	return current_catalog;
}

/*
 * Build a heap tuple whose fixed payload is `row` (a catalog row struct) and
 * whose variable-length attributes are set later with
 * typedpg_tuple_set_attr().  Allocations go to the current context: the
 * caller deletes it when the call ends.
 */
TypedpgTuple *
typedpg_tuple_create(const void *row, Size size, int natts)
{
	TypedpgTuple *tuple = palloc0(sizeof(TypedpgTuple));
	Size		hoff = MAXALIGN(SizeofHeapTupleHeader);
	Size		len = hoff + size;
	HeapTupleHeader td;

	if (row != NULL)
	{
		tuple->tup.t_data = td = (HeapTupleHeader) palloc0(len);
		tuple->tup.t_len = len;
		ItemPointerSetInvalid(&tuple->tup.t_self);
		tuple->tup.t_tableOid = InvalidOid;
		HeapTupleHeaderSetDatumLength(td, len);
		ItemPointerSetInvalid(&td->t_ctid);
		HeapTupleHeaderSetNatts(td, natts);
		td->t_hoff = hoff;
		memcpy((char *) td + hoff, row, size);
	}

	tuple->natts = natts;
	tuple->values = palloc0(sizeof(Datum) * natts);
	tuple->nulls = palloc(sizeof(bool) * natts);
	memset(tuple->nulls, true, sizeof(bool) * natts);
	return tuple;
}

void
typedpg_tuple_set_attr(TypedpgTuple *tuple, AttrNumber attnum, Datum value)
{
	if (attnum < 1 || attnum > tuple->natts)
		elog(ERROR, "typedpg: attribute %d out of range", attnum);
	tuple->values[attnum - 1] = value;
	tuple->nulls[attnum - 1] = false;
}

/* A fresh xmin per compilation: funccache.c keys its cache on it. */
void
typedpg_tuple_set_xmin(TypedpgTuple *tuple, TransactionId xmin)
{
	HeapTupleHeaderSetXmin(tuple->tup.t_data, xmin);
}

Datum
typedpg_tuple_get_attr(HeapTuple tuple, AttrNumber attnum, bool *isNull)
{
	TypedpgTuple *wrapper = (TypedpgTuple *) tuple;

	if (attnum < 1 || attnum > wrapper->natts)
		elog(ERROR, "invalid attribute number %d for a catalog tuple", attnum);
	*isNull = wrapper->nulls[attnum - 1];
	return wrapper->values[attnum - 1];
}

void
typedpg_proc_install(Oid oid, TypedpgTuple *tuple)
{
	current_proc_oid = oid;
	current_proc_tuple = tuple;
}

HeapTuple
typedpg_proc_lookup(Oid oid)
{
	if (current_proc_tuple != NULL && oid == current_proc_oid)
		return &current_proc_tuple->tup;
	return NULL;
}

HeapTuple
typedpg_proc_tuple(void)
{
	return current_proc_tuple ? &current_proc_tuple->tup : NULL;
}

void
typedpg_proc_uninstall(void)
{
	current_proc_oid = InvalidOid;
	current_proc_tuple = NULL;
}
