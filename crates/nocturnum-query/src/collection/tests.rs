use std::collections::BTreeMap;
use std::time::Duration;

use nocturnum_core::{Distance, PayloadIndexKind};
use nocturnum_storage::{GraphOptions, WalOptions};
use proptest::prelude::*;
use proptest::strategy::Strategy;
use serde_json::json;
use tempfile::TempDir;

use super::*;

fn options() -> ShardOptions {
    ShardOptions {
        wal: WalOptions {
            segment_bytes: 8192,
            ..WalOptions::default()
        },
        flush_bytes: usize::MAX,
        background_sync: Duration::from_millis(50),
        // Every flushed segment builds a graph, so graphs go through every path tested here.
        graph: GraphOptions {
            min_bytes: 0,
            ..GraphOptions::default()
        },
    }
}

fn create(dir: &Path, distance: Distance) -> Collection {
    Collection::open(dir, options(), Some(VectorConfig { dim: 2, distance }))
        .unwrap()
        .0
}

fn payload(v: Value) -> Payload {
    serde_json::from_value(v).unwrap()
}

fn filter(v: Value) -> Filter {
    serde_json::from_value(v).unwrap()
}

fn point(id: u64, vector: [f32; 2], p: Value) -> PointWrite {
    PointWrite {
        id: PointId::Num(id),
        vector: vector.to_vec(),
        payload: payload(p),
    }
}

fn spec(path: &str, kind: PayloadIndexKind) -> PayloadIndexSpec {
    PayloadIndexSpec {
        path: path.parse().unwrap(),
        kind,
    }
}

/// Filters the model test checks every read with, over an indexable field, an unindexed one,
/// ids and combinations.
fn filters() -> Vec<Option<Filter>> {
    let mut out = vec![None];
    for f in [
        json!({"must": [{"key": "colour", "match": "red"}]}),
        json!({"must": [{"key": "n", "range": {"gte": 2}}]}),
        json!({"should": [{"key": "colour", "match": "blue"}, {"key": "n", "match": 0}]}),
        json!({"must_not": [{"key": "colour", "any": ["red", "green"]}]}),
        json!({"must": [{"has_id": [1, 3, 5, 7, 11]}]}),
        json!({"must": [{"key": "colour", "is_empty": true}]}),
    ] {
        out.push(Some(filter(f)));
    }
    out
}

#[derive(Debug, Clone)]
enum Step {
    Upsert(Vec<(u64, [i8; 2], u8, u8)>),
    Delete(Vec<u64>),
    DeleteWhere(usize),
    SetColour(Vec<u64>, u8),
    ClearColourWhere(usize),
    Flush,
    Index(bool, bool),
}

const COLOURS: [&str; 3] = ["red", "blue", "green"];

fn step() -> impl Strategy<Value = Step> {
    let id = 0u64..16;
    prop_oneof![
        4 => prop::collection::vec((id.clone(), any::<[i8; 2]>(), 0u8..4, 0u8..5), 1..5).prop_map(Step::Upsert),
        1 => prop::collection::vec(id.clone(), 1..4).prop_map(Step::Delete),
        1 => (1usize..7).prop_map(Step::DeleteWhere),
        1 => (prop::collection::vec(id, 1..4), 0u8..3).prop_map(|(ids, c)| Step::SetColour(ids, c)),
        1 => (1usize..7).prop_map(Step::ClearColourWhere),
        2 => Just(Step::Flush),
        1 => (any::<bool>(), any::<bool>()).prop_map(|(a, b)| Step::Index(a, b)),
    ]
}

type Model = BTreeMap<PointId, (Vec<f32>, Payload)>;

fn model_matches(model: &Model, f: Option<&Filter>) -> Vec<PointId> {
    model
        .iter()
        .filter(|(id, (_, p))| f.is_none_or(|f| f.matches(**id, p)))
        .map(|(id, _)| *id)
        .collect()
}

