//! A shard: one WAL, a memtable and immutable segments, with a manifest tying them together.
//!
//! # Layout
//!
//! ```text
//! <shard dir>/
//!   MANIFEST            live segments and the WAL watermark, see crate::manifest
//!   wal/                the write-ahead log, see crate::wal
//!   segments/seg-<id>/  immutable segments, see crate::segment
//!   segments/tmp-<id>/  a segment being built, removed at startup if left behind
//! ```
//!
//! # Writes
//!
//! A write is checked, logged to the WAL and applied to the in-memory memtable under one
//! lock, so the order in the log is the order of application. A `durable` write then waits
//! for the WAL fsync outside that lock, sharing it with concurrent writers.
//!
//! A write that changes part of a point in an older layer, such as setting one payload key,
//! copies the point into the memtable with the change applied, and the old copy is hidden.
//! A deletion of a point that exists is recorded as a deletion entry, because older layers
//! may still hold copies of it.
//!
//! # Flushes
//!
//! The memtable lives only in RAM, and the WAL is what makes it durable. A flush freezes the
//! memtable, writes it out as a new segment beside the live ones, publishes a manifest that
//! lists the segment and records the WAL sequence number it covers, and only then truncates
//! the WAL up to that number. Writes carry on into a fresh memtable while a flush runs.
//!
//! # Merges
//!
//! Every flush adds a small segment, so the optimizer merges adjacent segments and rewrites
//! ones with many dead rows, building aside and swapping them in through the manifest, see
//! the `merge` submodule.
//!
//! # Recovery
//!
//! Opening a shard reads the manifest, removes every segment directory it does not list
//! (built but never published), loads the listed segments, and replays the WAL from the
//! record after the manifest's watermark.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;

use nocturnum_core::{Ack, Payload, PayloadIndexSpec, PointId, VectorConfig};
use nocturnum_index::{Hnsw, PayloadIndex};
use roaring::RoaringBitmap;

use crate::crash::crash_point;
use crate::error::{Error, IoContext, Result};
use crate::manifest::Manifest;
use crate::op::{Operation, PointWrite};
use crate::segment::{
    Entry, EntryIn, GraphOptions, PayloadIn, Segment, segment_dir_name, write_segment,
};
use crate::sync::{create_dir_all_durable, sync_dir};
use crate::wal::{PeriodicSync, Wal, WalOptions};

#[derive(Debug, Clone, Copy)]
pub struct ShardOptions {
    pub wal: WalOptions,
    /// [`Shard::needs_flush`] turns true once the memtable has taken in this many bytes of
    /// logged operations.
    pub flush_bytes: usize,
    /// How often the WAL is synced in the background, which bounds what `applied` and
    /// `accepted` writes can lose on power loss.
    pub background_sync: Duration,
    /// Which new segments get a graph index.
    pub graph: GraphOptions,
}

impl Default for ShardOptions {
    fn default() -> Self {
        ShardOptions {
            wal: WalOptions::default(),
            flush_bytes: 32 * 1024 * 1024,
            background_sync: Duration::from_secs(1),
            graph: GraphOptions::default(),
        }
    }
}

/// A point as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub vector: Vec<f32>,
    pub payload: Payload,
}

#[derive(Debug, Clone)]
struct MemEntry {
    version: u64,
    /// `None` marks a deletion.
    value: Option<Record>,
}

#[derive(Debug, Default)]
struct Memtable {
    entries: BTreeMap<PointId, MemEntry>,
    bytes: usize,
}

/// Where a point's newest copy lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Loc {
    /// In the memtable, or in the frozen memtable when the memtable has no entry for it.
    Recent,
    Segment {
        id: u64,
        row: u32,
    },
}

struct Slot {
    segment: Arc<Segment>,
    /// Rows that hold their point's newest copy.
    live: RoaringBitmap,
    /// Each row's entry, by row.
    rows: Vec<Entry>,
    payload_index: Arc<PayloadIndex>,
}

impl Slot {
    /// A slot for a freshly loaded segment, with no rows live yet.
    fn new(segment: Arc<Segment>, indexes: &[PayloadIndexSpec]) -> Result<Slot> {
        let rows = segment_rows(&segment);
        let payload_index = Arc::new(build_payload_index(&segment, &rows, indexes)?);
        Ok(Slot {
            segment,
            live: RoaringBitmap::new(),
            rows,
            payload_index,
        })
    }

