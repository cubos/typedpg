//! The dependencies of an operator family's members (`pg_amop` /
//! `pg_amproc` rows) on what they name and what they belong to
//! (opclasscmds.c `storeOperators` / `storeProcedures`, and each access
//! method's `amadjustmembers`).
//!
//! A member is either *hard* — DEPENDENCY_INTERNAL on an operator class
//! (or, for a required support function added by ALTER OPERATOR FAMILY, on
//! the family), and DEPENDENCY_NORMAL on its operator or function: the
//! owner then blocks a DROP of the operator or function, and CASCADE takes
//! the owner along — or *soft*: DEPENDENCY_AUTO on the family and on the
//! object, so it goes with either, silently. The analyzer keeps the
//! members migrations create, and records each hard member as its owner's
//! normal dependency on the object.

use typedpg_pg_query::protobuf::{CreateOpClassItem, ObjectType, node};

use super::DdlError;
use super::depend::ObjectAddress;
use crate::oid::{PgGenericOid, PgNamespaceOid, PgTypeOid};
use crate::pg_catalog::{DepType, PG_OPERATOR_RELID, PG_PROC_RELID, PgCatalog};

/// An operator family, as the analyzer names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FamilyKey {
    pub name: String,
    pub namespace: PgNamespaceOid,
    pub method: String,
}

/// A member of an operator family a migration created.
#[derive(Clone, Debug)]
pub(crate) struct FamilyMember {
    pub family: FamilyKey,
    /// A support function (`pg_amproc`) rather than an operator.
    pub is_func: bool,
    /// The strategy or support function number.
    pub number: i16,
    pub left: PgTypeOid,
    pub right: PgTypeOid,
    /// The operator's or function's OID.
    pub object: PgGenericOid,
    /// What a hard member belongs to (an operator class, or the family);
    /// `None` for a soft one.
    pub hard_owner: Option<ObjectAddress>,
}

impl FamilyMember {
    fn object_address(&self) -> ObjectAddress {
        let classid = if self.is_func {
            PG_PROC_RELID
        } else {
            PG_OPERATOR_RELID
        };
        ObjectAddress {
            classid,
            objid: self.object,
            objsubid: 0,
        }
    }
}

/// Which command adds the members: CREATE OPERATOR CLASS ties them hard to
/// the new class by default, ALTER OPERATOR FAMILY ADD soft to the family.
#[derive(Clone, Copy)]
pub(crate) enum Path {
    CreateClass {
        class: ObjectAddress,
        intype: PgTypeOid,
    },
    AlterAdd,
}

/// The access method's required support function numbers, those
/// `amadjustmembers` ties hard: btree's BTORDER_PROC, hash's
/// HASHSTANDARD_PROC (btadjustmembers / hashadjustmembers), and GiST's,
/// GIN's and SP-GiST's required procs.
fn required_proc(method: &str, number: i16) -> bool {
    match method {
        "btree" | "hash" => number == 1,
        // GIST_CONSISTENT, UNION, PENALTY, PICKSPLIT, EQUAL.
        "gist" => matches!(number, 1 | 2 | 5 | 6 | 7),
        // GIN_EXTRACTVALUE, EXTRACTQUERY.
        "gin" => matches!(number, 2 | 3),
        // SPGIST_CONFIG, CHOOSE, PICKSPLIT, INNER_CONSISTENT,
        // LEAF_CONSISTENT.
        "spgist" => matches!(number, 1..=5),
        _ => true,
    }
}

/// `opclass_for_family_datatype`: an operator class of `family` for
/// `datatype`.
fn class_for_datatype(
    interp: &PgCatalog,
    family: &FamilyKey,
    datatype: PgTypeOid,
) -> Option<ObjectAddress> {
    interp
        .pg_opclass
        .iter()
        .find(|c| {
            c.opcfamily == family.name
                && c.opcfamilynamespace == family.namespace
                && c.opcmethod == family.method
                && c.opcintype == datatype
        })
        .map(|c| ObjectAddress::new(super::depend::PG_OPCLASS_RELID, c.oid.into_nonzero(), 0))
}

