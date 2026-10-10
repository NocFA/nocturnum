use std::collections::BTreeMap;

use nocturnum_core::Distance;
use proptest::prelude::*;
use serde_json::json;
use tempfile::TempDir;

use super::*;

const DIM: usize = 3;

fn config() -> VectorConfig {
    VectorConfig {
        dim: DIM,
        distance: Distance::Dot,
    }
}

fn opts() -> ShardOptions {
    ShardOptions {
        wal: WalOptions {
            segment_bytes: 2048,
            ..WalOptions::default()
        },
        flush_bytes: 4096,
        background_sync: Duration::from_millis(50),
    }
}

fn create(dir: &Path) -> Shard {
    Shard::open(dir, opts(), Some(config())).unwrap().0
}

fn reopen(dir: &Path) -> Shard {
    Shard::open(dir, opts(), None).unwrap().0
}

fn payload(v: serde_json::Value) -> Payload {
    serde_json::from_value(v).unwrap()
}

fn point(id: u64, x: f32, p: serde_json::Value) -> PointWrite {
    PointWrite {
        id: PointId::Num(id),
        vector: vec![x, x + 1.0, x + 2.0],
        payload: payload(p),
    }
}

fn upsert(shard: &Shard, points: Vec<PointWrite>) -> u64 {
    shard
        .write(Operation::Upsert(points), Ack::Durable)
        .unwrap()
}

/// Every point the shard holds, checking each id appears once.
fn contents(shard: &Shard) -> BTreeMap<PointId, Record> {
    let view = shard.view();
    let mut out = BTreeMap::new();
    for p in view.points() {
        let rec = Record {
            vector: p.vector.to_vec(),
            payload: p.payload().unwrap().into_owned(),
        };
        assert!(out.insert(p.id, rec).is_none(), "{} seen twice", p.id);
    }
    assert_eq!(out.len(), view.len());
    out
}

#[test]
fn writes_reads_and_deletes() {
    let dir = TempDir::new().unwrap();
    let shard = create(dir.path());
    upsert(
        &shard,
        vec![point(1, 0.0, json!({"a": 1})), point(2, 1.0, json!({}))],
    );
    assert_eq!(shard.len(), 2);
    assert_eq!(
        shard.get(PointId::Num(1)).unwrap().unwrap().payload,
        payload(json!({"a": 1}))
    );
    shard
        .write(
            Operation::Delete(vec![PointId::Num(1), PointId::Num(9)]),
            Ack::Durable,
        )
        .unwrap();
    assert_eq!(shard.get(PointId::Num(1)).unwrap(), None);
    assert_eq!(shard.len(), 1);
}

#[test]
fn payload_operations() {
    let dir = TempDir::new().unwrap();
    let shard = create(dir.path());
    upsert(&shard, vec![point(1, 0.0, json!({"a": 1, "b": 2}))]);
    let ids = vec![PointId::Num(1), PointId::Num(404)];
    let run = |op| shard.write(op, Ack::Applied).unwrap();
    run(Operation::SetPayload {
        ids: ids.clone(),
        payload: payload(json!({"b": 3, "c": 4})),
    });
    assert_eq!(
        shard.get(PointId::Num(1)).unwrap().unwrap().payload,
        payload(json!({"a": 1, "b": 3, "c": 4}))
    );
    run(Operation::DeletePayloadKeys {
        ids: ids.clone(),
        keys: vec!["a".into(), "zz".into()],
    });
    assert_eq!(
        shard.get(PointId::Num(1)).unwrap().unwrap().payload,
        payload(json!({"b": 3, "c": 4}))
    );
    run(Operation::OverwritePayload {
        ids: ids.clone(),
        payload: payload(json!({"x": true})),
    });
    assert_eq!(
        shard.get(PointId::Num(1)).unwrap().unwrap().payload,
        payload(json!({"x": true}))
    );
    run(Operation::ClearPayload(ids));
    assert_eq!(
        shard.get(PointId::Num(1)).unwrap().unwrap().payload,
        Payload::new()
    );
    // Missing ids were ignored rather than created.
    assert_eq!(shard.len(), 1);
}

