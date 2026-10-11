use super::*;
use crate::flat::{Scored, search_filtered, search_rows};
use nocturnum_core::Distance;
use proptest::prelude::*;
use serde_json::json;

const PATHS: [&str; 5] = ["a", "b", "n", "obj.x", "items[].c"];
const KINDS: [PayloadIndexKind; 4] = [
    PayloadIndexKind::Keyword,
    PayloadIndexKind::Integer,
    PayloadIndexKind::Float,
    PayloadIndexKind::Bool,
];

/// Scalars chosen to collide often and to hit the awkward cases: integers against whole
/// floats, -0.0, a u64 above i64::MAX, strings that look like numbers.
fn scalar() -> impl Strategy<Value = Value> {
    prop_oneof![
        prop::sample::select(vec!["red", "blue", "5", ""]).prop_map(|s| json!(s)),
        (-2i64..3).prop_map(|i| json!(i)),
        prop::sample::select(vec![-1.5f64, -0.0, 0.0, 2.0, 2.5, 1e300]).prop_map(|f| json!(f)),
        Just(json!(u64::MAX)),
        Just(json!(i64::MIN)),
        any::<bool>().prop_map(|b| json!(b)),
        Just(Value::Null),
    ]
}

fn field_value() -> impl Strategy<Value = Value> {
    prop_oneof![
        3 => scalar(),
        2 => prop::collection::vec(scalar(), 0..4).prop_map(Value::Array),
        // An array inside an array is not flattened, so its elements match nothing.
        1 => scalar().prop_map(|v| json!([[v]])),
        1 => Just(json!({"k": 1})),
    ]
}

fn payload() -> impl Strategy<Value = Payload> {
    let maybe = |s: BoxedStrategy<Value>| prop::option::weighted(0.8, s);
    (
        maybe(field_value().boxed()),
        maybe(field_value().boxed()),
        maybe(field_value().boxed()),
        maybe(field_value().prop_map(|x| json!({ "x": x })).boxed()),
        maybe(
            prop::collection::vec(field_value().prop_map(|c| json!({ "c": c })), 0..3)
                .prop_map(Value::Array)
                .boxed(),
        ),
    )
        .prop_map(|(a, b, n, obj, items)| {
            let mut map = serde_json::Map::new();
            for (k, v) in [("a", a), ("b", b), ("n", n), ("obj", obj), ("items", items)] {
                if let Some(v) = v {
                    map.insert(k.into(), v);
                }
            }
            Payload(map)
        })
}

fn match_value() -> impl Strategy<Value = Value> {
    prop_oneof![
        prop::sample::select(vec!["red", "blue", "5", ""]).prop_map(|s| json!(s)),
        prop::sample::select(vec![-2i64, 0, 2, 5, i64::MIN]).prop_map(|i| json!(i)),
        any::<bool>().prop_map(|b| json!(b)),
    ]
}

fn range() -> impl Strategy<Value = Value> {
    let bound = prop::sample::select(vec![-2.0f64, -0.0, 0.0, 1.0, 2.0, 2.5, 1e300]);
    let ops = prop::sample::subsequence(vec!["gt", "gte", "lt", "lte"], 1..3);
    (ops, prop::collection::vec(bound, 2)).prop_map(|(ops, bounds)| {
        let map: serde_json::Map<_, _> = ops
            .into_iter()
            .zip(bounds)
            .map(|(op, b)| (op.to_owned(), json!(b)))
            .collect();
        Value::Object(map)
    })
}

/// One field condition with one or two predicates.
fn field_condition(paths: Vec<&'static str>) -> impl Strategy<Value = Value> {
    let predicate = prop_oneof![
        match_value().prop_map(|m| ("match", m)),
        prop::collection::vec(match_value(), 1..3).prop_map(|l| ("any", json!(l))),
        prop::collection::vec(match_value(), 1..3).prop_map(|l| ("except", json!(l))),
        range().prop_map(|r| ("range", r)),
        (0u32..3, 0u32..3)
            .prop_map(|(lo, span)| ("values_count", json!({"gte": lo, "lte": lo + span}))),
        any::<bool>().prop_map(|b| ("is_empty", json!(b))),
        any::<bool>().prop_map(|b| ("is_null", json!(b))),
    ];
    (
        prop::sample::select(paths),
        prop::collection::vec(predicate, 1..3),
    )
        .prop_map(|(key, predicates)| {
            let mut map = serde_json::Map::new();
            map.insert("key".into(), json!(key));
            for (name, value) in predicates {
                map.insert(name.into(), value);
            }
            Value::Object(map)
        })
}

