//! The filter language and its reference evaluator.
//!
//! [`Filter::matches`] is the definition of what every filter means. Payload indexes and the
//! query planner are optimisations that must return exactly what this evaluator returns, and
//! their tests check that against it.
//!
//! # Semantics
//!
//! A filter holds four optional blocks, all of which must hold:
//!
//! - `must`: every condition is true,
//! - `should`: at least one condition is true, and an empty or absent `should` adds no
//!   constraint,
//! - `must_not`: no condition is true,
//! - `min_should`: at least `min_count` of its conditions are true.
//!
//! An empty filter matches every point.
//!
//! A field condition names a payload path in `key` and one or more predicates, and **every
//! predicate it lists must hold**. With arrays each predicate is checked on its own, so
//! `{"key": "tags", "match": "a", "except": ["b"]}` behaves exactly like two separate `must`
//! conditions, and a `nested` condition is the way to require several things of one element.
//!
//! The values a path reaches are read one array level deep: when the path ends at an array,
//! its elements are the values checked, so `{"key": "tags", "match": "red"}` matches
//! `{"tags": ["red", "blue"]}`. Missing, `null` and `[]` are told apart only by `is_null`:
//!
//! | payload at `key` | `is_empty` | `is_null` | `values_count` |
//! |---|---|---|---|
//! | missing | true | false | 0 |
//! | `null` | true | true | 0 |
//! | `[]` | true | false | 0 |
//! | `[null]` | true | true | 0 |
//! | `"x"` or `["x", null]` | false | false or true | 1 |
//!
//! Predicates:
//!
//! - `match`: some value equals the given string, integer or boolean. Types never coerce, so
//!   `"5"` does not match `5`, while the integer `5` does match a stored `5.0`.
//! - `any`: some value equals one of the given values.
//! - `except`: the field has at least one non-null value and none of its values is in the
//!   list. Unlike Qdrant's existential `except`, `["a", "b"]` never satisfies `except: ["a"]`.
//! - `range`: some numeric value satisfies every bound given (`gt`, `gte`, `lt`, `lte`).
//!   Numbers compare as 64-bit floats, so integers above 2^53 compare approximately.
//! - `values_count`: the number of non-null values satisfies every bound given.
//! - `is_empty: true` holds when there are no non-null values, `false` when there are some.
//! - `is_null: true` holds when some value is `null`, `false` when none is.
//!
//! Other conditions:
//!
//! - `{"has_id": [...]}` holds when the point's id is in the list.
//! - `{"nested": {"key": "items", "filter": {...}}}` holds when some object at `key` (each
//!   object element when it is an array) satisfies the whole inner filter on its own, with
//!   paths inside the inner filter relative to that object. `has_id` is not allowed inside.
//! - Any object with `must`, `should`, `must_not` or `min_should` is a sub-filter.

use std::collections::BTreeSet;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::error::{Error, Result};
use crate::id::PointId;
use crate::payload::{JsonPath, Payload};

