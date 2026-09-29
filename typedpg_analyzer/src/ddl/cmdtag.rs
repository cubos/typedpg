//! Command tags (cmdtaglist.h / cmdtag.c) and the tag of a statement
//! (`CreateCommandTag`, utility.c): what event trigger `WHEN tag IN (...)`
//! filters name, and what a read-only transaction's refusal reports.

use typedpg_pg_query::protobuf::{
    DiscardMode, ObjectType, TransactionStmtKind, VariableSetKind, node,
};

/// `(name, event_trigger_ok, table_rewrite_ok)` of every command tag but
/// CMDTAG_UNKNOWN, as cmdtaglist.h lists them.
const COMMAND_TAGS: &[(&str, bool, bool)] = &[
    ("ALTER ACCESS METHOD", true, false),
    ("ALTER AGGREGATE", true, false),
    ("ALTER CAST", true, false),
    ("ALTER COLLATION", true, false),
    ("ALTER CONSTRAINT", true, false),
    ("ALTER CONVERSION", true, false),
    ("ALTER DATABASE", false, false),
    ("ALTER DEFAULT PRIVILEGES", true, false),
    ("ALTER DOMAIN", true, false),
    ("ALTER EVENT TRIGGER", false, false),
    ("ALTER EXTENSION", true, false),
    ("ALTER FOREIGN DATA WRAPPER", true, false),
    ("ALTER FOREIGN TABLE", true, false),
    ("ALTER FUNCTION", true, false),
    ("ALTER INDEX", true, false),
    ("ALTER LANGUAGE", true, false),
    ("ALTER LARGE OBJECT", true, false),
    ("ALTER MATERIALIZED VIEW", true, true),
    ("ALTER OPERATOR", true, false),
    ("ALTER OPERATOR CLASS", true, false),
    ("ALTER OPERATOR FAMILY", true, false),
    ("ALTER POLICY", true, false),
    ("ALTER PROCEDURE", true, false),
    ("ALTER PUBLICATION", true, false),
    ("ALTER ROLE", false, false),
    ("ALTER ROUTINE", true, false),
    ("ALTER RULE", true, false),
    ("ALTER SCHEMA", true, false),
    ("ALTER SEQUENCE", true, false),
    ("ALTER SERVER", true, false),
    ("ALTER STATISTICS", true, false),
    ("ALTER SUBSCRIPTION", true, false),
    ("ALTER SYSTEM", false, false),
    ("ALTER TABLE", true, true),
    ("ALTER TABLESPACE", false, false),
    ("ALTER TEXT SEARCH CONFIGURATION", true, false),
    ("ALTER TEXT SEARCH DICTIONARY", true, false),
    ("ALTER TEXT SEARCH PARSER", true, false),
    ("ALTER TEXT SEARCH TEMPLATE", true, false),
    ("ALTER TRANSFORM", true, false),
    ("ALTER TRIGGER", true, false),
    ("ALTER TYPE", true, true),
    ("ALTER USER MAPPING", true, false),
    ("ALTER VIEW", true, false),
    ("ANALYZE", false, false),
    ("BEGIN", false, false),
    ("CALL", false, false),
    ("CHECKPOINT", false, false),
    ("CLOSE", false, false),
    ("CLOSE CURSOR", false, false),
    ("CLOSE CURSOR ALL", false, false),
    ("CLUSTER", false, false),
    ("COMMENT", true, false),
    ("COMMIT", false, false),
    ("COMMIT PREPARED", false, false),
    ("COPY", false, false),
    ("COPY FROM", false, false),
    ("CREATE ACCESS METHOD", true, false),
    ("CREATE AGGREGATE", true, false),
    ("CREATE CAST", true, false),
    ("CREATE COLLATION", true, false),
    ("CREATE CONSTRAINT", true, false),
    ("CREATE CONVERSION", true, false),
    ("CREATE DATABASE", false, false),
    ("CREATE DOMAIN", true, false),
    ("CREATE EVENT TRIGGER", false, false),
    ("CREATE EXTENSION", true, false),
    ("CREATE FOREIGN DATA WRAPPER", true, false),
    ("CREATE FOREIGN TABLE", true, false),
    ("CREATE FUNCTION", true, false),
    ("CREATE INDEX", true, false),
    ("CREATE LANGUAGE", true, false),
    ("CREATE MATERIALIZED VIEW", true, false),
    ("CREATE OPERATOR", true, false),
    ("CREATE OPERATOR CLASS", true, false),
    ("CREATE OPERATOR FAMILY", true, false),
    ("CREATE POLICY", true, false),
    ("CREATE PROCEDURE", true, false),
    ("CREATE PUBLICATION", true, false),
    ("CREATE ROLE", false, false),
    ("CREATE ROUTINE", true, false),
    ("CREATE RULE", true, false),
    ("CREATE SCHEMA", true, false),
    ("CREATE SEQUENCE", true, false),
    ("CREATE SERVER", true, false),
    ("CREATE STATISTICS", true, false),
    ("CREATE SUBSCRIPTION", true, false),
    ("CREATE TABLE", true, false),
    ("CREATE TABLE AS", true, false),
    ("CREATE TABLESPACE", false, false),
    ("CREATE TEXT SEARCH CONFIGURATION", true, false),
    ("CREATE TEXT SEARCH DICTIONARY", true, false),
    ("CREATE TEXT SEARCH PARSER", true, false),
    ("CREATE TEXT SEARCH TEMPLATE", true, false),
    ("CREATE TRANSFORM", true, false),
    ("CREATE TRIGGER", true, false),
    ("CREATE TYPE", true, false),
    ("CREATE USER MAPPING", true, false),
    ("CREATE VIEW", true, false),
    ("DEALLOCATE", false, false),
    ("DEALLOCATE ALL", false, false),
    ("DECLARE CURSOR", false, false),
    ("DELETE", false, false),
    ("DISCARD", false, false),
    ("DISCARD ALL", false, false),
    ("DISCARD PLANS", false, false),
    ("DISCARD SEQUENCES", false, false),
    ("DISCARD TEMP", false, false),
    ("DO", false, false),
    ("DROP ACCESS METHOD", true, false),
    ("DROP AGGREGATE", true, false),
    ("DROP CAST", true, false),
    ("DROP COLLATION", true, false),
    ("DROP CONSTRAINT", true, false),
    ("DROP CONVERSION", true, false),
    ("DROP DATABASE", false, false),
    ("DROP DOMAIN", true, false),
    ("DROP EVENT TRIGGER", false, false),
    ("DROP EXTENSION", true, false),
    ("DROP FOREIGN DATA WRAPPER", true, false),
    ("DROP FOREIGN TABLE", true, false),
    ("DROP FUNCTION", true, false),
    ("DROP INDEX", true, false),
    ("DROP LANGUAGE", true, false),
    ("DROP MATERIALIZED VIEW", true, false),
    ("DROP OPERATOR", true, false),
    ("DROP OPERATOR CLASS", true, false),
    ("DROP OPERATOR FAMILY", true, false),
    ("DROP OWNED", true, false),
    ("DROP POLICY", true, false),
    ("DROP PROCEDURE", true, false),
    ("DROP PUBLICATION", true, false),
    ("DROP ROLE", false, false),
    ("DROP ROUTINE", true, false),
    ("DROP RULE", true, false),
    ("DROP SCHEMA", true, false),
    ("DROP SEQUENCE", true, false),
    ("DROP SERVER", true, false),
    ("DROP STATISTICS", true, false),
    ("DROP SUBSCRIPTION", true, false),
    ("DROP TABLE", true, false),
    ("DROP TABLESPACE", false, false),
    ("DROP TEXT SEARCH CONFIGURATION", true, false),
    ("DROP TEXT SEARCH DICTIONARY", true, false),
    ("DROP TEXT SEARCH PARSER", true, false),
    ("DROP TEXT SEARCH TEMPLATE", true, false),
    ("DROP TRANSFORM", true, false),
    ("DROP TRIGGER", true, false),
    ("DROP TYPE", true, false),
    ("DROP USER MAPPING", true, false),
    ("DROP VIEW", true, false),
    ("EXECUTE", false, false),
    ("EXPLAIN", false, false),
    ("FETCH", false, false),
    ("GRANT", true, false),
    ("GRANT ROLE", false, false),
    ("IMPORT FOREIGN SCHEMA", true, false),
    ("INSERT", false, false),
    ("LISTEN", false, false),
    ("LOAD", false, false),
    ("LOCK TABLE", false, false),
    ("LOGIN", true, false),
    ("MERGE", false, false),
    ("MOVE", false, false),
    ("NOTIFY", false, false),
    ("PREPARE", false, false),
    ("PREPARE TRANSACTION", false, false),
    ("REASSIGN OWNED", false, false),
    ("REFRESH MATERIALIZED VIEW", true, false),
    ("REINDEX", true, false),
    ("RELEASE", false, false),
    ("RESET", false, false),
    ("REVOKE", true, false),
    ("REVOKE ROLE", false, false),
    ("ROLLBACK", false, false),
    ("ROLLBACK PREPARED", false, false),
    ("SAVEPOINT", false, false),
    ("SECURITY LABEL", true, false),
    ("SELECT", false, false),
    ("SELECT FOR KEY SHARE", false, false),
    ("SELECT FOR NO KEY UPDATE", false, false),
    ("SELECT FOR SHARE", false, false),
    ("SELECT FOR UPDATE", false, false),
    ("SELECT INTO", true, false),
    ("SET", false, false),
    ("SET CONSTRAINTS", false, false),
    ("SHOW", false, false),
    ("START TRANSACTION", false, false),
    ("TRUNCATE TABLE", false, false),
    ("UNLISTEN", false, false),
    ("UPDATE", false, false),
    ("VACUUM", false, false),
];