fn filter_value(nested: bool) -> impl Strategy<Value = Value> {
    let mut leaves = vec![
        field_condition(PATHS.to_vec()).boxed(),
        prop::collection::btree_set(0u64..45, 1..4)
            .prop_map(|ids| json!({ "has_id": ids }))
            .boxed(),
    ];
    if nested {
        leaves.push(
            field_condition(vec!["c"])
                .prop_map(|c| json!({"nested": {"key": "items", "filter": {"must": [c]}}}))
                .boxed(),
        );
    }
    let leaf = prop::strategy::Union::new(leaves);
    let block = |inner: BoxedStrategy<Value>| prop::collection::vec(inner, 0..3);
    let tree = leaf.prop_recursive(3, 24, 3, move |inner| {
        let inner = inner.boxed();
        (
            block(inner.clone()),
            block(inner.clone()),
            block(inner.clone()),
            prop::option::weighted(0.3, (prop::collection::vec(inner, 1..4), 1usize..4)),
        )
            .prop_map(|(must, should, must_not, ms)| {
                let mut f = json!({"must": must, "should": should, "must_not": must_not});
                if let Some((conditions, k)) = ms {
                    let k = k.min(conditions.len());
                    f["min_should"] = json!({"conditions": conditions, "min_count": k});
                }
                f
            })
    });
    prop::collection::vec(tree, 0..3).prop_map(|must| json!({ "must": must }))
}

fn parse(f: &Value) -> Filter {
    let filter: Filter = serde_json::from_value(f.clone()).expect("generated filters parse");
    filter.validate().expect("generated filters are valid");
    filter
}

#[derive(Debug)]
struct Data {
    payloads: Vec<Payload>,
    ids: Vec<PointId>,
}

fn data() -> impl Strategy<Value = Data> {
    prop::collection::vec(payload(), 0..40).prop_map(|payloads| Data {
        // Ids are unordered so rows and ids differ, and unique since a segment holds each
        // point once.
        ids: (0..payloads.len() as u64)
            .map(|r| PointId::Num(if r < 13 { (r * 7) % 13 } else { r }))
            .collect(),
        payloads,
    })
}

fn full_schema() -> Vec<(JsonPath, PayloadIndexKind)> {
    PATHS
        .iter()
        .flat_map(|p| KINDS.iter().map(move |k| (p.parse().unwrap(), *k)))
        .collect()
}

fn build(d: &Data, schema: &[(JsonPath, PayloadIndexKind)]) -> PayloadIndex {
    let points = (0..d.payloads.len()).map(|r| (r as u32, d.ids[r], &d.payloads[r]));
    PayloadIndex::build(d.payloads.len() as u32, schema, points)
}

fn reference(d: &Data, filter: &Filter) -> RoaringBitmap {
    (0..d.payloads.len())
        .filter(|&r| filter.matches(d.ids[r], &d.payloads[r]))
        .map(|r| r as u32)
        .collect()
}

