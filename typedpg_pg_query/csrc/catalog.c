/*
 * typedpg catalog hooks — see catalog.h.
 *
 * Besides the dispatch helpers the patched mocks call, this file supplies the
 * catalog functions libpg_query's extraction left out (relation and column
 * lookups), implemented over the installed catalog. They mirror PostgreSQL
 * 18's namespace.c / lsyscache.c / syscache.c, including their error
 * wording.
 */
#include "postgres.h"

#include "access/htup_details.h"
#include "catalog/namespace.h"
#include "catalog/pg_attribute.h"
#include "catalog/pg_type.h"
#include "nodes/pg_list.h"
#include "utils/lsyscache.h"
#include "utils/syscache.h"

#include "catalog.h"
#include "typedpg_catalog_internal.h"

static __thread const TypedpgCatalog *current_catalog = NULL;

void
typedpg_catalog_install(const TypedpgCatalog *catalog)
{
	current_catalog = catalog;
}

bool
typedpg_catalog_active(void)
{
	return current_catalog != NULL;
}

/* A heap tuple holding `form` (a catalog row struct of `size` bytes), built
 * the way libpg_query's pg_type mock builds its tuples. */
static HeapTuple
form_tuple(const void *form, Size size, int natts)
{
	HeapTuple	tuple;
	HeapTupleHeader td;
	Size		len;
	int			hoff;

	hoff = len = MAXALIGN(offsetof(HeapTupleHeaderData, t_bits));
	len += MAXALIGN(size);
	tuple = (HeapTuple) palloc0(HEAPTUPLESIZE + len);
	tuple->t_data = td = (HeapTupleHeader) ((char *) tuple + HEAPTUPLESIZE);
	tuple->t_len = len;
	ItemPointerSetInvalid(&(tuple->t_self));
	tuple->t_tableOid = InvalidOid;
	HeapTupleHeaderSetDatumLength(td, len);
	ItemPointerSetInvalid(&(td->t_ctid));
	HeapTupleHeaderSetNatts(td, natts);
	td->t_hoff = hoff;
	memcpy((char *) td + hoff, form, size);
	return tuple;
}

HeapTuple
typedpg_search_type(Oid typid)
{
	TypedpgType row;
	Form_pg_type t;

	memset(&row, 0, sizeof(row));
	if (!current_catalog->type_by_oid(current_catalog->ctx, typid, &row))
		return NULL;

	t = palloc0(sizeof(FormData_pg_type));
	t->oid = row.oid;
	strlcpy(NameStr(t->typname), row.typname, NAMEDATALEN);
	t->typnamespace = row.typnamespace;
	t->typlen = row.typlen;
	t->typbyval = row.typbyval;
	t->typtype = row.typtype;
	t->typcategory = row.typcategory;
	t->typispreferred = row.typispreferred;
	t->typisdefined = row.typisdefined;
	t->typdelim = ',';
	t->typrelid = row.typrelid;
	t->typsubscript = row.typsubscript;
	t->typelem = row.typelem;
	t->typarray = row.typarray;
	t->typalign = row.typalign;
	t->typstorage = row.typlen == -1 ? TYPSTORAGE_EXTENDED : TYPSTORAGE_PLAIN;
	t->typnotnull = row.typnotnull;
	t->typbasetype = row.typbasetype;
	t->typtypmod = row.typtypmod;
	t->typcollation = row.typcollation;
	return form_tuple(t, sizeof(FormData_pg_type), Natts_pg_type);
}

Oid
typedpg_type_by_name(Oid namespace_oid, const char *name)
{
	return current_catalog->type_by_name(current_catalog->ctx, namespace_oid, name);
}

/* LookupExplicitNamespace, over the catalog. */
Oid
typedpg_lookup_namespace(const char *nspname, bool missing_ok)
{
	Oid			nsp = current_catalog->namespace_by_name(current_catalog->ctx, nspname);

	if (!OidIsValid(nsp) && !missing_ok)
		ereport(ERROR,
				(errcode(ERRCODE_UNDEFINED_SCHEMA),
				 errmsg("schema \"%s\" does not exist", nspname)));
	return nsp;
}

char *
typedpg_namespace_name(Oid nspid)
{
	const char *name = current_catalog->namespace_name(current_catalog->ctx, nspid);

	return name ? pstrdup(name) : NULL;
}

List *
typedpg_search_path(void)
{
	uint32_t	oids[64];
	size_t		n = current_catalog->search_path(current_catalog->ctx, oids, lengthof(oids));
	List	   *path = NIL;

	for (size_t i = 0; i < n && i < lengthof(oids); i++)
		path = lappend_oid(path, oids[i]);
	return path;
}

