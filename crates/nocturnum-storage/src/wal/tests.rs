use std::sync::Arc;

use proptest::prelude::*;
use tempfile::TempDir;

use super::*;

fn small() -> WalOptions {
    WalOptions {
        segment_bytes: 256,
        max_record_bytes: 4096,
    }
}

fn payload(seq: u64) -> Vec<u8> {
    // Varying lengths, including empty, so records straddle buffer and segment boundaries.
    let len = (seq * 7 % 50) as usize;
    (0..len).map(|i| (seq as usize + i) as u8).collect()
}

fn read_all(wal: &Wal) -> Vec<(u64, Vec<u8>)> {
    wal.read_from(wal.first_seq())
        .unwrap()
        .collect::<Result<_>>()
        .unwrap()
}

fn segments(dir: &Path) -> Vec<PathBuf> {
    list_segments(dir)
        .unwrap()
        .into_iter()
        .map(|s| s.1)
        .collect()
}

fn copy_dir(from: &Path) -> TempDir {
    let to = TempDir::new().unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        fs::copy(entry.path(), to.path().join(entry.file_name())).unwrap();
    }
    to
}

fn write_n(dir: &Path, n: u64, opts: WalOptions) {
    let (wal, _) = Wal::open(dir, opts).unwrap();
    for seq in 1..=n {
        assert_eq!(wal.append(&payload(seq)).unwrap(), seq);
    }
    wal.sync_all().unwrap();
}

#[test]
fn empty_log_starts_at_one() {
    let dir = TempDir::new().unwrap();
    let (wal, rec) = Wal::open(dir.path(), small()).unwrap();
    assert_eq!(
        rec,
        Recovery {
            first_seq: 1,
            last_seq: 0,
            truncated_bytes: 0,
            removed_segments: vec![],
        }
    );
    assert_eq!(wal.last_seq(), 0);
    assert!(read_all(&wal).is_empty());
}

#[test]
fn records_survive_reopen_across_many_segments() {
    let dir = TempDir::new().unwrap();
    write_n(dir.path(), 100, small());
    assert!(segments(dir.path()).len() > 5);

    let (wal, rec) = Wal::open(dir.path(), small()).unwrap();
    assert_eq!(
        (rec.first_seq, rec.last_seq, rec.truncated_bytes),
        (1, 100, 0)
    );
    let all = read_all(&wal);
    assert_eq!(all.len(), 100);
    for (seq, data) in all {
        assert_eq!(data, payload(seq));
    }
    // Numbering carries on.
    assert_eq!(wal.append(b"next").unwrap(), 101);
}

#[test]
fn read_from_the_middle() {
    let dir = TempDir::new().unwrap();
    write_n(dir.path(), 60, small());
    let (wal, _) = Wal::open(dir.path(), small()).unwrap();
    let seqs: Vec<u64> = wal.read_from(37).unwrap().map(|r| r.unwrap().0).collect();
    assert_eq!(seqs, (37..=60).collect::<Vec<_>>());
    assert_eq!(wal.read_from(61).unwrap().count(), 0);
}

