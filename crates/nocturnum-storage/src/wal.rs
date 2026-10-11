//! The write-ahead log.
//!
//! Every write to a collection becomes one record in the log before it touches anything else,
//! and after a crash the log is replayed to rebuild whatever had not reached the segments.
//!
//! # On disk
//!
//! The log is a directory of segment files named after the sequence number of their first
//! record, `00000000000000000001.wal` and so on. A segment starts with a 28-byte header:
//!
//! | bytes | field |
//! |---|---|
//! | 8 | magic `nocwal01` |
//! | 4 | format version, little endian |
//! | 4 | seed, random per file |
//! | 8 | sequence number of the first record |
//! | 4 | CRC32C of the 24 bytes above |
//!
//! Then records follow back to back, each with a 16-byte header and the payload:
//!
//! | bytes | field |
//! |---|---|
//! | 4 | payload length |
//! | 4 | CRC32C over the sequence number and payload, chained |
//! | 8 | sequence number |
//! | n | payload |
//!
//! The CRC of each record starts from the CRC of the record before it, and the first record
//! starts from the file's seed. A record therefore only checks out in its exact place in its
//! exact file, so leftover bytes from another file or an older run can never pass as valid.
//!
//! # Durability
//!
//! [`Wal::append`] writes the record to the operating system before returning, so it survives
//! the process crashing but not power loss. [`Wal::sync_to`] makes everything up to a sequence
//! number durable, and concurrent callers share one fsync (group commit): while one caller
//! syncs, the others wait and the next sync covers all of them.
//!
//! A full segment is fsynced before the next one is created, and a new segment's header and
//! directory entry are fsynced before any record goes into it, so only the newest segment can
//! ever have a torn end.
//!
//! Every fsync goes through one coordinator, so a failed fsync is published as poison before
//! any other fsync can run. If a write or an fsync fails the log refuses everything after,
//! because after a failed fsync the kernel may have dropped the dirty pages and a retry can
//! report success without the data being on disk. Restarting recovers from what is actually
//! there.
//!
//! On macOS a filesystem that cannot do `F_FULLFSYNC` fails every sync rather than quietly
//! settling for a weaker flush, so `durable` never means less than it says.
//!
//! One process at a time owns a log directory, enforced by an exclusive lock on its `LOCK`
//! file, since two writers would hand out the same sequence numbers.
//!
//! # Recovery
//!
//! [`Wal::open`] reads every segment and checks each record. The newest segment may end in a
//! torn or garbled record, left by a crash mid-write, and it is cut back to the last good
//! record. A newest segment holding nothing but an unwritten or half-written header, left by a
//! crash while it was being created, is removed.
//!
//! Anything else is reported as an error and left untouched for an operator to look at:
//! damage in an older segment, a gap in the sequence numbers between segments, a header that
//! is damaged with records after it, or a format version this build does not know.
//!
//! [`Wal::truncate_before`] removes segments oldest first and syncs the directory after each
//! removal, so an interrupted truncation always leaves an unbroken run of segments and a gap
//! can only mean something removed or damaged files from outside.

use std::fs::TryLockError;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::crash::crash_point;
use crate::error::{Error, IoContext, Result};
use crate::sync::{create_dir_all_durable, sync_dir, sync_file};

const MAGIC: [u8; 8] = *b"nocwal01";
const FORMAT_VERSION: u32 = 1;
const HEADER_LEN: u64 = 28;
const RECORD_HEADER_LEN: usize = 16;
const EXTENSION: &str = "wal";
/// The largest payload the format allows, whatever [`WalOptions::max_record_bytes`] says,
/// which also bounds what recovery will allocate for a damaged length field.
pub const MAX_RECORD_BYTES: u32 = 1 << 30;

#[derive(Debug, Clone, Copy)]
pub struct WalOptions {
    /// A segment that has grown past this many bytes is closed and a new one started.
    pub segment_bytes: u64,
    /// The largest payload one record may carry, capped at [`MAX_RECORD_BYTES`].
    pub max_record_bytes: u32,
}

impl Default for WalOptions {
    fn default() -> Self {
        WalOptions {
            segment_bytes: 32 * 1024 * 1024,
            max_record_bytes: 64 * 1024 * 1024,
        }
    }
}