fn apply(c: &Collection, model: &mut Model, step: &Step) {
    let fs = filters();
    match step {
        Step::Upsert(points) => {
            let writes = points
                .iter()
                .map(|(id, v, colour, n)| {
                    let mut p = json!({"n": n});
                    if let Some(c) = COLOURS.get(*colour as usize) {
                        p["colour"] = json!(c);
                    }
                    point(*id, [v[0] as f32 / 8.0, v[1] as f32 / 8.0], p)
                })
                .collect::<Vec<_>>();
            for w in &writes {
                model.insert(w.id, (w.vector.clone(), w.payload.clone()));
            }
            c.upsert(writes, Ack::Applied).unwrap();
        }
        Step::Delete(ids) => {
            let ids: Vec<_> = ids.iter().map(|i| PointId::Num(*i)).collect();
            for id in &ids {
                model.remove(id);
            }
            c.delete(&Selector::Ids(ids), Ack::Applied).unwrap();
        }
        Step::DeleteWhere(i) => {
            let f = fs[*i].clone().unwrap();
            for id in model_matches(model, Some(&f)) {
                model.remove(&id);
            }
            c.delete(&Selector::Filter(f), Ack::Applied).unwrap();
        }
        Step::SetColour(ids, colour) => {
            let ids: Vec<_> = ids.iter().map(|i| PointId::Num(*i)).collect();
            for id in &ids {
                if let Some((_, p)) = model.get_mut(id) {
                    p.0.insert("colour".into(), json!(COLOURS[*colour as usize]));
                }
            }
            let set = payload(json!({"colour": COLOURS[*colour as usize]}));
            c.set_payload(&Selector::Ids(ids), set, Ack::Applied)
                .unwrap();
        }
        Step::ClearColourWhere(i) => {
            let f = fs[*i].clone().unwrap();
            for id in model_matches(model, Some(&f)) {
                model.get_mut(&id).unwrap().1.0.remove("colour");
            }
            c.delete_payload_keys(&Selector::Filter(f), vec!["colour".into()], Ack::Applied)
                .unwrap();
        }
        Step::Flush => {
            c.flush().unwrap();
        }
        Step::Index(colour, n) => {
            for (on, s) in [
                (*colour, spec("colour", PayloadIndexKind::Keyword)),
                (*n, spec("n", PayloadIndexKind::Integer)),
            ] {
                if on {
                    c.create_payload_index(s).unwrap();
                } else {
                    c.drop_payload_index(&s).unwrap();
                }
            }
        }
    }
}

/// Every read the collection offers agrees with the model, for every filter.
fn check_reads(c: &Collection, model: &Model, q: [f32; 2], k: usize, offset: usize) {
    for f in filters() {
        let want = model_matches(model, f.as_ref());
        assert_eq!(
            c.count(f.as_ref()).unwrap(),
            want.len() as u64,
            "count {f:?}"
        );

        let mut scrolled = Vec::new();
        let mut after = None;
        loop {
            let page = c
                .scroll(&ScrollRequest {
                    filter: f.clone(),
                    after,
                    limit: 3,
                    with_payload: WithPayload::All,
                    with_vector: true,
                })
                .unwrap();
            for p in &page.points {
                let (v, pl) = &model[&p.id];
                assert_eq!(p.vector.as_ref(), Some(v));
                assert_eq!(p.payload.as_ref(), Some(pl));
            }
            scrolled.extend(page.points.iter().map(|p| p.id));
            match page.next {
                Some(n) => after = Some(n),
                None => break,
            }
        }
        assert_eq!(scrolled, want, "scroll {f:?}");

        let mut scored: Vec<(f32, PointId)> = want
            .iter()
            .map(|id| (Distance::Dot.score(&q, &model[id].0), *id))
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap().then(a.1.cmp(&b.1)));
        let expected: Vec<_> = scored.into_iter().skip(offset).take(k).collect();
        let mut req = SearchRequest::new(q.to_vec(), k);
        req.filter = f.clone();
        req.offset = offset;
        let got: Vec<_> = c
            .search(&req)
            .unwrap()
            .into_iter()
            .map(|s| (s.score, s.id))
            .collect();
        assert_eq!(got, expected, "search {f:?}");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn reads_match_a_model_through_writes_flushes_and_reopens(
        steps in prop::collection::vec(step(), 1..30),
        q in any::<[i8; 2]>(),
        k in 1usize..6,
        offset in 0usize..3,
        reopen_at in prop::option::of(0usize..30),
    ) {
        let dir = TempDir::new().unwrap();
        let mut c = create(dir.path(), Distance::Dot);
        let mut model = Model::new();
        let q = [q[0] as f32 / 8.0, q[1] as f32 / 8.0];
        for (i, s) in steps.iter().enumerate() {
            apply(&c, &mut model, s);
            if reopen_at == Some(i) {
                drop(c);
                c = Collection::open(dir.path(), options(), None).unwrap().0;
            }
        }
        check_reads(&c, &model, q, k, offset);
    }
}

