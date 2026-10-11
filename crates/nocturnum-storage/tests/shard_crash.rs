//! Crash tests for a whole shard: writes, flushes, the manifest and recovery together.
//!
//! A child process applies a deterministic stream of mixed operations from one thread with
//! `durable` acknowledgements, flushing every few operations, and prints the number of each
//! acknowledged operation. It is killed at a random moment or kills itself at a named crash
//! point inside a flush. The parent regenerates the same operations and checks that the
//! reopened shard holds exactly the result of every acknowledged operation, plus possibly the
//! one that was in flight, since a single writer only starts an operation after the previous
//! one was acknowledged.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use nocturnum_core::{Ack, Distance, Payload, PointId, VectorConfig};
use nocturnum_storage::shard::{prune, restore, snapshot_name};
use nocturnum_storage::{
    GraphOptions, Operation, OptimizerOptions, PointWrite, Record, Shard, ShardOptions, WalOptions,
};
use serde_json::json;
use tempfile::TempDir;

const CHILD_ENV: &str = "NOCTURNUM_SHARD_CRASH_CHILD";
const FLUSH_EVERY: u64 = 25;

static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy)]
enum Mode {
    Plain,
    /// Merges and vacuums halfway between flushes.
    Optimize,
    /// Takes a snapshot every 20 writes, keeping the newest three.
    Snapshot,
}

fn config() -> VectorConfig {
    VectorConfig {
        dim: 4,
        distance: Distance::Dot,
    }
}

fn opts() -> ShardOptions {
    ShardOptions {
        wal: WalOptions {
            segment_bytes: 4096,
            ..WalOptions::default()
        },
        flush_bytes: usize::MAX,
        background_sync: Duration::from_millis(20),
        // Every flushed segment builds a graph, so graphs go through every path tested here.
        graph: GraphOptions {
            min_bytes: 0,
            ..GraphOptions::default()
        },
    }
}

/// A small generator so the parent and child derive the same operations from a run and a
/// counter.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

fn operation(run: u64, counter: u64) -> Operation {
    let mut r = Rng::new((run << 32) ^ counter ^ 0xABCD);
    let id = |r: &mut Rng| PointId::Num(r.below(30));
    let ids = |r: &mut Rng| (0..1 + r.below(3)).map(|_| id(r)).collect::<Vec<_>>();
    let payload = |r: &mut Rng| -> Payload {
        serde_json::from_value(json!({
            "run": run,
            "n": counter,
            "k": r.below(4),
        }))
        .unwrap()
    };
    match r.below(10) {
        0..=4 => Operation::Upsert(
            (0..1 + r.below(3))
                .map(|_| PointWrite {
                    id: id(&mut r),
                    vector: (0..4).map(|i| (counter * 4 + i) as f32 * 0.25).collect(),
                    payload: payload(&mut r),
                })
                .collect(),
        ),
        5 | 6 => Operation::Delete(ids(&mut r)),
        7 => Operation::SetPayload {
            ids: ids(&mut r),
            payload: serde_json::from_value(json!({"set": counter})).unwrap(),
        },
        8 => Operation::DeletePayloadKeys {
            ids: ids(&mut r),
            keys: vec!["k".into()],
        },
        _ => Operation::ClearPayload(ids(&mut r)),
    }
}

/// Applies an operation the plainest possible way.
fn apply(model: &mut BTreeMap<PointId, Record>, op: &Operation) {
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
        Operation::Delete(ids) => ids.iter().for_each(|id| {
            model.remove(id);
        }),
        Operation::SetPayload { ids, payload } => {
            for id in ids {
                let Some(r) = model.get_mut(id) else { continue };
                for (k, v) in &payload.0 {
                    r.payload.0.insert(k.clone(), v.clone());
                }
            }
        }
        Operation::OverwritePayload { ids, payload } => {
            for id in ids {
                let Some(r) = model.get_mut(id) else { continue };
                r.payload = payload.clone();
            }
        }
        Operation::DeletePayloadKeys { ids, keys } => {
            for id in ids {
                let Some(r) = model.get_mut(id) else { continue };
                for k in keys {
                    r.payload.0.remove(k);
                }
            }
        }
        Operation::ClearPayload(ids) => {
            for id in ids {
                let Some(r) = model.get_mut(id) else { continue };
                r.payload = Payload::new();
            }
        }
    }
}