/// How deep filters may nest.
pub const MAX_FILTER_DEPTH: usize = 16;
/// How many conditions one filter may hold in total, at every level.
pub const MAX_FILTER_CONDITIONS: usize = 1024;
/// How many values one `any`, `except` or `has_id` list may hold.
pub const MAX_LIST_LEN: usize = 65_536;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Filter {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub must: Vec<Condition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub should: Vec<Condition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub must_not: Vec<Condition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_should: Option<MinShould>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MinShould {
    pub conditions: Vec<Condition>,
    pub min_count: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Condition {
    Field(FieldCondition),
    HasId(HasId),
    Nested(Nested),
    Filter(Filter),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HasId {
    pub has_id: BTreeSet<PointId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Nested {
    pub nested: NestedFilter,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NestedFilter {
    pub key: JsonPath,
    pub filter: Filter,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldCondition {
    pub key: JsonPath,
    #[serde(default, rename = "match", skip_serializing_if = "Option::is_none")]
    pub match_value: Option<MatchValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub any: Option<Vec<MatchValue>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub except: Option<Vec<MatchValue>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<Range>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub values_count: Option<Range>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_empty: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_null: Option<bool>,
}

impl FieldCondition {
    /// A condition on `key` with no predicates yet, for building filters in code.
    pub fn new(key: JsonPath) -> Self {
        FieldCondition {
            key,
            match_value: None,
            any: None,
            except: None,
            range: None,
            values_count: None,
            is_empty: None,
            is_null: None,
        }
    }
}

/// A value that `match`, `any` and `except` compare against.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged, expecting = "a string, an integer or a boolean")]
pub enum MatchValue {
    Bool(bool),
    Integer(i64),
    Keyword(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Range {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gt: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gte: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lt: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lte: Option<f64>,
}

impl Range {
    pub fn contains(&self, x: f64) -> bool {
        self.gt.is_none_or(|b| x > b)
            && self.gte.is_none_or(|b| x >= b)
            && self.lt.is_none_or(|b| x < b)
            && self.lte.is_none_or(|b| x <= b)
    }

    fn is_unbounded(&self) -> bool {
        self.gt.is_none() && self.gte.is_none() && self.lt.is_none() && self.lte.is_none()
    }
}

// Conditions are told apart by their keys, which gives a clear error for an object that is
// none of them, where an untagged enum would only say that nothing matched.
impl<'de> Deserialize<'de> for Condition {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let Value::Object(map) = &value else {
            return Err(D::Error::custom("a condition must be a JSON object"));
        };
        let has = |k: &str| map.contains_key(k);
        let parsed = if has("key") {
            serde_json::from_value(value).map(Condition::Field)
        } else if has("has_id") {
            serde_json::from_value(value).map(Condition::HasId)
        } else if has("nested") {
            serde_json::from_value(value).map(Condition::Nested)
        } else if has("must") || has("should") || has("must_not") || has("min_should") {
            serde_json::from_value(value).map(Condition::Filter)
        } else {
            return Err(D::Error::custom(
                "a condition needs one of key, has_id, nested, must, should, must_not or min_should",
            ));
        };
        parsed.map_err(D::Error::custom)
    }
}

impl MatchValue {
    fn equals(&self, value: &Value) -> bool {
        match (self, value) {
            (MatchValue::Bool(b), Value::Bool(v)) => b == v,
            (MatchValue::Keyword(s), Value::String(v)) => s == v,
            (MatchValue::Integer(i), Value::Number(n)) => {
                if let Some(v) = n.as_i64() {
                    v == *i
                } else if n.is_u64() {
                    // Above i64::MAX, so it cannot equal any i64.
                    false
                } else {
                    n.as_f64()
                        .is_some_and(|f| f.fract() == 0.0 && f == *i as f64)
                }
            }
            _ => false,
        }
    }
}

struct Limits {
    conditions: usize,
}

impl Filter {
    /// Checks the filter is well formed and within the size limits, which every filter from a
    /// client must pass before it reaches the evaluator or the planner.
    pub fn validate(&self) -> Result<()> {
        let mut limits = Limits { conditions: 0 };
        self.validate_at("filter", 1, false, &mut limits)
    }

    fn validate_at(&self, at: &str, depth: usize, nested: bool, limits: &mut Limits) -> Result<()> {
        let fail = |at: &str, reason: String| Error::InvalidFilter {
            at: at.to_owned(),
            reason,
        };
        if depth > MAX_FILTER_DEPTH {
            return Err(fail(
                at,
                format!("filters nest at most {MAX_FILTER_DEPTH} levels deep"),
            ));
        }
        let blocks = [
            ("must", &self.must),
            ("should", &self.should),
            ("must_not", &self.must_not),
        ];
        for (name, conditions) in blocks {
            for (i, c) in conditions.iter().enumerate() {
                c.validate_at(&format!("{at}.{name}[{i}]"), depth, nested, limits)?;
            }
        }
        if let Some(ms) = &self.min_should {
            let here = format!("{at}.min_should");
            if ms.min_count == 0 || ms.min_count > ms.conditions.len() {
                return Err(fail(
                    &here,
                    format!(
                        "min_count must be between 1 and the number of conditions ({}), got {}",
                        ms.conditions.len(),
                        ms.min_count
                    ),
                ));
            }
            for (i, c) in ms.conditions.iter().enumerate() {
                c.validate_at(&format!("{here}.conditions[{i}]"), depth, nested, limits)?;
            }
        }
        Ok(())
    }

    /// Whether the point with this id and payload passes the filter.
    pub fn matches(&self, id: PointId, payload: &Payload) -> bool {
        self.eval(Some(id), &payload.0)
    }

    fn eval(&self, id: Option<PointId>, root: &Map<String, Value>) -> bool {
        self.must.iter().all(|c| c.eval(id, root))
            && (self.should.is_empty() || self.should.iter().any(|c| c.eval(id, root)))
            && !self.must_not.iter().any(|c| c.eval(id, root))
            && self.min_should.as_ref().is_none_or(|ms| {
                ms.conditions
                    .iter()
                    .filter(|c| c.eval(id, root))
                    .take(ms.min_count)
                    .count()
                    == ms.min_count
            })
    }
}

impl Condition {
    fn validate_at(&self, at: &str, depth: usize, nested: bool, limits: &mut Limits) -> Result<()> {
        let fail = |reason: String| Error::InvalidFilter {
            at: at.to_owned(),
            reason,
        };
        limits.conditions += 1;
        if limits.conditions > MAX_FILTER_CONDITIONS {
            return Err(fail(format!(
                "filters hold at most {MAX_FILTER_CONDITIONS} conditions"
            )));
        }
        match self {
            Condition::Field(f) => f.validate().map_err(fail),
            Condition::HasId(h) => {
                if nested {
                    Err(fail("has_id cannot be used inside a nested filter".into()))
                } else if h.has_id.is_empty() {
                    Err(fail("has_id needs at least one id".into()))
                } else if h.has_id.len() > MAX_LIST_LEN {
                    Err(fail(format!("has_id holds at most {MAX_LIST_LEN} ids")))
                } else {
                    Ok(())
                }
            }
            Condition::Nested(n) => {
                n.nested
                    .filter
                    .validate_at(&format!("{at}.nested.filter"), depth + 1, true, limits)
            }
            Condition::Filter(f) => f.validate_at(at, depth + 1, nested, limits),
        }
    }

    fn eval(&self, id: Option<PointId>, root: &Map<String, Value>) -> bool {
        match self {
            Condition::Field(f) => f.eval(root),
            Condition::HasId(h) => id.is_some_and(|id| h.has_id.contains(&id)),
            Condition::Nested(n) => n.nested.key.resolve_in(root).into_iter().any(|v| match v {
                Value::Object(obj) => n.nested.filter.eval(None, obj),
                Value::Array(items) => items.iter().any(|item| match item {
                    Value::Object(obj) => n.nested.filter.eval(None, obj),
                    _ => false,
                }),
                _ => false,
            }),
            Condition::Filter(f) => f.eval(id, root),
        }
    }
}

/// The values a field condition on `path` checks: whatever the path reaches, with an array at
/// the end replaced by its elements, one level deep.
///
/// Payload indexes take their values from here too, so an index and the evaluator always see
/// the same values.
pub fn field_values<'a>(path: &JsonPath, root: &'a Map<String, Value>) -> Vec<&'a Value> {
    let mut values = Vec::new();
    for v in path.resolve_in(root) {
        match v {
            Value::Array(items) => values.extend(items.iter()),
            other => values.push(other),
        }
    }
    values
}

impl FieldCondition {
    fn validate(&self) -> std::result::Result<(), String> {
        let FieldCondition {
            key: _,
            match_value,
            any,
            except,
            range,
            values_count,
            is_empty,
            is_null,
        } = self;
        if match_value.is_none()
            && any.is_none()
            && except.is_none()
            && range.is_none()
            && values_count.is_none()
            && is_empty.is_none()
            && is_null.is_none()
        {
            return Err(format!(
                "the condition on {:?} needs at least one of match, any, except, range, \
                 values_count, is_empty or is_null",
                self.key.as_str()
            ));
        }
        for (name, list) in [("any", any), ("except", except)] {
            if let Some(list) = list {
                if list.is_empty() {
                    return Err(format!("{name} needs at least one value"));
                }
                if list.len() > MAX_LIST_LEN {
                    return Err(format!("{name} holds at most {MAX_LIST_LEN} values"));
                }
            }
        }
        for (name, r) in [("range", range), ("values_count", values_count)] {
            if r.is_some_and(|r| r.is_unbounded()) {
                return Err(format!("{name} needs at least one of gt, gte, lt or lte"));
            }
        }
        Ok(())
    }

    fn eval(&self, root: &Map<String, Value>) -> bool {
        let values = field_values(&self.key, root);
        let non_null = || values.iter().filter(|v| !v.is_null());

        self.match_value
            .as_ref()
            .is_none_or(|m| values.iter().any(|v| m.equals(v)))
            && self
                .any
                .as_ref()
                .is_none_or(|set| values.iter().any(|v| set.iter().any(|m| m.equals(v))))
            && self.except.as_ref().is_none_or(|set| {
                non_null().next().is_some()
                    && !values.iter().any(|v| set.iter().any(|m| m.equals(v)))
            })
            && self.range.as_ref().is_none_or(|r| {
                values
                    .iter()
                    .any(|v| v.as_f64().is_some_and(|x| r.contains(x)))
            })
            && self
                .values_count
                .as_ref()
                .is_none_or(|r| r.contains(non_null().count() as f64))
            && self
                .is_empty
                .is_none_or(|want| (non_null().next().is_none()) == want)
            && self
                .is_null
                .is_none_or(|want| values.iter().any(|v| v.is_null()) == want)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn filter(v: Value) -> Filter {
        let f: Filter = serde_json::from_value(v).expect("filter parses");
        f.validate().expect("filter is valid");
        f
    }

    fn payload(v: Value) -> Payload {
        serde_json::from_value(v).unwrap()
    }

    fn check(f: Value, p: Value) -> bool {
        filter(f).matches(PointId::Num(1), &payload(p))
    }

    #[test]
    fn empty_filter_matches_everything() {
        assert!(check(json!({}), json!({})));
        assert!(check(json!({"should": []}), json!({})));
    }

    #[test]
    fn empty_should_adds_no_constraint_inside_subfilters_too() {
        let f = json!({"must": [{"should": [], "must": [{"key": "a", "match": 1}]}]});
        assert!(check(f.clone(), json!({"a": 1})));
        assert!(!check(f, json!({"a": 2})));
    }

    #[test]
    fn boolean_blocks() {
        let f = json!({
            "must": [{"key": "colour", "match": "red"}],
            "should": [{"key": "size", "match": "s"}, {"key": "size", "match": "m"}],
            "must_not": [{"key": "sold", "match": true}]
        });
        assert!(check(
            f.clone(),
            json!({"colour": "red", "size": "m", "sold": false})
        ));
        assert!(!check(f.clone(), json!({"colour": "red", "size": "l"})));
        assert!(!check(
            f.clone(),
            json!({"colour": "red", "size": "s", "sold": true})
        ));
        assert!(!check(f, json!({"colour": "blue", "size": "s"})));
    }

    #[test]
    fn min_should_counts() {
        let f = json!({"min_should": {"min_count": 2, "conditions": [
            {"key": "a", "match": 1}, {"key": "b", "match": 1}, {"key": "c", "match": 1}
        ]}});
        assert!(check(f.clone(), json!({"a": 1, "c": 1})));
        assert!(!check(f, json!({"a": 1, "b": 2})));
    }

    #[test]
    fn predicates_in_one_condition_are_anded() {
        // Qdrant ORs these, nocturnum requires both.
        let f = json!({"must": [{"key": "price", "range": {"gte": 10}, "match": 50}]});
        assert!(check(f.clone(), json!({"price": 50})));
        assert!(!check(f.clone(), json!({"price": 20})));
        assert!(!check(f, json!({"price": 5})));
    }

    #[test]
    fn match_types_never_coerce() {
        let f = json!({"must": [{"key": "n", "match": 5}]});
        assert!(check(f.clone(), json!({"n": 5})));
        assert!(check(f.clone(), json!({"n": 5.0})));
        assert!(!check(f.clone(), json!({"n": "5"})));
        assert!(!check(f.clone(), json!({"n": 5.5})));
        assert!(!check(f, json!({"n": 18446744073709551615u64})));
        assert!(!check(
            json!({"must": [{"key": "b", "match": true}]}),
            json!({"b": "true"})
        ));
    }

    #[test]
    fn arrays_match_by_element() {
        let f = json!({"must": [{"key": "tags", "match": "red"}]});
        assert!(check(f.clone(), json!({"tags": ["blue", "red"]})));
        assert!(!check(f.clone(), json!({"tags": [["red"]]})));
        assert!(!check(f, json!({"tags": []})));
    }

    #[test]
    fn any_and_except() {
        let any = json!({"must": [{"key": "c", "any": ["a", "b"]}]});
        assert!(check(any.clone(), json!({"c": "b"})));
        assert!(check(any.clone(), json!({"c": ["x", "a"]})));
        assert!(!check(any, json!({"c": "z"})));

        let except = json!({"must": [{"key": "c", "except": ["a"]}]});
        assert!(check(except.clone(), json!({"c": "b"})));
        assert!(check(except.clone(), json!({"c": 1})));
        assert!(!check(except.clone(), json!({"c": "a"})));
        // Not existential: one excluded element fails the whole field.
        assert!(!check(except.clone(), json!({"c": ["a", "b"]})));
        // Needs at least one non-null value.
        assert!(!check(except.clone(), json!({})));
        assert!(!check(except.clone(), json!({"c": null})));
        assert!(!check(except, json!({"c": []})));
    }

    #[test]
    fn ranges() {
        let f = json!({"must": [{"key": "p", "range": {"gt": 1, "lte": 3}}]});
        assert!(!check(f.clone(), json!({"p": 1})));
        assert!(check(f.clone(), json!({"p": 1.5})));
        assert!(check(f.clone(), json!({"p": 3})));
        assert!(!check(f.clone(), json!({"p": 3.01})));
        assert!(check(f.clone(), json!({"p": [0, 2]})));
        assert!(!check(f.clone(), json!({"p": "2"})));
        assert!(!check(f, json!({})));
    }

    #[test]
    fn missing_null_and_empty_table() {
        let cases = [
            (json!({}), true, false, 0),
            (json!({"k": null}), true, true, 0),
            (json!({"k": []}), true, false, 0),
            (json!({"k": [null]}), true, true, 0),
            (json!({"k": "x"}), false, false, 1),
            (json!({"k": ["x", null]}), false, true, 1),
            (json!({"k": ["x", "y"]}), false, false, 2),
            (json!({"k": {}}), false, false, 1),
            (json!({"k": ""}), false, false, 1),
        ];
        for (p, empty, null, count) in cases {
            let is_empty = json!({"must": [{"key": "k", "is_empty": true}]});
            let not_empty = json!({"must": [{"key": "k", "is_empty": false}]});
            let is_null = json!({"must": [{"key": "k", "is_null": true}]});
            let not_null = json!({"must": [{"key": "k", "is_null": false}]});
            let count_eq =
                json!({"must": [{"key": "k", "values_count": {"gte": count, "lte": count}}]});
            assert_eq!(check(is_empty, p.clone()), empty, "is_empty on {p}");
            assert_eq!(check(not_empty, p.clone()), !empty, "is_empty false on {p}");
            assert_eq!(check(is_null, p.clone()), null, "is_null on {p}");
            assert_eq!(check(not_null, p.clone()), !null, "is_null false on {p}");
            assert!(check(count_eq, p.clone()), "values_count {count} on {p}");
        }
    }

    #[test]
    fn has_id() {
        let f = filter(json!({"must": [{"has_id": [1, "550e8400-e29b-41d4-a716-446655440000"]}]}));
        assert!(f.matches(PointId::Num(1), &Payload::new()));
        assert!(!f.matches(PointId::Num(2), &Payload::new()));
    }

    #[test]
    fn nested_requires_one_element_to_satisfy_everything() {
        let p = json!({"items": [{"colour": "red", "size": "s"}, {"colour": "blue", "size": "m"}]});
        let flat = json!({"must": [
            {"key": "items[].colour", "match": "red"},
            {"key": "items[].size", "match": "m"}
        ]});
        let nested = json!({"must": [{"nested": {"key": "items", "filter": {"must": [
            {"key": "colour", "match": "red"},
            {"key": "size", "match": "m"}
        ]}}}]});
        // Flattened conditions can be met by different elements, nested cannot.
        assert!(check(flat, p.clone()));
        assert!(!check(nested.clone(), p));
        assert!(check(
            nested.clone(),
            json!({"items": [{"colour": "red", "size": "m"}]})
        ));
        // A single object at the key counts as one element.
        assert!(check(
            nested,
            json!({"items": {"colour": "red", "size": "m"}})
        ));
    }

    #[test]
    fn validation_errors() {
        let bad = [
            json!({"must": [{"key": "a"}]}),
            json!({"must": [{"key": "a", "any": []}]}),
            json!({"must": [{"key": "a", "range": {}}]}),
            json!({"must": [{"has_id": []}]}),
            json!({"min_should": {"min_count": 0, "conditions": [{"key": "a", "match": 1}]}}),
            json!({"min_should": {"min_count": 2, "conditions": [{"key": "a", "match": 1}]}}),
            json!({"must": [{"nested": {"key": "x", "filter": {"must": [{"has_id": [1]}]}}}]}),
        ];
        for b in bad {
            let f: Filter = serde_json::from_value(b.clone()).unwrap();
            assert!(f.validate().is_err(), "{b} should fail validation");
        }

        let mut deep = json!({"must": [{"key": "a", "match": 1}]});
        for _ in 0..MAX_FILTER_DEPTH {
            deep = json!({"must": [deep]});
        }
        let f: Filter = serde_json::from_value(deep).unwrap();
        assert!(f.validate().is_err());

        let many: Vec<Value> = (0..=MAX_FILTER_CONDITIONS)
            .map(|i| json!({"key": "a", "match": i}))
            .collect();
        let f: Filter = serde_json::from_value(json!({"should": many})).unwrap();
        assert!(f.validate().is_err());
    }

    #[test]
    fn parse_errors_are_specific() {
        let err = serde_json::from_value::<Filter>(json!({"must": [{"colour": "red"}]}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("needs one of key"), "{err}");
        let err = serde_json::from_value::<Filter>(json!({"must": [{"key": "a", "match": 1.5}]}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("a string, an integer or a boolean"), "{err}");
        assert!(
            serde_json::from_value::<Filter>(json!({"must": [{"key": "a", "mtch": 1}]})).is_err()
        );
        assert!(serde_json::from_value::<Filter>(json!({"must": [], "extra": 1})).is_err());
    }

    #[test]
    fn round_trips_through_json() {
        let v = json!({
            "must": [{"key": "a", "match": "x"}, {"has_id": [3, 7]}],
            "should": [{"nested": {"key": "items", "filter": {"must": [{"key": "p", "range": {"lt": 2.0}}]}}}],
            "must_not": [{"must": [{"key": "b", "is_null": true}]}],
            "min_should": {"conditions": [{"key": "c", "any": [1, true, "z"]}], "min_count": 1}
        });
        let f: Filter = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(serde_json::to_value(&f).unwrap(), v);
    }
}
