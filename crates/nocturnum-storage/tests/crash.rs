//! Crash tests for the write-ahead log.
//!
//! Each test runs this same test binary again as a child process that writes to a log and
//! prints every acknowledgement it receives, then either kills the child with SIGKILL at a
//! random moment or lets it abort itself at a named crash point (see `src/crash.rs`). The
//! parent then reopens the log and checks that recovery succeeds and that everything the
//! child was told is safe really is there, whole and in order.
//!
//! What these tests can and cannot show is written up in `docs/storage/durability.md`.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nocturnum_storage::wal::PeriodicSync;
use nocturnum_storage::{Wal, WalOptions};
use tempfile::TempDir;

const CHILD_ENV: &str = "NOCTURNUM_CRASH_CHILD";

/// The crash tests take turns, because on macOS every `F_FULLFSYNC` flushes the whole drive
/// cache, so children of parallel tests mostly wait on each other's syncs.
static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn take_turn() -> std::sync::MutexGuard<'static, ()> {
    ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner())
}

fn opts() -> WalOptions {
    // Small segments, so crashes regularly land around rotation.
    WalOptions {
        segment_bytes: 4096,
        max_record_bytes: 1 << 20,
    }
}

/// How the child acknowledges a write before reporting it.
#[derive(Clone, Copy, Debug)]
enum Mode {
    /// Reports once `sync_to` returns, the `durable` level.
    Durable,
    /// Reports once `append` returns, with a background sync, the `applied` and `accepted`
    /// levels as far as the log is concerned.
    Appended,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Durable => "durable",
            Mode::Appended => "appended",
        }
    }
}

/// A record's payload says who wrote it, so the parent can check it independently of the
/// sequence number: run, thread and that thread's counter, then filler derived from them.
fn encode(run: u32, thread: u32, counter: u64) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&run.to_le_bytes());
    out.extend_from_slice(&thread.to_le_bytes());
    out.extend_from_slice(&counter.to_le_bytes());
    let filler = (counter % 97) as usize * 3;
    out.extend((0..filler).map(|i| (i as u64 ^ counter ^ u64::from(run)) as u8));
    out
}

fn decode(payload: &[u8]) -> (u32, u32, u64) {
    assert!(payload.len() >= 16, "payload too short: {}", payload.len());
    let run = u32::from_le_bytes(payload[0..4].try_into().unwrap());
    let thread = u32::from_le_bytes(payload[4..8].try_into().unwrap());
    let counter = u64::from_le_bytes(payload[8..16].try_into().unwrap());
    assert_eq!(
        payload,
        encode(run, thread, counter).as_slice(),
        "payload of run {run} thread {thread} counter {counter} is damaged"
    );
    (run, thread, counter)
}

/// The child's side. It is an ordinary test that does nothing unless the parent set
/// `NOCTURNUM_CRASH_CHILD`, and then writes until it is killed or hits its crash point.
#[test]
fn crash_child() {
    let Ok(spec) = std::env::var(CHILD_ENV) else {
        return;
    };
    let mut parts = spec.split(',');
    let dir = parts.next().unwrap().to_owned();
    let run: u32 = parts.next().unwrap().parse().unwrap();
    let mode = parts.next().unwrap().to_owned();
    let threads: u32 = parts.next().unwrap().parse().unwrap();
    let truncate = parts.next().unwrap() == "truncate";

    // Any failure in the child must reach the parent as an exit code, which a panic on a
    // writer thread alone would not.
    std::panic::set_hook(Box::new(|info| {
        eprintln!("crash child failed: {info}");
        std::process::exit(101);
    }));
    let (wal, _) = Wal::open(&dir, opts()).expect("the child opens the log");
    let wal = Arc::new(wal);
    let _sync =
        (mode == "appended").then(|| PeriodicSync::start(wal.clone(), Duration::from_millis(2)));
    println!("ready");

    let handles: Vec<_> = (0..threads)
        .map(|thread| {
            let wal = wal.clone();
            let mode = mode.clone();
            std::thread::spawn(move || {
                // Writes until the process is killed or aborts.
                let mut counter = 0u64;
                loop {
                    let seq = wal.append(&encode(run, thread, counter)).expect("append");
                    if mode == "durable" {
                        wal.sync_to(seq).expect("sync");
                    }
                    println!("ack {run} {thread} {counter} {seq}");
                    if truncate && thread == 0 && counter % 40 == 39 {
                        let to = wal.synced_seq().saturating_sub(30).max(1);
                        println!("trunc {to}");
                        wal.truncate_before(to).expect("truncate");
                    }
                    counter += 1;
                }
            })
        })
        .collect();
    // The writers never stop, a failing one ends the process through the panic hook, so
    // this only waits for the kill.
    let _writers = handles;
    loop {
        std::thread::park();
    }
}