#[test]
fn a_torn_tail_is_cut_back_at_every_possible_byte() {
    let opts = WalOptions {
        segment_bytes: u64::MAX,
        ..small()
    };
    let orig = TempDir::new().unwrap();
    write_n(orig.path(), 12, opts);
    let file = segments(orig.path()).pop().unwrap();
    let full = fs::read(&file).unwrap();

    // Where each record ends, to know how many survive a cut.
    let mut ends = vec![];
    let mut at = HEADER_LEN as usize;
    for seq in 1..=12 {
        at += RECORD_HEADER_LEN + payload(seq).len();
        ends.push(at);
    }
    assert_eq!(at, full.len());

    for cut in HEADER_LEN as usize..full.len() {
        let dir = copy_dir(orig.path());
        let name = file.file_name().unwrap();
        fs::write(dir.path().join(name), &full[..cut]).unwrap();

        let (wal, rec) = Wal::open(dir.path(), opts).unwrap();
        let survivors = ends.iter().filter(|&&e| e <= cut).count() as u64;
        let valid = ends
            .iter()
            .copied()
            .rfind(|&e| e <= cut)
            .unwrap_or(HEADER_LEN as usize);
        assert_eq!(rec.last_seq, survivors, "cut at {cut}");
        assert_eq!(rec.truncated_bytes, (cut - valid) as u64, "cut at {cut}");
        assert_eq!(read_all(&wal).len() as u64, survivors);
        // Writing carries on cleanly after the repair.
        assert_eq!(wal.append(b"after").unwrap(), survivors + 1);
        drop(wal);
        let (wal, _) = Wal::open(dir.path(), opts).unwrap();
        assert_eq!(wal.last_seq(), survivors + 1);
    }
}

#[test]
fn garbage_after_the_last_record_is_removed() {
    let dir = TempDir::new().unwrap();
    write_n(dir.path(), 5, WalOptions::default());
    let file = segments(dir.path()).pop().unwrap();
    let mut f = OpenOptions::new().append(true).open(&file).unwrap();
    f.write_all(&[0xAB; 100]).unwrap();
    f.write_all(&[0; 4096]).unwrap();
    drop(f);
    let (wal, rec) = Wal::open(dir.path(), WalOptions::default()).unwrap();
    assert_eq!((rec.last_seq, rec.truncated_bytes), (5, 4196));
    assert_eq!(read_all(&wal).len(), 5);
}

#[test]
fn records_from_another_log_never_validate() {
    // Two logs with the same history differ only in their seeds, so splicing the tail of one
    // onto the other must not be accepted.
    let opts = WalOptions::default();
    let a = TempDir::new().unwrap();
    let b = TempDir::new().unwrap();
    write_n(a.path(), 3, opts);
    write_n(b.path(), 6, opts);
    let fa = segments(a.path()).pop().unwrap();
    let fb = fs::read(segments(b.path()).pop().unwrap()).unwrap();
    let a_len = fs::metadata(&fa).unwrap().len() as usize;
    // b's records 4 to 6 start where a ends, both logs wrote the same payloads.
    let mut f = OpenOptions::new().append(true).open(&fa).unwrap();
    f.write_all(&fb[a_len..]).unwrap();
    drop(f);

    let (wal, rec) = Wal::open(a.path(), opts).unwrap();
    assert_eq!(rec.last_seq, 3);
    assert_eq!(rec.truncated_bytes as usize, fb.len() - a_len);
    assert_eq!(read_all(&wal).len(), 3);
}

#[test]
fn damage_in_an_older_segment_is_an_error() {
    let dir = TempDir::new().unwrap();
    write_n(dir.path(), 60, small());
    let first = segments(dir.path()).remove(0);
    let mut bytes = fs::read(&first).unwrap();
    let i = bytes.len() - 3;
    bytes[i] ^= 0xFF;
    fs::write(&first, bytes).unwrap();
    match Wal::open(dir.path(), small()) {
        Err(Error::Corrupt { path, .. }) => assert_eq!(path, first),
        other => panic!("expected corruption, got {:?}", other.map(|r| r.1)),
    }
}

#[test]
fn a_newest_segment_without_a_header_is_removed() {
    let dir = TempDir::new().unwrap();
    write_n(dir.path(), 60, small());
    let (wal, _) = Wal::open(dir.path(), small()).unwrap();
    let next = wal.last_seq() + 1;
    drop(wal);
    // A crash while creating the next segment leaves a short header.
    let stray = segment_path(dir.path(), next);
    fs::write(&stray, &MAGIC[..5]).unwrap();

    let (wal, rec) = Wal::open(dir.path(), small()).unwrap();
    assert_eq!(rec.removed_segments, vec![stray]);
    assert_eq!(rec.last_seq, 60);
    assert_eq!(wal.append(b"x").unwrap(), 61);
}