/// What [`Wal::open`] found and repaired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recovery {
    /// The sequence number of the oldest record still in the log, or of the next record when
    /// the log is empty.
    pub first_seq: u64,
    /// The sequence number of the newest record, `first_seq - 1` when the log is empty.
    pub last_seq: u64,
    /// Bytes cut from the end of the newest segment because they did not form a valid record.
    pub truncated_bytes: u64,
    /// Segment files removed, which can only be a newest segment whose header was never fully
    /// written.
    pub removed_segments: Vec<PathBuf>,
}

struct Segment {
    path: PathBuf,
    first_seq: u64,
    file: Arc<File>,
    len: u64,
    prev_crc: u32,
}

struct Writer {
    current: Segment,
    last_seq: u64,
    /// Closed segments, oldest first, as `(first_seq, path)`.
    closed: Vec<(u64, PathBuf)>,
}

struct SyncState {
    synced: u64,
    in_progress: bool,
}

// Lock order: `writer`, then `flush`, then `sync`, and `truncation` before `writer`. Nothing
// takes `writer` while holding `flush` or `sync`.
pub struct Wal {
    dir: PathBuf,
    opts: WalOptions,
    writer: Mutex<Writer>,
    /// Held around every fsync of a segment together with publishing its outcome.
    flush: Mutex<()>,
    sync: Mutex<SyncState>,
    sync_done: Condvar,
    truncation: Mutex<()>,
    poisoned: AtomicBool,
    syncs: AtomicU64,
    /// Time spent in fsync, in nanoseconds.
    sync_nanos: AtomicU64,
    fail_syncs: AtomicU64,
    /// Holds the exclusive lock on the directory for as long as the log is open.
    _lock: File,
}

fn segment_path(dir: &Path, first_seq: u64) -> PathBuf {
    dir.join(format!("{first_seq:020}.{EXTENSION}"))
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic while holding a WAL lock cannot leave half-applied state behind, every field is
    // updated only after the step it records has succeeded, so the poison flag is ignored.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn record_crc(prev: u32, seq: u64, payload: &[u8]) -> u32 {
    crc32c::crc32c_append(crc32c::crc32c_append(prev, &seq.to_le_bytes()), payload)
}