    fn set_live(&mut self, row: u32, on: bool) {
        if on {
            self.live.insert(row);
        } else {
            self.live.remove(row);
        }
    }
}

/// A segment's entries by row, deletions left out.
fn segment_rows(segment: &Segment) -> Vec<Entry> {
    let mut rows: Vec<Entry> = segment
        .entries()
        .iter()
        .filter(|e| e.row.is_some())
        .copied()
        .collect();
    rows.sort_by_key(|e| e.row);
    rows
}

fn build_payload_index(
    segment: &Segment,
    rows: &[Entry],
    indexes: &[PayloadIndexSpec],
) -> Result<PayloadIndex> {
    let schema: Vec<_> = indexes.iter().map(|s| (s.path.clone(), s.kind)).collect();
    PayloadIndex::build(
        rows.len() as u32,
        &schema,
        rows.iter().map(|e| (e.row.expect("rows have rows"), e.id)),
        |row| segment.payload(row),
    )
}

struct State {
    mem: Memtable,
    /// A memtable being written out by a flush, still readable until its segment is live.
    frozen: Option<Arc<Memtable>>,
    /// The last sequence number held by the frozen memtable.
    frozen_upto: u64,
    /// Oldest first, in manifest order.
    slots: Vec<Slot>,
    index: HashMap<PointId, Loc>,
    last_applied: u64,
    /// The manifest's list of payload indexes, kept here too so reading it never waits for
    /// the flush lock, which a flush holds for as long as it takes to write a segment.
    payload_indexes: Vec<PayloadIndexSpec>,
}

impl State {
    fn slot_mut(&mut self, id: u64) -> &mut Slot {
        self.slots
            .iter_mut()
            .find(|s| s.segment.id() == id)
            .expect("the index only points at loaded segments")
    }

    /// Records that `id`'s newest copy is now at `to`, or that it is deleted.
    fn relocate(&mut self, id: PointId, to: Option<Loc>) {
        let old = match to {
            Some(loc) => self.index.insert(id, loc),
            None => self.index.remove(&id),
        };
        if let Some(Loc::Segment { id: seg, row }) = old {
            self.slot_mut(seg).set_live(row, false);
        }
    }

    fn get(&self, id: PointId) -> Result<Option<Record>> {
        match self.index.get(&id) {
            None => Ok(None),
            Some(Loc::Recent) => {
                let entry = self
                    .mem
                    .entries
                    .get(&id)
                    .or_else(|| self.frozen.as_ref().and_then(|f| f.entries.get(&id)))
                    .expect("a recent point is in the memtable or the frozen one");
                Ok(entry.value.clone())
            }
            Some(&Loc::Segment { id: seg, row }) => {
                let slot = self
                    .slots
                    .iter()
                    .find(|s| s.segment.id() == seg)
                    .expect("the index only points at loaded segments");
                Ok(Some(Record {
                    vector: slot.segment.vector(row).to_vec(),
                    payload: slot.segment.payload(row)?,
                }))
            }
        }
    }

    /// Works out what an operation does to the memtable, reading the current points it
    /// changes. Doing this before logging means a failed read never leaves a logged but
    /// unapplied operation behind.
    fn plan(&self, op: &Operation) -> Result<Vec<(PointId, Option<Record>)>> {
        let mut out = Vec::new();
        match op {
            Operation::Upsert(points) => {
                for p in points {
                    out.push((
                        p.id,
                        Some(Record {
                            vector: p.vector.clone(),
                            payload: p.payload.clone(),
                        }),
                    ));
                }
            }
            Operation::Delete(ids) => {
                for id in ids {
                    if self.index.contains_key(id) {
                        out.push((*id, None));
                    }
                }
            }
            Operation::SetPayload { ids, payload }
            | Operation::OverwritePayload { ids, payload } => {
                let overwrite = matches!(op, Operation::OverwritePayload { .. });
                for id in ids {
                    if let Some(mut rec) = self.current(*id, &out)? {
                        if overwrite {
                            rec.payload = payload.clone();
                        } else {
                            for (k, v) in &payload.0 {
                                rec.payload.0.insert(k.clone(), v.clone());
                            }
                        }
                        out.push((*id, Some(rec)));
                    }
                }
            }
            Operation::DeletePayloadKeys { ids, keys } => {
                for id in ids {
                    if let Some(mut rec) = self.current(*id, &out)? {
                        for k in keys {
                            rec.payload.0.remove(k);
                        }
                        out.push((*id, Some(rec)));
                    }
                }
            }
            Operation::ClearPayload(ids) => {
                for id in ids {
                    if let Some(mut rec) = self.current(*id, &out)? {
                        rec.payload = Payload::new();
                        out.push((*id, Some(rec)));
                    }
                }
            }
        }
        Ok(out)
    }