/// The default dependencies of new members (DefineOpClass: hard on the
/// class; AlterOpFamilyAdd: soft on the family), then the access method's
/// `amadjustmembers`.
pub(crate) fn adjust(interp: &PgCatalog, path: Path, members: &mut [FamilyMember]) {
    for m in members.iter_mut() {
        // The defaults, and whether the member is "hard" before adjusting
        // (a required GiST proc keeps its default owner).
        let default_owner = match path {
            Path::CreateClass { class, .. } => Some(class),
            Path::AlterAdd => None,
        };
        let family_owner = super::depend::named_address_of(
            interp,
            &super::depend::NamedObject::Opfamily {
                name: m.family.name.clone(),
                namespace: m.family.namespace,
                method: m.family.method.clone(),
            },
        );
        m.hard_owner = match m.family.method.as_str() {
            // btadjustmembers / hashadjustmembers: optional procs and
            // cross-type members are loose; a same-type one is tied to the
            // family's class for its type, when there is one.
            "btree" | "hash" => {
                if (m.is_func && m.number != 1) || m.left != m.right {
                    None
                } else {
                    match path {
                        Path::CreateClass { class, intype } if m.left == intype => Some(class),
                        _ => class_for_datatype(interp, &m.family, m.left),
                    }
                }
            }
            // gist/gin/spgistadjustmembers: operators are loose; required
            // procs hard on their default referent (the class, or the
            // family under ALTER ... ADD).
            "gist" | "gin" | "spgist" => {
                if m.is_func && required_proc(&m.family.method, m.number) {
                    default_owner.or(family_owner)
                } else {
                    None
                }
            }
            // No amadjustmembers (BRIN, bloom): the defaults.
            _ => default_owner,
        };
    }
}

/// The operator class or family the hard members of `owner` belong to:
/// re-derive its normal dependencies on their operators and functions.
fn sync_owner(interp: &mut PgCatalog, owner: ObjectAddress) {
    interp.pg_depend.retain(|d| {
        !(d.classid == owner.classid
            && d.objid == owner.objid
            && (d.refclassid == PG_OPERATOR_RELID || d.refclassid == PG_PROC_RELID)
            && d.deptype == DepType::Normal)
    });
    let objects: Vec<ObjectAddress> = interp
        .family_members
        .iter()
        .filter(|m| m.hard_owner == Some(owner))
        .map(FamilyMember::object_address)
        .collect();
    super::depend::record(interp, owner, objects, DepType::Normal);
}

/// Store new members (after [`adjust`]).
pub(crate) fn store(interp: &mut PgCatalog, members: Vec<FamilyMember>) {
    let mut owners: Vec<ObjectAddress> = members.iter().filter_map(|m| m.hard_owner).collect();
    owners.sort();
    owners.dedup();
    interp.family_members.extend(members);
    for owner in owners {
        sync_owner(interp, owner);
    }
}

/// The member of `family` numbered `number` for `(left, right)`, if a
/// migration created it.
pub(crate) fn find(
    interp: &PgCatalog,
    family: &FamilyKey,
    is_func: bool,
    number: i16,
    left: PgTypeOid,
    right: PgTypeOid,
) -> Option<FamilyMember> {
    interp
        .family_members
        .iter()
        .find(|m| {
            &m.family == family
                && m.is_func == is_func
                && m.number == number
                && m.left == left
                && m.right == right
        })
        .cloned()
}

/// getObjectDescription of a member: `operator 3 (integer, integer) of
/// operator family fam for access method btree: ===(integer,integer)`.
fn describe(interp: &PgCatalog, m: &FamilyMember) -> String {
    let family = super::depend::named_address_of(
        interp,
        &super::depend::NamedObject::Opfamily {
            name: m.family.name.clone(),
            namespace: m.family.namespace,
            method: m.family.method.clone(),
        },
    )
    .map(|f| super::depend::describe(interp, f))
    .unwrap_or_default();
    let object = super::depend::describe(interp, m.object_address());
    let (kind, object) = if m.is_func {
        (
            "function",
            object.strip_prefix("function ").unwrap_or(&object),
        )
    } else {
        (
            "operator",
            object.strip_prefix("operator ").unwrap_or(&object),
        )
    };
    format!(
        "{kind} {} ({}, {}) of {family}: {object}",
        m.number,
        super::util::format_type_for_message(interp, m.left),
        super::util::format_type_for_message(interp, m.right),
    )
}

/// ALTER OPERATOR FAMILY ... DROP of member `m`: a hard member goes only
/// with what it belongs to (findDependentObjects, DEPENDENCY_INTERNAL).
pub(crate) fn drop_member(interp: &mut PgCatalog, m: &FamilyMember) -> Result<(), DdlError> {
    if let Some(owner) = m.hard_owner {
        let owner_desc = super::depend::describe(interp, owner);
        return Err(DdlError::DependencyError(format!(
            "cannot drop {} because {owner_desc} requires it (You can drop {owner_desc} instead.)",
            describe(interp, m)
        )));
    }
    interp.family_members.retain(|x| {
        !(x.family == m.family
            && x.is_func == m.is_func
            && x.number == m.number
            && x.left == m.left
            && x.right == m.right)
    });
    Ok(())
}