fn check_with<'a>(d: &'a Data, filter: &'a Filter) -> impl FnMut(u32) -> Result<bool, ()> + 'a {
    let rows = reference(d, filter);
    move |r| Ok(rows.contains(r))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(600))]

    /// With any mix of indexed and unindexed fields, the bounds bracket the reference
    /// evaluator, and reading the uncertain rows gives exactly its answer.
    #[test]
    fn bounds_bracket_the_reference(
        d in data(),
        f in filter_value(true),
        picks in prop::collection::vec(any::<bool>(), PATHS.len() * KINDS.len()),
    ) {
        let filter = parse(&f);
        let schema: Vec<_> = full_schema()
            .into_iter()
            .zip(picks)
            .filter_map(|(s, keep)| keep.then_some(s))
            .collect();
        let index = build(&d, &schema);
        let want = reference(&d, &filter);
        let b = index.bounds(&filter);
        prop_assert!(b.sure.is_subset(&want), "sure holds a row that does not match");
        prop_assert!(want.is_subset(&b.maybe), "maybe misses a matching row");

        let mut asked = 0;
        let mut check = check_with(&d, &filter);
        let got = index.matching_rows(&filter, |r| { asked += 1; check(r) }).unwrap();
        prop_assert_eq!(got, want);
        prop_assert_eq!(asked, b.uncertain().len());
    }

    /// With every field indexed for every kind and no nested conditions, the indexes answer
    /// every filter alone.
    #[test]
    fn full_indexes_answer_exactly(d in data(), f in filter_value(false)) {
        let filter = parse(&f);
        let index = build(&d, &full_schema());
        let b = index.bounds(&filter);
        prop_assert!(b.is_exact(), "rows were left uncertain: {:?}", b.uncertain());
        prop_assert_eq!(b.sure, reference(&d, &filter));
    }

    /// Filtered search returns what an exact search over the matching rows returns.
    #[test]
    fn filtered_search_matches_searching_the_matches(
        d in data(),
        f in filter_value(true),
        k in 0usize..8,
        q in prop::collection::vec(-2.0f32..2.0, 2),
        indexed in any::<bool>(),
    ) {
        let filter = parse(&f);
        let n = d.payloads.len() as u32;
        let vectors: Vec<f32> = (0..n).flat_map(|r| [(r % 5) as f32, (r % 3) as f32 - 1.0]).collect();
        let schema = if indexed { full_schema() } else { Vec::new() };
        let index = build(&d, &schema);
        let want = reference(&d, &filter);
        let expected = search_rows(Distance::Dot, &vectors, 2, &q, k, want.iter());
        let got: Vec<Scored<u32>> = search_filtered(
            Distance::Dot, &vectors, 2, &q, k, &index.bounds(&filter), check_with(&d, &filter),
        ).unwrap();
        prop_assert_eq!(got, expected);
    }
}

fn index_of(payloads: &[Value], schema: &[(&str, PayloadIndexKind)]) -> (PayloadIndex, Data) {
    let d = Data {
        payloads: payloads
            .iter()
            .map(|p| serde_json::from_value(p.clone()).unwrap())
            .collect(),
        ids: (0..payloads.len() as u64).map(PointId::Num).collect(),
    };
    let schema: Vec<_> = schema
        .iter()
        .map(|(p, k)| (p.parse().unwrap(), *k))
        .collect();
    (build(&d, &schema), d)
}

fn exact_rows(index: &PayloadIndex, f: Value) -> Vec<u32> {
    let b = index.bounds(&parse(&f));
    assert!(b.is_exact(), "{f} was not answered by the index alone");
    b.sure.iter().collect()
}

#[test]
fn integers_match_whole_floats_but_not_strings_or_huge_numbers() {
    let (index, _) = index_of(
        &[
            json!({"n": 5}),
            json!({"n": 5.0}),
            json!({"n": "5"}),
            json!({"n": 5.5}),
            json!({"n": u64::MAX}),
            json!({"n": -0.0}),
        ],
        &[("n", PayloadIndexKind::Integer)],
    );
    assert_eq!(
        exact_rows(&index, json!({"must": [{"key": "n", "match": 5}]})),
        [0, 1]
    );
    assert_eq!(
        exact_rows(&index, json!({"must": [{"key": "n", "match": 0}]})),
        [5]
    );
    // A string match needs a keyword index, which this field does not have.
    assert!(
        !index
            .bounds(&parse(&json!({"must": [{"key": "n", "match": "5"}]})))
            .is_exact()
    );
}

