/*
 * syscache.c — the syscache, answered from the caller's catalog or, with no
 * catalog installed, from the release's built-in pg_type and pg_namespace
 * rows (generated into postgres/typedpg/builtin_catalog.c from postgres.bki).
 *
 * PostgreSQL's parser and PL/pgSQL compiler reach every catalog object
 * through these entry points.  Outside a server there is no heap to scan, so
 * each lookup builds a real tuple of the catalog row struct instead, with the
 * variable-length attributes (pg_proc's prosrc, proargtypes' neighbours, …)
 * in the values[]/nulls[] arrays SysCacheGetAttr reads back (see
 * typedpg_catalog.h).
 */
#include "postgres.h"
#include "access/htup_details.h"
#include "catalog/pg_attribute.h"
#include "catalog/pg_authid.h"
#include "catalog/pg_class.h"
#include "catalog/pg_collation.h"
#include "catalog/pg_namespace.h"
#include "catalog/pg_proc.h"
#include "catalog/pg_type.h"
#include "utils/catcache.h"
#include "utils/memutils.h"
#include "utils/syscache.h"

#include "typedpg_catalog.h"

extern const FormData_pg_type typedpg_builtin_types[];
extern const int typedpg_builtin_ntypes;
extern const FormData_pg_namespace typedpg_builtin_namespaces[];
extern const int typedpg_builtin_nnamespaces;
extern const FormData_pg_collation typedpg_builtin_collations[];
extern const int typedpg_builtin_ncollations;

/* The built-in rows as tuples, built once in TopMemoryContext. */
static HeapTuple *type_tuples;
static HeapTuple *namespace_tuples;
static HeapTuple *collation_tuples;

static void
init_builtin(void)
{
	MemoryContext old;

	if (type_tuples)
		return;
	old = MemoryContextSwitchTo(TopMemoryContext);
	type_tuples = palloc(sizeof(HeapTuple) * typedpg_builtin_ntypes);
	for (int i = 0; i < typedpg_builtin_ntypes; i++)
		type_tuples[i] = (HeapTuple)
			typedpg_tuple_create(&typedpg_builtin_types[i],
								 sizeof(FormData_pg_type), Natts_pg_type);
	namespace_tuples = palloc(sizeof(HeapTuple) * typedpg_builtin_nnamespaces);
	for (int i = 0; i < typedpg_builtin_nnamespaces; i++)
		namespace_tuples[i] = (HeapTuple)
			typedpg_tuple_create(&typedpg_builtin_namespaces[i],
								 sizeof(FormData_pg_namespace), Natts_pg_namespace);
	collation_tuples = palloc(sizeof(HeapTuple) * typedpg_builtin_ncollations);
	for (int i = 0; i < typedpg_builtin_ncollations; i++)
		collation_tuples[i] = (HeapTuple)
			typedpg_tuple_create(&typedpg_builtin_collations[i],
								 sizeof(FormData_pg_collation), Natts_pg_collation);
	MemoryContextSwitchTo(old);
}

/* --- rows built from the installed catalog -------------------------------- */

/* The release's row for a built-in type, or NULL. */
static const FormData_pg_type *
builtin_type_row(Oid oid)
{
	for (int i = 0; i < typedpg_builtin_ntypes; i++)
		if (typedpg_builtin_types[i].oid == oid)
			return &typedpg_builtin_types[i];
	return NULL;
}