#[test]
fn one_operation_naming_an_id_twice_applies_both() {
    let dir = TempDir::new().unwrap();
    let shard = create(dir.path());
    upsert(&shard, vec![point(1, 0.0, json!({}))]);
    shard.flush().unwrap();
    shard
        .write(
            Operation::DeletePayloadKeys {
                ids: vec![PointId::Num(1)],
                keys: vec![],
            },
            Ack::Durable,
        )
        .unwrap();
    upsert(
        &shard,
        vec![
            point(5, 0.0, json!({"n": 1})),
            point(5, 9.0, json!({"n": 2})),
        ],
    );
    assert_eq!(
        shard.get(PointId::Num(5)).unwrap().unwrap().payload,
        payload(json!({"n": 2}))
    );
}

#[test]
fn invalid_vectors_are_refused_before_logging() {
    let dir = TempDir::new().unwrap();
    let shard = create(dir.path());
    let before = shard.wal.last_seq();
    let err = shard
        .write(
            Operation::Upsert(vec![PointWrite {
                id: PointId::Num(1),
                vector: vec![1.0],
                payload: Payload::new(),
            }]),
            Ack::Durable,
        )
        .unwrap_err();
    assert!(matches!(err, WriteError::Invalid(_)));
    assert_eq!(shard.wal.last_seq(), before);
    assert!(shard.is_empty());
}

#[test]
fn cosine_vectors_are_stored_normalised() {
    let dir = TempDir::new().unwrap();
    let cfg = VectorConfig {
        dim: 2,
        distance: Distance::Cosine,
    };
    let (shard, _) = Shard::open(dir.path(), opts(), Some(cfg)).unwrap();
    shard
        .write(
            Operation::Upsert(vec![PointWrite {
                id: PointId::Num(1),
                vector: vec![3.0, 4.0],
                payload: Payload::new(),
            }]),
            Ack::Durable,
        )
        .unwrap();
    assert_eq!(
        shard.get(PointId::Num(1)).unwrap().unwrap().vector,
        vec![0.6, 0.8]
    );
}

#[test]
fn survives_reopen_with_and_without_flushes() {
    let dir = TempDir::new().unwrap();
    let shard = create(dir.path());
    upsert(
        &shard,
        vec![point(1, 0.0, json!({"v": 1})), point(2, 1.0, json!({}))],
    );
    assert!(shard.flush().unwrap());
    assert!(!shard.flush().unwrap(), "nothing new to flush");
    upsert(&shard, vec![point(1, 5.0, json!({"v": 2}))]);
    shard
        .write(Operation::Delete(vec![PointId::Num(2)]), Ack::Durable)
        .unwrap();
    let before = contents(&shard);
    drop(shard);

    let (shard, rec) = Shard::open(dir.path(), opts(), None).unwrap();
    assert_eq!(rec.replayed, 2);
    assert_eq!(contents(&shard), before);
    shard.flush().unwrap();
    drop(shard);

    // Everything is in segments now, and the deletion in the newer segment still hides the
    // point in the older one.
    let (shard, rec) = Shard::open(dir.path(), opts(), None).unwrap();
    assert_eq!(rec.replayed, 0);
    assert_eq!(shard.segment_ids(), vec![1, 2]);
    assert_eq!(contents(&shard), before);
    assert_eq!(shard.get(PointId::Num(2)).unwrap(), None);
}

#[test]
fn writes_during_a_frozen_flush_win() {
    let dir = TempDir::new().unwrap();
    let shard = create(dir.path());
    upsert(&shard, vec![point(1, 0.0, json!({"v": "old"}))]);
    // Freeze by hand to stand in for a flush that is still writing.
    {
        let mut st = write(&shard.state);
        let mem = std::mem::take(&mut st.mem);
        st.frozen = Some(Arc::new(mem));
        st.frozen_upto = st.last_applied;
    }
    upsert(&shard, vec![point(1, 1.0, json!({"v": "new"}))]);
    assert_eq!(
        shard.get(PointId::Num(1)).unwrap().unwrap().payload,
        payload(json!({"v": "new"}))
    );
    assert_eq!(contents(&shard).len(), 1);
    // The flush finishes with the frozen memtable, the newer write stays on top.
    shard.flush().unwrap();
    assert_eq!(
        shard.get(PointId::Num(1)).unwrap().unwrap().payload,
        payload(json!({"v": "new"}))
    );
    assert_eq!(contents(&shard).len(), 1);
    shard.flush().unwrap();
    drop(shard);
    let shard = reopen(dir.path());
    assert_eq!(
        shard.get(PointId::Num(1)).unwrap().unwrap().payload,
        payload(json!({"v": "new"}))
    );
}