    /// A point as it stands, including changes planned earlier in the same operation, which
    /// matters when one operation names an id twice.
    fn current(
        &self,
        id: PointId,
        planned: &[(PointId, Option<Record>)],
    ) -> Result<Option<Record>> {
        match planned.iter().rev().find(|(p, _)| *p == id) {
            Some((_, rec)) => Ok(rec.clone()),
            None => self.get(id),
        }
    }

    fn install(&mut self, seq: u64, changes: Vec<(PointId, Option<Record>)>, bytes: usize) {
        for (id, value) in changes {
            let loc = value.as_ref().map(|_| Loc::Recent);
            self.mem.entries.insert(
                id,
                MemEntry {
                    version: seq,
                    value,
                },
            );
            self.relocate(id, loc);
        }
        self.mem.bytes += bytes;
        self.last_applied = seq;
    }
}

/// One shard of a collection.
pub struct Shard {
    dir: PathBuf,
    opts: ShardOptions,
    config: VectorConfig,
    wal: Arc<Wal>,
    state: RwLock<State>,
    flush: Mutex<Manifest>,
    flush_broken: AtomicBool,
    /// Held for a whole merge, so merges take turns.
    merging: Mutex<()>,
    optimizer_status: Mutex<OptimizerStatus>,
    _sync: PeriodicSync,
}

/// What [`Shard::open`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardRecovery {
    pub wal: crate::wal::Recovery,
    /// Operations replayed from the WAL into the memtable.
    pub replayed: u64,
    /// Segment directories removed because no manifest listed them.
    pub removed_segments: Vec<PathBuf>,
}

fn read<T>(l: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(|e| e.into_inner())
}