fn encode_header(seed: u32, first_seq: u64) -> [u8; HEADER_LEN as usize] {
    let mut h = [0u8; HEADER_LEN as usize];
    h[0..8].copy_from_slice(&MAGIC);
    h[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    h[12..16].copy_from_slice(&seed.to_le_bytes());
    h[16..24].copy_from_slice(&first_seq.to_le_bytes());
    let crc = crc32c::crc32c(&h[0..24]);
    h[24..28].copy_from_slice(&crc.to_le_bytes());
    h
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}

/// Reads one segment's records in order, checking each.
struct SegmentReader {
    path: PathBuf,
    reader: BufReader<File>,
    /// The file's length when it was opened, no record may claim to run past it.
    file_len: u64,
    offset: u64,
    next_seq: u64,
    prev_crc: u32,
}

/// What a segment's header says about the file.
enum Header {
    Valid(SegmentReader),
    /// The header was never fully written. `has_records` tells whether bytes follow it, which
    /// a crash during creation cannot leave behind.
    Unwritten {
        has_records: bool,
        reason: String,
    },
}

/// Why reading a segment stopped early.
struct Damage {
    offset: u64,
    reason: String,
}

impl SegmentReader {
    fn open(path: &Path, first_seq: u64) -> Result<Header> {
        let file = File::open(path).ctx(|| format!("opening {}", path.display()))?;
        let file_len = file
            .metadata()
            .ctx(|| format!("reading the size of {}", path.display()))?
            .len();
        let mut reader = BufReader::with_capacity(256 * 1024, file);
        let mut h = [0u8; HEADER_LEN as usize];
        let got = read_full(&mut reader, &mut h).ctx(|| format!("reading {}", path.display()))?;
        let unwritten = |reason: &str| {
            Ok(Header::Unwritten {
                has_records: file_len > HEADER_LEN,
                reason: reason.into(),
            })
        };
        if got < h.len() {
            return unwritten("the header is incomplete");
        }
        if h[0..8] != MAGIC {
            return unwritten("the header does not start with the wal magic");
        }
        if u32_at(&h, 24) != crc32c::crc32c(&h[0..24]) {
            return unwritten("the header checksum does not match");
        }
        let corrupt = |reason: String| {
            Err(Error::Corrupt {
                path: path.to_owned(),
                offset: 0,
                reason,
            })
        };
        let version = u32_at(&h, 8);
        if version != FORMAT_VERSION {
            return corrupt(format!(
                "wal format version {version} is not supported by this build, which reads \
                 version {FORMAT_VERSION}"
            ));
        }
        if u64_at(&h, 16) != first_seq {
            return corrupt(format!(
                "the header starts at sequence {} but the file name says {first_seq}",
                u64_at(&h, 16)
            ));
        }
        Ok(Header::Valid(SegmentReader {
            path: path.to_owned(),
            reader,
            file_len,
            offset: HEADER_LEN,
            next_seq: first_seq,
            prev_crc: u32_at(&h, 12),
        }))
    }

    /// The next record, `Ok(Ok(None))` at a clean end and `Ok(Err(..))` at damage, after which
    /// the reader must not be used again.
    #[allow(clippy::type_complexity)]
    fn next(&mut self) -> Result<Result<Option<(u64, Vec<u8>)>, Damage>> {
        let at = self.offset;
        let damage = |reason: String| Ok(Err(Damage { offset: at, reason }));
        let mut h = [0u8; RECORD_HEADER_LEN];
        match read_full(&mut self.reader, &mut h)
            .ctx(|| format!("reading {}", self.path.display()))?
        {
            0 => return Ok(Ok(None)),
            n if n < RECORD_HEADER_LEN => return damage("a record header is cut short".into()),
            _ => {}
        }
        let len = u32_at(&h, 0);
        let crc = u32_at(&h, 4);
        let seq = u64_at(&h, 8);
        // Checked before allocating, so garbage cannot ask for gigabytes.
        if len > MAX_RECORD_BYTES {
            return damage(format!(
                "a record claims {len} bytes, over the format's limit"
            ));
        }
        if at + (RECORD_HEADER_LEN as u64) + u64::from(len) > self.file_len {
            return damage(format!(
                "a record claims {len} bytes, past the end of the file"
            ));
        }
        if seq != self.next_seq {
            return damage(format!(
                "expected sequence {} but found {seq}",
                self.next_seq
            ));
        }
        let mut payload = vec![0u8; len as usize];
        if read_full(&mut self.reader, &mut payload)
            .ctx(|| format!("reading {}", self.path.display()))?
            < payload.len()
        {
            return damage("a record's payload is cut short".into());
        }
        if record_crc(self.prev_crc, seq, &payload) != crc {
            return damage(format!("record {seq} fails its checksum"));
        }
        self.prev_crc = crc;
        self.next_seq += 1;
        self.offset += (RECORD_HEADER_LEN + payload.len()) as u64;
        Ok(Ok(Some((seq, payload))))
    }
}

/// Reads until `buf` is full or the file ends, returning how many bytes were read.
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// The result of reading a whole segment during recovery.
struct Scan {
    first_seq: u64,
    path: PathBuf,
    /// Set when the header was never fully written, with whether bytes follow it.
    unwritten: Option<(bool, String)>,
    /// Bytes up to the end of the last good record.
    valid_len: u64,
    /// The sequence number of the last good record, `first_seq - 1` when there is none.
    last_seq: u64,
    prev_crc: u32,
    damage: Option<Damage>,
}

fn scan_segment(path: &Path, first_seq: u64) -> Result<Scan> {
    let mut scan = Scan {
        first_seq,
        path: path.to_owned(),
        unwritten: None,
        valid_len: 0,
        last_seq: first_seq - 1,
        prev_crc: 0,
        damage: None,
    };
    let mut reader = match SegmentReader::open(path, first_seq)? {
        Header::Valid(r) => r,
        Header::Unwritten {
            has_records,
            reason,
        } => {
            scan.unwritten = Some((has_records, reason));
            return Ok(scan);
        }
    };
    loop {
        match reader.next()? {
            Ok(Some((seq, _))) => scan.last_seq = seq,
            Ok(None) => break,
            Err(d) => {
                scan.damage = Some(d);
                break;
            }
        }
    }
    scan.valid_len = reader.offset;
    scan.prev_crc = reader.prev_crc;
    Ok(scan)
}

pub(crate) fn list_segments(dir: &Path) -> Result<Vec<(u64, PathBuf)>> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).ctx(|| format!("listing {}", dir.display()))? {
        let entry = entry.ctx(|| format!("listing {}", dir.display()))?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some(EXTENSION) {
            continue;
        }
        let first_seq = path
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| s.len() == 20)
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|&n| n >= 1);
        if let Some(first_seq) = first_seq {
            found.push((first_seq, path));
        }
    }
    found.sort();
    Ok(found)
}

