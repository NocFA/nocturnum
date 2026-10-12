use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use nocturnum_core::filter::field_values;
use nocturnum_core::{Ack, Distance, Payload, PayloadIndexSpec, VectorConfig};
use nocturnum_index::Bm25;
use nocturnum_index::text::{document, idf, query_terms};
use nocturnum_storage::{GraphOptions, PointWrite, ShardOptions, WalOptions};
use proptest::prelude::*;
use serde_json::{Value, json};
use tempfile::TempDir;

use super::super::{Collection, Query, SearchRequest, Selector, TextQuery};
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
    let c = Collection::open(dir, options(), Some(config)).unwrap().0;
    c.create_payload_index(text_index()).unwrap();
    c
}

fn text_index() -> PayloadIndexSpec {
    PayloadIndexSpec {
        path: "body".parse().unwrap(),
        kind: PayloadIndexKind::Text,
    }
}

fn write(id: u64, p: Value) -> PointWrite {
    PointWrite {
        id: PointId::Num(id),
        vector: vec![0.0, 0.0],
        sparse: None,
        payload: serde_json::from_value(p).unwrap(),
    }
}

fn text(query: &str, limit: usize) -> SearchRequest {
    SearchRequest::of(
        Query::Text(TextQuery {
            field: "body".parse().unwrap(),
            query: query.into(),
            bm25: Bm25::default(),
        }),
        limit,
    )
}

fn ranked(c: &Collection, req: &SearchRequest) -> Vec<(u64, f32)> {
    c.search(req)
        .unwrap()
        .into_iter()
        .map(|s| match s.id {
            PointId::Num(n) => (n, s.score),
            id => panic!("unexpected id {id}"),
        })
        .collect()
}

/// BM25 worked out from scratch over every point, the meaning the engine has to match.
fn reference(
    model: &BTreeMap<PointId, Payload>,
    query: &str,
    bm25: Bm25,
    filter: Option<&Filter>,
) -> Vec<(PointId, f32)> {
    let path = "body".parse().unwrap();
    let docs: Vec<(PointId, HashMap<String, u32>, u32, bool)> = model
        .iter()
        .map(|(id, p)| {
            let (counts, len) = document(&field_values(&path, &p.0));
            (*id, counts, len, filter.is_none_or(|f| f.matches(*id, p)))
        })
        .filter(|d| d.2 > 0)
        .collect();
    if docs.is_empty() {
        return Vec::new();
    }
    let n = docs.len() as u64;
    let avg = docs.iter().map(|d| u64::from(d.2)).sum::<u64>() as f64 / n as f64;
    let terms = query_terms(query, MAX_QUERY_TERMS).unwrap();
    let mut out = Vec::new();
    for (id, counts, len, wanted) in &docs {
        let mut score = 0.0f64;
        let mut hit = false;
        for t in &terms {
            let Some(&tf) = counts.get(t) else { continue };
            let df = docs.iter().filter(|d| d.1.contains_key(t)).count() as u64;
            score += bm25.term(idf(n, df), tf, *len, avg);
            hit = true;
        }
        if hit && *wanted {
            out.push((*id, score as f32));
        }
    }
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    out
}

#[test]
fn scores_follow_bm25_over_live_documents() {
    let dir = TempDir::new().unwrap();
    let c = create(dir.path());
    c.upsert(
        vec![
            write(1, json!({"body": "The red fox"})),
            write(2, json!({"body": ["red hen", "red barn"]})),
            write(3, json!({"body": "a quiet night"})),
            write(4, json!({"title": "no body here"})),
        ],
        Ack::Applied,
    )
    .unwrap();

    // Worked by hand: 3 documents of lengths 3, 4 and 3, so the average is 10/3, and "red"
    // is in two of them.
    let idf_red = (1.0f64 + 1.5 / 2.5).ln();
    let avg = 10.0 / 3.0;
    let s1 = idf_red * 2.2 / (1.0 + 1.2 * (0.25 + 0.75 * 3.0 / avg));
    let s2 = idf_red * 2.0 * 2.2 / (2.0 + 1.2 * (0.25 + 0.75 * 4.0 / avg));
    let got = ranked(&c, &text("RED!", 10));
    assert_eq!(got, vec![(2, s2 as f32), (1, s1 as f32)]);

    // A point scores the same once flushed, and deleting a document changes the statistics.
    c.flush().unwrap();
    assert_eq!(ranked(&c, &text("red", 10)), got);
    c.delete(&Selector::Ids(vec![PointId::Num(3)]), Ack::Applied)
        .unwrap();
    let after = ranked(&c, &text("red", 10));
    assert_eq!(after.len(), 2);
    assert_ne!(after[1].1, got[1].1);

    // Words in no document, and queries with no words, find nothing.
    assert!(ranked(&c, &text("zebra", 10)).is_empty());
    assert!(ranked(&c, &text(" ?! ", 10)).is_empty());
}

#[test]
fn a_filter_narrows_results_without_changing_scores() {
    let dir = TempDir::new().unwrap();
    let c = create(dir.path());
    c.upsert(
        (0..6)
            .map(|i| {
                write(
                    i,
                    json!({"body": format!("fox {}", "x ".repeat(i as usize)), "n": i}),
                )
            })
            .collect(),
        Ack::Applied,
    )
    .unwrap();
    c.flush().unwrap();
    let all = ranked(&c, &text("fox", 10));
    let mut req = text("fox", 10);
    req.filter =
        Some(serde_json::from_value(json!({"must": [{"key": "n", "range": {"gte": 3}}]})).unwrap());
    let some = ranked(&c, &req);
    let want: Vec<_> = all.iter().copied().filter(|(id, _)| *id >= 3).collect();
    assert_eq!(some, want);

    // Thresholds and offsets work as they do for vectors.
    let mut req = text("fox", 10);
    req.offset = 1;
    req.score_threshold = Some(all[3].1);
    assert_eq!(ranked(&c, &req), all[1..4].to_vec());
}