#[test]
fn an_only_segment_without_a_header_keeps_its_numbering() {
    let dir = TempDir::new().unwrap();
    fs::write(segment_path(dir.path(), 500), b"").unwrap();
    let (wal, rec) = Wal::open(dir.path(), small()).unwrap();
    assert_eq!((rec.first_seq, rec.last_seq), (500, 499));
    assert_eq!(wal.append(b"x").unwrap(), 500);
}

#[test]
fn truncation_removes_old_segments() {
    let dir = TempDir::new().unwrap();
    write_n(dir.path(), 100, small());
    let (wal, _) = Wal::open(dir.path(), small()).unwrap();

    let removed = wal.truncate_before(50).unwrap();
    assert!(removed > 0);
    assert!(wal.first_seq() <= 50);
    let after = read_all(&wal);
    assert_eq!(after.last().unwrap().0, 100);
    assert!(after.iter().any(|r| r.0 == 50));
    // Truncating everything keeps the current segment.
    wal.truncate_before(u64::MAX).unwrap();
    assert_eq!(segments(dir.path()).len(), 1);
    drop(wal);
    let (wal, rec) = Wal::open(dir.path(), small()).unwrap();
    assert_eq!(rec.last_seq, 100);
    assert_eq!(read_all(&wal).last().unwrap().0, 100);
}

#[test]
fn a_gap_between_segments_is_corruption_and_nothing_is_removed() {
    let dir = TempDir::new().unwrap();
    write_n(dir.path(), 100, small());
    let before = segments(dir.path());
    fs::remove_file(&before[2]).unwrap();
    match Wal::open(dir.path(), small()) {
        Err(Error::Corrupt { path, reason, .. }) => {
            assert_eq!(path, before[3]);
            assert!(reason.contains("missing"), "{reason}");
        }
        other => panic!("expected corruption, got {:?}", other.map(|r| r.1)),
    }
    assert_eq!(segments(dir.path()).len(), before.len() - 1);
}

#[cfg(unix)]
#[test]
fn a_failed_removal_is_retried_by_the_next_truncation() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().unwrap();
    write_n(dir.path(), 100, small());
    let (wal, _) = Wal::open(dir.path(), small()).unwrap();
    let count = segments(dir.path()).len();

    let set_mode = |mode| {
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(mode)).unwrap();
    };
    set_mode(0o555);
    let failed = wal.truncate_before(u64::MAX);
    set_mode(0o755);
    assert!(matches!(failed, Err(Error::Io { .. })));
    assert_eq!(segments(dir.path()).len(), count);

    assert_eq!(wal.truncate_before(u64::MAX).unwrap(), count - 1);
    assert_eq!(segments(dir.path()).len(), 1);
}

#[test]
fn only_one_handle_may_open_a_log() {
    let dir = TempDir::new().unwrap();
    let (first, _) = Wal::open(dir.path(), small()).unwrap();
    assert!(matches!(
        Wal::open(dir.path(), small()),
        Err(Error::Locked { .. })
    ));
    drop(first);
    assert!(Wal::open(dir.path(), small()).is_ok());
}

#[test]
fn a_smaller_record_limit_never_cuts_existing_records() {
    let dir = TempDir::new().unwrap();
    let (wal, _) = Wal::open(dir.path(), small()).unwrap();
    wal.append(&[7; 100]).unwrap();
    wal.sync_all().unwrap();
    drop(wal);
    let tight = WalOptions {
        max_record_bytes: 64,
        ..small()
    };
    let (wal, rec) = Wal::open(dir.path(), tight).unwrap();
    assert_eq!((rec.last_seq, rec.truncated_bytes), (1, 0));
    assert_eq!(read_all(&wal), vec![(1, vec![7; 100])]);
}

