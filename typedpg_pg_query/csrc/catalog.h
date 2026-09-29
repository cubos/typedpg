/*
 * typedpg catalog hooks: let libpg_query's catalog lookups read the caller's
 * catalog instead of its built-in mocks.
 *
 * libpg_query runs PostgreSQL's parser outside a server, so the lookups the
 * PL/pgSQL compiler makes (types by OID and name, schemas, the search path,
 * relations and columns for %TYPE / %ROWTYPE) have no catalog behind them.
 * While a TypedpgCatalog is installed on the current thread, the patched
 * lookups (see patches.rs) answer from it; otherwise the mocks stay in charge.
 *
 * Callbacks return 0 / false for "not found". Strings they return must stay
 * valid until the installing call returns. They must not call back into
 * libpg_query.
 */
#ifndef TYPEDPG_CATALOG_H
#define TYPEDPG_CATALOG_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

/* The pg_type columns the parser and the PL/pgSQL compiler read. */
typedef struct TypedpgType
{
	uint32_t	oid;
	const char *typname;
	uint32_t	typnamespace;
	int16_t		typlen;
	bool		typbyval;
	char		typtype;
	char		typcategory;
	bool		typispreferred;
	char		typalign;
	uint32_t	typrelid;
	uint32_t	typsubscript;
	uint32_t	typelem;
	uint32_t	typarray;
	uint32_t	typbasetype;
	int32_t		typtypmod;
	bool		typnotnull;
	uint32_t	typcollation;
	bool		typisdefined;
} TypedpgType;

/* The pg_attribute columns %TYPE reads. */
typedef struct TypedpgAttribute
{
	int16_t		attnum;
	uint32_t	atttypid;
	int32_t		atttypmod;
	uint32_t	attcollation;
} TypedpgAttribute;

typedef struct TypedpgCatalog
{
	void	   *ctx;
	bool		(*type_by_oid) (void *ctx, uint32_t oid, TypedpgType *out);
	uint32_t	(*type_by_name) (void *ctx, uint32_t namespace_oid, const char *name);
	uint32_t	(*namespace_by_name) (void *ctx, const char *name);
	const char *(*namespace_name) (void *ctx, uint32_t namespace_oid);
	/* Writes up to `capacity` schema OIDs, returns how many there are. */
	size_t		(*search_path) (void *ctx, uint32_t *out, size_t capacity);
	uint32_t	(*relation_by_name) (void *ctx, uint32_t namespace_oid, const char *name);
	/* pg_class.reltype, 0 for a relkind without a row type. */
	uint32_t	(*relation_type) (void *ctx, uint32_t relation_oid);
	bool		(*attribute_by_name) (void *ctx, uint32_t relation_oid, const char *name,
									  TypedpgAttribute *out);
	bool		(*attribute_by_number) (void *ctx, uint32_t relation_oid, int16_t attnum,
										TypedpgAttribute *out);
} TypedpgCatalog;

/* Install `catalog` on the current thread (NULL uninstalls). */
void		typedpg_catalog_install(const TypedpgCatalog *catalog);

#endif
