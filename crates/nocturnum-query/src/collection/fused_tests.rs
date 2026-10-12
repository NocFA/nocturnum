use std::collections::BTreeMap;
use std::time::Duration;

use nocturnum_core::{Distance, PayloadIndexKind};
use nocturnum_storage::{GraphOptions, WalOptions};
use proptest::prelude::*;
use serde_json::json;
use tempfile::TempDir;

use super::*;
use crate::fusion::DEFAULT_RRF_K;

fn options() -> ShardOptions {
    ShardOptions {
        wal: WalOptions {
            segment_bytes: 8192,
            ..WalOptions::default()
        },
        flush_bytes: usize::MAX,
        background_sync: Duration::from_millis(50),
        graph: GraphOptions {
            min_bytes: 0,
            ..GraphOptions::default()
        },
    }
}

fn create(dir: &Path, distance: Distance) -> Collection {
    let config = VectorConfig { dim: 2, distance };
    let c = Collection::open(dir, options(), Some(config)).unwrap().0;
    c.create_payload_index(PayloadIndexSpec {
        path: "body".parse().unwrap(),
        kind: PayloadIndexKind::Text,
    })
    .unwrap();
    c
}

fn point(id: u64, vector: [f32; 2], body: &str, n: u64) -> PointWrite {
    PointWrite {
        id: PointId::Num(id),
        vector: vector.to_vec(),
        sparse: None,
        payload: serde_json::from_value(json!({"body": body, "n": n})).unwrap(),
    }
}

fn text(words: &str) -> Query {
    Query::Text(TextQuery {
        field: "body".parse().unwrap(),
        query: words.into(),
        bm25: Bm25::default(),
    })
}

fn fused(prefetch: Vec<Prefetch>, fusion: Fusion, limit: usize) -> FusedRequest {
    FusedRequest {
        prefetch,
        fusion,
        filter: None,
        limit,
        offset: 0,
        with_payload: WithPayload::None,
        with_vector: false,
    }
}

fn ids(points: &[ScoredPoint]) -> Vec<u64> {
    points
        .iter()
        .map(|p| match p.id {
            PointId::Num(n) => n,
            _ => unreachable!(),
        })
        .collect()
}

/// Fusion worked out again from the searches' own results, which are tested elsewhere.
fn reference(lists: &[(f64, Vec<ScoredPoint>)], fusion: Fusion) -> Vec<(PointId, f32)> {
    let mut sums: BTreeMap<PointId, f64> = BTreeMap::new();
    for (w, list) in lists {
        let scores: Vec<f64> = list.iter().map(|p| f64::from(p.score)).collect();
        let n = scores.len() as f64;
        let mean = scores.iter().sum::<f64>() / n;
        let sd = (scores.iter().map(|s| (s - mean).powi(2)).sum::<f64>() / (n - 1.0)).sqrt();
        for (rank, p) in list.iter().enumerate() {
            let add = match fusion {
                Fusion::Rrf { k } => w / (k + rank as f64 + 1.0),
                Fusion::Dbsf if list.len() < 2 || sd == 0.0 => w * 0.5,
                Fusion::Dbsf => {
                    w * ((scores[rank] - (mean - 3.0 * sd)) / (6.0 * sd)).clamp(0.0, 1.0)
                }
            };
            *sums.entry(p.id).or_default() += add;
        }
    }
    let mut out: Vec<(PointId, f32)> = sums.into_iter().map(|(id, s)| (id, s as f32)).collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    out
}