struct Ack {
    run: u32,
    thread: u32,
    counter: u64,
}

/// What the parent learned from one child run.
#[derive(Default)]
struct Observed {
    acks: BTreeMap<u64, Ack>,
    max_truncate: u64,
}

/// When the parent kills a child that has no crash point.
#[derive(Clone, Copy)]
enum Kill {
    /// This long after spawning, often while it is still opening the log.
    DuringStartup(Duration),
    /// This long after it reports the log open.
    AfterReady(Duration),
}

struct Child<'a> {
    dir: &'a Path,
    run: u32,
    mode: Mode,
    threads: u32,
    truncate: bool,
    crash_at: Option<String>,
}

impl Child<'_> {
    /// Runs the child until it is killed or reaches its crash point, recording what it
    /// acknowledged.
    fn run(&self, kill_after: Kill, observed: &mut Observed) {
        let spec = format!(
            "{},{},{},{},{}",
            self.dir.display(),
            self.run,
            self.mode.as_str(),
            self.threads,
            if self.truncate { "truncate" } else { "plain" }
        );
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args([
            "--exact",
            "crash_child",
            "--nocapture",
            "--test-threads",
            "1",
        ])
        .env(CHILD_ENV, spec)
        .env_remove("NOCTURNUM_CRASH_AT")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
        if let Some(at) = &self.crash_at {
            cmd.env("NOCTURNUM_CRASH_AT", at);
        }
        let mut child = cmd.spawn().expect("spawning the child");
        let stdout = child.stdout.take().unwrap();

        // Read acknowledgements on a thread, so the kill can land at any moment.
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut lines = vec![];
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                // libtest prints "test crash_child ... " without a newline first, so the
                // marker arrives at the end of that line.
                if line.ends_with("ready") {
                    let _ = ready_tx.send(());
                }
                lines.push(line);
            }
            lines
        });

        match (&self.crash_at, kill_after) {
            (Some(at), _) => {
                // The child aborts itself, unless the point is never reached.
                let deadline = Instant::now() + Duration::from_secs(30);
                while child.try_wait().unwrap().is_none() {
                    if Instant::now() > deadline {
                        child.kill().unwrap();
                        panic!("crash point {at} was never reached");
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            (None, Kill::DuringStartup(after)) => {
                std::thread::sleep(after);
                child.kill().expect("killing the child");
            }
            (None, Kill::AfterReady(after)) => {
                // A child that crashes before it is ready shows up as a closed channel.
                let _ = ready_rx.recv_timeout(Duration::from_secs(30));
                std::thread::sleep(after);
                child.kill().expect("killing the child");
            }
        }
        let status = child.wait().unwrap();
        // Killed by a signal has no exit code, anything else means the child failed.
        assert!(
            status.code().is_none(),
            "the crash child failed on its own with {status}"
        );
        for line in reader.join().unwrap() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            match fields.as_slice() {
                ["ack", run, thread, counter, seq] => {
                    observed.acks.insert(
                        seq.parse().unwrap(),
                        Ack {
                            run: run.parse().unwrap(),
                            thread: thread.parse().unwrap(),
                            counter: counter.parse().unwrap(),
                        },
                    );
                }
                ["trunc", to] => {
                    observed.max_truncate = observed.max_truncate.max(to.parse().unwrap());
                }
                _ => {}
            }
        }
    }
}