fn create_segment(dir: &Path, first_seq: u64) -> Result<Segment> {
    let path = segment_path(dir, first_seq);
    let seed = uuid::Uuid::new_v4().as_u128() as u32;
    let mut file = OpenOptions::new()
        .read(true)
        .append(true)
        .create_new(true)
        .open(&path)
        .ctx(|| format!("creating {}", path.display()))?;
    let header = encode_header(seed, first_seq);
    crash_point("wal.segment.created", || {});
    crash_point("wal.segment.half_header", || {
        let _ = (&file).write_all(&header[..HEADER_LEN as usize / 2]);
    });
    file.write_all(&header)
        .ctx(|| format!("writing the header of {}", path.display()))?;
    crash_point("wal.segment.header_unsynced", || {});
    sync_file(&file).ctx(|| format!("syncing {}", path.display()))?;
    sync_dir(dir).ctx(|| format!("syncing {}", dir.display()))?;
    Ok(Segment {
        path,
        first_seq,
        file: Arc::new(file),
        len: HEADER_LEN,
        prev_crc: seed,
    })
}

impl Wal {
    /// Opens the log in `dir`, creating it when missing, and repairs a torn end left by a
    /// crash. See the module docs for what recovery does.
    pub fn open(dir: impl AsRef<Path>, opts: WalOptions) -> Result<(Wal, Recovery)> {
        let dir = dir.as_ref().to_owned();
        create_dir_all_durable(&dir).ctx(|| format!("creating {}", dir.display()))?;

        let lock_path = dir.join("LOCK");
        let lock_file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .ctx(|| format!("opening {}", lock_path.display()))?;
        match lock_file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(Error::Locked { dir }),
            Err(TryLockError::Error(e)) => {
                return Err(e).ctx(|| format!("locking {}", lock_path.display()));
            }
        }

        let mut scans = Vec::new();
        for (first_seq, path) in list_segments(&dir)? {
            scans.push(scan_segment(&path, first_seq)?);
        }

        // A newest segment holding only a half-written header was being created when the
        // process stopped, nothing in it was ever acknowledged.
        let mut removed = Vec::new();
        let mut reuse_seq = None;
        if let Some(s) = scans.pop_if(|s| matches!(s.unwritten, Some((false, _)))) {
            fs::remove_file(&s.path).ctx(|| format!("removing {}", s.path.display()))?;
            sync_dir(&dir).ctx(|| format!("syncing {}", dir.display()))?;
            removed.push(s.path);
            reuse_seq = Some(s.first_seq);
        }

        let newest = scans.len().saturating_sub(1);
        for (i, s) in scans.iter().enumerate() {
            let corrupt = |offset: u64, reason: String| {
                Err(Error::Corrupt {
                    path: s.path.clone(),
                    offset,
                    reason,
                })
            };
            if let Some((_, reason)) = &s.unwritten {
                return corrupt(0, format!("{reason}, with records after it"));
            }
            // Damage before the newest segment hit records that were already durable.
            if let (Some(d), true) = (&s.damage, i < newest) {
                return corrupt(d.offset, d.reason.clone());
            }
            if i > 0 && s.first_seq != scans[i - 1].last_seq + 1 {
                return corrupt(
                    0,
                    format!(
                        "records {} to {} are missing before this segment",
                        scans[i - 1].last_seq + 1,
                        s.first_seq - 1
                    ),
                );
            }
        }

        let mut truncated_bytes = 0;
        let current = match scans.last() {
            Some(s) => {
                let file = OpenOptions::new()
                    .read(true)
                    .append(true)
                    .open(&s.path)
                    .ctx(|| format!("opening {}", s.path.display()))?;
                let on_disk = file
                    .metadata()
                    .ctx(|| format!("reading the size of {}", s.path.display()))?
                    .len();
                if on_disk > s.valid_len {
                    truncated_bytes = on_disk - s.valid_len;
                    file.set_len(s.valid_len)
                        .ctx(|| format!("truncating {}", s.path.display()))?;
                    crash_point("wal.open.repaired_unsynced", || {});
                }
                // What recovery read may only have been in the page cache, sync it so the
                // recovered state is durable before anything new is acknowledged.
                sync_file(&file).ctx(|| format!("syncing {}", s.path.display()))?;
                Segment {
                    path: s.path.clone(),
                    first_seq: s.first_seq,
                    file: Arc::new(file),
                    len: s.valid_len,
                    prev_crc: s.prev_crc,
                }
            }
            // Keep the numbering of a removed half-created segment.
            None => create_segment(&dir, reuse_seq.unwrap_or(1))?,
        };