fn write<T>(l: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(|e| e.into_inner())
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shard {
    /// Opens the shard in `dir`, creating it with `create` when it does not exist yet.
    pub fn open(
        dir: impl AsRef<Path>,
        opts: ShardOptions,
        create: Option<VectorConfig>,
    ) -> Result<(Shard, ShardRecovery)> {
        let dir = dir.as_ref().to_owned();
        create_dir_all_durable(&dir).ctx(|| format!("creating {}", dir.display()))?;
        // The WAL's lock guards the whole shard directory, so it is taken first.
        let (wal, wal_recovery) = Wal::open(dir.join("wal"), opts.wal)?;
        let invalid = |reason: String| Error::Invalid(format!("{}: {reason}", dir.display()));
        let corrupt = |reason: String| Error::Corrupt {
            path: dir.clone(),
            offset: 0,
            reason,
        };

        let manifest = match (Manifest::read(&dir)?, create) {
            (Some(m), Some(c)) if m.vectors != c => {
                return Err(invalid(format!(
                    "the shard holds {:?} vectors, not {c:?}",
                    m.vectors
                )));
            }
            (Some(m), _) => m,
            (None, Some(c)) => {
                c.validate().map_err(|e| invalid(e.to_string()))?;
                if wal_recovery.last_seq > 0 {
                    return Err(corrupt(
                        "the WAL has records but there is no manifest".into(),
                    ));
                }
                let m = Manifest::new(c);
                m.write(&dir)?;
                m
            }
            (None, None) => return Err(invalid("there is no shard here".into())),
        };
        let config = manifest.vectors;

        // The manifest covers WAL records up to its watermark, and replay needs everything
        // after it, so neither end may be missing.
        if wal_recovery.last_seq < manifest.wal_upto {
            return Err(corrupt(format!(
                "the manifest covers WAL records up to {} but the WAL ends at {}",
                manifest.wal_upto, wal_recovery.last_seq
            )));
        }
        if wal_recovery.first_seq > manifest.wal_upto + 1 {
            return Err(corrupt(format!(
                "replay needs WAL records from {} but the WAL starts at {}",
                manifest.wal_upto + 1,
                wal_recovery.first_seq
            )));
        }

        let seg_root = dir.join("segments");
        create_dir_all_durable(&seg_root).ctx(|| format!("creating {}", seg_root.display()))?;
        let listed: Vec<String> = manifest
            .segments
            .iter()
            .map(|id| segment_dir_name(*id))
            .collect();
        let mut removed = Vec::new();
        for entry in fs::read_dir(&seg_root).ctx(|| format!("listing {}", seg_root.display()))? {
            let path = entry
                .ctx(|| format!("listing {}", seg_root.display()))?
                .path();
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
            if name.is_some_and(|n| !listed.contains(&n)) {
                fs::remove_dir_all(&path).ctx(|| format!("removing {}", path.display()))?;
                removed.push(path);
            }
        }
        if !removed.is_empty() {
            sync_dir(&seg_root).ctx(|| format!("syncing {}", seg_root.display()))?;
        }

        let mut state = State {
            mem: Memtable::default(),
            frozen: None,
            frozen_upto: 0,
            slots: Vec::new(),
            index: HashMap::new(),
            last_applied: manifest.wal_upto,
            payload_indexes: manifest.payload_indexes.clone(),
        };
        for (id, name) in manifest.segments.iter().zip(&listed) {
            let segment = Segment::open(&seg_root.join(name), *id, config)?;
            state
                .slots
                .push(Slot::new(Arc::new(segment), &manifest.payload_indexes)?);
            let slot_index = state.slots.len() - 1;
            let entries: Vec<_> = state.slots[slot_index].segment.entries().to_vec();
            for e in entries {
                let loc = e.row.map(|row| Loc::Segment { id: *id, row });
                state.relocate(e.id, loc);
                if let Some(row) = e.row {
                    state.slots[slot_index].set_live(row, true);
                }
            }
        }

        let mut replayed = 0;
        for record in wal.read_from(manifest.wal_upto + 1)? {
            let (seq, bytes) = record?;
            let op = Operation::decode(&bytes).map_err(|e| Error::Corrupt {
                path: dir.join("wal"),
                offset: seq,
                reason: format!("record {seq} does not decode: {}", e.0),
            })?;
            let changes = state.plan(&op)?;
            state.install(seq, changes, bytes.len());
            replayed += 1;
        }

        let wal = Arc::new(wal);
        let shard = Shard {
            _sync: PeriodicSync::start(wal.clone(), opts.background_sync),
            dir,
            opts,
            config,
            wal,
            state: RwLock::new(state),
            flush: Mutex::new(manifest),
            flush_broken: AtomicBool::new(false),
            merging: Mutex::new(()),
            optimizer_status: Mutex::new(OptimizerStatus::default()),
        };
        let recovery = ShardRecovery {
            wal: wal_recovery,
            replayed,
            removed_segments: removed,
        };
        Ok((shard, recovery))
    }

    pub fn config(&self) -> VectorConfig {
        self.config
    }

    /// Checks, logs and applies an operation, returning its sequence number once it has
    /// reached `ack`.
    pub fn write(&self, op: Operation, ack: Ack) -> Result<u64, WriteError> {
        let op = self.check(op)?;
        self.commit(ack, |_| Ok(op))
    }

    /// Like [`Shard::write`] for an operation that depends on the current points, such as a
    /// deletion by filter: `build` sees a view of the shard and returns the operation, and
    /// both happen under the write lock, so no other write lands in between.
    pub fn write_with(
        &self,
        ack: Ack,
        build: impl FnOnce(&View<'_>) -> Result<Operation>,
    ) -> Result<u64, WriteError> {
        self.commit(ack, |view| self.check(build(view)?))
    }

    /// Validates payloads and prepares vectors, which must happen before an operation is
    /// logged.
    fn check(&self, mut op: Operation) -> Result<Operation, WriteError> {
        match &op {
            Operation::Upsert(points) => {
                for p in points {
                    p.payload.validate().map_err(WriteError::Invalid)?;
                }
            }
            Operation::SetPayload { payload, .. } | Operation::OverwritePayload { payload, .. } => {
                payload.validate().map_err(WriteError::Invalid)?;
            }
            _ => {}
        }
        if let Operation::Upsert(points) = &mut op {
            for PointWrite { vector, .. } in points.iter_mut() {
                *vector = self
                    .config
                    .distance
                    .prepare(self.config.dim, std::mem::take(vector))
                    .map_err(WriteError::Invalid)?;
            }
        }
        Ok(op)
    }

    fn commit(
        &self,
        ack: Ack,
        build: impl FnOnce(&View<'_>) -> Result<Operation, WriteError>,
    ) -> Result<u64, WriteError> {
        let seq = {
            let mut state = write(&self.state);
            let op = build(&View {
                state: StateRef::Borrowed(&state),
            })?;
            let bytes = op.encode();
            let changes = state.plan(&op)?;
            let seq = self.wal.append(&bytes)?;
            state.install(seq, changes, bytes.len());
            seq
        };
        if ack == Ack::Durable {
            self.wal.sync_to(seq)?;
        }
        Ok(seq)
    }

    pub fn get(&self, id: PointId) -> Result<Option<Record>> {
        read(&self.state).get(id)
    }

    /// The number of points.
    pub fn len(&self) -> usize {
        read(&self.state).index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether the memtable has grown past [`ShardOptions::flush_bytes`].
    pub fn needs_flush(&self) -> bool {
        read(&self.state).mem.bytes >= self.opts.flush_bytes
    }

    /// The live segments' ids, oldest first.
    pub fn segment_ids(&self) -> Vec<u64> {
        read(&self.state)
            .slots
            .iter()
            .map(|s| s.segment.id())
            .collect()
    }

    /// A consistent view of every point, held until it is dropped, which blocks writes, so
    /// keep it short.
    ///
    /// Read only through the view while holding it: any other `Shard` method takes the lock
    /// again, and once a writer is queued for it that second request waits behind the writer,
    /// which waits for the view, so the thread deadlocks.
    pub fn view(&self) -> View<'_> {
        View {
            state: StateRef::Guard(read(&self.state)),
        }
    }

    /// Writes the memtable out as a segment and truncates the WAL behind it. Returns
    /// whether there was anything to write.
    pub fn flush(&self) -> Result<bool> {
        let mut manifest = lock(&self.flush);
        if self.flush_broken.load(Ordering::Acquire) {
            return Err(Error::Invalid(format!(
                "{}: an earlier manifest write failed, restart to flush again",
                self.dir.display()
            )));
        }
        let (frozen, upto) = {
            let mut state = write(&self.state);
            // A frozen memtable left by a failed flush goes first.
            if state.frozen.is_none() {
                if state.mem.entries.is_empty() {
                    return Ok(false);
                }
                let mem = std::mem::take(&mut state.mem);
                state.frozen = Some(Arc::new(mem));
                state.frozen_upto = state.last_applied;
            }
            (state.frozen.clone().expect("set above"), state.frozen_upto)
        };

        let seg_id = manifest.next_segment_id;
        let seg_root = self.dir.join("segments");
        let dest = seg_root.join(segment_dir_name(seg_id));
        // An earlier failed flush may have got as far as publishing this directory, no
        // manifest lists it, so it is rebuilt from the frozen memtable.
        if dest.exists() {
            fs::remove_dir_all(&dest).ctx(|| format!("removing {}", dest.display()))?;
        }
        let entries = frozen.entries.iter().map(|(id, e)| EntryIn {
            id: *id,
            version: e.version,
            value: e
                .value
                .as_ref()
                .map(|r| (r.vector.as_slice(), PayloadIn::Value(&r.payload))),
        });
        write_segment(
            &seg_root.join(format!("tmp-{seg_id:016x}")),
            &dest,
            seg_id,
            self.config,
            Some(&self.opts.graph),
            entries,
        )?;
        let segment = Arc::new(Segment::open(&dest, seg_id, self.config)?);
        let mut slot = Slot::new(segment, &manifest.payload_indexes)?;
        crash_point("shard.flush.segment_published", || {});

        let mut next = manifest.clone();
        next.generation += 1;
        next.wal_upto = upto;
        next.next_segment_id = seg_id + 1;
        next.segments.push(seg_id);
        // A failed manifest write may still have replaced the file on disk, so this process
        // no longer knows which manifest recovery will see. Flushing stops until a restart
        // reads it back, writes carry on safely in the WAL meanwhile.
        if let Err(e) = next.write(&self.dir) {
            self.flush_broken.store(true, Ordering::Release);
            return Err(e);
        }
        *manifest = next;
        crash_point("shard.flush.manifest_written", || {});

        {
            let mut state = write(&self.state);
            for e in slot.segment.clone().entries() {
                // Still the newest copy unless the memtable took a newer write meanwhile.
                if let Some(row) = e.row
                    && state.index.get(&e.id) == Some(&Loc::Recent)
                    && !state.mem.entries.contains_key(&e.id)
                {
                    state.index.insert(e.id, Loc::Segment { id: seg_id, row });
                    slot.set_live(row, true);
                }
            }
            state.slots.push(slot);
            state.frozen = None;
        }

        self.wal.truncate_before(upto + 1)?;
        Ok(true)
    }

    /// Makes every write so far durable.
    pub fn sync(&self) -> Result<()> {
        self.wal.sync_all()
    }

    /// The payload indexes every segment builds.
    pub fn payload_indexes(&self) -> Vec<PayloadIndexSpec> {
        read(&self.state).payload_indexes.clone()
    }

    /// Adds a payload index, building it for every segment, and returns whether it was new.
    pub fn create_payload_index(&self, spec: PayloadIndexSpec) -> Result<bool> {
        self.change_payload_indexes(|list| {
            if list.contains(&spec) {
                return false;
            }
            list.push(spec);
            true
        })
    }

    /// Removes a payload index and returns whether it existed.
    pub fn drop_payload_index(&self, spec: &PayloadIndexSpec) -> Result<bool> {
        self.change_payload_indexes(|list| {
            let before = list.len();
            list.retain(|s| s != spec);
            list.len() != before
        })
    }

    /// Applies `change` to the list of payload indexes and, when it reports a change, builds
    /// the new indexes for every segment, records the list in a new manifest and swaps the
    /// indexes in. The flush lock is held throughout, so no segment appears meanwhile.
    fn change_payload_indexes(
        &self,
        change: impl FnOnce(&mut Vec<PayloadIndexSpec>) -> bool,
    ) -> Result<bool> {
        let mut manifest = lock(&self.flush);
        if self.flush_broken.load(Ordering::Acquire) {
            return Err(Error::Invalid(format!(
                "{}: an earlier manifest write failed, restart to change payload indexes",
                self.dir.display()
            )));
        }
        let mut next = manifest.clone();
        if !change(&mut next.payload_indexes) {
            return Ok(false);
        }
        next.generation += 1;

        // Built before anything is written, so a failure changes nothing.
        let segments: Vec<(Arc<Segment>, Vec<Entry>)> = read(&self.state)
            .slots
            .iter()
            .map(|s| (s.segment.clone(), s.rows.clone()))
            .collect();
        let mut built = HashMap::new();
        for (segment, rows) in &segments {
            let index = build_payload_index(segment, rows, &next.payload_indexes)?;
            built.insert(segment.id(), Arc::new(index));
        }

        if let Err(e) = next.write(&self.dir) {
            self.flush_broken.store(true, Ordering::Release);
            return Err(e);
        }
        let mut state = write(&self.state);
        state.payload_indexes = next.payload_indexes.clone();
        *manifest = next;
        for slot in &mut state.slots {
            slot.payload_index = built
                .remove(&slot.segment.id())
                .expect("no segment appears while the flush lock is held");
        }
        Ok(true)
    }
}

/// Why a write was refused.
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error(transparent)]
    Invalid(nocturnum_core::Error),
    #[error(transparent)]
    Storage(#[from] Error),
}

/// A consistent read-only view of a shard's points.
pub struct View<'a> {
    state: StateRef<'a>,
}

/// The state behind a [`View`]: its own read lock, or the write lock a write already holds.
enum StateRef<'a> {
    Guard(RwLockReadGuard<'a, State>),
    Borrowed(&'a State),
}

impl std::ops::Deref for StateRef<'_> {
    type Target = State;

    fn deref(&self) -> &State {
        match self {
            StateRef::Guard(g) => g,
            StateRef::Borrowed(s) => s,
        }
    }
}

/// A point seen through a [`View`].
pub struct PointRef<'a> {
    pub id: PointId,
    /// The sequence number of the write that produced this copy.
    pub version: u64,
    pub vector: &'a [f32],
    payload: PayloadRef<'a>,
}