const WORDS: [&str; 5] = ["red", "fox", "hen", "barn", "night"];

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn a_fused_query_fuses_what_each_search_finds(
        points in prop::collection::vec((0u64..40, any::<[i8; 2]>(), prop::collection::vec(0usize..5, 0..5), 0u64..4), 1..40),
        flush_at in 0usize..40,
        q in any::<[i8; 2]>(),
        words in prop::collection::vec(0usize..5, 1..3),
        limits in (1usize..12, 1usize..12),
        weights in (0.0f64..3.0, 0.0f64..3.0),
        limit in 1usize..10,
        offset in 0usize..4,
        dbsf in any::<bool>(),
        k in 0.0f64..100.0,
        root_filter in any::<bool>(),
        leaf_filter in any::<bool>(),
    ) {
        let dir = TempDir::new().unwrap();
        let c = create(dir.path(), Distance::Dot);
        for (i, (id, v, body, n)) in points.iter().enumerate() {
            let body: Vec<&str> = body.iter().map(|w| WORDS[*w]).collect();
            let v = [f32::from(v[0]) / 8.0, f32::from(v[1]) / 8.0];
            c.upsert(vec![point(*id, v, &body.join(" "), *n)], Ack::Applied).unwrap();
            if i == flush_at {
                c.flush().unwrap();
            }
        }
        let q = vec![f32::from(q[0]) / 8.0, f32::from(q[1]) / 8.0];
        let words: Vec<&str> = words.iter().map(|w| WORDS[*w]).collect();
        let root = root_filter.then(|| filter(json!({"must": [{"key": "n", "range": {"lte": 2}}]})));
        let leaf = leaf_filter.then(|| filter(json!({"must_not": [{"key": "n", "match": 0}]})));
        let fusion = if dbsf { Fusion::Dbsf } else { Fusion::Rrf { k } };
        let window = limit + offset;
        let limits = (limits.0.max(window), limits.1);

        let mut vector = Prefetch::new(Query::Vector(q.clone()));
        vector.limit = Some(limits.0);
        vector.weight = weights.0;
        vector.filter = leaf.clone();
        let mut words_leaf = Prefetch::new(text(&words.join(" ")));
        words_leaf.limit = Some(limits.1);
        words_leaf.weight = weights.1;
        let mut req = fused(vec![vector.clone(), words_leaf.clone()], fusion, limit);
        req.offset = offset;
        req.filter = root.clone();
        let got = c.query(&req).unwrap();

        // Each search on its own, with the root filter folded in.
        let mut lists = Vec::new();
        for p in [&vector, &words_leaf] {
            let mut s = SearchRequest::of(p.query.clone(), p.limit.unwrap());
            s.filter = both(root.as_ref(), p.filter.as_ref());
            lists.push((p.weight, c.search(&s).unwrap()));
        }
        let want = reference(&lists, fusion);
        prop_assert_eq!(&got.candidates.sources, &vec![(limits.0, lists[0].1.len()), (limits.1, lists[1].1.len())]);
        prop_assert_eq!(got.candidates.pool, want.len());
        let page: Vec<(PointId, f32)> = got.points.iter().map(|p| (p.id, p.score)).collect();
        let want: Vec<(PointId, f32)> = want.into_iter().skip(offset).take(limit).collect();
        prop_assert_eq!(page.len(), want.len());
        for (g, w) in page.iter().zip(&want) {
            prop_assert!((g.1 - w.1).abs() <= 1e-5 * (1.0 + w.1.abs()), "{:?} vs {:?}", page, want);
        }
        // The two sums can differ in the last bit, so the order of ids is only compared where
        // no two expected scores are that close.
        let distinct = want.windows(2).all(|p| (p[0].1 - p[1].1).abs() > 1e-4);
        if distinct {
            prop_assert_eq!(page.iter().map(|p| p.0).collect::<Vec<_>>(), want.iter().map(|p| p.0).collect::<Vec<_>>());
        }
    }
}

#[test]
fn hybrid_search_finds_what_either_search_alone_misses() {
    let dir = TempDir::new().unwrap();
    let c = create(dir.path(), Distance::Cosine);
    c.upsert(
        vec![
            point(1, [1.0, 0.0], "caddy cert rotation", 0),
            point(2, [0.9, 0.1], "renewing tls on the proxy", 0),
            point(3, [0.0, 1.0], "error E1234 in caddy", 0),
            point(4, [0.1, 0.9], "garden notes", 0),
        ],
        Ack::Applied,
    )
    .unwrap();
    let req = fused(
        vec![
            Prefetch::new(Query::Vector(vec![1.0, 0.05])),
            Prefetch::new(text("E1234")),
        ],
        Fusion::Rrf { k: DEFAULT_RRF_K },
        3,
    );
    let got = c.query(&req).unwrap();
    // The vector search ranks 1 then 2 first, the words find only 3, and fusion keeps all.
    assert_eq!(ids(&got.points), [1, 3, 2]);
    assert_eq!(got.candidates.sources, [(3, 3), (3, 1)]);
    assert_eq!(got.candidates.pool, 4);
}

#[test]
fn dbsf_ranks_the_nearest_points_first_for_distances() {
    let dir = TempDir::new().unwrap();
    let c = create(dir.path(), Distance::Euclid);
    c.upsert(
        vec![
            point(1, [0.0, 0.0], "a", 0),
            point(2, [1.0, 0.0], "b", 0),
            point(3, [5.0, 0.0], "c", 0),
        ],
        Ack::Applied,
    )
    .unwrap();
    let got = c
        .query(&fused(
            vec![Prefetch::new(Query::Vector(vec![0.0, 0.0]))],
            Fusion::Dbsf,
            3,
        ))
        .unwrap();
    assert_eq!(ids(&got.points), [1, 2, 3]);
    assert!(got.points[0].score > got.points[2].score);
}

#[test]
fn budgets_are_checked_and_reported() {
    let dir = TempDir::new().unwrap();
    let c = create(dir.path(), Distance::Dot);
    c.upsert(
        (0..20)
            .map(|i| point(i, [i as f32, 1.0], "fox", 0))
            .collect(),
        Ack::Applied,
    )
    .unwrap();
    let mut v = Prefetch::new(Query::Vector(vec![1.0, 0.0]));
    v.limit = Some(5);
    let mut t = Prefetch::new(text("fox"));
    t.limit = Some(4);

    // Pages past what the prefetches can return together are refused.
    let mut req = fused(vec![v.clone(), t.clone()], Fusion::Dbsf, 6);
    req.offset = 4;
    let err = c.query(&req).unwrap_err();
    assert!(
        err.is_invalid() && err.to_string().contains("raise their limits"),
        "{err}"
    );

    // A page the budget allows reports how much each prefetch found.
    req.offset = 3;
    let got = c.query(&req).unwrap();
    assert_eq!(got.candidates.sources, [(5, 5), (4, 4)]);
    assert!(got.candidates.pool <= 9);

    // Without limits each prefetch takes offset plus limit.
    let req = fused(
        vec![
            Prefetch::new(Query::Vector(vec![1.0, 0.0])),
            Prefetch::new(text("fox")),
        ],
        Fusion::Rrf { k: 60.0 },
        7,
    );
    assert_eq!(c.query(&req).unwrap().candidates.sources, [(7, 7), (7, 7)]);
}

