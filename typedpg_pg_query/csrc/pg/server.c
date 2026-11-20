/*
 * server.c — the server services the copied PostgreSQL sources call on paths
 * typedpg runs: one superuser, every privilege granted, no concurrency (locks
 * are no-ops) and always inside a transaction.
 *
 * The syscache invalidation callbacks are recorded so that installing or
 * removing the caller's catalog (typedpg_catalog.h) can replay them: to
 * namespace.c, that is exactly what an invalidation storm after a catalog
 * change looks like, so its search path cache is rebuilt against the new
 * catalog.
 */
#include "postgres.h"
#include "miscadmin.h"
#include "catalog/pg_authid.h"
#include "storage/lmgr.h"
#include "utils/acl.h"
#include "utils/inval.h"
#include "access/xact.h"
#include "access/xlog.h"

#include "typedpg_catalog.h"

#define MAX_SYSCACHE_CALLBACKS 16

static SyscacheCallbackFunction syscache_callbacks[MAX_SYSCACHE_CALLBACKS];
static Datum syscache_callback_args[MAX_SYSCACHE_CALLBACKS];
static int	syscache_callback_cacheids[MAX_SYSCACHE_CALLBACKS];
static int	nsyscache_callbacks = 0;

Oid
GetUserId(void)
{
	return BOOTSTRAP_SUPERUSERID;
}

AclResult
object_aclcheck(Oid classid, Oid objectid, Oid roleid, AclMode mode)
{
	return ACLCHECK_OK;
}

void
CacheRegisterSyscacheCallback(int cacheid, SyscacheCallbackFunction func, Datum arg)
{
	if (nsyscache_callbacks >= MAX_SYSCACHE_CALLBACKS)
		elog(ERROR, "typedpg: too many syscache callbacks");
	syscache_callbacks[nsyscache_callbacks] = func;
	syscache_callback_args[nsyscache_callbacks] = arg;
	syscache_callback_cacheids[nsyscache_callbacks] = cacheid;
	nsyscache_callbacks++;
}

void
typedpg_invalidate_syscaches(void)
{
	for (int i = 0; i < nsyscache_callbacks; i++)
		syscache_callbacks[i] (syscache_callback_args[i],
							   syscache_callback_cacheids[i], 0);
}

void
AcceptInvalidationMessages(void)
{
}

void
LockRelationOid(Oid relid, LOCKMODE lockmode)
{
}

void
UnlockRelationOid(Oid relid, LOCKMODE lockmode)
{
}

bool
IsTransactionState(void)
{
	return true;
}

bool
RecoveryInProgress(void)
{
	return false;
}