#[test]
fn search_reports_distances_and_applies_thresholds() {
    let dir = TempDir::new().unwrap();
    let c = create(dir.path(), Distance::Euclid);
    c.upsert(
        vec![
            point(1, [0.0, 0.0], json!({})),
            point(2, [3.0, 4.0], json!({})),
            point(3, [6.0, 8.0], json!({})),
        ],
        Ack::Durable,
    )
    .unwrap();
    c.flush().unwrap();
    let mut req = SearchRequest::new(vec![0.0, 0.0], 3);
    let got: Vec<_> = c
        .search(&req)
        .unwrap()
        .iter()
        .map(|s| (s.id, s.score))
        .collect();
    assert_eq!(
        got,
        [
            (PointId::Num(1), 0.0),
            (PointId::Num(2), 5.0),
            (PointId::Num(3), 10.0)
        ]
    );
    // For a distance the threshold is an upper bound.
    req.score_threshold = Some(5.0);
    assert_eq!(c.search(&req).unwrap().len(), 2);
}

#[test]
fn payload_selection_includes_and_excludes_paths() {
    let dir = TempDir::new().unwrap();
    let c = create(dir.path(), Distance::Dot);
    let p = json!({"a": 1, "b": {"c": 2, "d": 3}, "e": [1, 2]});
    c.upsert(vec![point(1, [1.0, 0.0], p)], Ack::Durable)
        .unwrap();
    let get = |with: WithPayload| {
        c.retrieve(&[PointId::Num(1)], &with, false).unwrap()[0]
            .payload
            .clone()
    };
    let paths = |ps: &[&str]| ps.iter().map(|p| p.parse().unwrap()).collect();
    assert_eq!(get(WithPayload::None), None);
    assert_eq!(
        get(WithPayload::Include(paths(&["a", "b.d", "missing"]))),
        Some(payload(json!({"a": 1, "b": {"d": 3}})))
    );
    assert_eq!(
        get(WithPayload::Exclude(paths(&["b.c", "e"]))),
        Some(payload(json!({"a": 1, "b": {"d": 3}})))
    );
    let bad = c.retrieve(
        &[PointId::Num(1)],
        &WithPayload::Include(paths(&["e[0]"])),
        false,
    );
    assert!(matches!(bad, Err(ref e) if e.is_invalid()));
}

#[test]
fn retrieve_keeps_request_order_and_skips_missing_and_repeats() {
    let dir = TempDir::new().unwrap();
    let c = create(dir.path(), Distance::Dot);
    c.upsert(
        vec![
            point(1, [1.0, 0.0], json!({})),
            point(2, [0.0, 1.0], json!({})),
        ],
        Ack::Durable,
    )
    .unwrap();
    let ids = [2, 9, 1, 2].map(PointId::Num);
    let got: Vec<_> = c
        .retrieve(&ids, &WithPayload::None, true)
        .unwrap()
        .into_iter()
        .map(|p| (p.id, p.vector))
        .collect();
    assert_eq!(
        got,
        [
            (PointId::Num(2), Some(vec![0.0, 1.0])),
            (PointId::Num(1), Some(vec![1.0, 0.0]))
        ]
    );
}

#[test]
fn requests_out_of_bounds_are_refused() {
    let dir = TempDir::new().unwrap();
    let c = create(dir.path(), Distance::Cosine);
    let mut req = SearchRequest::new(vec![1.0, 0.0], MAX_RESULT_WINDOW);
    req.offset = 1;
    assert!(matches!(c.search(&req), Err(ref e) if e.is_invalid()));
    // A zero vector has no direction to compare by cosine.
    let req = SearchRequest::new(vec![0.0, 0.0], 1);
    assert!(matches!(
        c.search(&req),
        Err(QueryError::Input(nocturnum_core::Error::ZeroVector))
    ));
    let req = SearchRequest::new(vec![1.0], 1);
    assert!(matches!(
        c.search(&req),
        Err(QueryError::Input(
            nocturnum_core::Error::DimensionMismatch { .. }
        ))
    ));
    let mut req = SearchRequest::new(vec![1.0, 0.0], 1);
    req.hnsw_ef = Some(MAX_HNSW_EF + 1);
    assert!(matches!(c.search(&req), Err(QueryError::Invalid(_))));
    req.hnsw_ef = Some(0);
    assert!(matches!(c.search(&req), Err(QueryError::Invalid(_))));
    let scroll = ScrollRequest {
        filter: None,
        after: None,
        limit: 0,
        with_payload: WithPayload::None,
        with_vector: false,
    };
    assert!(matches!(c.scroll(&scroll), Err(ref e) if e.is_invalid()));
    let bad_filter = filter(json!({"must": [{"key": "a", "any": []}]}));
    assert!(matches!(
        c.count(Some(&bad_filter)),
        Err(QueryError::Input(
            nocturnum_core::Error::InvalidFilter { .. }
        ))
    ));
}