#[test]
fn an_unknown_format_version_is_an_error_and_the_file_is_kept() {
    let dir = TempDir::new().unwrap();
    write_n(dir.path(), 3, small());
    let file = segments(dir.path()).pop().unwrap();
    let mut bytes = fs::read(&file).unwrap();
    bytes[8..12].copy_from_slice(&2u32.to_le_bytes());
    let crc = crc32c::crc32c(&bytes[0..24]);
    bytes[24..28].copy_from_slice(&crc.to_le_bytes());
    fs::write(&file, &bytes).unwrap();

    match Wal::open(dir.path(), small()) {
        Err(Error::Corrupt { reason, .. }) => assert!(reason.contains("version 2"), "{reason}"),
        other => panic!("expected an error, got {:?}", other.map(|r| r.1)),
    }
    assert_eq!(fs::read(&file).unwrap(), bytes);
}

#[test]
fn a_damaged_header_with_records_after_it_is_corruption() {
    let dir = TempDir::new().unwrap();
    write_n(dir.path(), 3, small());
    let file = segments(dir.path()).pop().unwrap();
    let mut bytes = fs::read(&file).unwrap();
    bytes[13] ^= 0xFF;
    fs::write(&file, &bytes).unwrap();
    assert!(matches!(
        Wal::open(dir.path(), small()),
        Err(Error::Corrupt { .. })
    ));
    assert!(file.exists());
}

#[test]
fn creates_missing_parent_directories() {
    let root = TempDir::new().unwrap();
    let dir = root.path().join("a/b/wal");
    let (wal, _) = Wal::open(&dir, small()).unwrap();
    wal.append(b"x").unwrap();
    wal.sync_all().unwrap();
    assert_eq!(segments(&dir).len(), 1);
}