enum PayloadRef<'a> {
    Memory(&'a Payload),
    Segment(&'a Segment, u32),
}

impl PointRef<'_> {
    pub fn payload(&self) -> Result<Cow<'_, Payload>> {
        match self.payload {
            PayloadRef::Memory(p) => Ok(Cow::Borrowed(p)),
            PayloadRef::Segment(seg, row) => seg.payload(row).map(Cow::Owned),
        }
    }
}

/// The live points of a memtable, skipping those that `shadow`, a newer memtable, holds.
fn recent_points<'a>(
    table: &'a Memtable,
    shadow: Option<&'a Memtable>,
) -> impl Iterator<Item = PointRef<'a>> {
    table.entries.iter().filter_map(move |(id, e)| {
        let rec = e.value.as_ref()?;
        if shadow.is_some_and(|m| m.entries.contains_key(id)) {
            return None;
        }
        Some(PointRef {
            id: *id,
            version: e.version,
            vector: &rec.vector,
            payload: PayloadRef::Memory(&rec.payload),
        })
    })
}

/// One segment seen through a [`View`], with the rows that still hold their point's newest
/// copy.
pub struct SegmentView<'a> {
    slot: &'a Slot,
}

impl<'a> SegmentView<'a> {
    pub fn id(&self) -> u64 {
        self.slot.segment.id()
    }