        let last_seq = scans.last().map_or(current.first_seq - 1, |s| s.last_seq);
        let closed: Vec<(u64, PathBuf)> = scans[..newest.min(scans.len())]
            .iter()
            .map(|s| (s.first_seq, s.path.clone()))
            .collect();
        let first_seq = closed.first().map_or(current.first_seq, |c| c.0);

        let recovery = Recovery {
            first_seq,
            last_seq,
            truncated_bytes,
            removed_segments: removed,
        };
        let wal = Wal {
            dir,
            opts,
            writer: Mutex::new(Writer {
                current,
                last_seq,
                closed,
            }),
            flush: Mutex::new(()),
            sync: Mutex::new(SyncState {
                synced: last_seq,
                in_progress: false,
            }),
            sync_done: Condvar::new(),
            truncation: Mutex::new(()),
            poisoned: AtomicBool::new(false),
            syncs: AtomicU64::new(0),
            sync_nanos: AtomicU64::new(0),
            fail_syncs: AtomicU64::new(0),
            _lock: lock_file,
        };
        Ok((wal, recovery))
    }

    fn check_poisoned(&self) -> Result<()> {
        if self.poisoned.load(Ordering::Acquire) {
            Err(Error::Poisoned)
        } else {
            Ok(())
        }
    }

    fn poison(&self) {
        self.poisoned.store(true, Ordering::Release);
    }

    /// Fsyncs a segment and, on success, marks every record up to `upto` durable.
    ///
    /// The fsync and publishing its outcome happen under one lock, so once an fsync has
    /// failed no other fsync can run and report success for the same data.
    fn flush_segment(&self, file: &File, path: &Path, upto: u64) -> Result<()> {
        let _flush = lock(&self.flush);
        self.check_poisoned()?;
        let result = if self
            .fail_syncs
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
            .is_ok()
        {
            Err(std::io::Error::other("injected fsync failure"))
        } else {
            self.syncs.fetch_add(1, Ordering::Relaxed);
            let started = std::time::Instant::now();
            let r = sync_file(file);
            let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
            self.sync_nanos.fetch_add(nanos, Ordering::Relaxed);
            r
        };
        match result {
            Ok(()) => {
                let mut s = lock(&self.sync);
                s.synced = s.synced.max(upto);
                Ok(())
            }
            Err(e) => {
                self.poison();
                Err(e).ctx(|| format!("syncing {}", path.display()))
            }
        }
    }

    /// Appends one record and returns its sequence number.
    ///
    /// The record is handed to the operating system before this returns, so it survives the
    /// process crashing, and [`Wal::sync_to`] makes it survive power loss.
    pub fn append(&self, payload: &[u8]) -> Result<u64> {
        let max = self.opts.max_record_bytes.min(MAX_RECORD_BYTES);
        if payload.len() > max as usize {
            return Err(Error::RecordTooLarge {
                size: payload.len(),
                max,
            });
        }
        self.check_poisoned()?;
        let mut w = lock(&self.writer);
        // Checked again under the lock, a failure may have landed while waiting for it.
        self.check_poisoned()?;
        if w.current.len >= self.opts.segment_bytes && w.last_seq >= w.current.first_seq {
            self.rotate(&mut w).inspect_err(|_| self.poison())?;
        }

        let seq = w.last_seq + 1;
        let crc = record_crc(w.current.prev_crc, seq, payload);
        let mut buf = Vec::with_capacity(RECORD_HEADER_LEN + payload.len());
        buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        buf.extend_from_slice(&crc.to_le_bytes());
        buf.extend_from_slice(&seq.to_le_bytes());
        buf.extend_from_slice(payload);
        crash_point("wal.append.torn", || {
            let _ = (&*w.current.file).write_all(&buf[..buf.len() / 2]);
        });
        if let Err(e) = (&*w.current.file).write_all(&buf) {
            // Part of the record may be in the file now, recovery will cut it off.
            self.poison();
            return Err(e).ctx(|| format!("writing to {}", w.current.path.display()));
        }
        w.current.len += buf.len() as u64;
        w.current.prev_crc = crc;
        w.last_seq = seq;
        Ok(seq)
    }

    fn rotate(&self, w: &mut Writer) -> Result<()> {
        self.flush_segment(&w.current.file, &w.current.path, w.last_seq)?;
        let next = create_segment(&self.dir, w.last_seq + 1)?;
        let old = std::mem::replace(&mut w.current, next);
        w.closed.push((old.first_seq, old.path));
        Ok(())
    }

    /// Blocks until every record up to and including `seq` is durable.
    ///
    /// Concurrent callers share fsyncs. An error means the log is poisoned and the write may
    /// or may not have reached the disk, so it must not be acknowledged as durable.
    pub fn sync_to(&self, seq: u64) -> Result<()> {
        let last = self.last_seq();
        if seq > last {
            return Err(Error::NotWritten { seq, last });
        }
        let mut state = lock(&self.sync);
        loop {
            if state.synced >= seq {
                return Ok(());
            }
            self.check_poisoned()?;
            if state.in_progress {
                state = self
                    .sync_done
                    .wait(state)
                    .unwrap_or_else(|e| e.into_inner());
                continue;
            }
            state.in_progress = true;
            drop(state);

            // Every record up to `upto` is either in the current segment, already written to
            // the OS, or in a closed segment that was synced when it was closed.
            // If a rotation closes this segment before the fsync below, it fsynced the segment
            // itself, so syncing it again is harmless.
            let (file, path, upto) = {
                let w = lock(&self.writer);
                (w.current.file.clone(), w.current.path.clone(), w.last_seq)
            };
            let result = self.flush_segment(&file, &path, upto);

            state = lock(&self.sync);
            state.in_progress = false;
            self.sync_done.notify_all();
            result?;
        }
    }

    /// Makes everything written so far durable.
    pub fn sync_all(&self) -> Result<()> {
        self.sync_to(self.last_seq())
    }

    /// The sequence number of the newest record, 0 when nothing was ever written.
    pub fn last_seq(&self) -> u64 {
        lock(&self.writer).last_seq
    }

    /// Every record up to this sequence number is durable.
    pub fn synced_seq(&self) -> u64 {
        lock(&self.sync).synced
    }

    /// The sequence number of the oldest record still in the log.
    pub fn first_seq(&self) -> u64 {
        let w = lock(&self.writer);
        w.closed.first().map_or(w.current.first_seq, |c| c.0)
    }

    /// How many fsyncs the log has issued, which shows group commit at work.
    pub fn sync_count(&self) -> u64 {
        self.syncs.load(Ordering::Relaxed)
    }

    /// The total time fsyncs have taken.
    pub fn sync_time(&self) -> std::time::Duration {
        std::time::Duration::from_nanos(self.sync_nanos.load(Ordering::Relaxed))
    }

    /// The bytes the log takes on disk, counting closed segments not yet truncated.
    pub fn disk_bytes(&self) -> u64 {
        let (current, closed) = {
            let w = lock(&self.writer);
            let closed: Vec<PathBuf> = w.closed.iter().map(|c| c.1.clone()).collect();
            (w.current.len, closed)
        };
        // A closed segment truncated since the list was taken has no size left to count.
        closed
            .iter()
            .filter_map(|p| fs::metadata(p).ok())
            .fold(current, |sum, m| sum + m.len())
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Acquire)
    }

    /// Makes the next `n` fsyncs fail, for fault-injection tests.
    #[doc(hidden)]
    pub fn inject_sync_failures(&self, n: u64) {
        self.fail_syncs.store(n, Ordering::Release);
    }

    /// Removes closed segments holding only records older than `seq`.
    ///
    /// The caller passes a sequence number whose effects are already durable elsewhere, so
    /// the records before it are no longer needed for replay. The current segment is never
    /// removed, so some older records may stay. Returns how many segments were removed.
    ///
    /// Segments go oldest first, each removal synced before the next, and one that fails to
    /// go stays listed so a later call retries it.
    pub fn truncate_before(&self, seq: u64) -> Result<usize> {
        let _one_at_a_time = lock(&self.truncation);
        let doomed: Vec<PathBuf> = {
            let w = lock(&self.writer);
            // A closed segment ends right before the next one starts.
            let starts = w
                .closed
                .iter()
                .skip(1)
                .map(|c| c.0)
                .chain([w.current.first_seq]);
            w.closed
                .iter()
                .zip(starts)
                .take_while(|(_, next)| *next <= seq)
                .map(|(c, _)| c.1.clone())
                .collect()
        };
        for path in &doomed {
            // Already gone means an earlier call removed it and then failed to sync the
            // directory, the sync below finishes that job.
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(e).ctx(|| format!("removing {}", path.display())),
            }
            crash_point("wal.truncate.removed_unsynced", || {});
            sync_dir(&self.dir).ctx(|| format!("syncing {}", self.dir.display()))?;
            let mut w = lock(&self.writer);
            debug_assert_eq!(w.closed.first().map(|c| &c.1), Some(path));
            w.closed.remove(0);
        }
        Ok(doomed.len())
    }

    /// Reads every record from `from` up to the newest one written when this is called.
    ///
    /// Meant for replay at startup, before writes begin, and for snapshots, which must keep
    /// [`Wal::truncate_before`] from removing segments while they read.
    pub fn read_from(&self, from: u64) -> Result<WalIter> {
        let w = lock(&self.writer);
        let mut segments: Vec<(u64, PathBuf)> = w.closed.clone();
        segments.push((w.current.first_seq, w.current.path.clone()));
        // Skip segments that end before `from`.
        let skip = segments
            .windows(2)
            .take_while(|pair| pair[1].0 <= from)
            .count();
        segments.drain(..skip);
        Ok(WalIter {
            segments: segments.into_iter(),
            reader: None,
            from,
            last: w.last_seq,
            done: false,
        })
    }
}

