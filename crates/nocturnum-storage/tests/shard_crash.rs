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
use nocturnum_storage::{
    GraphOptions, Operation, PointWrite, Record, Shard, ShardOptions, WalOptions,
};
use serde_json::json;
use tempfile::TempDir;

const CHILD_ENV: &str = "NOCTURNUM_SHARD_CRASH_CHILD";
const FLUSH_EVERY: u64 = 25;

static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

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
    let (dir, run) = spec.rsplit_once(',').unwrap();
    let run: u64 = run.parse().unwrap();
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
        counter += 1;
    }
}

/// Runs one child and returns the highest acknowledged counter, if any.
fn run_child(
    dir: &Path,
    run: u64,
    kill_after: Option<Duration>,
    crash_at: Option<&str>,
) -> Option<u64> {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args([
        "--exact",
        "shard_crash_child",
        "--nocapture",
        "--test-threads",
        "1",
    ])
    .env(CHILD_ENV, format!("{},{run}", dir.display()))
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
        let acked = run_child(dir.path(), run, Some(after), None);
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
        let acked = run_child(dir.path(), 0, None, Some(point));
        verify(dir.path(), &mut model, 0, acked, point);
        let acked = run_child(dir.path(), 1, Some(Duration::from_millis(60)), None);
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
        run_child(dir.path(), 0, None, Some("manifest.tmp_written:1")),
        None
    );
    assert!(matches!(
        Shard::open(dir.path(), opts(), None),
        Err(nocturnum_storage::Error::Invalid(_))
    ));
    let (shard, _) = Shard::open(dir.path(), opts(), Some(config())).unwrap();
    assert!(shard.is_empty());
}
