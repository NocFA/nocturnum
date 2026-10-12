use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use nocturnum_core::{Ack, Distance, Payload, VectorConfig};
use nocturnum_index::text::idf;
use nocturnum_storage::{GraphOptions, PointWrite, ShardOptions, WalOptions};
use proptest::prelude::*;
use serde_json::{Value, json};
use tempfile::TempDir;

use super::super::{Collection, Query, SearchRequest, Selector, WithPayload};
use super::*;

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

fn create(dir: &Path) -> Collection {
    let config = VectorConfig {
        dim: 2,
        distance: Distance::Dot,
    };
    Collection::open(dir, options(), Some(config)).unwrap().0
}

fn sv(indices: &[u32], values: &[f32]) -> SparseVector {
    SparseVector {
        indices: indices.to_vec(),
        values: values.to_vec(),
    }
}

fn write(id: u64, sparse: Option<SparseVector>, p: Value) -> PointWrite {
    PointWrite {
        id: PointId::Num(id),
        vector: vec![0.0, 0.0],
        sparse,
        payload: serde_json::from_value(p).unwrap(),
    }
}

fn search(c: &Collection, vector: SparseVector, idf: bool, limit: usize) -> Vec<(u64, f32)> {
    let req = SearchRequest::of(Query::Sparse(SparseQuery { vector, idf }), limit);
    c.search(&req)
        .unwrap()
        .into_iter()
        .map(|s| match s.id {
            PointId::Num(n) => (n, s.score),
            id => panic!("unexpected id {id}"),
        })
        .collect()
}

#[test]
fn scores_are_dot_products_over_shared_dimensions() {
    let dir = TempDir::new().unwrap();
    let c = create(dir.path());
    c.upsert(
        vec![
            // Given out of order, stored sorted.
            write(1, Some(sv(&[7, 2], &[1.0, 0.5])), json!({})),
            write(2, Some(sv(&[2, 3], &[2.0, 1.0])), json!({})),
            write(3, None, json!({})),
            write(4, Some(sv(&[9], &[4.0])), json!({})),
        ],
        Ack::Applied,
    )
    .unwrap();
    let q = sv(&[2, 7], &[1.0, 2.0]);
    let want = vec![(1, 2.5f32), (2, 2.0)];
    assert_eq!(search(&c, q.clone(), false, 10), want);
    c.flush().unwrap();
    assert_eq!(search(&c, q.clone(), false, 10), want);

    // With idf, dimension 7 is in one of the three sparse points and 2 in two of them.
    let (i2, i7) = (idf(3, 2), idf(3, 1));
    let got = search(&c, q.clone(), true, 10);
    assert_eq!(
        got,
        vec![(1, (0.5 * i2 + 2.0 * i7) as f32), (2, (2.0 * i2) as f32)]
    );

    // A point read back has its sparse vector sorted, and only with the vector.
    let p = &c
        .retrieve(&[PointId::Num(1)], &WithPayload::None, true)
        .unwrap()[0];
    assert_eq!(p.sparse, Some(sv(&[2, 7], &[0.5, 1.0])));
    let p = &c
        .retrieve(&[PointId::Num(1)], &WithPayload::None, false)
        .unwrap()[0];
    assert_eq!(p.sparse, None);

    // Queries with no dimensions find nothing, and bad ones are refused.
    assert!(search(&c, sv(&[], &[]), false, 10).is_empty());
    let bad = SearchRequest::of(Query::Sparse(sv(&[1, 1], &[1.0, 1.0]).into()), 3);
    assert!(c.search(&bad).unwrap_err().is_invalid());
}

#[test]
fn bad_sparse_vectors_are_refused_before_logging() {
    let dir = TempDir::new().unwrap();
    let c = create(dir.path());
    let before = c.wal_stats().last_seq;
    for bad in [
        sv(&[1], &[f32::NAN]),
        sv(&[1, 2], &[1.0]),
        sv(&[4, 4], &[1.0, 2.0]),
    ] {
        assert!(
            c.upsert(vec![write(1, Some(bad), json!({}))], Ack::Applied)
                .is_err()
        );
    }
    assert_eq!(c.wal_stats().last_seq, before);
    // An empty one is stored as none.
    c.upsert(vec![write(1, Some(sv(&[], &[])), json!({}))], Ack::Applied)
        .unwrap();
    let p = &c
        .retrieve(&[PointId::Num(1)], &WithPayload::None, true)
        .unwrap()[0];
    assert_eq!(p.sparse, None);
}

/// A point to upsert: its id, its sparse weights by dimension in eighths, and `n`.
type Upsert = (u64, Option<BTreeMap<u32, i8>>, u8);

#[derive(Debug, Clone)]
enum Step {
    Upsert(Vec<Upsert>),
    Delete(Vec<u64>),
    SetN(Vec<u64>, u8),
    Flush,
    Merge,
    Reopen,
}

