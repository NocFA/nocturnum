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
    pub vectors: VectorConfig,
    pub segments: usize,
    /// Why the collection stopped taking writes, `null` while it is healthy.
    pub failure: Option<&'static str>,
}

#[derive(Debug, Serialize)]
pub struct CollectionInfo {
    pub name: String,
    pub vectors: VectorConfig,
    pub points: usize,
    pub segments: usize,
    pub indexes: Vec<PayloadIndexSpec>,
}

/// The body of `POST .../points/recall`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recall {
    #[serde(default = "hundred")]
    pub samples: usize,
    #[serde(default = "ten")]
    pub limit: usize,
    /// Defaults to 16, 32, 64, 128 and 256, raised to at least `limit`.
    pub hnsw_ef: Option<Vec<usize>>,
    pub filter: Option<Filter>,
    pub seed: Option<u64>,
}

impl Recall {
    pub fn widths(&self) -> Vec<usize> {
        match &self.hnsw_ef {
            Some(ef) => ef.clone(),
            None => [16, 32, 64, 128, 256]
                .into_iter()
                .map(|ef: usize| ef.max(self.limit))
                .collect(),
        }
    }
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
    /// Exactly one of `vector` and `text` says what to rank by.
    pub vector: Option<Vec<f32>>,
    pub text: Option<TextIn>,
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

/// A hybrid query, fusing the results of several searches.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryIn {
    pub prefetch: Vec<PrefetchIn>,
    #[serde(default)]
    pub fusion: FusionIn,
    /// Reciprocal rank fusion's `k`, only with `rrf`.
    pub k: Option<f64>,
    pub filter: Option<Filter>,
    #[serde(default = "ten")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
    #[serde(default)]
    pub with_payload: WithPayloadIn,
    #[serde(default)]
    pub with_vector: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FusionIn {
    #[default]
    Rrf,
    Dbsf,
}

/// One search in a hybrid query.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrefetchIn {
    /// Exactly one of `vector` and `text` says what to rank by.
    pub vector: Option<Vec<f32>>,
    pub text: Option<TextIn>,
    pub filter: Option<Filter>,
    pub limit: Option<usize>,
    pub score_threshold: Option<f32>,
    #[serde(default)]
    pub exact: bool,
    pub hnsw_ef: Option<usize>,
    pub weight: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct QueryOut {
    pub points: Vec<ScoredOut>,
    pub candidates: CandidatesOut,
}

#[derive(Debug, Serialize)]
pub struct CandidatesOut {
    pub pool: usize,
    pub sources: Vec<SourceOut>,
}

#[derive(Debug, Serialize)]
pub struct SourceOut {
    pub limit: usize,
    pub found: usize,
}

/// A text search's words and BM25 knobs.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextIn {
    pub field: JsonPath,
    pub query: String,
    pub k1: Option<f64>,
    pub b: Option<f64>,
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