/// A known command tag, as `(event_trigger_ok, table_rewrite_ok)`:
/// `GetCommandTagEnum` compares case-insensitively, and CMDTAG_UNKNOWN
/// (`None` here) is the answer for an empty or unknown name.
pub(crate) fn lookup(name: &str) -> Option<(bool, bool)> {
    COMMAND_TAGS
        .iter()
        .find(|(tag, ..)| tag.eq_ignore_ascii_case(name))
        .map(|&(_, event_trigger_ok, table_rewrite_ok)| (event_trigger_ok, table_rewrite_ok))
}

/// `AlterObjectTypeCommandTag` (utility.c).
fn alter_object_type_tag(objtype: i32) -> &'static str {
    use ObjectType as O;
    match O::try_from(objtype).unwrap_or(O::Undefined) {
        O::ObjectAggregate => "ALTER AGGREGATE",
        O::ObjectAttribute => "ALTER TYPE",
        O::ObjectCast => "ALTER CAST",
        O::ObjectCollation => "ALTER COLLATION",
        O::ObjectColumn => "ALTER TABLE",
        O::ObjectConversion => "ALTER CONVERSION",
        O::ObjectDatabase => "ALTER DATABASE",
        O::ObjectDomain | O::ObjectDomconstraint => "ALTER DOMAIN",
        O::ObjectExtension => "ALTER EXTENSION",
        O::ObjectFdw => "ALTER FOREIGN DATA WRAPPER",
        O::ObjectForeignServer => "ALTER SERVER",
        O::ObjectForeignTable => "ALTER FOREIGN TABLE",
        O::ObjectFunction => "ALTER FUNCTION",
        O::ObjectIndex => "ALTER INDEX",
        O::ObjectLanguage => "ALTER LANGUAGE",
        O::ObjectLargeobject => "ALTER LARGE OBJECT",
        O::ObjectOpclass => "ALTER OPERATOR CLASS",
        O::ObjectOperator => "ALTER OPERATOR",
        O::ObjectOpfamily => "ALTER OPERATOR FAMILY",
        O::ObjectPolicy => "ALTER POLICY",
        O::ObjectProcedure => "ALTER PROCEDURE",
        O::ObjectRole => "ALTER ROLE",
        O::ObjectRoutine => "ALTER ROUTINE",
        O::ObjectRule => "ALTER RULE",
        O::ObjectSchema => "ALTER SCHEMA",
        O::ObjectSequence => "ALTER SEQUENCE",
        O::ObjectTable | O::ObjectTabconstraint => "ALTER TABLE",
        O::ObjectTablespace => "ALTER TABLESPACE",
        O::ObjectTrigger => "ALTER TRIGGER",
        O::ObjectEventTrigger => "ALTER EVENT TRIGGER",
        O::ObjectTsconfiguration => "ALTER TEXT SEARCH CONFIGURATION",
        O::ObjectTsdictionary => "ALTER TEXT SEARCH DICTIONARY",
        O::ObjectTsparser => "ALTER TEXT SEARCH PARSER",
        O::ObjectTstemplate => "ALTER TEXT SEARCH TEMPLATE",
        O::ObjectType => "ALTER TYPE",
        O::ObjectView => "ALTER VIEW",
        O::ObjectMatview => "ALTER MATERIALIZED VIEW",
        O::ObjectPublication => "ALTER PUBLICATION",
        O::ObjectSubscription => "ALTER SUBSCRIPTION",
        O::ObjectStatisticExt => "ALTER STATISTICS",
        _ => "???",
    }
}