/* TypeIsVisible: the type is the one its name finds along the search path. */
bool
typedpg_type_is_visible(Oid typid)
{
	TypedpgType row;
	List	   *path;
	ListCell   *l;

	memset(&row, 0, sizeof(row));
	if (!current_catalog->type_by_oid(current_catalog->ctx, typid, &row))
		return false;
	path = typedpg_search_path();
	foreach(l, path)
	{
		Oid			found = typedpg_type_by_name(lfirst_oid(l), row.typname);

		if (OidIsValid(found))
			return found == typid;
	}
	return false;
}

/*
 * Catalog functions the extraction left out, used by the restored %TYPE /
 * %ROWTYPE code. Outside a typedpg catalog they are unreachable.
 */

static void
require_catalog(const char *function)
{
	if (current_catalog == NULL)
		elog(ERROR, "Not implemented (%s needs a typedpg catalog)", function);
}

Oid
get_relname_relid(const char *relname, Oid relnamespace)
{
	require_catalog("get_relname_relid");
	return current_catalog->relation_by_name(current_catalog->ctx, relnamespace, relname);
}

Oid
RelnameGetRelid(const char *relname)
{
	List	   *path;
	ListCell   *l;

	require_catalog("RelnameGetRelid");
	path = typedpg_search_path();
	foreach(l, path)
	{
		Oid			relid = get_relname_relid(relname, lfirst_oid(l));

		if (OidIsValid(relid))
			return relid;
	}
	return InvalidOid;
}

/* RangeVarGetRelidExtended without locking or temporary relations: nothing
 * is locked outside a server. */
Oid
RangeVarGetRelidExtended(const RangeVar *relation, LOCKMODE lockmode, uint32 flags,
						 RangeVarGetRelidCallback callback, void *callback_arg)
{
	bool		missing_ok = (flags & RVR_MISSING_OK) != 0;
	Oid			relId;

	require_catalog("RangeVarGetRelidExtended");
	if (relation->catalogname)
		ereport(ERROR,
				(errcode(ERRCODE_FEATURE_NOT_SUPPORTED),
				 errmsg("cross-database references are not implemented: \"%s.%s.%s\"",
						relation->catalogname, relation->schemaname,
						relation->relname)));
	if (relation->schemaname)
	{
		Oid			namespaceId = typedpg_lookup_namespace(relation->schemaname, missing_ok);

		relId = OidIsValid(namespaceId) ? get_relname_relid(relation->relname, namespaceId)
			: InvalidOid;
	}
	else
		relId = RelnameGetRelid(relation->relname);

	if (callback)
		callback(relation, relId, InvalidOid, callback_arg);

	if (!OidIsValid(relId) && !missing_ok)
	{
		if (relation->schemaname)
			ereport(ERROR,
					(errcode(ERRCODE_UNDEFINED_TABLE),
					 errmsg("relation \"%s.%s\" does not exist",
							relation->schemaname, relation->relname)));
		else
			ereport(ERROR,
					(errcode(ERRCODE_UNDEFINED_TABLE),
					 errmsg("relation \"%s\" does not exist",
							relation->relname)));
	}
	return relId;
}

Oid
get_rel_type_id(Oid relid)
{
	require_catalog("get_rel_type_id");
	return current_catalog->relation_type(current_catalog->ctx, relid);
}

HeapTuple
SearchSysCacheAttName(Oid relid, const char *attname)
{
	TypedpgAttribute att;
	Form_pg_attribute a;

	require_catalog("SearchSysCacheAttName");
	memset(&att, 0, sizeof(att));
	if (!current_catalog->attribute_by_name(current_catalog->ctx, relid, attname, &att))
		return NULL;
	a = palloc0(sizeof(FormData_pg_attribute));
	a->attrelid = relid;
	strlcpy(NameStr(a->attname), attname, NAMEDATALEN);
	a->atttypid = att.atttypid;
	a->attnum = att.attnum;
	a->atttypmod = att.atttypmod;
	a->attcollation = att.attcollation;
	a->attisdropped = false;
	return form_tuple(a, sizeof(FormData_pg_attribute), Natts_pg_attribute);
}

AttrNumber
get_attnum(Oid relid, const char *attname)
{
	TypedpgAttribute att;

	require_catalog("get_attnum");
	memset(&att, 0, sizeof(att));
	if (!current_catalog->attribute_by_name(current_catalog->ctx, relid, attname, &att))
		return InvalidAttrNumber;
	return att.attnum;
}

Oid
get_atttype(Oid relid, AttrNumber attnum)
{
	TypedpgAttribute att;

	require_catalog("get_atttype");
	memset(&att, 0, sizeof(att));
	if (!current_catalog->attribute_by_number(current_catalog->ctx, relid, attnum, &att))
		return InvalidOid;
	return att.atttypid;
}