fn step() -> impl Strategy<Value = Step> {
    let id = 0u64..12;
    let sparse = prop::option::of(prop::collection::btree_map(0u32..8, any::<i8>(), 0..4));
    prop_oneof![
        4 => prop::collection::vec((id.clone(), sparse, 0u8..4), 1..5).prop_map(Step::Upsert),
        1 => prop::collection::vec(id.clone(), 1..4).prop_map(Step::Delete),
        1 => (prop::collection::vec(id, 1..4), 0u8..4).prop_map(|(ids, n)| Step::SetN(ids, n)),
        2 => Just(Step::Flush),
        1 => Just(Step::Merge),
        1 => Just(Step::Reopen),
    ]
}

fn to_sparse(m: &BTreeMap<u32, i8>) -> SparseVector {
    SparseVector {
        indices: m.keys().copied().collect(),
        values: m.values().map(|&v| f32::from(v) / 8.0).collect(),
    }
}

/// Sparse search worked out from scratch over every point.
fn reference(
    model: &BTreeMap<PointId, (Option<SparseVector>, Payload)>,
    q: &SparseVector,
    use_idf: bool,
    filter: Option<&Filter>,
) -> Vec<(PointId, f32)> {
    let docs = model.values().filter(|(v, _)| v.is_some()).count() as u64;
    let weights: Vec<f64> = q
        .indices
        .iter()
        .zip(&q.values)
        .map(|(&d, &w)| {
            let df = model
                .values()
                .filter(|(v, _)| v.as_ref().is_some_and(|v| v.get(d).is_some()))
                .count() as u64;
            if use_idf {
                f64::from(w) * idf(docs, df)
            } else {
                f64::from(w)
            }
        })
        .collect();
    let mut out = Vec::new();
    for (id, (v, p)) in model {
        let Some(v) = v else { continue };
        if !filter.is_none_or(|f| f.matches(*id, p)) {
            continue;
        }
        let mut score = 0.0f64;
        let mut hit = false;
        for (i, &d) in q.indices.iter().enumerate() {
            if let Some(w) = v.get(d) {
                score += weights[i] * f64::from(w);
                hit = true;
            }
        }
        if hit {
            out.push((*id, score as f32));
        }
    }
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn sparse_search_matches_a_dot_product_worked_from_scratch(
        steps in prop::collection::vec(step(), 1..30),
        query in prop::collection::btree_map(0u32..10, any::<i8>(), 1..4),
        limit in 1usize..8,
        offset in 0usize..3,
        use_idf in any::<bool>(),
    ) {
        let dir = TempDir::new().unwrap();
        let mut c = create(dir.path());
        let mut model: BTreeMap<PointId, (Option<SparseVector>, Payload)> = BTreeMap::new();
        for s in &steps {
            match s {
                Step::Upsert(points) => {
                    let writes: Vec<_> = points
                        .iter()
                        .map(|(id, v, n)| write(*id, v.as_ref().map(to_sparse), json!({"n": n})))
                        .collect();
                    for w in &writes {
                        let stored = w.sparse.clone().and_then(|v| v.prepare().unwrap());
                        model.insert(w.id, (stored, w.payload.clone()));
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
                Step::SetN(ids, n) => {
                    let ids: Vec<_> = ids.iter().map(|i| PointId::Num(*i)).collect();
                    for id in &ids {
                        if let Some((_, p)) = model.get_mut(id) {
                            p.0.insert("n".into(), json!(n));
                        }
                    }
                    let set = serde_json::from_value(json!({"n": n})).unwrap();
                    c.set_payload(&Selector::Ids(ids), set, Ack::Applied).unwrap();
                }
                Step::Flush => {
                    c.flush().unwrap();
                }
                Step::Merge => {
                    let ids = c.shard.segment_ids();
                    if !ids.is_empty() {
                        c.shard.merge(&ids).unwrap();
                    }
                }
                Step::Reopen => {
                    drop(c);
                    c = Collection::open(dir.path(), options(), None).unwrap().0;
                }
            }
        }
        let q = to_sparse(&query);
        let even: Filter = serde_json::from_value(json!({"must": [{"key": "n", "any": [0, 2]}]})).unwrap();
        for f in [None, Some(even)] {
            let want: Vec<_> = reference(&model, &q, use_idf, f.as_ref())
                .into_iter()
                .skip(offset)
                .take(limit)
                .collect();
            let mut req = SearchRequest::of(
                Query::Sparse(SparseQuery { vector: q.clone(), idf: use_idf }),
                limit,
            );
            req.offset = offset;
            req.filter = f.clone();
            let got: Vec<_> = c.search(&req).unwrap().into_iter().map(|s| (s.id, s.score)).collect();
            prop_assert_eq!(got, want, "filter {:?}", f);
        }
        // Every point reads back with the sparse vector it was given.
        let ids: Vec<_> = model.keys().copied().collect();
        let got = c.retrieve(&ids, &WithPayload::None, true).unwrap();
        for p in got {
            prop_assert_eq!(&p.sparse, &model[&p.id].0);
        }
    }
}