/// Reopens the log after a crash and checks every promise, returning how many records it
/// holds.
fn verify(dir: &Path, observed: &Observed, context: &str) -> u64 {
    let (wal, rec) =
        Wal::open(dir, opts()).unwrap_or_else(|e| panic!("{context}: recovery failed: {e}"));
    let records: Vec<(u64, Vec<u8>)> = wal
        .read_from(wal.first_seq())
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap_or_else(|e| panic!("{context}: reading the recovered log failed: {e}"));

    // Sequence numbers run unbroken from the first record to the last.
    for (i, (seq, _)) in records.iter().enumerate() {
        assert_eq!(*seq, rec.first_seq + i as u64, "{context}: sequence gap");
    }
    assert_eq!(
        records.last().map_or(rec.first_seq - 1, |r| r.0),
        rec.last_seq,
        "{context}"
    );
    // Truncation never removes a segment holding a record at or after its boundary.
    if observed.max_truncate > 0 {
        assert!(
            rec.first_seq <= observed.max_truncate,
            "{context}: truncated too far"
        );
    } else {
        assert_eq!(rec.first_seq, 1, "{context}: records lost from the front");
    }

    // Each writer thread's records appear whole and in the order it wrote them, with no
    // holes, since a thread only writes its next record after the previous one returned.
    let by_seq: HashMap<u64, (u32, u32, u64)> =
        records.iter().map(|(s, p)| (*s, decode(p))).collect();
    let mut last_counter: HashMap<(u32, u32), u64> = HashMap::new();
    for (seq, _) in &records {
        let (run, thread, counter) = by_seq[seq];
        if let Some(prev) = last_counter.insert((run, thread), counter) {
            assert_eq!(
                counter,
                prev + 1,
                "{context}: thread {thread} of run {run} skipped or reordered"
            );
        }
    }

    // Everything acknowledged is there, unless truncation legitimately removed it.
    for (seq, ack) in &observed.acks {
        if *seq < rec.first_seq {
            continue;
        }
        let got = by_seq.get(seq).unwrap_or_else(|| {
            panic!(
                "{context}: acknowledged record {seq} is missing, the log ends at {}",
                rec.last_seq
            )
        });
        assert_eq!(
            *got,
            (ack.run, ack.thread, ack.counter),
            "{context}: record {seq} holds the wrong write"
        );
    }
    rec.last_seq
}

/// A small deterministic generator, seeded from the clock unless `NOCTURNUM_CRASH_SEED` is
/// set, so a failure can be replayed.
struct Rng(u64);

impl Rng {
    fn new() -> Rng {
        let seed = std::env::var("NOCTURNUM_CRASH_SEED")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos() as u64
            });
        eprintln!("crash test seed {seed}, rerun with NOCTURNUM_CRASH_SEED={seed}");
        Rng(seed | 1)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// Repeated kill and restart cycles on one log, with the given ack level.