#[test]
fn concurrent_writers_share_fsyncs() {
    let dir = TempDir::new().unwrap();
    let (wal, _) = Wal::open(dir.path(), WalOptions::default()).unwrap();
    let wal = Arc::new(wal);
    let threads: Vec<_> = (0..8)
        .map(|t| {
            let wal = wal.clone();
            std::thread::spawn(move || {
                for i in 0..100u32 {
                    let seq = wal.append(&[t, i as u8]).unwrap();
                    wal.sync_to(seq).unwrap();
                    assert!(wal.synced_seq() >= seq);
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    assert_eq!(wal.last_seq(), 800);
    assert_eq!(wal.synced_seq(), 800);
    // One fsync per write would be 800, group commit batches them.
    assert!(wal.sync_count() < 800, "{} fsyncs", wal.sync_count());
}

#[test]
fn a_failed_fsync_poisons_the_log() {
    let dir = TempDir::new().unwrap();
    let (wal, _) = Wal::open(dir.path(), small()).unwrap();
    let a = wal.append(b"a").unwrap();
    wal.sync_to(a).unwrap();
    let b = wal.append(b"b").unwrap();

    wal.inject_sync_failures(1);
    assert!(matches!(wal.sync_to(b), Err(Error::Io { .. })));
    assert!(wal.is_poisoned());
    // Never retried into a false success.
    assert!(matches!(wal.sync_to(b), Err(Error::Poisoned)));
    assert!(matches!(wal.append(b"c"), Err(Error::Poisoned)));
    // What was already durable stays reported as durable.
    assert!(wal.sync_to(a).is_ok());
    drop(wal);

    // A restart recovers what reached the OS.
    let (wal, rec) = Wal::open(dir.path(), small()).unwrap();
    assert_eq!(rec.last_seq, 2);
    assert!(!wal.is_poisoned());
}

#[test]
fn a_failed_fsync_while_rotating_poisons_the_log() {
    let dir = TempDir::new().unwrap();
    let (wal, _) = Wal::open(dir.path(), small()).unwrap();
    wal.append(&[0; 40]).unwrap();
    // Appends only fsync when a full segment is closed, so the failure lands there.
    wal.inject_sync_failures(1);
    let err = (0..10)
        .find_map(|_| wal.append(&[0; 40]).err())
        .expect("rotation should have hit the injected failure");
    assert!(matches!(err, Error::Io { .. }));
    assert!(wal.is_poisoned());
    assert_eq!(segments(dir.path()).len(), 1);
}

#[test]
fn limits_are_enforced() {
    let dir = TempDir::new().unwrap();
    let (wal, _) = Wal::open(dir.path(), small()).unwrap();
    assert!(matches!(
        wal.append(&vec![0; 4097]),
        Err(Error::RecordTooLarge { size: 4097, .. })
    ));
    assert!(matches!(
        wal.sync_to(1),
        Err(Error::NotWritten { seq: 1, last: 0 })
    ));
    // A record over the segment size still fits, in a segment of its own.
    wal.append(&[1; 1000]).unwrap();
    wal.append(&[2; 1000]).unwrap();
    assert_eq!(read_all(&wal).len(), 2);
}

#[test]
fn periodic_sync_catches_up() {
    let dir = TempDir::new().unwrap();
    let (wal, _) = Wal::open(dir.path(), small()).unwrap();
    let wal = Arc::new(wal);
    let _sync = PeriodicSync::start(wal.clone(), Duration::from_millis(5));
    let seq = wal.append(b"x").unwrap();
    for _ in 0..400 {
        if wal.synced_seq() >= seq {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("the background sync never ran");
}

#[test]
fn periodic_sync_stops_promptly_even_with_a_long_interval() {
    let dir = TempDir::new().unwrap();
    let (wal, _) = Wal::open(dir.path(), small()).unwrap();
    let sync = PeriodicSync::start(Arc::new(wal), Duration::from_secs(3600));
    let started = std::time::Instant::now();
    drop(sync);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[derive(Debug, Clone)]
enum Op {
    Append(usize),
    Sync,
    Reopen,
    Truncate(u8),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (0usize..200).prop_map(Op::Append),
        1 => Just(Op::Sync),
        1 => Just(Op::Reopen),
        1 => any::<u8>().prop_map(Op::Truncate),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Any mix of appends, syncs, reopens and truncations reads back exactly what a simple
    /// in-memory model holds.
    #[test]
    fn matches_a_simple_model(ops in proptest::collection::vec(op(), 1..80)) {
        let dir = TempDir::new().unwrap();
        let (mut wal, _) = Wal::open(dir.path(), small()).unwrap();
        let mut model: Vec<Vec<u8>> = vec![];
        let mut truncated_to = 1u64;
        for op in ops {
            match op {
                Op::Append(len) => {
                    let data: Vec<u8> = (0..len).map(|i| (i * 31 + model.len()) as u8).collect();
                    let seq = wal.append(&data).unwrap();
                    model.push(data);
                    prop_assert_eq!(seq, model.len() as u64);
                }
                Op::Sync => wal.sync_all().unwrap(),
                Op::Reopen => {
                    drop(wal);
                    let (w, rec) = Wal::open(dir.path(), small()).unwrap();
                    prop_assert_eq!(rec.truncated_bytes, 0);
                    prop_assert_eq!(rec.last_seq, model.len() as u64);
                    wal = w;
                }
                Op::Truncate(pct) => {
                    let to = (model.len() as u64 * pct as u64 / 255).max(1);
                    wal.truncate_before(to).unwrap();
                    truncated_to = truncated_to.max(to);
                }
            }
            let first = wal.first_seq();
            prop_assert!(first <= truncated_to.max(1));
            let got = read_all(&wal);
            let want: Vec<(u64, Vec<u8>)> = model
                .iter()
                .enumerate()
                .map(|(i, d)| (i as u64 + 1, d.clone()))
                .filter(|(s, _)| *s >= first)
                .collect();
            prop_assert_eq!(got, want);
        }
    }
}
