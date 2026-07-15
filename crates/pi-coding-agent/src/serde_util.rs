//! Serde helpers for wire-compatible JSON (null vs missing fields).

use serde::de::{self, Deserializer, Visitor};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::marker::PhantomData;

/// Three-state optional field matching JSON:
/// - missing key → `Absent`
/// - `null` → `Null`
/// - value → `Value(T)`
///
/// Needed for session `parentId: null` vs v1 entries that omit `parentId`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum NullOr<T> {
    #[default]
    Absent,
    Null,
    Value(T),
}

impl<T> NullOr<T> {
    pub fn as_option(&self) -> Option<&T> {
        match self {
            Self::Value(v) => Some(v),
            _ => None,
        }
    }

    pub fn into_option(self) -> Option<T> {
        match self {
            Self::Value(v) => Some(v),
            _ => None,
        }
    }

    pub fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }

    pub fn from_option(opt: Option<T>) -> Self {
        match opt {
            Some(v) => Self::Value(v),
            None => Self::Null,
        }
    }
}

impl<T> Serialize for NullOr<T>
where
    T: Serialize,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Absent => serializer.serialize_none(),
            Self::Null => serializer.serialize_none(),
            Self::Value(v) => v.serialize(serializer),
        }
    }
}

impl<'de, T> Deserialize<'de> for NullOr<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct NullOrVisitor<T>(PhantomData<T>);

        impl<'de, T> Visitor<'de> for NullOrVisitor<T>
        where
            T: Deserialize<'de>,
        {
            type Value = NullOr<T>;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("null or a value")
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(NullOr::Null)
            }

            fn visit_none<E>(self) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(NullOr::Null)
            }

            fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
            where
                D: Deserializer<'de>,
            {
                T::deserialize(deserializer).map(NullOr::Value)
            }
        }

        // Field-level: when the key is present, serde calls deserialize with the value.
        // For Option-wrapping we use a different path; here we handle present null/value.
        deserializer.deserialize_option(NullOrVisitor(PhantomData))
    }
}

/// Serialize only when not `Absent` (emit `null` for `Null`, value for `Value`).
pub fn serialize_null_or<S, T>(value: &NullOr<T>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
    T: Serialize,
{
    match value {
        NullOr::Absent => unreachable!("skip_serializing_if should prevent Absent"),
        NullOr::Null => serializer.serialize_none(),
        NullOr::Value(v) => v.serialize(serializer),
    }
}

pub fn is_absent<T>(value: &NullOr<T>) -> bool {
    value.is_absent()
}

/// Optional field that is omitted when `None` and present when `Some`.
/// Use for `id` on v1 (absent) vs v3 (present string).
pub fn skip_if_none<T>(value: &Option<T>) -> bool {
    value.is_none()
}
