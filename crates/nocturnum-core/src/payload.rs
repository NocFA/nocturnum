//! Point payloads and the paths that address values inside them.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

use crate::error::{Error, Result};

/// A point's payload, always a JSON object at the root.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Payload(pub Map<String, Value>);

impl Payload {
    pub fn new() -> Self {
        Self::default()
    }

    /// Checks the payload nests no deeper than [`MAX_PAYLOAD_DEPTH`], which every payload must
    /// pass before it is stored, since JSON readers refuse documents nested much deeper.
    pub fn validate(&self) -> Result<()> {
        fn depth_ok(v: &Value, depth: usize) -> bool {
            match v {
                _ if depth > MAX_PAYLOAD_DEPTH => false,
                Value::Array(items) => items.iter().all(|i| depth_ok(i, depth + 1)),
                Value::Object(map) => map.values().all(|i| depth_ok(i, depth + 1)),
                _ => true,
            }
        }
        if self.0.values().all(|v| depth_ok(v, 2)) {
            Ok(())
        } else {
            Err(Error::PayloadTooDeep {
                max: MAX_PAYLOAD_DEPTH,
            })
        }
    }

    /// Every value the path reaches, in document order, empty when the path is missing.
    pub fn values_at<'a>(&'a self, path: &JsonPath) -> Vec<&'a Value> {
        path.resolve_in(&self.0)
    }
}

impl From<Map<String, Value>> for Payload {
    fn from(map: Map<String, Value>) -> Self {
        Payload(map)
    }
}

/// How deeply a payload may nest objects and arrays, counting the payload itself as 1.
pub const MAX_PAYLOAD_DEPTH: usize = 64;

/// The deepest path a filter or index may address.
pub const MAX_PATH_DEPTH: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PathStep {
    /// An object key.
    Key(String),
    /// One element of an array, `[n]`.
    Index(usize),
    /// Every element of an array, `[]`.
    Each,
}

/// A path into a payload such as `city`, `address.city`, `tags[]`, `items[].price` or
/// `matrix[0][1]`, with keys containing dots or brackets written quoted, as in `"a.b".c`.
///
/// Paths never descend into arrays on their own, so `items.price` reaches nothing when
/// `items` is an array, and `items[].price` is the way to address each element. Filters
/// then treat an array at the end of a path as its elements, see [`crate::filter`].
#[derive(Debug, Clone)]
pub struct JsonPath {
    steps: Vec<PathStep>,
    text: String,
}

// Paths are equal when they address the same values, so `a` and `"a"` or `x[0]` and `x[00]`
// are one path, which lets an index on one spelling serve a filter written with another.
impl PartialEq for JsonPath {
    fn eq(&self, other: &Self) -> bool {
        self.steps == other.steps
    }
}

impl Eq for JsonPath {}

impl std::hash::Hash for JsonPath {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.steps.hash(state);
    }
}

impl JsonPath {
    pub fn steps(&self) -> &[PathStep] {
        &self.steps
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn resolve_in<'a>(&self, root: &'a Map<String, Value>) -> Vec<&'a Value> {
        let mut steps = self.steps.iter();
        let mut current: Vec<&'a Value> = match steps.next() {
            Some(PathStep::Key(k)) => root.get(k).into_iter().collect(),
            // The parser guarantees a path starts with a key.
            _ => return Vec::new(),
        };
        for step in steps {
            let mut next = Vec::with_capacity(current.len());
            for value in current {
                match (step, value) {
                    (PathStep::Key(k), Value::Object(map)) => next.extend(map.get(k)),
                    (PathStep::Index(i), Value::Array(items)) => next.extend(items.get(*i)),
                    (PathStep::Each, Value::Array(items)) => next.extend(items.iter()),
                    _ => {}
                }
            }
            if next.is_empty() {
                return next;
            }
            current = next;
        }
        current
    }
}

impl fmt::Display for JsonPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

