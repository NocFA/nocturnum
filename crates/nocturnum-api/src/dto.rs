//! Request and response bodies, as `docs/api/v1.md` describes them.

use nocturnum_core::{Ack, Filter, JsonPath, Payload, PayloadIndexSpec, PointId, VectorConfig};
use nocturnum_query::{Point, ScoredPoint, Selector, WithPayload};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::error::ApiError;

/// The largest integer a JSON number holds exactly in a client that reads numbers as 64-bit
/// floats, 2^53 − 1.
pub const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

/// Writes an id as a number up to [`MAX_SAFE_INTEGER`] and as a string of digits above it,
/// so JavaScript clients keep every digit.
fn id<S: Serializer>(id: &PointId, s: S) -> Result<S::Ok, S::Error> {
    match id {
        PointId::Num(n) if *n > MAX_SAFE_INTEGER => s.collect_str(n),
        id => id.serialize(s),
    }
}

fn opt_id<S: Serializer>(v: &Option<PointId>, s: S) -> Result<S::Ok, S::Error> {
    match v {
        Some(v) => id(v, s),
        None => s.serialize_none(),
    }
}

fn yes() -> bool {
    true
}

fn ten() -> usize {
    10
}

fn hundred() -> usize {
    100
}

// Collections.

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateCollection {
    pub vectors: VectorConfig,
}

#[derive(Debug, Serialize)]
pub struct CollectionSummary {
    pub name: String,
    pub points: usize,
}

#[derive(Debug, Serialize)]
pub struct CollectionInfo {
    pub name: String,
    pub vectors: VectorConfig,
    pub points: usize,
    pub segments: usize,
    pub indexes: Vec<PayloadIndexSpec>,
}

// Writes.

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AckParams {
    #[serde(default)]
    pub ack: Ack,
}

