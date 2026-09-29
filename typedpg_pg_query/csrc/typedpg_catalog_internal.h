/* Helpers the patched libpg_query sources call (see patches.rs). */
#ifndef TYPEDPG_CATALOG_INTERNAL_H
#define TYPEDPG_CATALOG_INTERNAL_H

#include "postgres.h"
#include "access/htup.h"
#include "nodes/pg_list.h"

extern bool typedpg_catalog_active(void);
extern HeapTuple typedpg_search_type(Oid typid);
extern Oid	typedpg_type_by_name(Oid namespace_oid, const char *name);
extern Oid	typedpg_lookup_namespace(const char *nspname, bool missing_ok);
extern char *typedpg_namespace_name(Oid nspid);
extern List *typedpg_search_path(void);
extern bool typedpg_type_is_visible(Oid typid);

#endif