static HeapTuple
type_tuple(const TypedpgType *t)
{
	FormData_pg_type *row = palloc0(sizeof(FormData_pg_type));
	const FormData_pg_type *builtin = builtin_type_row(t->oid);

	row->oid = t->oid;
	strlcpy(NameStr(row->typname), t->typname, NAMEDATALEN);
	row->typnamespace = t->typnamespace;
	row->typlen = t->typlen;
	row->typbyval = t->typbyval;
	row->typtype = t->typtype;
	row->typcategory = t->typcategory;
	row->typispreferred = t->typispreferred;
	row->typisdefined = t->typisdefined;
	row->typdelim = ',';
	row->typrelid = t->typrelid;
	row->typsubscript = t->typsubscript;
	row->typelem = t->typelem;
	row->typarray = t->typarray;
	row->typalign = t->typalign;
	row->typstorage = t->typlen == -1 ? TYPSTORAGE_EXTENDED : TYPSTORAGE_PLAIN;
	row->typnotnull = t->typnotnull;
	row->typbasetype = t->typbasetype;
	row->typtypmod = t->typtypmod;
	row->typcollation = t->typcollation;

	/*
	 * The function OIDs pg_type carries (input/output, typmod, analyze, …)
	 * are not part of the Catalog trait: take them from the release's row
	 * when this is a built-in type (a real database has the same ones), and
	 * leave them zero otherwise.  typmodin is what a type modifier needs.
	 */
	if (builtin != NULL)
	{
		row->typinput = builtin->typinput;
		row->typoutput = builtin->typoutput;
		row->typreceive = builtin->typreceive;
		row->typsend = builtin->typsend;
		row->typmodin = builtin->typmodin;
		row->typmodout = builtin->typmodout;
		row->typanalyze = builtin->typanalyze;
		row->typstorage = builtin->typstorage;
	}

	return (HeapTuple) typedpg_tuple_create(row, sizeof(FormData_pg_type),
											Natts_pg_type);
}

static HeapTuple
namespace_tuple(Oid oid, const char *name)
{
	FormData_pg_namespace *row = palloc0(sizeof(FormData_pg_namespace));

	row->oid = oid;
	strlcpy(NameStr(row->nspname), name, NAMEDATALEN);
	row->nspowner = BOOTSTRAP_SUPERUSERID;
	return (HeapTuple) typedpg_tuple_create(row, sizeof(FormData_pg_namespace),
											Natts_pg_namespace);
}

static HeapTuple
class_tuple(Oid oid, const char *name, Oid namespace, Oid reltype)
{
	FormData_pg_class *row = palloc0(sizeof(FormData_pg_class));

	row->oid = oid;
	strlcpy(NameStr(row->relname), name, NAMEDATALEN);
	row->relnamespace = namespace;
	row->reltype = reltype;
	row->relowner = BOOTSTRAP_SUPERUSERID;
	row->relpersistence = RELPERSISTENCE_PERMANENT;
	row->relkind = RELKIND_RELATION;
	row->relreplident = REPLICA_IDENTITY_DEFAULT;
	row->relispopulated = true;
	return (HeapTuple) typedpg_tuple_create(row, sizeof(FormData_pg_class),
											Natts_pg_class);
}

static HeapTuple
attribute_tuple(Oid relid, const char *name, const TypedpgAttribute *a)
{
	FormData_pg_attribute *row = palloc0(sizeof(FormData_pg_attribute));

	row->attrelid = relid;
	strlcpy(NameStr(row->attname), name, NAMEDATALEN);
	row->atttypid = a->atttypid;
	row->attnum = a->attnum;
	row->atttypmod = a->atttypmod;
	row->attcollation = a->attcollation;
	row->attisdropped = false;
	row->attislocal = true;
	return (HeapTuple) typedpg_tuple_create(row, sizeof(FormData_pg_attribute),
											Natts_pg_attribute);
}