#[test]
fn unpublished_segments_are_removed_at_startup() {
    let dir = TempDir::new().unwrap();
    let shard = create(dir.path());
    upsert(&shard, vec![point(1, 0.0, json!({}))]);
    shard.flush().unwrap();
    drop(shard);
    let segs = dir.path().join("segments");
    fs::create_dir(segs.join("tmp-0000000000000009")).unwrap();
    fs::create_dir(segs.join(segment_dir_name(9))).unwrap();
    let (shard, rec) = Shard::open(dir.path(), opts(), None).unwrap();
    assert_eq!(rec.removed_segments.len(), 2);
    assert_eq!(shard.len(), 1);
}

#[test]
fn opening_checks_config_and_existence() {
    let dir = TempDir::new().unwrap();
    assert!(matches!(
        Shard::open(dir.path(), opts(), None),
        Err(Error::Invalid(_))
    ));
    drop(create(dir.path()));
    let other = VectorConfig {
        dim: 4,
        distance: Distance::Dot,
    };
    assert!(matches!(
        Shard::open(dir.path(), opts(), Some(other)),
        Err(Error::Invalid(_))
    ));
    assert!(Shard::open(dir.path(), opts(), Some(config())).is_ok());
}

#[test]
fn a_wal_missing_what_the_manifest_needs_is_corruption() {
    let dir = TempDir::new().unwrap();
    let shard = create(dir.path());
    upsert(&shard, vec![point(1, 0.0, json!({}))]);
    shard.flush().unwrap();
    drop(shard);
    fs::remove_dir_all(dir.path().join("wal")).unwrap();
    assert!(matches!(
        Shard::open(dir.path(), opts(), None),
        Err(Error::Corrupt { .. })
    ));
}

#[test]
fn payloads_too_deep_to_read_back_are_refused_before_logging() {
    let dir = TempDir::new().unwrap();
    let shard = create(dir.path());
    let mut deep = json!(1);
    for _ in 0..200 {
        deep = json!([deep]);
    }
    let before = shard.wal.last_seq();
    let refused = shard.write(
        Operation::Upsert(vec![point(1, 0.0, json!({"d": deep.clone()}))]),
        Ack::Durable,
    );
    assert!(matches!(refused, Err(WriteError::Invalid(_))));
    upsert(&shard, vec![point(1, 0.0, json!({}))]);
    let refused = shard.write(
        Operation::SetPayload {
            ids: vec![PointId::Num(1)],
            payload: payload(json!({"d": deep})),
        },
        Ack::Durable,
    );
    assert!(matches!(refused, Err(WriteError::Invalid(_))));
    assert_eq!(shard.wal.last_seq(), before + 1);
}

#[cfg(unix)]
#[test]
fn a_failed_manifest_write_stops_flushing_until_restart() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().unwrap();
    let shard = create(dir.path());
    upsert(&shard, vec![point(1, 0.0, json!({"v": 1}))]);
    let set_mode = |mode| {
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(mode)).unwrap();
    };
    set_mode(0o555);
    let failed = shard.flush();
    set_mode(0o755);
    assert!(failed.is_err());
    // No retry is allowed to guess what reached the disk.
    assert!(matches!(shard.flush(), Err(Error::Invalid(_))));
    // Writes still land safely in the WAL.
    upsert(&shard, vec![point(2, 1.0, json!({}))]);
    let before = contents(&shard);
    drop(shard);
    let shard = reopen(dir.path());
    assert_eq!(contents(&shard), before);
    assert!(shard.flush().unwrap());
}

