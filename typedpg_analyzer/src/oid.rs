//! Strongly-typed OID newtypes — one per `pg_catalog` table.
//!
//! Every column in our [`crate::pg_catalog`] structs that mirrors PG's
//! `oid` type uses one of these newtypes instead of bare `u32`. Each newtype
//! is a `NonZeroU32` so the type system enforces the "OIDs are never 0"
//! invariant at the boundary, and a missing FK (PG's convention of
//! `oid = 0`) shows up as `Option<XxxOid>` rather than a magic constant.
//!
//! # JSON layout
//!
//! - Required OID columns (`oid`, mandatory FKs): serialize as a bare integer
//!   via `#[serde(transparent)]`. Deserialization rejects `0`.
//! - Optional OID columns (FKs that PG stores as `0` for "none"): use
//!   `#[serde(with = "oid_or_zero")]` on the field. `None` round-trips as
//!   `0`; any non-zero value yields `Some(XxxOid)`.
//!
//! # Constructing
//!
//! - [`Self::new`] — fallible: returns `Some` for non-zero values, `None`
//!   for zero. Use this when reading from external sources (Postgres rows,
//!   user input).
//! - [`Self::from_raw`] — `const`-friendly, panics on zero. Reserved for
//!   compile-time constants like the well-known OIDs in
//!   [`crate::pg_catalog::oid`] / [`crate::pg_catalog::PG_CLASS_RELID`].
//!
//! # Reading the value
//!
//! - [`Self::get`] returns the underlying `u32`. Use whenever you need to
//!   interoperate with code that takes raw OIDs (HashMap keys today, error
//!   messages, etc.).

use std::fmt;
use std::num::NonZeroU32;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Behavior shared by every PG-catalog OID newtype. Lets generic helpers
/// (like the [`oid_or_zero`] serde adapter) work uniformly across all of
/// them without macro-generated duplication.
pub trait OidLike: Sized + Copy {
    fn new(value: u32) -> Option<Self>;
    fn get(self) -> u32;
}

macro_rules! define_oid {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(NonZeroU32);

        impl $name {
            /// Construct from a raw `u32`. Returns `None` for zero (PG's
            /// "no value" sentinel) and `Some` otherwise.
            #[inline]
            pub const fn new(value: u32) -> Option<Self> {
                match NonZeroU32::new(value) {
                    Some(v) => Some(Self(v)),
                    None => None,
                }
            }

            /// `const`-friendly constructor that panics on zero. Reserved
            /// for compile-time well-known OIDs (`pg_type` builtins, the
            /// `PG_*_RELID` table constants).
            #[inline]
            pub const fn from_raw(value: u32) -> Self {
                match NonZeroU32::new(value) {
                    Some(v) => Self(v),
                    None => panic!("OID must be non-zero"),
                }
            }

            /// Wrap an existing `NonZeroU32` as this OID. Infallible — the
            /// non-zero invariant is already encoded in the input.
            #[inline]
            pub const fn from_nonzero(nz: NonZeroU32) -> Self {
                Self(nz)
            }

            /// The underlying `u32`. Always non-zero by construction.
            #[inline]
            pub const fn get(self) -> u32 {
                self.0.get()
            }

            /// The underlying `NonZeroU32`. Useful for re-tagging this OID
            /// as a different kind (typically the polymorphic
            /// [`PgGenericOid`]) without going through the fallible
            /// [`Self::new`].
            #[inline]
            pub const fn into_nonzero(self) -> NonZeroU32 {
                self.0
            }
        }

        impl OidLike for $name {
            #[inline]
            fn new(value: u32) -> Option<Self> {
                Self::new(value)
            }
            #[inline]
            fn get(self) -> u32 {
                self.get()
            }
        }

        impl From<NonZeroU32> for $name {
            #[inline]
            fn from(nz: NonZeroU32) -> Self {
                Self(nz)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
                self.0.get().serialize(ser)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
                let raw = u32::deserialize(de)?;
                NonZeroU32::new(raw)
                    .map(Self)
                    .ok_or_else(|| serde::de::Error::custom(concat!(stringify!($name), " must be non-zero")))
            }
        }
    };
}

define_oid!(
    /// `pg_namespace.oid` — schema OID.
    PgNamespaceOid
);
define_oid!(
    /// `pg_type.oid` — type OID.
    PgTypeOid
);
define_oid!(
    /// `pg_class.oid` — relation OID. Also used as `pg_depend.classid` /
    /// `refclassid` since those reference rows in `pg_class` (one per
    /// catalog table).
    PgClassOid
);
define_oid!(
    /// `pg_proc.oid` — function/procedure/aggregate OID.
    PgProcOid
);
define_oid!(
    /// `pg_operator.oid`.
    PgOperatorOid
);
define_oid!(
    /// `pg_cast.oid`.
    PgCastOid
);
define_oid!(
    /// `pg_extension.oid`.
    PgExtensionOid
);
define_oid!(
    /// `pg_enum.oid` — one per enum label.
    PgEnumOid
);
define_oid!(
    /// `pg_constraint.oid` — one per CHECK / UNIQUE / PRIMARY KEY / FOREIGN
    /// KEY / EXCLUSION constraint.
    PgConstraintOid
);
define_oid!(
    /// `pg_rewrite.oid` — one per rule. Views' SELECT bodies live here
    /// under `rulename = '_RETURN'`.
    PgRewriteOid
);
define_oid!(
    /// `pg_opclass.oid` — one per operator class.
    PgOpclassOid
);
define_oid!(
    /// `pg_collation.oid` — one per registered collation
    /// (`"C"`, `"POSIX"`, `"en_US.UTF-8"`, …).
    PgCollationOid
);