static HeapTuple
lookup_catalog(int cacheId, Datum k1, Datum k2)
{
	const TypedpgCatalog *catalog = typedpg_catalog();

	switch (cacheId)
	{
		case TYPEOID:
			{
				TypedpgType row;

				if (!catalog->type_by_oid(catalog->ctx, DatumGetObjectId(k1), &row))
					return NULL;
				return type_tuple(&row);
			}
		case TYPENAMENSP:
			{
				Oid			oid = catalog->type_by_name(catalog->ctx,
													  DatumGetObjectId(k2),
													  DatumGetCString(k1));
				TypedpgType row;

				if (!OidIsValid(oid) ||
					!catalog->type_by_oid(catalog->ctx, oid, &row))
					return NULL;
				return type_tuple(&row);
			}
		case NAMESPACEOID:
			{
				Oid			oid = DatumGetObjectId(k1);
				const char *name = catalog->namespace_name(catalog->ctx, oid);

				if (name == NULL)
					return NULL;
				return namespace_tuple(oid, name);
			}
		case NAMESPACENAME:
			{
				const char *name = DatumGetCString(k1);
				Oid			oid = catalog->namespace_by_name(catalog->ctx, name);

				if (!OidIsValid(oid))
					return NULL;
				return namespace_tuple(oid, name);
			}
		case RELNAMENSP:
			{
				const char *name = DatumGetCString(k1);
				Oid			namespace = DatumGetObjectId(k2);
				Oid			relid = catalog->relation_by_name(catalog->ctx,
															  namespace, name);

				if (!OidIsValid(relid))
					return NULL;
				return class_tuple(relid, name, namespace,
								   catalog->relation_type(catalog->ctx, relid));
			}
		case RELOID:
			{
				Oid			relid = DatumGetObjectId(k1);
				Oid			reltype = catalog->relation_type(catalog->ctx, relid);

				/* Nothing else about the relation is exposed; the only
				 * column read through RELOID is reltype. */
				if (!OidIsValid(reltype))
					return NULL;
				return class_tuple(relid, "", InvalidOid, reltype);
			}
		case ATTNAME:
			{
				Oid			relid = DatumGetObjectId(k1);
				const char *name = DatumGetCString(k2);
				TypedpgAttribute a;

				if (!catalog->attribute_by_name(catalog->ctx, relid, name, &a))
					return NULL;
				return attribute_tuple(relid, name, &a);
			}
		case ATTNUM:
			{
				Oid			relid = DatumGetObjectId(k1);
				int16		attnum = DatumGetInt16(k2);
				TypedpgAttribute a;

				/*
				 * The catalog has no attribute names to hand: %TYPE reaches
				 * attributes by name, so only the type columns are read here.
				 */
				if (!catalog->attribute_by_number(catalog->ctx, relid, attnum, &a))
					return NULL;
				return attribute_tuple(relid, "", &a);
			}
		default:
			return NULL;
	}
}

/* --- built-in rows -------------------------------------------------------- */

static HeapTuple
lookup_builtin(int cacheId, Datum k1, Datum k2, Datum k3)
{
	init_builtin();
	switch (cacheId)
	{
		case COLLOID:
			for (int i = 0; i < typedpg_builtin_ncollations; i++)
				if (typedpg_builtin_collations[i].oid == DatumGetObjectId(k1))
					return collation_tuples[i];
			return NULL;
		case COLLNAMEENCNSP:
			for (int i = 0; i < typedpg_builtin_ncollations; i++)
				if (typedpg_builtin_collations[i].collnamespace == DatumGetObjectId(k3) &&
					typedpg_builtin_collations[i].collencoding == DatumGetInt32(k2) &&
					strcmp(NameStr(typedpg_builtin_collations[i].collname),
						   DatumGetCString(k1)) == 0)
					return collation_tuples[i];
			return NULL;
		case TYPEOID:
			for (int i = 0; i < typedpg_builtin_ntypes; i++)
				if (typedpg_builtin_types[i].oid == DatumGetObjectId(k1))
					return type_tuples[i];
			return NULL;
		case TYPENAMENSP:
			for (int i = 0; i < typedpg_builtin_ntypes; i++)
				if (typedpg_builtin_types[i].typnamespace == DatumGetObjectId(k2) &&
					strcmp(NameStr(typedpg_builtin_types[i].typname),
						   DatumGetCString(k1)) == 0)
					return type_tuples[i];
			return NULL;
		case NAMESPACEOID:
			for (int i = 0; i < typedpg_builtin_nnamespaces; i++)
				if (typedpg_builtin_namespaces[i].oid == DatumGetObjectId(k1))
					return namespace_tuples[i];
			return NULL;
		case NAMESPACENAME:
			for (int i = 0; i < typedpg_builtin_nnamespaces; i++)
				if (strcmp(NameStr(typedpg_builtin_namespaces[i].nspname),
						   DatumGetCString(k1)) == 0)
					return namespace_tuples[i];
			return NULL;
		default:
			return NULL;
	}
}

static HeapTuple
lookup(int cacheId, Datum k1, Datum k2, Datum k3, Datum k4)
{
	/* The fake pg_proc row a compilation registered answers either way. */
	if (cacheId == PROCOID)
	{
		HeapTuple	proc = typedpg_proc_lookup(DatumGetObjectId(k1));

		if (proc != NULL)
			return proc;
		if (!typedpg_catalog_active())
			return NULL;
	}

	/*
	 * Collations are not part of the Catalog trait, so the release's built-in
	 * pg_collation rows answer them either way (a real database has the same
	 * ones).
	 */
	if (cacheId == COLLOID || cacheId == COLLNAMEENCNSP)
		return lookup_builtin(cacheId, k1, k2, k3);

	if (typedpg_catalog_active())
		return lookup_catalog(cacheId, k1, k2);
	return lookup_builtin(cacheId, k1, k2, k3);
}

