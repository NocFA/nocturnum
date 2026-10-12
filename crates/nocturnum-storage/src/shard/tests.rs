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
        // Every flushed segment builds a graph, so graphs go through every path tested here.
        graph: GraphOptions {
            min_bytes: 0,
            ..GraphOptions::default()
        },
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
        sparse: None,
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
    view_contents(&shard.view())
}

/// Every point a view holds, checking each id appears once.
fn view_contents(view: &View) -> BTreeMap<PointId, Record> {
    let mut out = BTreeMap::new();
    for p in view.points() {
        let rec = Record {
            vector: p.vector.to_vec(),
            sparse: p.sparse().map(|v| v.into_owned()),
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
                sparse: None,
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
                sparse: None,
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
    assert_eq!(shard.failure(), None);
    set_mode(0o555);
    let failed = shard.flush();
    set_mode(0o755);
    assert!(failed.is_err());
    assert!(shard.failure().is_some());
    // No retry is allowed to guess what reached the disk.
    assert!(matches!(shard.flush(), Err(Error::Invalid(_))));
    // Writes still land safely in the WAL.
    upsert(&shard, vec![point(2, 1.0, json!({}))]);
    let before = contents(&shard);
    drop(shard);
    let shard = reopen(dir.path());
    assert_eq!(contents(&shard), before);
    assert_eq!(shard.failure(), None);
    assert!(shard.flush().unwrap());
}

#[test]
fn a_failed_fsync_is_reported_until_restart() {
    let dir = TempDir::new().unwrap();
    let shard = create(dir.path());
    upsert(&shard, vec![point(1, 0.0, json!({}))]);
    let stats = shard.wal_stats();
    assert!(stats.syncs >= 1 && stats.sync_time > Duration::ZERO);
    assert!(stats.disk_bytes > 0 && stats.last_seq == 1);
    shard.inject_sync_failures(1);
    let failed = shard.write(Operation::Delete(vec![PointId::Num(1)]), Ack::Durable);
    assert!(failed.is_err());
    assert!(shard.failure().is_some());
    drop(shard);
    let shard = reopen(dir.path());
    assert_eq!(shard.failure(), None);
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
    /// Merges a run of segments, picked by position and length from those live at the time.
    Merge(usize, usize),
    Optimize,
}

fn keyword(path: &str) -> PayloadIndexSpec {
    PayloadIndexSpec {
        path: path.parse().unwrap(),
        kind: nocturnum_core::PayloadIndexKind::Keyword,
    }
}

/// The ids of live rows each segment's index says match `colour == value`.
fn indexed_matches(shard: &Shard, value: &str) -> Vec<PointId> {
    let f: nocturnum_core::Filter =
        serde_json::from_value(json!({"must": [{"key": "colour", "match": value}]})).unwrap();
    let view = shard.view();
    let mut ids = Vec::new();
    for seg in view.segments() {
        let mut b = seg.payload_index().bounds(&f);
        b.restrict(seg.live());
        assert!(b.is_exact(), "segment {} needed payload reads", seg.id());
        ids.extend(b.sure.iter().map(|row| seg.point(row).id));
    }
    ids.sort();
    ids
}

#[test]
fn payload_indexes_cover_every_segment_and_survive_reopen() {
    let dir = TempDir::new().unwrap();
    let shard = create(dir.path());
    upsert(
        &shard,
        vec![
            point(1, 0.0, json!({"colour": "red"})),
            point(2, 0.0, json!({"colour": "blue"})),
        ],
    );
    shard.flush().unwrap();

    // Created after a segment exists, so that segment is rebuilt.
    assert!(shard.create_payload_index(keyword("colour")).unwrap());
    assert!(!shard.create_payload_index(keyword("colour")).unwrap());
    assert_eq!(indexed_matches(&shard, "red"), [PointId::Num(1)]);

    // A newer copy hides the old row, and the next flush indexes the new segment.
    upsert(
        &shard,
        vec![
            point(2, 0.0, json!({"colour": "red"})),
            point(3, 0.0, json!({"colour": "red"})),
        ],
    );
    shard.flush().unwrap();
    assert_eq!(
        indexed_matches(&shard, "red"),
        [PointId::Num(1), PointId::Num(2), PointId::Num(3)]
    );
    drop(shard);

    let shard = reopen(dir.path());
    assert_eq!(shard.payload_indexes(), [keyword("colour")]);
    assert_eq!(shard.view().payload_indexes(), [keyword("colour")]);
    assert_eq!(indexed_matches(&shard, "red").len(), 3);
    assert!(shard.drop_payload_index(&keyword("colour")).unwrap());
    assert!(!shard.drop_payload_index(&keyword("colour")).unwrap());
    drop(shard);
    assert!(reopen(dir.path()).payload_indexes().is_empty());
}

#[test]
fn segment_views_and_recent_points_hold_every_point_once() {
    let dir = TempDir::new().unwrap();
    let shard = create(dir.path());
    upsert(
        &shard,
        vec![point(1, 1.0, json!({})), point(2, 2.0, json!({}))],
    );
    shard.flush().unwrap();
    upsert(
        &shard,
        vec![
            point(2, 5.0, json!({"new": true})),
            point(3, 3.0, json!({})),
        ],
    );
    let view = shard.view();
    let recent: Vec<_> = view.recent().map(|p| p.id).collect();
    assert_eq!(recent, [PointId::Num(2), PointId::Num(3)]);
    let seg = view.segments().next().unwrap();
    assert_eq!(seg.rows(), 2);
    let live: Vec<_> = seg.live().iter().map(|r| seg.point(r).id).collect();
    assert_eq!(live, [PointId::Num(1)]);
    assert_eq!(seg.point(1).vector, [2.0, 3.0, 4.0]);
    assert_eq!(seg.vectors().len(), 2 * DIM);
    assert_eq!(
        view.get(PointId::Num(2)).unwrap().unwrap().payload,
        payload(json!({"new": true}))
    );
}

#[test]
fn write_with_builds_the_operation_from_a_view_under_the_write_lock() {
    let dir = TempDir::new().unwrap();
    let shard = create(dir.path());
    upsert(
        &shard,
        vec![
            point(1, 0.0, json!({"k": 1})),
            point(2, 0.0, json!({"k": 2})),
        ],
    );
    let seq = shard
        .write_with(Ack::Durable, |view| {
            let ids = view
                .points()
                .filter(|p| p.payload().unwrap().0["k"] == json!(2))
                .map(|p| p.id)
                .collect();
            Ok(Operation::Delete(ids))
        })
        .unwrap();
    assert_eq!(seq, 2);
    assert_eq!(
        contents(&shard).keys().collect::<Vec<_>>(),
        [&PointId::Num(1)]
    );

    // A failure while building logs nothing.
    let err = shard.write_with(Ack::Durable, |_| Err(Error::Invalid("no".into())));
    assert!(matches!(err, Err(WriteError::Storage(Error::Invalid(_)))));
    assert_eq!(upsert(&shard, vec![point(3, 0.0, json!({}))]), 3);
}

/// The segment ids that have a directory on disk, in id order, which outside a flush or merge
/// in progress are exactly the live ones.
fn segment_dirs(dir: &Path) -> Vec<u64> {
    let mut ids: Vec<u64> = fs::read_dir(dir.join("segments"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .map(|n| u64::from_str_radix(n.strip_prefix("seg-").expect("only segments"), 16).unwrap())
        .collect();
    ids.sort();
    ids
}

fn segment_rows(shard: &Shard) -> Vec<(u32, u64)> {
    shard
        .view()
        .segments()
        .map(|s| (s.rows(), s.live().len()))
        .collect()
}

#[test]
fn merging_keeps_the_newest_copies_and_drops_dead_rows() {
    let dir = TempDir::new().unwrap();
    let shard = create(dir.path());
    upsert(
        &shard,
        (0..10)
            .map(|i| point(i, i as f32, json!({"v": 1})))
            .collect(),
    );
    shard.flush().unwrap();
    upsert(
        &shard,
        (0..5).map(|i| point(i, 50.0, json!({"v": 2}))).collect(),
    );
    shard.flush().unwrap();
    shard
        .write(Operation::Delete(vec![PointId::Num(9)]), Ack::Durable)
        .unwrap();
    shard.flush().unwrap();
    let before = contents(&shard);
    // 10 rows, 5 hidden by the second segment and 1 by the deletion in the third.
    assert_eq!(segment_rows(&shard), [(10, 4), (5, 5), (0, 0)]);

    let ids = shard.segment_ids();
    let merged = shard.merge(&ids[..2]).unwrap();
    assert_eq!(shard.segment_ids(), [merged, ids[2]]);
    assert_eq!(segment_rows(&shard), [(9, 9), (0, 0)]);
    assert_eq!(contents(&shard), before);
    assert_eq!(segment_dirs(dir.path()), sorted(shard.segment_ids()));
    drop(shard);
    let shard = reopen(dir.path());
    assert_eq!(contents(&shard), before);
}

fn sorted(mut ids: Vec<u64>) -> Vec<u64> {
    ids.sort();
    ids
}

/// Found by the model test: a row hidden only by a payload change still in the memtable must
/// survive a rewrite, since replaying that change after a crash needs it.
#[test]
fn rows_hidden_by_the_memtable_survive_a_rewrite() {
    let dir = TempDir::new().unwrap();
    let shard = create(dir.path());
    upsert(&shard, vec![point(4, 0.0, json!({"a": 1}))]);
    shard.flush().unwrap();
    shard
        .write(
            Operation::SetPayload {
                ids: vec![PointId::Num(4)],
                payload: payload(json!({"b": 2})),
            },
            Ack::Durable,
        )
        .unwrap();
    let before = contents(&shard);
    // Dead, but not droppable, so the policy leaves it alone and a forced rewrite keeps it.
    let options = OptimizerOptions {
        vacuum_ratio: 0.0,
        ..OptimizerOptions::default()
    };
    assert_eq!(shard.optimize_once(&options).unwrap(), None);
    shard.merge(&shard.segment_ids()).unwrap();
    assert_eq!(segment_rows(&shard), [(1, 0)]);
    drop(shard);
    assert_eq!(contents(&reopen(dir.path())), before);
}

#[test]
fn deletions_survive_merges_unless_the_run_starts_at_the_oldest_segment() {
    let dir = TempDir::new().unwrap();
    let shard = create(dir.path());
    upsert(&shard, vec![point(1, 1.0, json!({}))]);
    shard.flush().unwrap();
    shard
        .write(Operation::Delete(vec![PointId::Num(1)]), Ack::Durable)
        .unwrap();
    shard.flush().unwrap();
    upsert(&shard, vec![point(2, 2.0, json!({}))]);
    shard.flush().unwrap();
    let ids = shard.segment_ids();

    // Merging the deletion with a newer segment must keep it, or point 1 comes back from the
    // oldest segment.
    let merged = shard.merge(&ids[1..]).unwrap();
    drop(shard);
    let shard = reopen(dir.path());
    assert_eq!(shard.get(PointId::Num(1)).unwrap(), None);
    let entries = shard.view().segments().nth(1).unwrap().point(0).id;
    assert_eq!(entries, PointId::Num(2));

    // Merging everything from the oldest drops the deletion and the copy it hid.
    let all = shard.merge(&[ids[0], merged]).unwrap();
    assert_eq!(shard.segment_ids(), [all]);
    assert_eq!(segment_rows(&shard), [(1, 1)]);
    assert_eq!(shard.get(PointId::Num(1)).unwrap(), None);
}

#[test]
fn vacuum_and_merge_policy() {
    let dir = TempDir::new().unwrap();
    let shard = create(dir.path());
    let options = OptimizerOptions {
        max_segments: 2,
        ..OptimizerOptions::default()
    };
    upsert(&shard, (0..10).map(|i| point(i, 0.0, json!({}))).collect());
    shard.flush().unwrap();
    assert_eq!(shard.optimize_once(&options).unwrap(), None);
    // Rewriting 3 of 10 points kills 30% of the first segment's rows.
    upsert(&shard, (0..3).map(|i| point(i, 1.0, json!({}))).collect());
    shard.flush().unwrap();
    let first = shard.segment_ids()[0];
    let job = shard.optimize_once(&options).unwrap();
    assert!(
        matches!(job, Some(Job::Vacuum { input, .. }) if input == first),
        "{job:?}"
    );
    assert_eq!(segment_rows(&shard), [(7, 7), (3, 3)]);

    upsert(&shard, vec![point(20, 0.0, json!({}))]);
    shard.flush().unwrap();
    // Three segments and a limit of two: the smallest adjacent pair merges.
    let ids = shard.segment_ids();
    let job = shard.optimize_once(&options).unwrap();
    assert!(
        matches!(&job, Some(Job::Merge { inputs, .. }) if inputs == &ids[1..]),
        "{job:?}"
    );
    assert_eq!(shard.optimizer_status().last, job);
    assert_eq!(shard.len(), 11);
}

#[test]
fn only_adjacent_live_segments_merge() {
    let dir = TempDir::new().unwrap();
    let shard = create(dir.path());
    for i in 0..3 {
        upsert(&shard, vec![point(i, 0.0, json!({}))]);
        shard.flush().unwrap();
    }
    let ids = shard.segment_ids();
    assert!(shard.merge(&[ids[0], ids[2]]).is_err());
    assert!(shard.merge(&[ids[2], ids[1]]).is_err());
    assert!(shard.merge(&[999]).is_err());
    assert!(shard.merge(&[]).is_err());
    assert_eq!(shard.segment_ids(), ids);
}

/// Writes and flushes from one thread while another merges, and the result matches what the
/// writes alone produce.
#[test]
fn writes_and_flushes_carry_on_during_merges() {
    let dir = TempDir::new().unwrap();
    let shard = Arc::new(create(dir.path()));
    let done = Arc::new(AtomicBool::new(false));
    let merger = {
        let (shard, done) = (shard.clone(), done.clone());
        std::thread::spawn(move || {
            let options = OptimizerOptions {
                max_segments: 2,
                ..OptimizerOptions::default()
            };
            while !done.load(Ordering::Acquire) {
                shard.optimize_once(&options).unwrap();
            }
        })
    };
    let mut model = BTreeMap::new();
    for round in 0..60u64 {
        let op = if round % 5 == 4 {
            Operation::Delete(vec![PointId::Num(round % 7)])
        } else {
            Operation::Upsert(vec![point(round % 13, round as f32, json!({"r": round}))])
        };
        apply_model(&mut model, &op);
        shard.write(op, Ack::Applied).unwrap();
        if round % 3 == 0 {
            shard.flush().unwrap();
        }
    }
    done.store(true, Ordering::Release);
    merger.join().unwrap();
    assert_eq!(contents(&shard), model);
    drop(Arc::into_inner(shard).unwrap());
    assert_eq!(contents(&reopen(dir.path())), model);
}

/// Found by the audit: a merge may drop a row because an `applied`, still unsynced deletion
/// hides it, so it syncs the WAL before publishing, or power loss could lose the deletion
/// after the row is gone.
#[test]
fn a_merge_syncs_the_writes_it_relies_on() {
    let dir = TempDir::new().unwrap();
    let opts = ShardOptions {
        background_sync: Duration::from_secs(3600),
        ..opts()
    };
    let shard = Shard::open(dir.path(), opts, Some(config())).unwrap().0;
    upsert(&shard, vec![point(1, 0.0, json!({}))]);
    shard.flush().unwrap();
    let seq = shard
        .write(Operation::Delete(vec![PointId::Num(1)]), Ack::Applied)
        .unwrap();
    assert!(
        shard.wal.synced_seq() < seq,
        "the deletion is not synced yet"
    );
    shard.merge(&shard.segment_ids()).unwrap();
    assert!(
        shard.wal.synced_seq() >= seq,
        "the merge published before syncing"
    );
}

#[test]
fn a_vacuum_ratio_outside_0_to_1_is_refused() {
    let dir = TempDir::new().unwrap();
    let shard = create(dir.path());
    for ratio in [-0.1, 1.5, f64::NAN] {
        let options = OptimizerOptions {
            vacuum_ratio: ratio,
            ..OptimizerOptions::default()
        };
        assert!(shard.optimize_once(&options).is_err(), "{ratio}");
    }
}

/// The bug this guards against: a view held the shard's read lock, so a writer waited for it,
/// and a thread holding a view deadlocked once it called the shard again with a writer queued.
#[test]
fn a_view_holds_no_lock() {
    let dir = TempDir::new().unwrap();
    let shard = Arc::new(create(dir.path()));
    upsert(&shard, vec![point(1, 1.0, json!({"a": 1}))]);
    shard.flush().unwrap();
    let view = shard.view();

    let (tx, rx) = std::sync::mpsc::channel();
    let writer = {
        let shard = shard.clone();
        std::thread::spawn(move || {
            upsert(&shard, vec![point(2, 2.0, json!({}))]);
            shard.flush().unwrap();
            shard.merge(&shard.segment_ids()).unwrap();
            tx.send(()).unwrap();
        })
    };
    rx.recv_timeout(Duration::from_secs(30))
        .expect("the writer finished while the view was held");
    writer.join().unwrap();
    // The same thread still holds the view and can use the shard.
    assert_eq!(shard.len(), 2);
    assert!(shard.get(PointId::Num(2)).unwrap().is_some());
    assert_eq!(view.len(), 1);
    assert_eq!(view.get(PointId::Num(2)).unwrap(), None);
}

/// A view keeps showing the shard as it was, through writes, flushes and merges that delete
/// the segment directories it reads from.
#[test]
fn a_view_is_unchanged_by_later_writes() {
    let dir = TempDir::new().unwrap();
    let shard = create(dir.path());
    for round in 0..4u64 {
        upsert(
            &shard,
            (0..6)
                .map(|i| point(round * 3 + i, i as f32, json!({"r": round})))
                .collect(),
        );
        shard.flush().unwrap();
    }
    upsert(&shard, vec![point(100, 1.0, json!({"in": "memtable"}))]);
    let view = shard.view();
    let before = contents(&shard);
    assert_eq!(view_contents(&view), before);

    shard
        .write(
            Operation::Delete(vec![PointId::Num(0), PointId::Num(100)]),
            Ack::Applied,
        )
        .unwrap();
    shard
        .write(
            Operation::SetPayload {
                ids: vec![PointId::Num(4), PointId::Num(9)],
                payload: payload(json!({"changed": true})),
            },
            Ack::Applied,
        )
        .unwrap();
    upsert(&shard, vec![point(200, 5.0, json!({}))]);
    shard.flush().unwrap();
    let old_segments = shard.segment_ids();
    shard.merge(&old_segments).unwrap();
    for id in &old_segments {
        assert!(
            !segment_dirs(dir.path()).contains(id),
            "segment {id} is gone from disk"
        );
    }

    assert_eq!(view_contents(&view), before);
    assert_eq!(view.len(), before.len());
    for id in (0..20).chain([100, 200]).map(PointId::Num) {
        assert_eq!(view.get(id).unwrap(), before.get(&id).cloned(), "{id}");
    }
    let after = shard.view();
    assert_eq!(view_contents(&after), contents(&shard));
    assert_ne!(view_contents(&after), before);
}

/// Pauses the shard's next merge at each checkpoint, returning a receiver of the checkpoint
/// names and a sender that lets the merge carry on.
fn pause_merges(
    shard: &Shard,
) -> (
    std::sync::mpsc::Receiver<&'static str>,
    std::sync::mpsc::Sender<()>,
) {
    let (reached, at) = std::sync::mpsc::channel();
    let (go, wait) = std::sync::mpsc::channel::<()>();
    let wait = Mutex::new(wait);
    *lock(&shard.checkpoint) = Some(Arc::new(move |name| {
        reached.send(name).unwrap();
        lock(&wait).recv().unwrap();
    }));
    (at, go)
}

/// The next checkpoint a paused merge reaches, failing rather than hanging when the merge
/// stopped early. Failing drops the sender that lets the merge carry on, which ends it too.
fn reached(at: &std::sync::mpsc::Receiver<&'static str>) -> &'static str {
    at.recv_timeout(Duration::from_secs(60))
        .expect("the merge reached its next checkpoint")
}

/// Two segments of 1500 points each, overlapping on 500 ids.
fn two_segments(shard: &Shard) {
    for round in 0..2u64 {
        upsert(
            shard,
            (0..1500)
                .map(|i| point(round * 1000 + i, (i % 97) as f32, json!({})))
                .collect(),
        );
        shard.flush().unwrap();
    }
}

/// A merge reports how far it has got while it runs, and nothing once it is done.
#[test]
fn a_merge_reports_its_progress() {
    let dir = TempDir::new().unwrap();
    let shard = Arc::new(create(dir.path()));
    two_segments(&shard);
    let ids = shard.segment_ids();
    let (at, go) = pause_merges(&shard);
    let merger = {
        let (shard, ids) = (shard.clone(), ids.clone());
        std::thread::spawn(move || shard.merge(&ids).unwrap())
    };

    assert_eq!(reached(&at), "copying");
    let status = shard.optimizer_status();
    assert_eq!(status.running, Some(ids.clone()));
    let progress = MergeProgress {
        entries: 3000,
        entries_done: 1500,
        graph_rows: 0,
        graph_done: 0,
    };
    assert_eq!(status.progress, Some(progress));
    go.send(()).unwrap();

    // 2500 distinct points survive the merge, and every one goes into the graph.
    assert_eq!(reached(&at), "indexing");
    let progress = MergeProgress {
        entries_done: 3000,
        graph_rows: 2500,
        ..progress
    };
    assert_eq!(shard.optimizer_status().progress, Some(progress));
    go.send(()).unwrap();

    merger.join().unwrap();
    let status = shard.optimizer_status();
    assert_eq!((status.running, status.progress), (None, None));
}

/// A merge that fails clears its progress along with the segments it was rewriting.
#[test]
fn a_failed_merge_clears_its_progress() {
    let dir = TempDir::new().unwrap();
    let shard = Arc::new(create(dir.path()));
    two_segments(&shard);
    let ids = shard.segment_ids();
    let (at, go) = pause_merges(&shard);
    let merger = {
        let (shard, ids) = (shard.clone(), ids.clone());
        std::thread::spawn(move || shard.merge(&ids))
    };
    assert_eq!(reached(&at), "copying");
    assert!(shard.optimizer_status().progress.is_some());
    // Stands in for a manifest write that failed meanwhile, so publishing is refused.
    shard.flush_broken.store(true, Ordering::Release);
    go.send(()).unwrap();
    assert_eq!(reached(&at), "indexing");
    go.send(()).unwrap();

    assert!(merger.join().unwrap().is_err());
    let status = shard.optimizer_status();
    assert_eq!((status.running, status.progress), (None, None));
    assert_eq!(shard.segment_ids(), ids);
}

/// A view taken while a flush is writing the frozen memtable out keeps showing it after the
/// flush publishes the segment.
#[test]
fn a_view_taken_mid_flush_keeps_its_moment() {
    let dir = TempDir::new().unwrap();
    let shard = create(dir.path());
    upsert(
        &shard,
        (0..4).map(|i| point(i, 0.0, json!({"v": "old"}))).collect(),
    );
    {
        let mut st = write(&shard.state);
        let mem = std::mem::take(&mut st.mem);
        st.frozen = Some(Arc::new(mem));
        st.frozen_upto = st.last_applied;
    }
    upsert(&shard, vec![point(1, 1.0, json!({"v": "new"}))]);
    shard
        .write(Operation::Delete(vec![PointId::Num(2)]), Ack::Applied)
        .unwrap();
    let view = shard.view();
    let then = view_contents(&view);
    assert_eq!(then.len(), 3);
    assert_eq!(then[&PointId::Num(1)].payload, payload(json!({"v": "new"})));

    shard.flush().unwrap();
    shard.flush().unwrap();
    upsert(&shard, vec![point(3, 2.0, json!({"v": "newer"}))]);
    assert_eq!(view_contents(&view), then);
    for id in (0..5).map(PointId::Num) {
        assert_eq!(view.get(id).unwrap(), then.get(&id).cloned(), "{id}");
    }
}

#[test]
fn merged_entries_count_every_input_entry() {
    let dir = TempDir::new().unwrap();
    let shard = create(dir.path());
    upsert(&shard, (0..5).map(|i| point(i, 0.0, json!({}))).collect());
    shard.flush().unwrap();
    upsert(&shard, (3..8).map(|i| point(i, 1.0, json!({}))).collect());
    shard
        .write(Operation::Delete(vec![PointId::Num(0)]), Ack::Applied)
        .unwrap();
    shard.flush().unwrap();
    let inputs: Vec<merge::Input> = {
        let state = read(&shard.state);
        state
            .slots
            .iter()
            .map(|s| merge::Input::of(&state, s))
            .collect()
    };
    let total: usize = inputs.iter().map(|i| i.segment.entries().len()).sum();
    let counters = merge::Counters::default();
    let written = merge::merged_entries(&inputs, true, &counters).count();
    assert_eq!(counters.read().entries_done, total as u64);
    assert_eq!(written, 7, "ids 1 to 7, with 0 deleted");
}

fn snapshot_dir() -> TempDir {
    TempDir::new().unwrap()
}

#[test]
fn a_snapshot_restores_to_the_same_shard() {
    let dir = TempDir::new().unwrap();
    let snaps = snapshot_dir();
    let shard = create(dir.path());
    upsert(
        &shard,
        (0..20)
            .map(|i| point(i, i as f32, json!({"colour": "red"})))
            .collect(),
    );
    shard.flush().unwrap();
    shard.create_payload_index(keyword("colour")).unwrap();
    upsert(
        &shard,
        (10..25)
            .map(|i| point(i, 7.0, json!({"colour": "blue"})))
            .collect(),
    );
    shard
        .write(Operation::Delete(vec![PointId::Num(3)]), Ack::Applied)
        .unwrap();
    let before = contents(&shard);

    let path = snaps.path().join(snapshot_name("test"));
    let info = shard.snapshot(&path).unwrap();
    assert_eq!(info.seq, 3);
    assert!(path.exists() && info.bytes == fs::metadata(&path).unwrap().len());
    assert!(
        shard.snapshot(&path).is_err(),
        "an existing snapshot is never overwritten"
    );

    let target = TempDir::new().unwrap().path().join("restored");
    restore(&path, &target).unwrap();
    let restored = reopen(&target);
    assert_eq!(contents(&restored), before);
    assert_eq!(restored.payload_indexes(), [keyword("colour")]);
    assert!(restore(&path, &target).is_err(), "restore never overwrites");
}

/// A snapshot taken while another thread writes holds every write acknowledged before it
/// started, and is exactly the result of some prefix of the writes.
#[test]
fn a_snapshot_during_writes_is_a_prefix_of_history() {
    let dir = TempDir::new().unwrap();
    let snaps = snapshot_dir();
    let shard = Arc::new(create(dir.path()));
    let ops: Vec<Operation> = (0..400u64)
        .map(|i| match i % 4 {
            3 => Operation::SetPayload {
                ids: vec![PointId::Num(i % 11)],
                payload: payload(json!({"i": i})),
            },
            2 => Operation::Delete(vec![PointId::Num(i % 13)]),
            _ => Operation::Upsert(vec![point(i % 17, i as f32, json!({"i": i}))]),
        })
        .collect();
    let acked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let writer = {
        let (shard, acked, ops) = (shard.clone(), acked.clone(), ops.clone());
        std::thread::spawn(move || {
            for (i, op) in ops.into_iter().enumerate() {
                shard.write(op, Ack::Applied).unwrap();
                acked.store(i + 1, Ordering::Release);
                if i % 50 == 49 {
                    shard.flush().unwrap();
                }
            }
        })
    };
    while acked.load(Ordering::Acquire) < 150 {
        std::thread::yield_now();
    }
    let before = acked.load(Ordering::Acquire);
    let path = snaps.path().join("mid.snapshot");
    shard.snapshot(&path).unwrap();
    writer.join().unwrap();

    let target = snaps.path().join("restored");
    restore(&path, &target).unwrap();
    let got = contents(&reopen(&target));
    let mut model = BTreeMap::new();
    let mut prefixes = Vec::new();
    for op in &ops {
        prefixes.push(model.clone());
        apply_model(&mut model, op);
    }
    prefixes.push(model);
    let k = prefixes
        .iter()
        .position(|m| *m == got)
        .expect("the snapshot matches no prefix");
    assert!(
        k >= before,
        "the snapshot holds {k} writes, {before} were acknowledged before it"
    );
}

#[test]
fn truncated_archives_with_matching_checksums_are_corrupt() {
    use sha2::Digest;
    let dir = TempDir::new().unwrap();
    let snaps = snapshot_dir();
    let shard = create(dir.path());
    upsert(
        &shard,
        vec![point(1, 1.0, json!({"pad": "x".repeat(3000)}))],
    );
    let path = snaps.path().join("a.snapshot");
    shard.snapshot(&path).unwrap();
    let original = fs::read(&path).unwrap();
    // Cuts inside a header, inside file data and at the end of the last entry's data.
    for cut in [100, 700, 2000, original.len() - 1024 - 10] {
        let bytes = &original[..cut];
        let short = snaps.path().join(format!("cut-{cut}.snapshot"));
        fs::write(&short, bytes).unwrap();
        let sum: String = sha2::Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let sidecar = snaps.path().join(format!("cut-{cut}.snapshot.sha256"));
        fs::write(sidecar, format!("{sum}\n")).unwrap();
        let target = snaps.path().join(format!("restored-{cut}"));
        let got = restore(&short, &target);
        assert!(
            matches!(got, Err(Error::Corrupt { .. })),
            "cut at {cut}: {got:?}"
        );
        assert!(!target.exists());
    }
}

#[test]
fn damaged_or_foreign_snapshots_are_refused() {
    let dir = TempDir::new().unwrap();
    let snaps = snapshot_dir();
    let shard = create(dir.path());
    upsert(&shard, vec![point(1, 1.0, json!({}))]);
    let path = snaps.path().join("a.snapshot");
    shard.snapshot(&path).unwrap();
    let target = snaps.path().join("restored");

    let original = fs::read(&path).unwrap();
    let mut damaged = original.clone();
    damaged[600] ^= 1;
    fs::write(&path, &damaged).unwrap();
    assert!(matches!(
        restore(&path, &target),
        Err(Error::Corrupt { .. })
    ));
    assert!(!target.exists());
    fs::write(&path, &original).unwrap();
    fs::remove_file(snaps.path().join("a.snapshot.sha256")).unwrap();
    assert!(
        restore(&path, &target).is_err(),
        "a snapshot without its sidecar is unfinished"
    );

    // A tar naming a file outside the shard, with a correct sidecar, is refused.
    let evil = snaps.path().join("evil.snapshot");
    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.as_gnu_mut().unwrap().name[..7].copy_from_slice(b"../evil");
    header.set_size(1);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append(&header, &b"x"[..]).unwrap();
    let bytes = tar.into_inner().unwrap();
    fs::write(&evil, &bytes).unwrap();
    use sha2::Digest;
    let sum: String = sha2::Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    fs::write(
        snaps.path().join("evil.snapshot.sha256"),
        format!("{sum}  evil.snapshot\n"),
    )
    .unwrap();
    assert!(matches!(
        restore(&evil, &target),
        Err(Error::Corrupt { .. })
    ));
    assert!(!snaps.path().join("evil").exists() && !target.exists());
}

#[test]
fn prune_keeps_the_newest_good_snapshots() {
    let dir = TempDir::new().unwrap();
    let snaps = snapshot_dir();
    let shard = create(dir.path());
    for i in 0..5u64 {
        upsert(&shard, vec![point(i, 1.0, json!({}))]);
        shard
            .snapshot(&snaps.path().join(format!("c-{i:015}.snapshot")))
            .unwrap();
    }
    // The newest is damaged, so it does not count, and an unfinished one is too young to be
    // abandoned.
    let newest = snaps.path().join("c-000000000000004.snapshot");
    let mut bytes = fs::read(&newest).unwrap();
    bytes[700] ^= 1;
    fs::write(&newest, &bytes).unwrap();
    fs::write(snaps.path().join("c-9.snapshot.1-0.partial"), b"half").unwrap();
    // A sidecar that holds no checksum counts as damage too, and pruning carries on past it.
    let garbled = snaps.path().join("c-000000000000003.snapshot");
    fs::write(
        snaps.path().join("c-000000000000003.snapshot.sha256"),
        "x".repeat(5000),
    )
    .unwrap();

    let pruned = prune(snaps.path(), 2).unwrap();
    assert_eq!(pruned.invalid, [newest, garbled]);
    assert_eq!(pruned.deleted.len(), 1);
    let mut left: Vec<_> = fs::read_dir(snaps.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(
        left,
        [
            "c-000000000000001.snapshot",
            "c-000000000000001.snapshot.sha256",
            "c-000000000000002.snapshot",
            "c-000000000000002.snapshot.sha256",
            "c-000000000000003.snapshot",
            "c-000000000000003.snapshot.sha256",
            "c-000000000000004.snapshot",
            "c-000000000000004.snapshot.sha256",
            "c-9.snapshot.1-0.partial",
        ]
    );
}

/// A snapshot that checks out but does not hold a shard that opens is refused, and the
/// target name is left free.
#[test]
fn an_incomplete_snapshot_is_refused_before_it_takes_the_target() {
    let snaps = snapshot_dir();
    let path = snaps.path().join("thin.snapshot");
    let mut tar = tar::Builder::new(Vec::new());
    let info = br#"{"format":1,"seq":5,"vectors":{"dim":3,"distance":"dot"}}"#;
    let mut header = tar::Header::new_gnu();
    header.set_size(info.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append_data(&mut header, "nocturnum-snapshot.json", &info[..])
        .unwrap();
    let bytes = tar.into_inner().unwrap();
    fs::write(&path, &bytes).unwrap();
    use sha2::Digest;
    let sum: String = sha2::Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    fs::write(
        snaps.path().join("thin.snapshot.sha256"),
        format!("{sum}  thin.snapshot\n"),
    )
    .unwrap();
    let target = snaps.path().join("restored");
    assert!(matches!(
        restore(&path, &target),
        Err(Error::Corrupt { .. })
    ));
    assert!(!target.exists());
    let leftovers = fs::read_dir(snaps.path()).unwrap().count();
    assert_eq!(leftovers, 2, "staging was left behind");
}

#[test]
fn snapshots_never_replace_an_existing_file() {
    let dir = TempDir::new().unwrap();
    let snaps = snapshot_dir();
    let shard = Arc::new(create(dir.path()));
    upsert(&shard, vec![point(1, 1.0, json!({}))]);
    let path = snaps.path().join("same.snapshot");
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let (shard, path) = (shard.clone(), path.clone());
            std::thread::spawn(move || shard.snapshot(&path).is_ok())
        })
        .collect();
    let won = threads
        .into_iter()
        .map(|t| t.join().unwrap())
        .filter(|ok| *ok)
        .count();
    assert_eq!(won, 1);
    let target = snaps.path().join("restored");
    restore(&path, &target).unwrap();
    let names: Vec<_> = fs::read_dir(snaps.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(!names.iter().any(|n| n.ends_with(".partial")), "{names:?}");
}

/// Found by the audit: tar writes files with holes as sparse entries, which restore refuses,
/// so snapshots turn sparse output off. The tar crate only looks for holes on Linux and
/// FreeBSD, so this test can only fail there, which includes the homelab's deploy target.
#[test]
fn a_shard_with_sparse_files_snapshots_and_restores() {
    let dir = TempDir::new().unwrap();
    let snaps = snapshot_dir();
    let shard = create(dir.path());
    upsert(
        &shard,
        (0..2000)
            .map(|i| point(i, 0.0, json!({})))
            .collect::<Vec<_>>()
            .into_iter()
            .map(|mut p| {
                p.vector = vec![0.0; DIM];
                p
            })
            .collect(),
    );
    shard.flush().unwrap();
    drop(shard);
    // Rewrite the all-zero vectors file as one big hole, same bytes, same checksum.
    let seg = fs::read_dir(dir.path().join("segments"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let vectors = seg.join("vectors.bin");
    let len = fs::metadata(&vectors).unwrap().len();
    assert!(fs::read(&vectors).unwrap().iter().all(|b| *b == 0));
    fs::remove_file(&vectors).unwrap();
    fs::File::create(&vectors).unwrap().set_len(len).unwrap();
    let shard = reopen(dir.path());
    let before = contents(&shard);

    let path = snaps.path().join("sparse.snapshot");
    shard.snapshot(&path).unwrap();
    let target = snaps.path().join("restored");
    restore(&path, &target).unwrap();
    assert_eq!(contents(&reopen(&target)), before);
}

#[test]
fn prune_leaves_staging_a_live_process_owns() {
    let snaps = snapshot_dir();
    let old = std::time::SystemTime::now() - Duration::from_secs(7200);
    let make = |name: &str| {
        let path = snaps.path().join(name);
        let f = fs::File::create(&path).unwrap();
        f.set_modified(old).unwrap();
        path
    };
    // This process, not writing it: abandoned. Process 1 always exists: in use.
    let mine = make(&format!("a.snapshot.{}-999999.partial", std::process::id()));
    let live = make("b.snapshot.1-0.partial");
    let pruned = prune(snaps.path(), 3).unwrap();
    assert_eq!(pruned.deleted, [mine]);
    assert!(live.exists());

    // A staging file this process is still writing is kept, whatever path prune is given.
    let writing = super::snapshot::staging(&snaps.path().join("c.snapshot"));
    let f = fs::File::create(&*writing).unwrap();
    f.set_modified(old).unwrap();
    let relative_dir = snaps.path().join(".").join("");
    assert!(prune(&relative_dir, 3).unwrap().deleted.is_empty());
    assert!(writing.exists());
}

#[test]
fn an_oversized_manifest_is_refused_before_it_is_read() {
    let dir = TempDir::new().unwrap();
    drop(create(dir.path()));
    let manifest = dir.path().join("MANIFEST");
    fs::File::options()
        .write(true)
        .open(&manifest)
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    assert!(Shard::open(dir.path(), opts(), None).is_err());
}

fn apply_model(model: &mut BTreeMap<PointId, Record>, op: &Operation) {
    match op {
        Operation::Upsert(points) => {
            for p in points {
                model.insert(
                    p.id,
                    Record {
                        vector: p.vector.clone(),
                        sparse: p.sparse.clone(),
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

/// A sparse vector in its prepared form half the time, none otherwise.
fn small_sparse() -> impl Strategy<Value = Option<SparseVector>> {
    proptest::option::of(
        proptest::collection::btree_map(0u32..40, -2.0f32..2.0, 1..5).prop_map(|m| SparseVector {
            indices: m.keys().copied().collect(),
            values: m.values().copied().collect(),
        }),
    )
}

fn step() -> impl Strategy<Value = Step> {
    let write = proptest::collection::vec(
        (0u64..12, -4.0f32..4.0, small_payload(), small_sparse()).prop_map(
            |(id, x, payload, sparse)| PointWrite {
                id: PointId::Num(id),
                vector: vec![x, -x, x * 0.5],
                sparse,
                payload,
            },
        ),
        0..4,
    );
    prop_oneof![
        8 => write.prop_map(|p| Step::Op(Operation::Upsert(p))),
        3 => ids().prop_map(|i| Step::Op(Operation::Delete(i))),
        2 => (ids(), small_payload()).prop_map(|(ids, payload)| Step::Op(Operation::SetPayload { ids, payload })),
        1 => (ids(), small_payload()).prop_map(|(ids, payload)| Step::Op(Operation::OverwritePayload { ids, payload })),
        1 => ids().prop_map(|ids| Step::Op(Operation::DeletePayloadKeys { ids, keys: vec!["a".into()] })),
        1 => ids().prop_map(|i| Step::Op(Operation::ClearPayload(i))),
        3 => Just(Step::Flush),
        1 => Just(Step::Reopen),
        2 => (0usize..8, 1usize..4).prop_map(|(at, len)| Step::Merge(at, len)),
        1 => Just(Step::Optimize),
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
                Step::Merge(at, len) => {
                    let ids = shard.segment_ids();
                    if !ids.is_empty() {
                        let start = at % ids.len();
                        let end = (start + len).min(ids.len());
                        shard.merge(&ids[start..end]).unwrap();
                    }
                }
                Step::Optimize => {
                    let options = OptimizerOptions { max_segments: 2, ..OptimizerOptions::default() };
                    shard.optimize_once(&options).unwrap();
                }
            }
            prop_assert_eq!(&contents(&shard), &model);
            prop_assert_eq!(segment_dirs(dir.path()), sorted(shard.segment_ids()), "directories on disk");
            for (id, rec) in &model {
                prop_assert_eq!(shard.get(*id).unwrap(), Some(rec.clone()));
            }
        }
    }

    /// A view taken at any step keeps showing that step's points through every later write,
    /// flush, merge and reopen, by iteration and by id.
    #[test]
    fn a_view_keeps_its_moment(
        steps in proptest::collection::vec(step(), 1..60),
        at in 0usize..60,
    ) {
        let dir = TempDir::new().unwrap();
        let mut shard = create(dir.path());
        let mut model = BTreeMap::new();
        let at = at % steps.len();
        let mut held = None;
        for (i, step) in steps.into_iter().enumerate() {
            if i == at {
                held = Some((shard.view(), model.clone()));
            }
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
                Step::Merge(at, len) => {
                    let ids = shard.segment_ids();
                    if !ids.is_empty() {
                        let start = at % ids.len();
                        let end = (start + len).min(ids.len());
                        shard.merge(&ids[start..end]).unwrap();
                    }
                }
                Step::Optimize => {
                    let options = OptimizerOptions { max_segments: 2, ..OptimizerOptions::default() };
                    shard.optimize_once(&options).unwrap();
                }
            }
        }
        let (view, then) = held.expect("the view was taken");
        prop_assert_eq!(&view_contents(&view), &then);
        prop_assert_eq!(view.len(), then.len());
        for id in (0..12).map(PointId::Num) {
            prop_assert_eq!(view.get(id).unwrap(), then.get(&id).cloned());
        }
        prop_assert_eq!(&contents(&shard), &model);
    }
}