#[test]
fn shard_crash_child() {
    let Ok(spec) = std::env::var(CHILD_ENV) else {
        return;
    };
    std::panic::set_hook(Box::new(|info| {
        eprintln!("shard crash child failed: {info}");
        std::process::exit(101);
    }));
    let mut parts = spec.rsplitn(3, ',');
    let mode = parts.next().unwrap();
    let optimize = mode == "optimize";
    let snapshots = mode == "snapshot";
    let run: u64 = parts.next().unwrap().parse().unwrap();
    let dir = parts.next().unwrap();
    // A limit of two segments makes nearly every flush trigger a merge, and vacuums follow
    // from the overwrites.
    let options = OptimizerOptions {
        max_segments: 2,
        ..OptimizerOptions::default()
    };
    let (shard, _) = Shard::open(dir, opts(), Some(config())).expect("open");
    println!("ready");
    // Writes until the process is killed or reaches its crash point.
    let mut counter = 0;
    loop {
        shard
            .write(operation(run, counter), Ack::Durable)
            .expect("write");
        println!("ack {counter}");
        if counter % FLUSH_EVERY == FLUSH_EVERY - 1 {
            shard.flush().expect("flush");
        }
        if snapshots && counter % 20 == 10 {
            let dir = format!("{dir}-snaps");
            std::fs::create_dir_all(&dir).unwrap();
            let path = Path::new(&dir).join(snapshot_name("s"));
            shard.snapshot(&path).expect("snapshot");
            prune(Path::new(&dir), 3).expect("prune");
        }
        // Halfway between flushes, so merges run while the memtable holds changes that hide
        // rows in the segments being merged.
        if optimize && counter % FLUSH_EVERY == FLUSH_EVERY / 2 {
            while shard.optimize_once(&options).expect("optimize").is_some() {}
        }
        counter += 1;
    }
}