fn drop_tag(remove_type: i32) -> &'static str {
    use ObjectType as O;
    match O::try_from(remove_type).unwrap_or(O::Undefined) {
        O::ObjectTable => "DROP TABLE",
        O::ObjectSequence => "DROP SEQUENCE",
        O::ObjectView => "DROP VIEW",
        O::ObjectMatview => "DROP MATERIALIZED VIEW",
        O::ObjectIndex => "DROP INDEX",
        O::ObjectType => "DROP TYPE",
        O::ObjectDomain => "DROP DOMAIN",
        O::ObjectCollation => "DROP COLLATION",
        O::ObjectConversion => "DROP CONVERSION",
        O::ObjectSchema => "DROP SCHEMA",
        O::ObjectTsparser => "DROP TEXT SEARCH PARSER",
        O::ObjectTsdictionary => "DROP TEXT SEARCH DICTIONARY",
        O::ObjectTstemplate => "DROP TEXT SEARCH TEMPLATE",
        O::ObjectTsconfiguration => "DROP TEXT SEARCH CONFIGURATION",
        O::ObjectForeignTable => "DROP FOREIGN TABLE",
        O::ObjectExtension => "DROP EXTENSION",
        O::ObjectFunction => "DROP FUNCTION",
        O::ObjectProcedure => "DROP PROCEDURE",
        O::ObjectRoutine => "DROP ROUTINE",
        O::ObjectAggregate => "DROP AGGREGATE",
        O::ObjectOperator => "DROP OPERATOR",
        O::ObjectLanguage => "DROP LANGUAGE",
        O::ObjectCast => "DROP CAST",
        O::ObjectTrigger => "DROP TRIGGER",
        O::ObjectEventTrigger => "DROP EVENT TRIGGER",
        O::ObjectRule => "DROP RULE",
        O::ObjectFdw => "DROP FOREIGN DATA WRAPPER",
        O::ObjectForeignServer => "DROP SERVER",
        O::ObjectOpclass => "DROP OPERATOR CLASS",
        O::ObjectOpfamily => "DROP OPERATOR FAMILY",
        O::ObjectPolicy => "DROP POLICY",
        O::ObjectTransform => "DROP TRANSFORM",
        O::ObjectAccessMethod => "DROP ACCESS METHOD",
        O::ObjectPublication => "DROP PUBLICATION",
        O::ObjectStatisticExt => "DROP STATISTICS",
        _ => "???",
    }
}