#[test]
fn ranges_count_every_number_and_respect_open_bounds() {
    let (index, _) = index_of(
        &[
            json!({"p": 1}),
            json!({"p": 1.5}),
            json!({"p": [0, 3]}),
            json!({"p": 3}),
            json!({}),
        ],
        &[("p", PayloadIndexKind::Float)],
    );
    let rows = |r: Value| exact_rows(&index, json!({"must": [{"key": "p", "range": r}]}));
    assert_eq!(rows(json!({"gt": 1, "lte": 3})), [1, 2, 3]);
    assert_eq!(rows(json!({"gte": 1, "lt": 3})), [0, 1]);
    assert_eq!(rows(json!({"gt": 3})), Vec::<u32>::new());
    assert_eq!(rows(json!({"lt": 0.5})), [2]);
}

#[test]
fn except_needs_a_value_and_excludes_on_any_element() {
    let (index, _) = index_of(
        &[
            json!({"c": "a"}),
            json!({"c": ["a", "b"]}),
            json!({"c": "b"}),
            json!({"c": null}),
            json!({}),
        ],
        &[("c", PayloadIndexKind::Keyword)],
    );
    assert_eq!(
        exact_rows(&index, json!({"must": [{"key": "c", "except": ["a"]}]})),
        [2]
    );
}

#[test]
fn presence_answers_empty_null_and_counts_without_a_value_index() {
    let (index, _) = index_of(
        &[
            json!({}),
            json!({"k": null}),
            json!({"k": []}),
            json!({"k": ["x", null]}),
            json!({"k": [1, 2]}),
        ],
        &[("k", PayloadIndexKind::Bool)],
    );
    let rows = |c: Value| exact_rows(&index, json!({"must": [c]}));
    assert_eq!(rows(json!({"key": "k", "is_empty": true})), [0, 1, 2]);
    assert_eq!(rows(json!({"key": "k", "is_null": true})), [1, 3]);
    assert_eq!(rows(json!({"key": "k", "values_count": {"gte": 2}})), [4]);
    assert_eq!(
        rows(json!({"key": "k", "values_count": {"lt": 1}})),
        [0, 1, 2]
    );
}

#[test]
fn unindexed_and_nested_conditions_leave_rows_uncertain() {
    let (index, d) = index_of(
        &[
            json!({"a": "x", "items": [{"c": 1}]}),
            json!({"a": "y", "items": [{"c": 2}]}),
        ],
        &[("a", PayloadIndexKind::Keyword)],
    );
    let f = parse(&json!({"must": [
        {"key": "a", "match": "x"},
        {"nested": {"key": "items", "filter": {"must": [{"key": "c", "match": 1}]}}}
    ]}));
    let b = index.bounds(&f);
    // The keyword index rules out row 1, the nested condition needs row 0 read.
    assert!(b.sure.is_empty());
    assert_eq!(b.maybe.iter().collect::<Vec<_>>(), [0]);
    let got = index.matching_rows(&f, check_with(&d, &f)).unwrap();
    assert_eq!(got.iter().collect::<Vec<_>>(), [0]);
}

#[test]
fn restrict_drops_rows_that_are_no_longer_live() {
    let (index, _) = index_of(&[json!({}), json!({}), json!({})], &[]);
    let mut b = index.bounds(&Filter::default());
    b.restrict(&[0u32, 2].into_iter().collect());
    assert_eq!(b.sure.iter().collect::<Vec<_>>(), [0, 2]);
}

#[test]
fn an_index_serves_every_spelling_of_its_path() {
    let (index, _) = index_of(
        &[json!({"a": "red", "x": [{"y": 1}]}), json!({"a": "blue"})],
        &[
            ("a", PayloadIndexKind::Keyword),
            ("x[0].y", PayloadIndexKind::Integer),
        ],
    );
    assert_eq!(
        exact_rows(&index, json!({"must": [{"key": "\"a\"", "match": "red"}]})),
        [0]
    );
    assert_eq!(
        exact_rows(
            &index,
            json!({"must": [{"key": "x[00].\"y\"", "match": 1}]})
        ),
        [0]
    );
}
