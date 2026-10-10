use std::fmt;
use std::str::FromStr;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

/// The external id of a point, either an unsigned 64-bit integer or a UUID.
///
/// Numbers keep their full `u64` range. In JSON a number id is written as a number, and a
/// string holding only digits is accepted too, for clients whose JSON numbers lose precision
/// above 2^53. Any other string must be a UUID.
///
/// Ids order numbers first, then UUIDs, which gives scrolling a stable order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PointId {
    Num(u64),
    Uuid(Uuid),
}

impl From<u64> for PointId {
    fn from(n: u64) -> Self {
        PointId::Num(n)
    }
}

impl From<Uuid> for PointId {
    fn from(u: Uuid) -> Self {
        PointId::Uuid(u)
    }
}

impl fmt::Display for PointId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PointId::Num(n) => write!(f, "{n}"),
            PointId::Uuid(u) => write!(f, "{}", u.hyphenated()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsePointIdError(String);

impl fmt::Display for ParsePointIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?} is neither an unsigned 64-bit integer nor a UUID",
            self.0
        )
    }
}

impl std::error::Error for ParsePointIdError {}

impl FromStr for PointId {
    type Err = ParsePointIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
            return s
                .parse::<u64>()
                .map(PointId::Num)
                .map_err(|_| ParsePointIdError(s.to_owned()));
        }
        Uuid::parse_str(s)
            .map(PointId::Uuid)
            .map_err(|_| ParsePointIdError(s.to_owned()))
    }
}

impl Serialize for PointId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            PointId::Num(n) => serializer.serialize_u64(*n),
            PointId::Uuid(u) => serializer.collect_str(&u.hyphenated()),
        }
    }
}

impl<'de> Deserialize<'de> for PointId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct IdVisitor;

        impl Visitor<'_> for IdVisitor {
            type Value = PointId;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an unsigned 64-bit integer or a UUID string")
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<PointId, E> {
                Ok(PointId::Num(v))
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<PointId, E> {
                u64::try_from(v)
                    .map(PointId::Num)
                    .map_err(|_| E::custom(format!("point id {v} is negative")))
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<PointId, E> {
                v.parse().map_err(E::custom)
            }
        }

        deserializer.deserialize_any(IdVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_keep_the_full_u64_range() {
        let id: PointId = serde_json::from_str("18446744073709551615").unwrap();
        assert_eq!(id, PointId::Num(u64::MAX));
        assert_eq!(serde_json::to_string(&id).unwrap(), "18446744073709551615");
    }

    #[test]
    fn digit_strings_are_numbers() {
        let id: PointId = serde_json::from_str("\"9007199254740993\"").unwrap();
        assert_eq!(id, PointId::Num(9_007_199_254_740_993));
    }

    #[test]
    fn uuids_round_trip_hyphenated() {
        let text = "\"550e8400-e29b-41d4-a716-446655440000\"";
        let id: PointId = serde_json::from_str(text).unwrap();
        assert!(matches!(id, PointId::Uuid(_)));
        assert_eq!(serde_json::to_string(&id).unwrap(), text);
    }

    #[test]
    fn rejects_negatives_floats_and_junk() {
        assert!(serde_json::from_str::<PointId>("-1").is_err());
        assert!(serde_json::from_str::<PointId>("1.5").is_err());
        assert!(serde_json::from_str::<PointId>("\"abc\"").is_err());
        assert!(serde_json::from_str::<PointId>("\"\"").is_err());
        assert!(serde_json::from_str::<PointId>("\"99999999999999999999\"").is_err());
    }

    #[test]
    fn numbers_sort_before_uuids() {
        let u = PointId::Uuid(Uuid::nil());
        assert!(PointId::Num(u64::MAX) < u);
    }
}