impl FromStr for JsonPath {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self> {
        let fail = |reason: &str| Error::InvalidPath {
            path: text.to_owned(),
            reason: reason.to_owned(),
        };
        let bytes = text.as_bytes();
        let mut steps = Vec::new();
        let mut i = 0;
        loop {
            // A key, plain or quoted.
            let key = if bytes.get(i) == Some(&b'"') {
                let start = i + 1;
                let len = text[start..]
                    .find('"')
                    .ok_or_else(|| fail("a quoted key is missing its closing quote"))?;
                i = start + len + 1;
                &text[start..start + len]
            } else {
                let start = i;
                while i < bytes.len() && !matches!(bytes[i], b'.' | b'[' | b']' | b'"') {
                    i += 1;
                }
                &text[start..i]
            };
            if key.is_empty() {
                return Err(fail("keys cannot be empty"));
            }
            steps.push(PathStep::Key(key.to_owned()));

            // Any number of [] or [n] after the key.
            while bytes.get(i) == Some(&b'[') {
                let close = text[i..]
                    .find(']')
                    .ok_or_else(|| fail("a [ is missing its closing ]"))?;
                let inner = &text[i + 1..i + close];
                if inner.is_empty() {
                    steps.push(PathStep::Each);
                } else if inner.bytes().all(|b| b.is_ascii_digit()) {
                    let n = inner
                        .parse()
                        .map_err(|_| fail("array index is too large"))?;
                    steps.push(PathStep::Index(n));
                } else {
                    return Err(fail("brackets hold either nothing or an array index"));
                }
                i += close + 1;
            }

            match bytes.get(i) {
                None => break,
                Some(b'.') => i += 1,
                Some(_) => return Err(fail("expected . or [ after a key")),
            }
        }
        if steps.len() > MAX_PATH_DEPTH {
            return Err(fail(&format!(
                "paths are limited to {MAX_PATH_DEPTH} steps"
            )));
        }
        Ok(JsonPath {
            steps,
            text: text.to_owned(),
        })
    }
}

impl Serialize for JsonPath {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.text)
    }
}

impl<'de> Deserialize<'de> for JsonPath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn payload(v: Value) -> Payload {
        serde_json::from_value(v).unwrap()
    }

    fn path(s: &str) -> JsonPath {
        s.parse().unwrap()
    }

    #[test]
    fn parses_keys_indexes_and_wildcards() {
        assert_eq!(
            path("a.b[][2].c").steps(),
            &[
                PathStep::Key("a".into()),
                PathStep::Key("b".into()),
                PathStep::Each,
                PathStep::Index(2),
                PathStep::Key("c".into()),
            ]
        );
        assert_eq!(
            path("\"a.b\".c").steps(),
            &[PathStep::Key("a.b".into()), PathStep::Key("c".into())]
        );
    }

    #[test]
    fn spellings_of_one_path_are_equal() {
        let p = |s: &str| s.parse::<JsonPath>().unwrap();
        assert_eq!(p("a"), p("\"a\""));
        assert_eq!(p("x[0].y"), p("x[00].\"y\""));
        assert_ne!(p("a.b"), p("\"a.b\""));
        let set: std::collections::HashSet<_> = [p("a"), p("\"a\"")].into();
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn rejects_malformed_paths() {
        for bad in [
            "", ".a", "a.", "a..b", "[0]", "a[x]", "a[", "a]b", "\"a", "a\"b\"", "a[0]b",
        ] {
            assert!(bad.parse::<JsonPath>().is_err(), "{bad:?} should not parse");
        }
        let deep = vec!["k"; MAX_PATH_DEPTH + 1].join(".");
        assert!(deep.parse::<JsonPath>().is_err());
    }

    #[test]
    fn resolves_values() {
        let p = payload(json!({
            "city": "Lisbon",
            "address": {"city": "Porto"},
            "items": [{"price": 1}, {"price": 2}, {"name": "x"}],
            "matrix": [[1, 2], [3, 4]]
        }));
        assert_eq!(p.values_at(&path("city")), vec![&json!("Lisbon")]);
        assert_eq!(p.values_at(&path("address.city")), vec![&json!("Porto")]);
        assert_eq!(
            p.values_at(&path("items[].price")),
            vec![&json!(1), &json!(2)]
        );
        assert_eq!(p.values_at(&path("matrix[1][0]")), vec![&json!(3)]);
        assert!(p.values_at(&path("missing")).is_empty());
        // No implicit descent into arrays.
        assert!(p.values_at(&path("items.price")).is_empty());
    }

    #[test]
    fn payload_depth_is_limited() {
        let mut v = json!(1);
        for _ in 0..MAX_PAYLOAD_DEPTH - 1 {
            v = json!([v]);
        }
        assert!(payload(json!({"k": v.clone()})).validate().is_err());
        let inner = v.as_array().unwrap()[0].clone();
        assert!(payload(json!({"k": inner})).validate().is_ok());
        assert!(
            payload(json!({"a": {"b": [1, {"c": null}]}}))
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn serde_round_trip() {
        let p: JsonPath = serde_json::from_str("\"items[].price\"").unwrap();
        assert_eq!(serde_json::to_string(&p).unwrap(), "\"items[].price\"");
        assert!(serde_json::from_str::<JsonPath>("\"a..b\"").is_err());
    }
}
