/*
 * typedpg_catalog.h — the bridge between the caller's Rust Catalog and the
 * PostgreSQL code this crate compiles.
 *
 * PostgreSQL's parser and PL/pgSQL compiler read the system catalogs through
 * the syscache.  Outside a server there is no catalog to read, so
 * parse_plpgsql_with_catalog installs a TypedpgCatalog for the duration of
 * the call: the syscache (csrc/pg/syscache.c) answers every lookup from it,
 * building real heap tuples of the pg_type, pg_namespace, pg_class and
 * pg_attribute row structs.  With no catalog installed, the built-in rows of
 * the release's postgres.bki (postgres/typedpg/builtin_catalog.c) answer
 * instead, which is what an empty database holds.
 *
 * The callbacks return 0 / false / NULL for "not found".  Strings they return
 * must stay valid until the installing call returns.  They must not call back
 * into PostgreSQL.
 *
 * Rust side: src/catalog.rs (the same layout, in a #[repr(C)] struct).
 */
#ifndef TYPEDPG_CATALOG_H
#define TYPEDPG_CATALOG_H

#include "postgres.h"
#include "access/htup.h"

/* The pg_type columns the parser and the PL/pgSQL compiler read. */
typedef struct TypedpgType
{
	Oid			oid;
	const char *typname;
	Oid			typnamespace;
	int16		typlen;
	bool		typbyval;
	char		typtype;
	char		typcategory;
	bool		typispreferred;
	char		typalign;
	Oid			typrelid;
	Oid			typsubscript;
	Oid			typelem;
	Oid			typarray;
	Oid			typbasetype;
	int32		typtypmod;
	bool		typnotnull;
	Oid			typcollation;
	bool		typisdefined;
} TypedpgType;

/* The pg_attribute columns %TYPE reads. */
typedef struct TypedpgAttribute
{
	int16		attnum;
	Oid			atttypid;
	int32		atttypmod;
	Oid			attcollation;
} TypedpgAttribute;

typedef struct TypedpgCatalog
{
	void	   *ctx;
	bool		(*type_by_oid) (void *ctx, Oid oid, TypedpgType *out);
	Oid			(*type_by_name) (void *ctx, Oid namespace, const char *name);
	Oid			(*namespace_by_name) (void *ctx, const char *name);
	const char *(*namespace_name) (void *ctx, Oid namespace);
	/* Writes up to `capacity` schema OIDs, returns how many there are. */
	size_t		(*search_path) (void *ctx, Oid *out, size_t capacity);
	Oid			(*relation_by_name) (void *ctx, Oid namespace, const char *name);
	/* pg_class.reltype, 0 for a relkind without a row type. */
	Oid			(*relation_type) (void *ctx, Oid relation);
	bool		(*attribute_by_name) (void *ctx, Oid relation, const char *name,
									  TypedpgAttribute *out);
	bool		(*attribute_by_number) (void *ctx, Oid relation, int16 attnum,
										TypedpgAttribute *out);
} TypedpgCatalog;

/* Install `catalog` on the current thread (NULL uninstalls).  Either way the
 * search path is rebuilt (from the catalog, or the default) and the syscache
 * invalidation callbacks run, as changing the catalog would in a server. */
void		typedpg_catalog_install(const TypedpgCatalog *catalog);

bool		typedpg_catalog_active(void);
const TypedpgCatalog *typedpg_catalog(void);

/*
 * A catalog row as a heap tuple: the HeapTupleData first (so a HeapTuple can
 * be cast back to it), the fixed part after it, and the variable-length
 * attributes in values[]/nulls[], which SysCacheGetAttr reads.
 */
typedef struct TypedpgTuple
{
	HeapTupleData tup;			/* must be first */
	int			natts;
	Datum	   *values;			/* [natts], 0-based (attrno - 1) */
	bool	   *nulls;			/* [natts], true = NULL */
} TypedpgTuple;

TypedpgTuple *typedpg_tuple_create(const void *row, Size size, int natts);
void		typedpg_tuple_set_attr(TypedpgTuple *tuple, AttrNumber attnum, Datum value);
void		typedpg_tuple_set_xmin(TypedpgTuple *tuple, TransactionId xmin);
Datum		typedpg_tuple_get_attr(HeapTuple tuple, AttrNumber attnum, bool *isNull);

/*
 * The fake pg_proc row a compilation registers: PROCOID lookups answer from
 * it, so the compiler can run through plpgsql_compile() exactly as a
 * validator would.  `oid` is InvalidOid, which no real function has.
 */
void		typedpg_proc_install(Oid oid, TypedpgTuple *tuple);
HeapTuple	typedpg_proc_lookup(Oid oid);
/* The registered row, whatever its oid (used to answer list searches). */
HeapTuple	typedpg_proc_tuple(void);
void		typedpg_proc_uninstall(void);

/* Replay the callbacks CacheRegisterSyscacheCallback recorded (server.c). */
void		typedpg_invalidate_syscaches(void);

#endif							/* TYPEDPG_CATALOG_H */
