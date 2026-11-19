/*
 * server.c — the server services the copied PostgreSQL sources call on paths
 * typedpg runs: one superuser, every privilege granted, no concurrency (locks
 * and cache invalidations are no-ops) and always inside a transaction.
 */
#include "postgres.h"
#include "miscadmin.h"
#include "catalog/pg_authid.h"
#include "storage/lmgr.h"
#include "utils/acl.h"
#include "utils/inval.h"
#include "access/xact.h"
#include "access/xlog.h"

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