// Generic OID for `pg_depend.objid` / `refobjid`. Their concrete catalog
// table varies with `classid`/`refclassid`, so we can't pin them to one of
// the per-table newtypes. Still strictly non-zero.
define_oid!(
    /// `pg_depend.objid` / `pg_depend.refobjid` — polymorphic OID whose
    /// catalog table is determined by the sibling `classid` / `refclassid`.
    PgGenericOid
);

/// Serde adapter for `Option<XxxOid>` fields that should round-trip through
/// JSON as `0` for `None`. Mirrors PG's convention of using `oid = 0` to
/// mean "no FK" (e.g. `pg_type.typrelid = 0` for non-composites).
///
/// Apply on a struct field with `#[serde(with = "crate::oid::oid_or_zero")]`.
#[allow(dead_code)]
pub mod oid_or_zero {
    use super::OidLike;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<O: OidLike, S: Serializer>(
        value: &Option<O>,
        ser: S,
    ) -> Result<S::Ok, S::Error> {
        let raw = value.map(|o| o.get()).unwrap_or(0);
        serde::Serialize::serialize(&raw, ser)
    }

    pub fn deserialize<'de, O: OidLike, D: Deserializer<'de>>(
        de: D,
    ) -> Result<Option<O>, D::Error> {
        let raw = u32::deserialize(de)?;
        Ok(O::new(raw))
    }
}

/// Serde adapter for `Vec<XxxOid>` columns that PG models as `oidvector` /
/// arrays of OIDs. Stored as a JSON array of integers; entries with value
/// `0` are filtered out at load time (PG never stores zeros there in
/// practice, but keeping the adapter strict means we'd refuse to load
/// older snapshots that might).
#[allow(dead_code)]
pub mod vec_oid {
    use super::OidLike;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<O: OidLike, S: Serializer>(value: &[O], ser: S) -> Result<S::Ok, S::Error> {
        let raws: Vec<u32> = value.iter().map(|o| o.get()).collect();
        raws.serialize(ser)
    }

    pub fn deserialize<'de, O: OidLike, D: Deserializer<'de>>(de: D) -> Result<Vec<O>, D::Error> {
        let raws: Vec<u32> = Vec::deserialize(de)?;
        raws.into_iter()
            .map(|r| {
                O::new(r).ok_or_else(|| serde::de::Error::custom("oid in vector must be non-zero"))
            })
            .collect()
    }
}

/// Serde adapter for `Vec<Option<XxxOid>>` columns — an `oidvector` whose
/// entries may be `0` for "none" (`pg_index.indcollation` of a
/// non-collatable column). Stored as a JSON array of integers.
#[allow(dead_code)]
pub mod vec_oid_or_zero {
    use super::OidLike;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<O: OidLike, S: Serializer>(
        value: &[Option<O>],
        ser: S,
    ) -> Result<S::Ok, S::Error> {
        let raws: Vec<u32> = value.iter().map(|o| o.map_or(0, |o| o.get())).collect();
        raws.serialize(ser)
    }

    pub fn deserialize<'de, O: OidLike, D: Deserializer<'de>>(
        de: D,
    ) -> Result<Vec<Option<O>>, D::Error> {
        let raws: Vec<u32> = Vec::deserialize(de)?;
        Ok(raws.into_iter().map(O::new).collect())
    }
}

/// Serde adapter for `Option<i32>` fields that PG models with the sentinel
/// `-1` ("no value"). `None` round-trips as `-1`; `Some(v)` as `v`. Used for
/// `pg_attribute.atttypmod` / `pg_type.typtypmod` so the in-memory shape can
/// stay `Option<i32>` while the seed JSON keeps PG-faithful integers.
#[allow(dead_code)]
pub mod option_i32_neg_one {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &Option<i32>, ser: S) -> Result<S::Ok, S::Error> {
        let raw = value.unwrap_or(-1);
        serde::Serialize::serialize(&raw, ser)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Option<i32>, D::Error> {
        let raw = i32::deserialize(de)?;
        Ok(if raw < 0 { None } else { Some(raw) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_rejects_zero() {
        assert!(PgTypeOid::new(0).is_none());
        assert_eq!(PgTypeOid::new(23).map(|o| o.get()), Some(23));
    }

    #[test]
    fn serialize_transparent() {
        let v = PgTypeOid::from_raw(23);
        let json = serde_json::to_string(&v).unwrap();
        assert_eq!(json, "23");
        let back: PgTypeOid = serde_json::from_str(&json).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn deserialize_rejects_zero() {
        assert!(serde_json::from_str::<PgTypeOid>("0").is_err());
    }

    #[test]
    fn oid_or_zero_roundtrip() {
        #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
        struct Wrapper {
            #[serde(with = "oid_or_zero")]
            inner: Option<PgTypeOid>,
        }
        let some = Wrapper {
            inner: PgTypeOid::new(42),
        };
        let json = serde_json::to_string(&some).unwrap();
        assert_eq!(json, r#"{"inner":42}"#);
        let back: Wrapper = serde_json::from_str(&json).unwrap();
        assert_eq!(back, some);

        let none = Wrapper { inner: None };
        let json = serde_json::to_string(&none).unwrap();
        assert_eq!(json, r#"{"inner":0}"#);
        let back: Wrapper = serde_json::from_str(&json).unwrap();
        assert_eq!(back, none);
    }
}