fn kill_cycles(mode: Mode, cycles: u32, truncate: bool) {
    let _turn = take_turn();
    let mut rng = Rng::new();
    let dir = TempDir::new().unwrap();
    let mut observed = Observed::default();
    for run in 0..cycles {
        let child = Child {
            dir: dir.path(),
            run,
            mode,
            threads: 2 + rng.below(3) as u32,
            truncate,
            crash_at: None,
        };
        // One in five lands during startup, which can interrupt recovery, the rest 20 to
        // 200 ms into writing. A durable sync costs a few milliseconds on a Mac, so that is
        // anywhere from a handful to a few hundred acknowledged writes per crash.
        let kill = if rng.below(5) == 0 {
            Kill::DuringStartup(Duration::from_micros(rng.below(40_000)))
        } else {
            Kill::AfterReady(Duration::from_micros(20_000 + rng.below(180_000)))
        };
        child.run(kill, &mut observed);
        verify(dir.path(), &observed, &format!("{mode:?} run {run}"));
    }
    eprintln!(
        "{mode:?}: {cycles} crashes, {} acknowledged writes checked",
        observed.acks.len()
    );
    assert!(
        observed.acks.len() >= 100,
        "only {} writes were acknowledged, too few to mean much",
        observed.acks.len()
    );
}

#[test]
fn durable_acks_survive_sigkill() {
    kill_cycles(Mode::Durable, 25, false);
}

#[test]
fn durable_acks_survive_sigkill_during_truncation() {
    kill_cycles(Mode::Durable, 15, true);
}

/// Not a promise the API makes, `applied` and `accepted` may lose writes on a crash, but the
/// log hands every record to the OS before `append` returns, so a process crash alone loses
/// nothing. The test pins that down so a change to it is noticed.
#[test]
fn appended_records_survive_sigkill() {
    kill_cycles(Mode::Appended, 15, false);
}

/// Aborts the child at each named crash point, then checks recovery, then checks a normal
/// run carries on from there.
#[test]
fn recovery_handles_every_crash_point() {
    let _turn = take_turn();
    // (point, which hit, whether to truncate) chosen so each fires mid-run: segment points
    // are hit on rotation, every few dozen records with 4 KiB segments.
    let cases = [
        ("wal.append.torn", 1),
        ("wal.append.torn", 57),
        ("wal.segment.created", 1),
        ("wal.segment.half_header", 1),
        ("wal.segment.header_unsynced", 2),
        ("wal.truncate.removed_unsynced", 1),
        ("wal.truncate.removed_unsynced", 3),
    ];
    for (point, hit) in cases {
        let dir = TempDir::new().unwrap();
        let mut observed = Observed::default();
        let context = format!("{point}:{hit}");

        // A first run so the log already has segments and the crash lands mid-history.
        let base = |run, crash_at: Option<String>| Child {
            dir: dir.path(),
            run,
            mode: Mode::Durable,
            threads: 2,
            truncate: point.starts_with("wal.truncate"),
            crash_at,
        };
        base(0, None).run(Kill::AfterReady(Duration::from_millis(15)), &mut observed);
        verify(dir.path(), &observed, &format!("{context} warm-up"));

        base(1, Some(context.clone())).run(Kill::AfterReady(Duration::ZERO), &mut observed);
        verify(dir.path(), &observed, &context);

        base(2, None).run(Kill::AfterReady(Duration::from_millis(10)), &mut observed);
        let last = verify(dir.path(), &observed, &format!("{context} after restart"));
        assert!(
            observed.acks.keys().any(|s| *s > 0) && last > 0,
            "{context}: nothing written"
        );
    }
}

/// A crash while recovery is repairing a torn tail leaves a log the next recovery handles.
#[test]
fn a_crash_during_recovery_is_recoverable() {
    let _turn = take_turn();
    let dir = TempDir::new().unwrap();
    let mut observed = Observed::default();
    let child = |run, crash_at: &str| Child {
        dir: dir.path(),
        run,
        mode: Mode::Durable,
        threads: 1,
        truncate: false,
        crash_at: Some(crash_at.to_owned()),
    };
    // Leave a torn record, then crash right after recovery cuts it off but before it syncs.
    child(0, "wal.append.torn:20").run(Kill::AfterReady(Duration::ZERO), &mut observed);
    child(1, "wal.open.repaired_unsynced:1").run(Kill::AfterReady(Duration::ZERO), &mut observed);
    verify(dir.path(), &observed, "after a crash during recovery");
}