    /// The number of rows, live or not.
    pub fn rows(&self) -> u32 {
        self.slot.rows.len() as u32
    }

    /// Rows holding their point's newest copy, every other row is hidden by a newer write.
    pub fn live(&self) -> &'a RoaringBitmap {
        &self.slot.live
    }

    /// Every row's vector back to back, `dim` floats each.
    pub fn vectors(&self) -> &'a [f32] {
        self.slot.segment.vectors()
    }

    /// The point at `row`, live or not. Panics if `row` is out of range.
    pub fn point(&self, row: u32) -> PointRef<'a> {
        let e = self.slot.rows[row as usize];
        PointRef {
            id: e.id,
            version: e.version,
            vector: self.slot.segment.vector(row),
            payload: PayloadRef::Segment(&self.slot.segment, row),
        }
    }

    pub fn payload_index(&self) -> &'a PayloadIndex {
        &self.slot.payload_index
    }

    /// The segment's graph index, if it was big enough to get one.
    pub fn graph(&self) -> Option<&'a Hnsw> {
        self.slot.segment.graph()
    }
}

impl View<'_> {
    pub fn len(&self) -> usize {
        self.state.index.len()
    }

    pub fn get(&self, id: PointId) -> Result<Option<Record>> {
        self.state.get(id)
    }

    /// The payload indexes every segment builds.
    pub fn payload_indexes(&self) -> &[PayloadIndexSpec] {
        &self.state.payload_indexes
    }

    /// Points not yet in a segment, each exactly once, in no particular order.
    pub fn recent(&self) -> impl Iterator<Item = PointRef<'_>> {
        let state = &*self.state;
        let frozen = state
            .frozen
            .as_deref()
            .into_iter()
            .flat_map(move |f| recent_points(f, Some(&state.mem)));
        recent_points(&state.mem, None).chain(frozen)
    }

    /// The segments, oldest first. Together with [`View::recent`] their live rows hold every
    /// point exactly once.
    pub fn segments(&self) -> impl Iterator<Item = SegmentView<'_>> {
        self.state.slots.iter().map(|slot| SegmentView { slot })
    }

    pub fn is_empty(&self) -> bool {
        self.state.index.is_empty()
    }

    /// Every point, each exactly once, in no particular order.
    pub fn points(&self) -> impl Iterator<Item = PointRef<'_>> {
        let segments = self
            .segments()
            .flat_map(|seg| seg.live().iter().map(move |row| seg.point(row)));
        self.recent().chain(segments)
    }
}

mod merge;
mod snapshot;

pub use merge::{Job, OptimizerOptions, OptimizerStatus};
pub use snapshot::{Pruned, SnapshotInfo, prune, restore, snapshot_name};

#[cfg(test)]
mod tests;
