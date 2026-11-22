//! Read-only catalog accessors for the lint crate's query, migration and
//! schema rules.
//!
//! The rules need a few facts the catalog keeps privately: whether a column
//! default is the `nextval()` of the column's own sequence (the serial
//! shorthand), the typmod a column's declared type carries through domains
//! and arrays, and whether a relation has row security or rewrite rules.
//! Everything else they read is already public.

use crate::oid::{PgClassOid, PgOperatorOid, PgProcOid, PgTypeOid};

use super::PgCatalog;

impl PgCatalog {
    /// Whether operator `operator`, with operands `left` and `right`, is the
    /// default btree equality of `left`'s type (strategy 3 of the type's
    /// default btree operator family).
    pub fn lint_is_default_btree_equality(
        &self,
        left: PgTypeOid,
        right: PgTypeOid,
        operator: PgOperatorOid,
    ) -> bool {
        let Some(opclass) = crate::ddl::opclass::default_opclass_id(self, left, "btree")
            .and_then(|oid| self.pg_opclass.iter().find(|class| class.oid == oid))
        else {
            return false;
        };
        self.pg_amop.iter().any(|member| {
            member.amopmethod == "btree"
                && member.amopstrategy == 3
                && member.amopopr == operator
                && member.amopfamily == opclass.opcfamily
                && member.amopfamilynamespace == opclass.opcfamilynamespace
                && member.amoplefttype == left
                && member.amoprighttype == right
        })
    }

    /// Whether row-level security is enabled on `relid`
    /// (`pg_class.relrowsecurity`): its SELECT policies then restrict the
    /// rows a statement sees, `RETURNING` included.
    pub fn lint_relation_has_row_security(&self, relid: PgClassOid) -> bool {
        self.row_security.contains(&relid)
    }

    /// Whether `relid` has a rewrite rule (`CREATE RULE`): a statement on it
    /// may then run the rule's actions instead of, or besides, its own.
    pub fn lint_relation_has_rules(&self, relid: PgClassOid) -> bool {
        self.rules
            .get(&relid)
            .is_some_and(|rules| !rules.is_empty())
    }

    /// Whether the unique index `index` treats NULLs as equal
    /// (`indnullsnotdistinct`). `false` for a non-unique index, which has no
    /// such flag.
    pub fn lint_index_nulls_not_distinct(&self, index: PgClassOid) -> bool {
        self.index_keys
            .get(&index)
            .is_some_and(|keys| keys.nulls_not_distinct)
    }

    /// Whether the index `index` was created by a DEFERRABLE constraint
    /// (`pg_index.indimmediate = false`): its uniqueness is checked at the
    /// end of the transaction, not at each statement.
    pub fn lint_index_is_deferrable(&self, index: PgClassOid) -> bool {
        self.nonimmediate_indexes.contains(&index)
    }

    /// Resolve a `TypeName` AST node to a type OID, the way the analyzer
    /// resolves casts and column definitions.
    pub fn lint_resolve_type_name(
        &self,
        type_name: &typedpg_pg_query::ast::TypeName,
    ) -> Option<PgTypeOid> {
        crate::ddl::util::resolve_type_name(type_name, self)
    }

    /// Whether the default of column `attnum` of `relid` is `nextval()` of a
    /// sequence owned by that column, i.e. the column was written with the
    /// `serial` / `bigserial` / `smallserial` shorthand.
    pub fn lint_column_is_serial(&self, relid: PgClassOid, attnum: i16) -> bool {
        matches!(
            self.attr_default_exprs.get(&(relid, attnum)),
            Some(crate::ddl::tables::check_inherit::StoredExpr::Serial(..))
        )
    }

    /// Whether an `UPDATE OF` trigger of `relid`, or of a table inheriting
    /// from it (a partition), lists column `column`, whatever its state: it
    /// fires when an `UPDATE`'s SET list names the column, even assigning
    /// the value it already has.
    pub fn lint_update_of_trigger_lists(&self, relid: PgClassOid, column: &str) -> bool {
        std::iter::once(relid)
            .chain(crate::resolve::inheritance_descendants(self, relid))
            .any(|rel| {
                self.triggers.get(&rel).is_some_and(|triggers| {
                    triggers.iter().any(|trigger| {
                        trigger.events & crate::ddl::triggers::TRIGGER_TYPE_UPDATE != 0
                            && trigger.columns.iter().any(|c| c == column)
                    })
                })
            })
    }

    /// The typmod a column's declared type carries, through domains and
    /// arrays, exactly as the analyzer resolves it.
    pub fn lint_effective_typmod(&self, oid: PgTypeOid, typmod: Option<i32>) -> Option<i32> {
        self.effective_typmod(oid, typmod)
    }

    /// Whether `FORCE ROW LEVEL SECURITY` is set on `relid`
    /// (`pg_class.relforcerowsecurity`): row security applies to the
    /// table's owner too.
    pub fn lint_relation_forces_row_security(&self, relid: PgClassOid) -> bool {
        self.force_row_security.contains(&relid)
    }

    /// The names of the row-level security policies on `relid`
    /// (`pg_policy.polname`), in creation order.
    pub fn lint_relation_policies(&self, relid: PgClassOid) -> Vec<&str> {
        self.policies
            .get(&relid)
            .map(|policies| policies.iter().map(|p| p.name.as_str()).collect())
            .unwrap_or_default()
    }

    /// `pg_class.relpersistence` of `relid`: `p` (permanent), `u`
    /// (unlogged) or `t` (temporary).
    pub fn lint_relation_persistence(&self, relid: PgClassOid) -> char {
        self.relpersistence.get(&relid).copied().unwrap_or('p')
    }

    /// The routines with a body the migrations created (built-in ones
    /// excluded), each with whether it is `SECURITY DEFINER` (`prosecdef`)
    /// and whether its names resolve along the caller's search path: no
    /// `SET search_path` in its `proconfig`, and a body resolved when it
    /// runs (a SQL-standard `BEGIN ATOMIC` / `RETURN` body is resolved at
    /// `CREATE`, so it is not). Sorted by OID.
    pub fn lint_routine_security(&self) -> Vec<(PgProcOid, bool, bool)> {
        let mut out: Vec<_> = self
            .routine_bodies
            .iter()
            .map(|(oid, body)| {
                let resolves_at_run_time = body.search_path.is_none()
                    && !matches!(
                        body.source,
                        crate::ddl::functions::RoutineSource::SqlStandard(_)
                    );
                (*oid, body.security_definer, resolves_at_run_time)
            })
            .collect();
        out.sort_by_key(|(oid, _, _)| *oid);
        out
    }

    /// The column names a partitioned table's key reads: each key column,
    /// and the columns each key expression is strict in. `None` when
    /// `relid` is not a partitioned table the analyzer knows the key of.
    pub fn lint_partition_key_columns(&self, relid: PgClassOid) -> Option<Vec<String>> {
        crate::ddl::tables::partbound::key_column_names(self, relid)
    }

    /// Whether the partitioned table `relid` has a default partition.
    pub fn lint_has_default_partition(&self, relid: PgClassOid) -> bool {
        self.pg_inherits.iter().any(|inherits| {
            inherits.inhparent == relid
                && matches!(
                    self.partition_bounds.get(&inherits.inhrelid),
                    Some(crate::ddl::tables::partbound::Bound::Default)
                )
        })
    }
}