/// Records from [`Wal::read_from`], each as `(seq, payload)`.
pub struct WalIter {
    segments: std::vec::IntoIter<(u64, PathBuf)>,
    reader: Option<SegmentReader>,
    from: u64,
    last: u64,
    done: bool,
}

impl WalIter {
    fn advance(&mut self) -> Result<Option<(u64, Vec<u8>)>> {
        loop {
            if self.reader.is_none() {
                let Some((first_seq, path)) = self.segments.next() else {
                    return Ok(None);
                };
                if first_seq > self.last {
                    return Ok(None);
                }
                match SegmentReader::open(&path, first_seq)? {
                    Header::Valid(r) => self.reader = Some(r),
                    Header::Unwritten { reason, .. } => {
                        return Err(Error::Corrupt {
                            path,
                            offset: 0,
                            reason,
                        });
                    }
                }
            }
            let reader = self.reader.as_mut().expect("set above");
            // Stop before a record that may still be half written by a concurrent append.
            if reader.next_seq > self.last {
                return Ok(None);
            }
            match reader.next()? {
                Ok(Some((seq, _))) if seq < self.from => continue,
                Ok(Some((seq, _))) if seq > self.last => return Ok(None),
                Ok(Some(record)) => return Ok(Some(record)),
                Ok(None) => self.reader = None,
                Err(d) => {
                    return Err(Error::Corrupt {
                        path: reader.path.clone(),
                        offset: d.offset,
                        reason: d.reason,
                    });
                }
            }
        }
    }
}