#[test]
fn needs_flush_tracks_the_memtable() {
    let dir = TempDir::new().unwrap();
    let shard = create(dir.path());
    while !shard.needs_flush() {
        upsert(&shard, vec![point(1, 0.0, json!({"pad": "x".repeat(200)}))]);
    }
    shard.flush().unwrap();
    assert!(!shard.needs_flush());
}

// A model of what the shard should hold, using the same rules in the plainest way.

#[derive(Debug, Clone)]
enum Step {
    Op(Operation),
    Flush,
    Reopen,
}

fn apply_model(model: &mut BTreeMap<PointId, Record>, op: &Operation) {
    match op {
        Operation::Upsert(points) => {
            for p in points {
                model.insert(
                    p.id,
                    Record {
                        vector: p.vector.clone(),
                        payload: p.payload.clone(),
                    },
                );
            }
        }
        Operation::Delete(ids) => {
            for id in ids {
                model.remove(id);
            }
        }
        Operation::SetPayload { ids, payload } => {
            for id in ids {
                if let Some(r) = model.get_mut(id) {
                    for (k, v) in &payload.0 {
                        r.payload.0.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        Operation::OverwritePayload { ids, payload } => {
            for id in ids {
                if let Some(r) = model.get_mut(id) {
                    r.payload = payload.clone();
                }
            }
        }
        Operation::DeletePayloadKeys { ids, keys } => {
            for id in ids {
                if let Some(r) = model.get_mut(id) {
                    for k in keys {
                        r.payload.0.remove(k);
                    }
                }
            }
        }
        Operation::ClearPayload(ids) => {
            for id in ids {
                if let Some(r) = model.get_mut(id) {
                    r.payload = Payload::new();
                }
            }
        }
    }
}

fn ids() -> impl Strategy<Value = Vec<PointId>> {
    proptest::collection::vec((0u64..12).prop_map(PointId::Num), 0..4)
}

fn small_payload() -> impl Strategy<Value = Payload> {
    proptest::collection::btree_map("[abc]", 0i64..5, 0..3)
        .prop_map(|m| Payload(m.into_iter().map(|(k, v)| (k, json!(v))).collect()))
}

fn step() -> impl Strategy<Value = Step> {
    let write = proptest::collection::vec(
        (0u64..12, -4.0f32..4.0, small_payload()).prop_map(|(id, x, payload)| PointWrite {
            id: PointId::Num(id),
            vector: vec![x, -x, x * 0.5],
            payload,
        }),
        0..4,
    );
    prop_oneof![
        8 => write.prop_map(|p| Step::Op(Operation::Upsert(p))),
        3 => ids().prop_map(|i| Step::Op(Operation::Delete(i))),
        2 => (ids(), small_payload()).prop_map(|(ids, payload)| Step::Op(Operation::SetPayload { ids, payload })),
        1 => (ids(), small_payload()).prop_map(|(ids, payload)| Step::Op(Operation::OverwritePayload { ids, payload })),
        1 => ids().prop_map(|ids| Step::Op(Operation::DeletePayloadKeys { ids, keys: vec!["a".into()] })),
        1 => ids().prop_map(|i| Step::Op(Operation::ClearPayload(i))),
        2 => Just(Step::Flush),
        1 => Just(Step::Reopen),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Any mix of writes, flushes and reopens leaves the shard holding exactly what the
    /// model holds.
    #[test]
    fn matches_a_simple_model(steps in proptest::collection::vec(step(), 1..60)) {
        let dir = TempDir::new().unwrap();
        let mut shard = create(dir.path());
        let mut model = BTreeMap::new();
        for step in steps {
            match step {
                Step::Op(op) => {
                    apply_model(&mut model, &op);
                    shard.write(op, Ack::Applied).unwrap();
                }
                Step::Flush => {
                    shard.flush().unwrap();
                }
                Step::Reopen => {
                    drop(shard);
                    shard = reopen(dir.path());
                }
            }
            prop_assert_eq!(&contents(&shard), &model);
            for (id, rec) in &model {
                prop_assert_eq!(shard.get(*id).unwrap(), Some(rec.clone()));
            }
        }
    }
}