#[test]
fn text_search_needs_a_text_index_and_sane_knobs() {
    let dir = TempDir::new().unwrap();
    let c = create(dir.path());
    let mut req = text("fox", 10);
    let Query::Text(q) = &mut req.query else {
        unreachable!()
    };
    q.field = "title".parse().unwrap();
    let err = c.search(&req).unwrap_err();
    assert!(
        err.is_invalid() && err.to_string().contains("no text index"),
        "{err}"
    );

    let mut req = text("fox", 10);
    let Query::Text(q) = &mut req.query else {
        unreachable!()
    };
    q.bm25.b = 2.0;
    assert!(c.search(&req).unwrap_err().is_invalid());

    // Dropping the index stops text search on the field.
    c.drop_payload_index(&text_index()).unwrap();
    assert!(c.search(&text("fox", 10)).unwrap_err().is_invalid());
}

#[derive(Debug, Clone)]
enum Step {
    Upsert(Vec<(u64, u8, u8)>),
    Delete(Vec<u64>),
    SetBody(Vec<u64>, u8),
    Flush,
    Merge,
    Reindex,
    Reopen,
}

const WORDS: [&str; 6] = ["red", "Fox", "hen", "the", "barn", "night"];

/// A body built from `seed`: missing, a number, a sentence or an array of two sentences.
fn body(seed: u8) -> Option<Value> {
    let sentence = |s: u8| {
        (0..(s % 5))
            .map(|i| WORDS[((s / 5 + i * 3) % 6) as usize])
            .collect::<Vec<_>>()
            .join(if s.is_multiple_of(2) { " " } else { ", " })
    };
    match seed % 8 {
        0 => None,
        1 => Some(json!(7)),
        2 => Some(json!([sentence(seed), sentence(seed / 3)])),
        _ => Some(json!(sentence(seed))),
    }
}

fn step() -> impl Strategy<Value = Step> {
    let id = 0u64..12;
    prop_oneof![
        4 => prop::collection::vec((id.clone(), any::<u8>(), 0u8..4), 1..5).prop_map(Step::Upsert),
        1 => prop::collection::vec(id.clone(), 1..4).prop_map(Step::Delete),
        1 => (prop::collection::vec(id, 1..4), any::<u8>()).prop_map(|(ids, s)| Step::SetBody(ids, s)),
        2 => Just(Step::Flush),
        1 => Just(Step::Merge),
        1 => Just(Step::Reindex),
        1 => Just(Step::Reopen),
    ]
}

fn payload_of(seed: u8, n: u8) -> Value {
    let mut p = json!({"n": n});
    if let Some(b) = body(seed) {
        p["body"] = b;
    }
    p
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn text_search_matches_bm25_worked_from_scratch(
        steps in prop::collection::vec(step(), 1..30),
        query in prop::collection::vec(0usize..7, 1..4),
        limit in 1usize..8,
        offset in 0usize..3,
        k1 in 0.0f64..3.0,
        b in 0.0f64..1.0,
    ) {
        let dir = TempDir::new().unwrap();
        let mut c = create(dir.path());
        let mut model: BTreeMap<PointId, Payload> = BTreeMap::new();
        for s in &steps {
            match s {
                Step::Upsert(points) => {
                    let writes: Vec<_> = points.iter().map(|(id, seed, n)| write(*id, payload_of(*seed, *n))).collect();
                    for w in &writes {
                        model.insert(w.id, w.payload.clone());
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
                Step::SetBody(ids, seed) => {
                    let ids: Vec<_> = ids.iter().map(|i| PointId::Num(*i)).collect();
                    let set = json!({"body": body(*seed).unwrap_or(Value::Null)});
                    for id in &ids {
                        if let Some(p) = model.get_mut(id) {
                            p.0.insert("body".into(), set["body"].clone());
                        }
                    }
                    c.set_payload(&Selector::Ids(ids), serde_json::from_value(set).unwrap(), Ack::Applied).unwrap();
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
                Step::Reindex => {
                    c.drop_payload_index(&text_index()).unwrap();
                    c.create_payload_index(text_index()).unwrap();
                }
                Step::Reopen => {
                    drop(c);
                    c = Collection::open(dir.path(), options(), None).unwrap().0;
                }
            }
        }
        // Index 6 is a word in no document.
        let words: Vec<&str> = query.iter().map(|&i| WORDS.get(i).copied().unwrap_or("zebra")).collect();
        let query = words.join(" ");
        let bm25 = Bm25 { k1, b };
        let even: Filter = serde_json::from_value(json!({"must": [{"key": "n", "any": [0, 2]}]})).unwrap();
        for f in [None, Some(even)] {
            let want: Vec<_> = reference(&model, &query, bm25, f.as_ref())
                .into_iter()
                .skip(offset)
                .take(limit)
                .collect();
            let mut req = SearchRequest::of(
                Query::Text(TextQuery { field: "body".parse().unwrap(), query: query.clone(), bm25 }),
                limit,
            );
            req.offset = offset;
            req.filter = f.clone();
            let got: Vec<_> = c.search(&req).unwrap().into_iter().map(|s| (s.id, s.score)).collect();
            prop_assert_eq!(got, want, "query {:?} filter {:?}", query, f);
        }
    }
}
