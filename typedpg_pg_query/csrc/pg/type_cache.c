/*
 * type_cache.c — lookup_type_cache(), as far as compiling PL/pgSQL needs it.
 *
 * PostgreSQL's typcache.c takes its composite types' descriptors from the
 * relation cache, which typedpg does not carry (a Relation needs the storage
 * manager and the shared buffers).  PL/pgSQL's build_datatype asks the type
 * cache for exactly two things while compiling: a composite type's descriptor
 * (it only requires it to exist — the descriptor is kept to detect changes at
 * execution time), and a domain's base type.  Both are built here from the
 * syscache instead, which is backed by the caller's catalog.
 *
 * The catalog exposes a relation's attributes by number but not by
 * enumeration and without names, so the descriptor holds the columns
 * 1..n the catalog answers for and names them "att<n>".  Nothing typedpg does
 * with the descriptor reads those names; %TYPE reaches a column by name
 * through the ATTNAME syscache entry, which keeps the real one.
 */
#include "postgres.h"
#include "access/htup_details.h"
#include "access/tupdesc.h"
#include "catalog/pg_type.h"
#include "utils/lsyscache.h"
#include "utils/syscache.h"
#include "utils/typcache.h"

#include "typedpg_catalog.h"

static uint64 next_tupdesc_id = 1;

static TupleDesc
build_composite_tupdesc(Oid relid)
{
	const TypedpgCatalog *catalog = typedpg_catalog();
	TupleDesc	desc;
	TypedpgAttribute attr;
	int			natts = 0;

	if (catalog != NULL && OidIsValid(relid))
	{
		while (natts < MaxHeapAttributeNumber &&
			   catalog->attribute_by_number(catalog->ctx, relid, natts + 1, &attr))
			natts++;
	}

	desc = CreateTemplateTupleDesc(natts);
	for (int i = 1; i <= natts; i++)
	{
		if (!catalog->attribute_by_number(catalog->ctx, relid, i, &attr))
			continue;
		TupleDescInitEntry(desc, (AttrNumber) i, psprintf("att%d", i),
						   attr.atttypid, attr.atttypmod, 0);
		if (OidIsValid(attr.attcollation))
			TupleDescInitEntryCollation(desc, (AttrNumber) i, attr.attcollation);
	}
	return desc;
}

TypeCacheEntry *
lookup_type_cache(Oid type_id, int flags)
{
	TypeCacheEntry *entry = palloc0(sizeof(TypeCacheEntry));
	HeapTuple	tup = SearchSysCache1(TYPEOID, ObjectIdGetDatum(type_id));
	Form_pg_type t;

	if (!HeapTupleIsValid(tup))
		elog(ERROR, "cache lookup failed for type %u", type_id);
	t = (Form_pg_type) GETSTRUCT(tup);

	entry->type_id = type_id;
	entry->typlen = t->typlen;
	entry->typbyval = t->typbyval;
	entry->typalign = t->typalign;
	entry->typstorage = t->typstorage;
	entry->typtype = t->typtype;
	entry->typrelid = t->typrelid;
	entry->typsubscript = t->typsubscript;
	entry->typelem = t->typelem;
	entry->typarray = t->typarray;
	entry->typcollation = t->typcollation;
	entry->domainBaseType = t->typbasetype;
	entry->domainBaseTypmod = t->typtypmod;

	if ((flags & TYPECACHE_TUPDESC) != 0 && t->typtype == TYPTYPE_COMPOSITE)
	{
		entry->tupDesc = build_composite_tupdesc(t->typrelid);
		entry->tupDesc_identifier = next_tupdesc_id++;
	}

	ReleaseSysCache(tup);
	return entry;
}