fn define_tag(kind: i32) -> &'static str {
    use ObjectType as O;
    match O::try_from(kind).unwrap_or(O::Undefined) {
        O::ObjectAggregate => "CREATE AGGREGATE",
        O::ObjectOperator => "CREATE OPERATOR",
        O::ObjectType => "CREATE TYPE",
        O::ObjectTsparser => "CREATE TEXT SEARCH PARSER",
        O::ObjectTsdictionary => "CREATE TEXT SEARCH DICTIONARY",
        O::ObjectTstemplate => "CREATE TEXT SEARCH TEMPLATE",
        O::ObjectTsconfiguration => "CREATE TEXT SEARCH CONFIGURATION",
        O::ObjectCollation => "CREATE COLLATION",
        O::ObjectAccessMethod => "CREATE ACCESS METHOD",
        _ => "???",
    }
}

/// `CreateCommandTag` (utility.c) of a raw statement.
pub(crate) fn command_tag(stmt: &node::Node) -> &'static str {
    use node::Node as N;
    match stmt {
        N::InsertStmt(_) => "INSERT",
        N::DeleteStmt(_) => "DELETE",
        N::UpdateStmt(_) => "UPDATE",
        N::MergeStmt(_) => "MERGE",
        N::SelectStmt(_) | N::PlassignStmt(_) => "SELECT",
        N::TransactionStmt(t) => {
            use TransactionStmtKind as K;
            match K::try_from(t.kind).unwrap_or(K::Undefined) {
                K::TransStmtBegin => "BEGIN",
                K::TransStmtStart => "START TRANSACTION",
                K::TransStmtCommit => "COMMIT",
                K::TransStmtRollback | K::TransStmtRollbackTo => "ROLLBACK",
                K::TransStmtSavepoint => "SAVEPOINT",
                K::TransStmtRelease => "RELEASE",
                K::TransStmtPrepare => "PREPARE TRANSACTION",
                K::TransStmtCommitPrepared => "COMMIT PREPARED",
                K::TransStmtRollbackPrepared => "ROLLBACK PREPARED",
                K::Undefined => "???",
            }
        }
        N::DeclareCursorStmt(_) => "DECLARE CURSOR",
        N::ClosePortalStmt(c) if c.portalname.is_empty() => "CLOSE CURSOR ALL",
        N::ClosePortalStmt(_) => "CLOSE CURSOR",
        N::FetchStmt(f) if f.ismove => "MOVE",
        N::FetchStmt(_) => "FETCH",
        N::CreateDomainStmt(_) => "CREATE DOMAIN",
        N::CreateSchemaStmt(_) => "CREATE SCHEMA",
        N::CreateStmt(_) => "CREATE TABLE",
        N::CreateTableSpaceStmt(_) => "CREATE TABLESPACE",
        N::DropTableSpaceStmt(_) => "DROP TABLESPACE",
        N::AlterTableSpaceOptionsStmt(_) => "ALTER TABLESPACE",
        N::CreateExtensionStmt(_) => "CREATE EXTENSION",
        N::AlterExtensionStmt(_) | N::AlterExtensionContentsStmt(_) => "ALTER EXTENSION",
        N::CreateFdwStmt(_) => "CREATE FOREIGN DATA WRAPPER",
        N::AlterFdwStmt(_) => "ALTER FOREIGN DATA WRAPPER",
        N::CreateForeignServerStmt(_) => "CREATE SERVER",
        N::AlterForeignServerStmt(_) => "ALTER SERVER",
        N::CreateUserMappingStmt(_) => "CREATE USER MAPPING",
        N::AlterUserMappingStmt(_) => "ALTER USER MAPPING",
        N::DropUserMappingStmt(_) => "DROP USER MAPPING",
        N::CreateForeignTableStmt(_) => "CREATE FOREIGN TABLE",
        N::ImportForeignSchemaStmt(_) => "IMPORT FOREIGN SCHEMA",
        N::DropStmt(d) => drop_tag(d.remove_type),
        N::TruncateStmt(_) => "TRUNCATE TABLE",
        N::CommentStmt(_) => "COMMENT",
        N::SecLabelStmt(_) => "SECURITY LABEL",
        N::CopyStmt(_) => "COPY",
        // A renamed column takes the tag of its relation's kind.
        N::RenameStmt(r) => {
            alter_object_type_tag(if r.rename_type == ObjectType::ObjectColumn as i32 {
                r.relation_type
            } else {
                r.rename_type
            })
        }
        N::AlterObjectDependsStmt(s) => alter_object_type_tag(s.object_type),
        N::AlterObjectSchemaStmt(s) => alter_object_type_tag(s.object_type),
        N::AlterOwnerStmt(s) => alter_object_type_tag(s.object_type),
        N::AlterTableMoveAllStmt(s) => alter_object_type_tag(s.objtype),
        N::AlterTableStmt(s) => alter_object_type_tag(s.objtype),
        N::AlterDomainStmt(_) => "ALTER DOMAIN",
        N::AlterFunctionStmt(f) => match ObjectType::try_from(f.objtype) {
            Ok(ObjectType::ObjectFunction) => "ALTER FUNCTION",
            Ok(ObjectType::ObjectProcedure) => "ALTER PROCEDURE",
            Ok(ObjectType::ObjectRoutine) => "ALTER ROUTINE",
            _ => "???",
        },
        N::GrantStmt(g) if g.is_grant => "GRANT",
        N::GrantStmt(_) => "REVOKE",
        N::GrantRoleStmt(g) if g.is_grant => "GRANT ROLE",
        N::GrantRoleStmt(_) => "REVOKE ROLE",
        N::AlterDefaultPrivilegesStmt(_) => "ALTER DEFAULT PRIVILEGES",
        N::DefineStmt(d) => define_tag(d.kind),
        N::CompositeTypeStmt(_) | N::CreateEnumStmt(_) | N::CreateRangeStmt(_) => "CREATE TYPE",
        N::AlterEnumStmt(_) => "ALTER TYPE",
        N::ViewStmt(_) => "CREATE VIEW",
        N::CreateFunctionStmt(f) if f.is_procedure => "CREATE PROCEDURE",
        N::CreateFunctionStmt(_) => "CREATE FUNCTION",
        N::IndexStmt(_) => "CREATE INDEX",
        N::RuleStmt(_) => "CREATE RULE",
        N::CreateSeqStmt(_) => "CREATE SEQUENCE",
        N::AlterSeqStmt(_) => "ALTER SEQUENCE",
        N::DoStmt(_) => "DO",
        N::CreatedbStmt(_) => "CREATE DATABASE",
        N::AlterDatabaseStmt(_)
        | N::AlterDatabaseRefreshCollStmt(_)
        | N::AlterDatabaseSetStmt(_) => "ALTER DATABASE",
        N::DropdbStmt(_) => "DROP DATABASE",
        N::NotifyStmt(_) => "NOTIFY",
        N::ListenStmt(_) => "LISTEN",
        N::UnlistenStmt(_) => "UNLISTEN",
        N::LoadStmt(_) => "LOAD",
        N::CallStmt(_) => "CALL",
        N::ClusterStmt(_) => "CLUSTER",
        N::VacuumStmt(v) if v.is_vacuumcmd => "VACUUM",
        N::VacuumStmt(_) => "ANALYZE",
        N::ExplainStmt(_) => "EXPLAIN",
        N::CreateTableAsStmt(c) => match ObjectType::try_from(c.objtype) {
            Ok(ObjectType::ObjectTable) if c.is_select_into => "SELECT INTO",
            Ok(ObjectType::ObjectTable) => "CREATE TABLE AS",
            Ok(ObjectType::ObjectMatview) => "CREATE MATERIALIZED VIEW",
            _ => "???",
        },
        N::RefreshMatViewStmt(_) => "REFRESH MATERIALIZED VIEW",
        N::AlterSystemStmt(_) => "ALTER SYSTEM",
        N::VariableSetStmt(v) => match VariableSetKind::try_from(v.kind) {
            Ok(VariableSetKind::VarReset | VariableSetKind::VarResetAll) => "RESET",
            Ok(VariableSetKind::Undefined) | Err(_) => "???",
            Ok(_) => "SET",
        },
        N::VariableShowStmt(_) => "SHOW",
        N::DiscardStmt(d) => match DiscardMode::try_from(d.target) {
            Ok(DiscardMode::DiscardAll) => "DISCARD ALL",
            Ok(DiscardMode::DiscardPlans) => "DISCARD PLANS",
            Ok(DiscardMode::DiscardTemp) => "DISCARD TEMP",
            Ok(DiscardMode::DiscardSequences) => "DISCARD SEQUENCES",
            _ => "???",
        },
        N::CreateTransformStmt(_) => "CREATE TRANSFORM",
        N::CreateTrigStmt(_) => "CREATE TRIGGER",
        N::CreateEventTrigStmt(_) => "CREATE EVENT TRIGGER",
        N::AlterEventTrigStmt(_) => "ALTER EVENT TRIGGER",
        N::CreatePlangStmt(_) => "CREATE LANGUAGE",
        N::CreateRoleStmt(_) => "CREATE ROLE",
        N::AlterRoleStmt(_) | N::AlterRoleSetStmt(_) => "ALTER ROLE",
        N::DropRoleStmt(_) => "DROP ROLE",
        N::DropOwnedStmt(_) => "DROP OWNED",
        N::ReassignOwnedStmt(_) => "REASSIGN OWNED",
        N::LockStmt(_) => "LOCK TABLE",
        N::ConstraintsSetStmt(_) => "SET CONSTRAINTS",
        N::CheckPointStmt(_) => "CHECKPOINT",
        N::ReindexStmt(_) => "REINDEX",
        N::CreateConversionStmt(_) => "CREATE CONVERSION",
        N::CreateCastStmt(_) => "CREATE CAST",
        N::CreateOpClassStmt(_) => "CREATE OPERATOR CLASS",
        N::CreateOpFamilyStmt(_) => "CREATE OPERATOR FAMILY",
        N::AlterOpFamilyStmt(_) => "ALTER OPERATOR FAMILY",
        N::AlterOperatorStmt(_) => "ALTER OPERATOR",
        N::AlterTypeStmt(_) => "ALTER TYPE",
        N::AlterTsdictionaryStmt(_) => "ALTER TEXT SEARCH DICTIONARY",
        N::AlterTsconfigurationStmt(_) => "ALTER TEXT SEARCH CONFIGURATION",
        N::CreatePolicyStmt(_) => "CREATE POLICY",
        N::AlterPolicyStmt(_) => "ALTER POLICY",
        N::CreateAmStmt(_) => "CREATE ACCESS METHOD",
        N::CreatePublicationStmt(_) => "CREATE PUBLICATION",
        N::AlterPublicationStmt(_) => "ALTER PUBLICATION",
        N::CreateSubscriptionStmt(_) => "CREATE SUBSCRIPTION",
        N::AlterSubscriptionStmt(_) => "ALTER SUBSCRIPTION",
        N::DropSubscriptionStmt(_) => "DROP SUBSCRIPTION",
        N::AlterCollationStmt(_) => "ALTER COLLATION",
        N::PrepareStmt(_) => "PREPARE",
        N::ExecuteStmt(_) => "EXECUTE",
        N::CreateStatsStmt(_) => "CREATE STATISTICS",
        N::AlterStatsStmt(_) => "ALTER STATISTICS",
        N::DeallocateStmt(d) if d.isall => "DEALLOCATE ALL",
        N::DeallocateStmt(_) => "DEALLOCATE",
        _ => "???",
    }
}