impl Iterator for WalIter {
    type Item = Result<(u64, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let item = self.advance().transpose();
        if !matches!(item, Some(Ok(_))) {
            self.done = true;
        }
        item
    }
}

/// Syncs the log on a timer, which bounds how much `applied` writes can lose on power loss.
/// Stops when dropped.
pub struct PeriodicSync {
    stop: Arc<(Mutex<bool>, Condvar)>,
    handle: Option<JoinHandle<()>>,
}

impl PeriodicSync {
    pub fn start(wal: Arc<Wal>, every: Duration) -> PeriodicSync {
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let signal = stop.clone();
        let handle = std::thread::Builder::new()
            .name("wal-sync".into())
            .spawn(move || {
                let (flag, cv) = &*signal;
                loop {
                    // Checks the flag before every wait, so a stop sent while a sync was
                    // running is never missed.
                    let stopped = cv
                        .wait_timeout_while(lock(flag), every, |stop| !*stop)
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                    if *stopped {
                        return;
                    }
                    drop(stopped);
                    let last = wal.last_seq();
                    // A failure poisons the log and every writer sees it, nothing more to do.
                    if last > wal.synced_seq() && wal.sync_to(last).is_err() {
                        return;
                    }
                }
            })
            .expect("spawning the wal sync thread");
        PeriodicSync {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for PeriodicSync {
    fn drop(&mut self) {
        *lock(&self.stop.0) = true;
        self.stop.1.notify_all();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests;