#[test]
fn bad_prefetches_say_which() {
    let dir = TempDir::new().unwrap();
    let c = create(dir.path(), Distance::Dot);
    let ok = Prefetch::new(Query::Vector(vec![1.0, 0.0]));
    let mut no_index = Prefetch::new(Query::Text(TextQuery {
        field: "title".parse().unwrap(),
        query: "fox".into(),
        bm25: Bm25::default(),
    }));
    no_index.limit = Some(3);
    let err = c
        .query(&fused(vec![ok.clone(), no_index], Fusion::Dbsf, 3))
        .unwrap_err();
    assert!(
        matches!(&err, QueryError::Prefetch(1, e) if matches!(**e, QueryError::NoTextIndex(_))),
        "{err}"
    );

    let mut heavy = ok.clone();
    heavy.weight = -1.0;
    assert!(matches!(
        c.query(&fused(vec![heavy], Fusion::Dbsf, 3)).unwrap_err(),
        QueryError::Prefetch(0, _)
    ));
    assert!(
        c.query(&fused(vec![], Fusion::Dbsf, 3))
            .unwrap_err()
            .is_invalid()
    );
    let many = vec![ok.clone(); MAX_PREFETCH + 1];
    assert!(
        c.query(&fused(many, Fusion::Dbsf, 3))
            .unwrap_err()
            .is_invalid()
    );
    let mut bad_filter = ok;
    bad_filter.filter = Some(filter(json!({"must": [{"key": "a", "any": []}]})));
    assert!(matches!(
        c.query(&fused(vec![bad_filter], Fusion::Dbsf, 3)).unwrap_err(),
        QueryError::Prefetch(0, e) if matches!(*e, QueryError::Input(_))
    ));
}

#[test]
fn a_prefetch_filter_too_deep_with_the_query_filter_is_refused() {
    let dir = TempDir::new().unwrap();
    let c = create(dir.path(), Distance::Dot);
    // The deepest filter that passes on its own.
    let mut deep = filter(json!({"must": [{"key": "n", "match": 1}]}));
    loop {
        let deeper = Filter {
            must: vec![Condition::Filter(deep.clone())],
            ..Filter::default()
        };
        if deeper.validate().is_err() {
            break;
        }
        deep = deeper;
    }
    let mut p = Prefetch::new(Query::Vector(vec![1.0, 0.0]));
    p.filter = Some(deep.clone());
    let mut req = fused(vec![p.clone()], Fusion::Dbsf, 3);
    assert!(c.query(&req).is_ok());
    req.filter = Some(filter(json!({"must": [{"key": "n", "match": 1}]})));
    let err = c.query(&req).unwrap_err();
    assert!(
        matches!(&err, QueryError::Prefetch(0, e) if matches!(&**e, QueryError::Field { at, .. } if at == "filter")),
        "{err}"
    );
}

fn filter(v: serde_json::Value) -> Filter {
    serde_json::from_value(v).unwrap()
}

#[test]
fn a_passed_deadline_stops_every_kind_of_search() {
    let dir = TempDir::new().unwrap();
    let c = create(dir.path(), Distance::Dot);
    c.upsert(
        (0..20)
            .map(|i| point(i, [i as f32, 1.0], "fox", 0))
            .collect(),
        Ack::Applied,
    )
    .unwrap();
    c.flush().unwrap();
    c.upsert(vec![point(99, [1.0, 1.0], "fox", 0)], Ack::Applied)
        .unwrap();
    let passed = Deadline::from(
        std::time::Instant::now() - Duration::from_secs(2),
        Duration::from_secs(1),
    );
    let sparse = Query::Sparse(SparseQuery {
        vector: nocturnum_core::SparseVector {
            indices: vec![1],
            values: vec![1.0],
        },
        idf: false,
    });
    for q in [Query::Vector(vec![1.0, 0.0]), text("fox"), sparse] {
        let req = SearchRequest::of(q.clone(), 3);
        assert!(c.search_until(&req, Deadline::NONE).is_ok());
        let err = c.search_until(&req, passed).unwrap_err();
        assert!(matches!(err, QueryError::TimedOut(_)), "{err}");
        let fused = fused(vec![Prefetch::new(q)], Fusion::Dbsf, 3);
        let err = c.query_until(&fused, passed).unwrap_err();
        assert!(
            matches!(&err, QueryError::Prefetch(0, e) if matches!(**e, QueryError::TimedOut(_))),
            "{err}"
        );
    }
}