/// With segments big enough for graphs and a planner willing to use them, search walks the
/// graphs: a tiny beam misses some true neighbours, which proves the graph path ran, and the
/// default beam finds nearly all of them, with and without a filter.
#[test]
fn search_walks_segment_graphs_when_the_planner_says_so() {
    let dir = TempDir::new().unwrap();
    let c = Collection::open(
        dir.path(),
        options(),
        Some(VectorConfig {
            dim: 16,
            distance: Distance::Euclid,
        }),
    )
    .unwrap()
    .0
    .with_plan_options(crate::PlanOptions {
        enumerate_up_to: 100,
        samples: 64,
    });
    let mut x = 0x1234_5678_9abc_def1u64;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        (x >> 40) as f32 / (1u64 << 24) as f32
    };
    for batch in 0..4u64 {
        let points = (0..1_000u64)
            .map(|i| {
                let id = batch * 1_000 + i;
                PointWrite {
                    id: PointId::Num(id),
                    vector: (0..16).map(|_| next()).collect(),
                    payload: payload(json!({"half": id % 2 == 0})),
                }
            })
            .collect();
        c.upsert(points, Ack::Applied).unwrap();
        c.flush().unwrap();
    }
    c.create_payload_index(spec("half", PayloadIndexKind::Bool))
        .unwrap();

    assert!(
        c.shard.view().segments().all(|s| s.graph().is_some()),
        "every segment has a graph"
    );

    let half = filter(json!({"must": [{"key": "half", "match": true}]}));
    for f in [None, Some(half)] {
        let mut recall = |ef: usize, queries: usize| {
            let mut hits = 0;
            for _ in 0..queries {
                let mut req = SearchRequest::new((0..16).map(|_| next()).collect(), 10);
                req.filter = f.clone();
                req.exact = true;
                let exact: HashSet<_> = c.search(&req).unwrap().iter().map(|s| s.id).collect();
                req.exact = false;
                req.hnsw_ef = Some(ef);
                let got = c.search(&req).unwrap();
                if let Some(f) = &f {
                    for s in &got {
                        let p = &c.retrieve(&[s.id], &WithPayload::All, false).unwrap()[0];
                        assert!(f.matches(s.id, p.payload.as_ref().unwrap()));
                    }
                }
                hits += got.iter().filter(|s| exact.contains(&s.id)).count();
            }
            hits as f64 / (queries * 10) as f64
        };
        // Graph builds run in parallel, so the graphs differ between runs, and a narrow beam
        // over a few hundred results has come back perfect once.
        let (low, high) = (recall(10, 100), recall(128, 30));
        assert!(
            low < 1.0,
            "a beam of 10 found everything, so the graphs were not used"
        );
        assert!(high >= 0.95, "recall at ef 128 was {high:.3}, filter {f:?}");
    }
}

#[test]
fn the_background_optimizer_merges_while_writes_carry_on() {
    let dir = TempDir::new().unwrap();
    let mut c = create(dir.path(), Distance::Dot);
    c.start_optimizer(nocturnum_storage::OptimizerOptions {
        max_segments: 2,
        interval: Duration::from_millis(5),
        ..Default::default()
    });
    for round in 0..40u64 {
        c.upsert(
            vec![point(round % 25, [round as f32, 1.0], json!({"r": round}))],
            Ack::Applied,
        )
        .unwrap();
        if round % 4 == 3 {
            c.flush().unwrap();
        }
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while c.segment_count() > 2 {
        assert!(
            std::time::Instant::now() < deadline,
            "the optimizer never caught up"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(c.count(None).unwrap(), 25);
    assert!(c.optimizer_status().last.is_some());
    drop(c);
    let c = Collection::open(dir.path(), options(), None).unwrap().0;
    assert_eq!(c.count(None).unwrap(), 25);
}

#[test]
fn a_collection_snapshot_restores_and_searches_the_same() {
    let dir = TempDir::new().unwrap();
    let c = create(dir.path(), Distance::Dot);
    c.upsert(
        (0..30)
            .map(|i| point(i, [i as f32, 1.0], json!({"i": i})))
            .collect(),
        Ack::Durable,
    )
    .unwrap();
    c.flush().unwrap();
    c.upsert(vec![point(99, [5.0, 5.0], json!({}))], Ack::Durable)
        .unwrap();
    let snaps = TempDir::new().unwrap();
    let path = snaps.path().join("c.snapshot");
    c.snapshot(&path).unwrap();
    let target = snaps.path().join("restored");
    nocturnum_storage::shard::restore(&path, &target).unwrap();
    let r = Collection::open(&target, options(), None).unwrap().0;
    let req = SearchRequest::new(vec![1.0, 0.5], 5);
    assert_eq!(r.search(&req).unwrap(), c.search(&req).unwrap());
    assert_eq!(r.count(None).unwrap(), 31);
}