#[derive(Debug, Serialize)]
pub struct WriteResult {
    pub seq: u64,
    pub ack: Ack,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Upsert {
    pub points: Vec<PointIn>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PointIn {
    pub id: PointId,
    pub vector: Vec<f32>,
    #[serde(default)]
    pub payload: Payload,
}

/// Picks the points a write applies to: exactly one of `ids` and `filter`.
fn selector(ids: Option<Vec<PointId>>, filter: Option<Filter>) -> Result<Selector, ApiError> {
    match (ids, filter) {
        (Some(ids), None) => Ok(Selector::Ids(ids)),
        (None, Some(f)) => Ok(Selector::Filter(f)),
        _ => Err(ApiError::invalid("give exactly one of ids and filter")),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Select {
    pub ids: Option<Vec<PointId>>,
    pub filter: Option<Filter>,
}

impl Select {
    pub fn selector(self) -> Result<Selector, ApiError> {
        selector(self.ids, self.filter)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetPayload {
    pub ids: Option<Vec<PointId>>,
    pub filter: Option<Filter>,
    pub payload: Payload,
}

impl SetPayload {
    pub fn split(self) -> Result<(Selector, Payload), ApiError> {
        Ok((selector(self.ids, self.filter)?, self.payload))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteKeys {
    pub ids: Option<Vec<PointId>>,
    pub filter: Option<Filter>,
    pub keys: Vec<String>,
}

impl DeleteKeys {
    pub fn split(self) -> Result<(Selector, Vec<String>), ApiError> {
        Ok((selector(self.ids, self.filter)?, self.keys))
    }
}

// Reads.

/// `with_payload`: `true`, `false`, `{"include": [...]}` or `{"exclude": [...]}`.
#[derive(Debug, Clone, PartialEq)]
pub struct WithPayloadIn(pub WithPayload);

impl Default for WithPayloadIn {
    fn default() -> Self {
        WithPayloadIn(WithPayload::All)
    }
}

impl<'de> Deserialize<'de> for WithPayloadIn {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let expected = "true, false, {\"include\": [paths]} or {\"exclude\": [paths]}";
        let paths = |v: Value| {
            serde_json::from_value::<Vec<JsonPath>>(v).map_err(|e| D::Error::custom(e.to_string()))
        };
        match Value::deserialize(d)? {
            Value::Bool(true) => Ok(WithPayloadIn(WithPayload::All)),
            Value::Bool(false) => Ok(WithPayloadIn(WithPayload::None)),
            Value::Object(mut m) if m.len() == 1 => {
                if let Some(v) = m.remove("include") {
                    Ok(WithPayloadIn(WithPayload::Include(paths(v)?)))
                } else if let Some(v) = m.remove("exclude") {
                    Ok(WithPayloadIn(WithPayload::Exclude(paths(v)?)))
                } else {
                    Err(D::Error::custom(format!("expected {expected}")))
                }
            }
            _ => Err(D::Error::custom(format!("expected {expected}"))),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetParams {
    #[serde(default = "yes")]
    pub with_payload: bool,
    #[serde(default)]
    pub with_vector: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetPoints {
    pub ids: Vec<PointId>,
    #[serde(default)]
    pub with_payload: WithPayloadIn,
    #[serde(default)]
    pub with_vector: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scroll {
    pub filter: Option<Filter>,
    #[serde(default = "hundred")]
    pub limit: usize,
    pub after: Option<PointId>,
    #[serde(default)]
    pub with_payload: WithPayloadIn,
    #[serde(default)]
    pub with_vector: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Count {
    pub filter: Option<Filter>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Search {
    pub vector: Vec<f32>,
    pub filter: Option<Filter>,
    #[serde(default = "ten")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
    pub score_threshold: Option<f32>,
    #[serde(default)]
    pub exact: bool,
    pub hnsw_ef: Option<usize>,
    #[serde(default)]
    pub with_payload: WithPayloadIn,
    #[serde(default)]
    pub with_vector: bool,
}

#[derive(Debug, Serialize)]
pub struct PointOut {
    #[serde(serialize_with = "id")]
    pub id: PointId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<Payload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vector: Option<Vec<f32>>,
}

impl From<Point> for PointOut {
    fn from(p: Point) -> Self {
        PointOut {
            id: p.id,
            payload: p.payload,
            vector: p.vector,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ScoredOut {
    #[serde(serialize_with = "id")]
    pub id: PointId,
    pub score: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<Payload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vector: Option<Vec<f32>>,
}

impl From<ScoredPoint> for ScoredOut {
    fn from(p: ScoredPoint) -> Self {
        ScoredOut {
            id: p.id,
            score: p.score,
            payload: p.payload,
            vector: p.vector,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Points<T> {
    pub points: Vec<T>,
}

#[derive(Debug, Serialize)]
pub struct Page {
    pub points: Vec<PointOut>,
    #[serde(serialize_with = "opt_id")]
    pub next: Option<PointId>,
}

// Snapshots.

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Restore {
    pub snapshot: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_ids_are_strings() {
        let out = |n| {
            serde_json::to_string(&PointOut {
                id: PointId::Num(n),
                payload: None,
                vector: None,
            })
            .unwrap()
        };
        assert_eq!(out(MAX_SAFE_INTEGER), r#"{"id":9007199254740991}"#);
        assert_eq!(out(MAX_SAFE_INTEGER + 1), r#"{"id":"9007199254740992"}"#);
        assert_eq!(out(u64::MAX), r#"{"id":"18446744073709551615"}"#);
        let page = Page {
            points: vec![],
            next: Some(PointId::Num(u64::MAX)),
        };
        assert_eq!(
            serde_json::to_string(&page).unwrap(),
            r#"{"points":[],"next":"18446744073709551615"}"#
        );
    }

    #[test]
    fn with_payload_forms() {
        let parse = |s: &str| serde_json::from_str::<WithPayloadIn>(s).map(|w| w.0);
        assert_eq!(parse("true").unwrap(), WithPayload::All);
        assert_eq!(parse("false").unwrap(), WithPayload::None);
        assert!(matches!(
            parse(r#"{"include": ["a", "b.c"]}"#).unwrap(),
            WithPayload::Include(p) if p.len() == 2
        ));
        assert!(matches!(
            parse(r#"{"exclude": ["a"]}"#).unwrap(),
            WithPayload::Exclude(_)
        ));
        for bad in [
            "1",
            "null",
            r#"{"only": ["a"]}"#,
            r#"{"include": ["a"], "exclude": ["b"]}"#,
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }
}