/// Runs one child and returns the highest acknowledged counter, if any.
fn run_child(
    dir: &Path,
    run: u64,
    kill_after: Option<Duration>,
    crash_at: Option<&str>,
    mode: Mode,
) -> Option<u64> {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args([
        "--exact",
        "shard_crash_child",
        "--nocapture",
        "--test-threads",
        "1",
    ])
    .env(
        CHILD_ENV,
        format!(
            "{},{run},{}",
            dir.display(),
            match mode {
                Mode::Plain => "plain",
                Mode::Optimize => "optimize",
                Mode::Snapshot => "snapshot",
            }
        ),
    )
    .env_remove("NOCTURNUM_CRASH_AT")
    .stdout(Stdio::piped())
    .stderr(Stdio::inherit());
    if let Some(at) = crash_at {
        cmd.env("NOCTURNUM_CRASH_AT", at);
    }
    let mut child = cmd.spawn().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut last = None;
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if line.ends_with("ready") {
                let _ = ready_tx.send(());
            } else if let Some(n) = line.strip_prefix("ack ") {
                last = Some(n.trim().parse::<u64>().unwrap());
            }
        }
        last
    });
    match kill_after {
        Some(after) => {
            let _ = ready_rx.recv_timeout(Duration::from_secs(30));
            std::thread::sleep(after);
            child.kill().unwrap();
        }
        None => {
            let deadline = Instant::now() + Duration::from_secs(60);
            while child.try_wait().unwrap().is_none() {
                if Instant::now() > deadline {
                    child.kill().unwrap();
                    panic!("crash point {crash_at:?} was never reached");
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
    let status = child.wait().unwrap();
    assert!(
        status.code().is_none(),
        "the child failed on its own with {status}"
    );
    reader.join().unwrap()
}

fn contents(shard: &Shard) -> BTreeMap<PointId, Record> {
    let view = shard.view();
    let out: BTreeMap<_, _> = view
        .points()
        .map(|p| {
            (
                p.id,
                Record {
                    vector: p.vector.to_vec(),
                    payload: p.payload().unwrap().into_owned(),
                },
            )
        })
        .collect();
    assert_eq!(out.len(), view.len(), "a point was listed twice");
    out
}

/// Checks the reopened shard against the model after `run`, and moves the model forward to
/// whatever the shard actually kept.
fn verify(
    dir: &Path,
    model: &mut BTreeMap<PointId, Record>,
    run: u64,
    acked: Option<u64>,
    context: &str,
) {
    let (shard, _) = Shard::open(dir, opts(), None)
        .unwrap_or_else(|e| panic!("{context}: recovery failed: {e}"));
    let got = contents(&shard);
    for (id, rec) in &got {
        assert_eq!(
            shard.get(*id).unwrap().as_ref(),
            Some(rec),
            "{context}: get({id})"
        );
    }

    let applied = acked.map_or(0, |k| k + 1);
    let mut with_acked = model.clone();
    for counter in 0..applied {
        apply(&mut with_acked, &operation(run, counter));
    }
    if got == with_acked {
        *model = with_acked;
        return;
    }
    let mut with_in_flight = with_acked.clone();
    apply(&mut with_in_flight, &operation(run, applied));
    assert!(
        got == with_in_flight,
        "{context}: the shard holds neither the {applied} acknowledged operations nor those \
         plus the one in flight"
    );
    *model = with_in_flight;
}

#[test]
fn acknowledged_writes_survive_sigkill_across_flushes() {
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    eprintln!("shard crash seed {seed}");
    let mut rng = Rng::new(seed);
    let dir = TempDir::new().unwrap();
    let mut model = BTreeMap::new();
    let mut total = 0;
    for run in 0..20 {
        let after = Duration::from_micros(10_000 + rng.below(250_000));
        let acked = run_child(dir.path(), run, Some(after), None, Mode::Plain);
        total += acked.map_or(0, |k| k + 1);
        verify(dir.path(), &mut model, run, acked, &format!("run {run}"));
    }
    eprintln!("20 crashes, {total} acknowledged operations checked");
    assert!(total >= 100, "only {total} operations were acknowledged");
}

#[test]
fn every_flush_crash_point_recovers() {
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let points = [
        "segment.built_not_renamed:1",
        "segment.built_not_renamed:3",
        "shard.flush.segment_published:1",
        "shard.flush.segment_published:2",
        // The first manifest is written when the shard is created, so flushes start at 2.
        "manifest.tmp_written:2",
        "manifest.tmp_written:4",
        "shard.flush.manifest_written:1",
        "shard.flush.manifest_written:2",
        "wal.truncate.removed_unsynced:1",
    ];
    for point in points {
        let dir = TempDir::new().unwrap();
        let mut model = BTreeMap::new();
        // Run 0 crashes at the point, run 1 carries on from there and is killed, both must
        // recover to an acknowledged state.
        let acked = run_child(dir.path(), 0, None, Some(point), Mode::Plain);
        verify(dir.path(), &mut model, 0, acked, point);
        let acked = run_child(
            dir.path(),
            1,
            Some(Duration::from_millis(60)),
            None,
            Mode::Plain,
        );
        verify(
            dir.path(),
            &mut model,
            1,
            acked,
            &format!("{point}, next run"),
        );
    }
}

/// A crash while the shard is being created leaves no shard, nothing was acknowledged, and
/// creating it again works.
#[test]
fn a_crash_during_creation_leaves_no_shard() {
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new().unwrap();
    assert_eq!(
        run_child(
            dir.path(),
            0,
            None,
            Some("manifest.tmp_written:1"),
            Mode::Plain
        ),
        None
    );
    assert!(matches!(
        Shard::open(dir.path(), opts(), None),
        Err(nocturnum_storage::Error::Invalid(_))
    ));
    let (shard, _) = Shard::open(dir.path(), opts(), Some(config())).unwrap();
    assert!(shard.is_empty());
}

#[test]
fn acknowledged_writes_survive_sigkill_while_merging() {
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    eprintln!("merge crash seed {seed}");
    let mut rng = Rng::new(seed);
    let dir = TempDir::new().unwrap();
    let mut model = BTreeMap::new();
    let mut total = 0;
    for run in 0..20 {
        let after = Duration::from_micros(10_000 + rng.below(250_000));
        let acked = run_child(dir.path(), run, Some(after), None, Mode::Optimize);
        total += acked.map_or(0, |k| k + 1);
        verify(
            dir.path(),
            &mut model,
            run,
            acked,
            &format!("merging run {run}"),
        );
    }
    eprintln!("20 crashes while merging, {total} acknowledged operations checked");
    assert!(total >= 100, "only {total} operations were acknowledged");
}

#[test]
fn every_merge_crash_point_recovers() {
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let points = [
        "shard.merge.segment_built:1",
        "shard.merge.segment_built:4",
        "shard.merge.manifest_written:1",
        "shard.merge.manifest_written:4",
        "shard.merge.first_input_removed:1",
        "shard.merge.first_input_removed:3",
    ];
    for point in points {
        let dir = TempDir::new().unwrap();
        let mut model = BTreeMap::new();
        let acked = run_child(dir.path(), 0, None, Some(point), Mode::Optimize);
        verify(dir.path(), &mut model, 0, acked, point);
        let acked = run_child(
            dir.path(),
            1,
            Some(Duration::from_millis(60)),
            None,
            Mode::Optimize,
        );
        verify(
            dir.path(),
            &mut model,
            1,
            acked,
            &format!("{point}, next run"),
        );
    }
}

/// Every state the shard went through in one run, for matching snapshots against.
fn states_of_run(
    start: &BTreeMap<PointId, Record>,
    run: u64,
    acked: Option<u64>,
) -> Vec<BTreeMap<PointId, Record>> {
    let mut state = start.clone();
    let mut out = vec![state.clone()];
    for counter in 0..acked.map_or(1, |k| k + 2) {
        apply(&mut state, &operation(run, counter));
        out.push(state.clone());
    }
    out
}

/// The newest finished snapshot restores to some state the shard actually passed through.
fn check_newest_snapshot(snaps: &Path, history: &[BTreeMap<PointId, Record>], context: &str) {
    let Ok(entries) = std::fs::read_dir(snaps) else {
        return;
    };
    let mut finished: Vec<_> = entries
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "snapshot"))
        .filter(|p| p.with_extension("snapshot.sha256").exists())
        .collect();
    finished.sort();
    let Some(newest) = finished.last() else {
        return;
    };
    let target = snaps.join(format!("restored-{}", history.len()));
    restore(newest, &target).unwrap_or_else(|e| panic!("{context}: restore failed: {e}"));
    let (shard, _) = Shard::open(&target, opts(), None).unwrap();
    let got = contents(&shard);
    assert!(
        history.contains(&got),
        "{context}: the newest snapshot matches no state the shard passed through"
    );
}

#[test]
fn snapshots_survive_crashes_and_restore_to_a_real_state() {
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("shard");
    let snaps = tmp.path().join("shard-snaps");
    let mut model = BTreeMap::new();
    let mut history = Vec::new();
    let mut rng = Rng::new(7);
    for run in 0..8 {
        let start = model.clone();
        let (crash_at, kill) = if run % 2 == 0 {
            (Some("snapshot.written_not_renamed:2"), None)
        } else {
            (
                None,
                Some(Duration::from_micros(20_000 + rng.below(200_000))),
            )
        };
        let acked = run_child(&dir, run, kill, crash_at, Mode::Snapshot);
        verify(&dir, &mut model, run, acked, &format!("snapshot run {run}"));
        history.extend(states_of_run(&start, run, acked));
        check_newest_snapshot(&snaps, &history, &format!("snapshot run {run}"));
    }
    let restored = std::fs::read_dir(&snaps)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("restored")
        })
        .count();
    assert!(restored >= 6, "only {restored} snapshots were checked");
}