/// Operator class or family `owner` is gone: its hard members (and their
/// `pg_amop` rows) go with it.
pub(crate) fn forget_owner(interp: &mut PgCatalog, owner: ObjectAddress) {
    let gone: Vec<FamilyMember> = interp
        .family_members
        .iter()
        .filter(|m| m.hard_owner == Some(owner))
        .cloned()
        .collect();
    interp
        .family_members
        .retain(|m| m.hard_owner != Some(owner));
    for m in gone.iter().filter(|m| !m.is_func) {
        interp.pg_amop.retain(|o| {
            !(o.amopfamily == m.family.name
                && o.amopfamilynamespace == m.family.namespace
                && o.amopmethod == m.family.method
                && o.amopstrategy == m.number
                && o.amoplefttype == m.left
                && o.amoprighttype == m.right)
        });
    }
}

/// Family `family` is gone: all its members go.
pub(crate) fn forget_family(interp: &mut PgCatalog, family: &FamilyKey) {
    interp.family_members.retain(|m| &m.family != family);
}

/// The FUNCTION items of a CREATE OPERATOR CLASS / ALTER OPERATOR FAMILY
/// ADD, resolved (LookupFuncWithArgs) with their associated types
/// (assignProcTypes).
pub(crate) fn function_members(
    interp: &PgCatalog,
    family: &FamilyKey,
    items: &[typedpg_pg_query::protobuf::Node],
    intype: Option<PgTypeOid>,
) -> Result<Vec<FamilyMember>, DdlError> {
    let mut members: Vec<FamilyMember> = Vec::new();
    for item in items {
        let Some(node::Node::CreateOpClassItem(item)) = item.node.as_ref() else {
            continue;
        };
        // OPCLASS_ITEM_FUNCTION.
        if item.itemtype != 2 {
            continue;
        }
        let Some(member) = function_member(interp, family, item, intype)? else {
            continue;
        };
        if members
            .iter()
            .any(|m| m.number == member.number && m.left == member.left && m.right == member.right)
        {
            return Err(DdlError::Parse(format!(
                "function number {} for ({},{}) appears more than once",
                member.number,
                super::util::format_type_for_message(interp, member.left),
                super::util::format_type_for_message(interp, member.right),
            )));
        }
        members.push(member);
    }
    Ok(members)
}

fn function_member(
    interp: &PgCatalog,
    family: &FamilyKey,
    item: &CreateOpClassItem,
    intype: Option<PgTypeOid>,
) -> Result<Option<FamilyMember>, DdlError> {
    let Some(owa) = item.name.as_ref() else {
        return Ok(None);
    };
    let Some(proc) =
        super::functions::lookup_func_with_args(interp, ObjectType::ObjectFunction, owa, false)?
    else {
        return Ok(None);
    };
    let number = i16::try_from(item.number).unwrap_or_default();
    // processTypesSpec: one type names both sides.
    let given: Vec<Option<PgTypeOid>> = item
        .class_args
        .iter()
        .map(|n| match n.node.as_ref() {
            Some(node::Node::TypeName(tn)) => super::util::resolve_type_name(tn, interp),
            _ => None,
        })
        .collect();
    let (mut left, mut right) = match given.as_slice() {
        [Some(t)] => (Some(*t), Some(*t)),
        [Some(l), Some(r)] => (Some(*l), Some(*r)),
        _ => (None, None),
    };
    let args = interp
        .pg_proc
        .get(&proc)
        .map(|p| p.proargtypes.clone())
        .unwrap_or_default();
    match (family.method.as_str(), number) {
        // amcanorder: the comparison proc's and in_range's argument types.
        ("btree", 1) => {
            left = left.or(args.first().copied());
            right = right.or(args.get(1).copied());
        }
        ("btree", 3) => {
            left = left.or(args.first().copied());
            right = right.or(args.get(2).copied());
        }
        // amcanhash: the hashed type.
        ("hash", _) => {
            left = left.or(args.first().copied());
            right = right.or(args.first().copied());
        }
        _ => {}
    }
    let (Some(left), Some(right)) = (left.or(intype), right.or(intype)) else {
        return Err(DdlError::Parse(
            "associated data types must be specified for index support function".into(),
        ));
    };
    Ok(Some(FamilyMember {
        family: family.clone(),
        is_func: true,
        number,
        left,
        right,
        object: PgGenericOid::from_nonzero(proc.into_nonzero()),
        hard_owner: None,
    }))
}

/// A search operator member (`pg_amop` row) as a [`FamilyMember`].
pub(crate) fn operator_member(family: &FamilyKey, op: &crate::pg_catalog::PgAmop) -> FamilyMember {
    FamilyMember {
        family: family.clone(),
        is_func: false,
        number: op.amopstrategy,
        left: op.amoplefttype,
        right: op.amoprighttype,
        object: PgGenericOid::from_nonzero(op.amopopr.into_nonzero()),
        hard_owner: None,
    }
}