/// `ClassifyUtilityCommandAsReadOnly` (utility.c): the utility statements
/// that aren't allowed in a read-only transaction — DDL and TRUNCATE. DML
/// is refused by the executor (`ExecCheckXactReadOnly`) when it writes a
/// permanent relation.
pub(crate) fn utility_is_not_read_only(stmt: &node::Node) -> bool {
    use node::Node as N;
    matches!(
        stmt,
        N::AlterCollationStmt(_)
            | N::AlterDatabaseRefreshCollStmt(_)
            | N::AlterDatabaseSetStmt(_)
            | N::AlterDatabaseStmt(_)
            | N::AlterDefaultPrivilegesStmt(_)
            | N::AlterDomainStmt(_)
            | N::AlterEnumStmt(_)
            | N::AlterEventTrigStmt(_)
            | N::AlterExtensionContentsStmt(_)
            | N::AlterExtensionStmt(_)
            | N::AlterFdwStmt(_)
            | N::AlterForeignServerStmt(_)
            | N::AlterFunctionStmt(_)
            | N::AlterObjectDependsStmt(_)
            | N::AlterObjectSchemaStmt(_)
            | N::AlterOpFamilyStmt(_)
            | N::AlterOperatorStmt(_)
            | N::AlterOwnerStmt(_)
            | N::AlterPolicyStmt(_)
            | N::AlterPublicationStmt(_)
            | N::AlterRoleSetStmt(_)
            | N::AlterRoleStmt(_)
            | N::AlterSeqStmt(_)
            | N::AlterStatsStmt(_)
            | N::AlterSubscriptionStmt(_)
            | N::AlterTsconfigurationStmt(_)
            | N::AlterTsdictionaryStmt(_)
            | N::AlterTableMoveAllStmt(_)
            | N::AlterTableSpaceOptionsStmt(_)
            | N::AlterTableStmt(_)
            | N::AlterTypeStmt(_)
            | N::AlterUserMappingStmt(_)
            | N::CommentStmt(_)
            | N::CompositeTypeStmt(_)
            | N::CreateAmStmt(_)
            | N::CreateCastStmt(_)
            | N::CreateConversionStmt(_)
            | N::CreateDomainStmt(_)
            | N::CreateEnumStmt(_)
            | N::CreateEventTrigStmt(_)
            | N::CreateExtensionStmt(_)
            | N::CreateFdwStmt(_)
            | N::CreateForeignServerStmt(_)
            | N::CreateForeignTableStmt(_)
            | N::CreateFunctionStmt(_)
            | N::CreateOpClassStmt(_)
            | N::CreateOpFamilyStmt(_)
            | N::CreatePlangStmt(_)
            | N::CreatePolicyStmt(_)
            | N::CreatePublicationStmt(_)
            | N::CreateRangeStmt(_)
            | N::CreateRoleStmt(_)
            | N::CreateSchemaStmt(_)
            | N::CreateSeqStmt(_)
            | N::CreateStatsStmt(_)
            | N::CreateStmt(_)
            | N::CreateSubscriptionStmt(_)
            | N::CreateTableAsStmt(_)
            | N::CreateTableSpaceStmt(_)
            | N::CreateTransformStmt(_)
            | N::CreateTrigStmt(_)
            | N::CreateUserMappingStmt(_)
            | N::CreatedbStmt(_)
            | N::DefineStmt(_)
            | N::DropOwnedStmt(_)
            | N::DropRoleStmt(_)
            | N::DropStmt(_)
            | N::DropSubscriptionStmt(_)
            | N::DropTableSpaceStmt(_)
            | N::DropUserMappingStmt(_)
            | N::DropdbStmt(_)
            | N::GrantRoleStmt(_)
            | N::GrantStmt(_)
            | N::ImportForeignSchemaStmt(_)
            | N::IndexStmt(_)
            | N::ReassignOwnedStmt(_)
            | N::RefreshMatViewStmt(_)
            | N::RenameStmt(_)
            | N::RuleStmt(_)
            | N::SecLabelStmt(_)
            | N::TruncateStmt(_)
            | N::ViewStmt(_)
    )
}