HeapTuple
SearchSysCache(int cacheId, Datum k1, Datum k2, Datum k3, Datum k4)
{
	return lookup(cacheId, k1, k2, k3, k4);
}

HeapTuple
SearchSysCache1(int cacheId, Datum k1)
{
	return lookup(cacheId, k1, 0, 0, 0);
}

HeapTuple
SearchSysCache2(int cacheId, Datum k1, Datum k2)
{
	return lookup(cacheId, k1, k2, 0, 0);
}

HeapTuple
SearchSysCache3(int cacheId, Datum k1, Datum k2, Datum k3)
{
	return lookup(cacheId, k1, k2, k3, 0);
}

HeapTuple
SearchSysCache4(int cacheId, Datum k1, Datum k2, Datum k3, Datum k4)
{
	return lookup(cacheId, k1, k2, k3, k4);
}

void
ReleaseSysCache(HeapTuple tuple)
{
}

bool
SearchSysCacheExists(int cacheId, Datum k1, Datum k2, Datum k3, Datum k4)
{
	return lookup(cacheId, k1, k2, k3, k4) != NULL;
}

Oid
GetSysCacheOid(int cacheId, AttrNumber oidcol, Datum k1, Datum k2,
			   Datum k3, Datum k4)
{
	HeapTuple	tup = lookup(cacheId, k1, k2, k3, k4);

	/* Every catalog this file serves keeps its oid first. */
	if (tup == NULL || oidcol != 1)
		return InvalidOid;
	return *(Oid *) GETSTRUCT(tup);
}

Datum
SysCacheGetAttr(int cacheId, HeapTuple tup, AttrNumber attributeNumber,
				bool *isNull)
{
	return typedpg_tuple_get_attr(tup, attributeNumber, isNull);
}

Datum
SysCacheGetAttrNotNull(int cacheId, HeapTuple tup, AttrNumber attributeNumber)
{
	bool		isnull;
	Datum		attr = SysCacheGetAttr(cacheId, tup, attributeNumber, &isnull);

	if (isnull)
		elog(ERROR, "unexpected null value in cached tuple for cache %d column %d",
			 cacheId, attributeNumber);
	return attr;
}

HeapTuple
SearchSysCacheAttName(Oid relid, const char *attname)
{
	return lookup(ATTNAME, ObjectIdGetDatum(relid), CStringGetDatum(attname), 0, 0);
}

bool
SearchSysCacheExistsAttName(Oid relid, const char *attname)
{
	return SearchSysCacheAttName(relid, attname) != NULL;
}

HeapTuple
SearchSysCacheAttNum(Oid relid, int16 attnum)
{
	return lookup(ATTNUM, ObjectIdGetDatum(relid), Int16GetDatum(attnum), 0, 0);
}

/* An empty list, or the fake pg_proc row for a PROCNAMEARGSNSP search. */
static CatCList *
empty_catclist(void)
{
	CatCList   *list = palloc0(offsetof(CatCList, members) + sizeof(CatCTup *));

	list->cl_magic = CL_MAGIC;
	list->refcount = 1;
	list->nkeys = 1;
	list->n_members = 0;
	return list;
}

CatCList *
SearchSysCacheList(int cacheId, int nkeys, Datum k1, Datum k2, Datum k3)
{
	if (cacheId == PROCNAMEARGSNSP && nkeys == 1)
	{
		HeapTuple	proc = typedpg_proc_tuple();
		CatCList   *list;

		if (proc == NULL ||
			strcmp(NameStr(((Form_pg_proc) GETSTRUCT(proc))->proname),
				   DatumGetCString(k1)) != 0)
			return empty_catclist();

		list = empty_catclist();
		list->n_members = 1;
		list->members[0] = palloc0(sizeof(CatCTup));
		list->members[0]->ct_magic = CT_MAGIC;
		list->members[0]->refcount = 1;
		list->members[0]->tuple = *proc;
		return list;
	}
	return empty_catclist();
}

void
ReleaseCatCacheList(CatCList *list)
{
}
