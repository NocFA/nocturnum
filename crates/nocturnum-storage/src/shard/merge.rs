//! Merging and vacuuming segments, and the policy that picks what to do next.
//!
//! A merge rewrites a run of adjacent segments as one: each point keeps its newest entry in
//! the run, rows hidden by newer writes are dropped, and deletion entries are dropped only
//! when the run starts at the oldest segment, since otherwise an older segment may still hold
//! a copy they hide. Vacuuming is a merge of one segment.
//!
//! The new segment is built beside the live ones, published by a manifest that swaps the run
//! for it, and only then are the old directories deleted, so a crash at any step leaves the
//! old segments or the new one listed, never neither. Writes carry on into the memtable
//! throughout, and flushes can publish while a merge builds, since a flush only appends
//! segments after the run.

use std::collections::BTreeSet;

use super::*;
use crate::segment::PayloadIn;

/// What [`Shard::optimize_once`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Job {
    Flush,
    /// Adjacent segments merged into the one named last.
    Merge {
        inputs: Vec<u64>,
        output: u64,
    },
    /// A segment rewritten without its dead rows.
    Vacuum {
        input: u64,
        output: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OptimizerOptions {
    /// Above this many segments, the two adjacent ones with the fewest rows between them are
    /// merged.
    pub max_segments: usize,
    /// A segment with more than this share of its rows dead is rewritten.
    pub vacuum_ratio: f64,
    /// How long the background optimizer waits when there is nothing to do.
    pub interval: Duration,
}

impl Default for OptimizerOptions {
    fn default() -> Self {
        OptimizerOptions {
            max_segments: 8,
            vacuum_ratio: 0.2,
            interval: Duration::from_secs(1),
        }
    }
}

/// What the optimizer is doing, for the console.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OptimizerStatus {
    /// The segments being rewritten right now.
    pub running: Option<Vec<u64>>,
    /// The last job that finished.
    pub last: Option<Job>,
    /// Why the last attempt failed, cleared by the next success.
    pub last_error: Option<String>,
}

/// One input to a merge: a segment and the rows it can leave out.
struct Input {
    segment: Arc<Segment>,
    droppable: RoaringBitmap,
}

/// Rows of `slot` a rewrite can leave out: dead rows whose point's newest state is already in a
/// segment, or which is deleted. A deletion may still be unsynced in the WAL, so a merge syncs
/// the WAL up to the moment it looked before publishing.
///
/// A dead row hidden by the memtable has to stay, because the WAL holds operations rather than
/// records: a payload change still in the memtable is replayed after a crash against this
/// older copy, and without it the point would vanish.
fn droppable(state: &State, slot: &Slot) -> RoaringBitmap {
    let mut dead = RoaringBitmap::new();
    dead.insert_range(0..slot.rows.len() as u32);
    dead -= &slot.live;
    dead.iter()
        .filter(|&row| state.index.get(&slot.rows[row as usize].id) != Some(&Loc::Recent))
        .collect()
}

impl Shard {
    /// Rewrites the adjacent segments `ids`, oldest first, as one segment, returning its id.
    /// One id vacuums that segment.
    pub fn merge(&self, ids: &[u64]) -> Result<u64> {
        let _one_at_a_time = lock(&self.merging);
        let invalid = |reason: String| Error::Invalid(format!("{}: {reason}", self.dir.display()));
        if ids.is_empty() {
            return Err(invalid("a merge needs at least one segment".into()));
        }

        // Rows that are dead now stay dead, newer copies never get older, so they can be
        // dropped. Rows that die while the merge builds are written and left dead.
        let (inputs, from_oldest, seen_upto) = {
            let state = read(&self.state);
            let start = state
                .slots
                .iter()
                .position(|s| s.segment.id() == ids[0])
                .ok_or_else(|| invalid(format!("segment {} is not live", ids[0])))?;
            let run = state
                .slots
                .get(start..start + ids.len())
                .filter(|run| run.iter().map(|s| s.segment.id()).eq(ids.iter().copied()))
                .ok_or_else(|| invalid(format!("segments {ids:?} are not adjacent and live")))?;
            let inputs: Vec<Input> = run
                .iter()
                .map(|s| Input {
                    segment: s.segment.clone(),
                    droppable: droppable(&state, s),
                })
                .collect();
            (inputs, start == 0, state.last_applied)
        };
        self.check_disk(&inputs)?;

        let output = {
            let mut manifest = lock(&self.flush);
            self.check_flushable(&manifest)?;
            let id = manifest.next_segment_id;
            // Reserved in memory, the next manifest written records it as used.
            manifest.next_segment_id += 1;
            id
        };
        self.set_running(Some(ids.to_vec()));
        let mut publishing = false;
        let result = self.build_and_publish(
            ids,
            &inputs,
            from_oldest,
            seen_upto,
            output,
            &mut publishing,
        );
        self.set_running(None);
        if result.is_err() && !publishing {
            // Nothing lists the output yet, so it goes now rather than holding disk space
            // until a restart. A manifest write that failed may still have taken effect, so
            // after one the files stay.
            let seg_root = self.dir.join("segments");
            let _ = fs::remove_dir_all(seg_root.join(format!("tmp-{output:016x}")));
            let _ = fs::remove_dir_all(seg_root.join(segment_dir_name(output)));
        }
        result.map(|()| output)
    }

    fn build_and_publish(
        &self,
        ids: &[u64],
        inputs: &[Input],
        from_oldest: bool,
        seen_upto: u64,
        output: u64,
        publishing: &mut bool,
    ) -> Result<()> {
        let seg_root = self.dir.join("segments");
        let dest = seg_root.join(segment_dir_name(output));
        // A flush that failed before its manifest may have left a directory under this id.
        if dest.exists() {
            fs::remove_dir_all(&dest).ctx(|| format!("removing {}", dest.display()))?;
        }
        write_segment(
            &seg_root.join(format!("tmp-{output:016x}")),
            &dest,
            output,
            self.config,
            Some(&self.opts.graph),
            merged_entries(inputs, from_oldest),
        )?;
        let segment = Arc::new(Segment::open(&dest, output, self.config)?);
        let indexes = lock(&self.flush).payload_indexes.clone();
        let mut slot = Slot::new(segment.clone(), &indexes)?;
        crash_point("shard.merge.segment_built", || {});

        {
            let mut manifest = lock(&self.flush);
            self.check_flushable(&manifest)?;
            // The index list may have changed while the segment was built.
            if manifest.payload_indexes != indexes {
                slot = Slot::new(segment.clone(), &manifest.payload_indexes)?;
            }
            let start = manifest
                .segments
                .iter()
                .position(|s| *s == ids[0])
                .expect("only merges remove segments, and they take turns");
            let mut next = manifest.clone();
            next.generation += 1;
            next.segments.splice(start..start + ids.len(), [output]);
            // Rows were dropped because writes up to `seen_upto` hid them, and some of those
            // may sit in the WAL unsynced, such as an `applied` deletion. They have to be on
            // disk before the rows they replace leave it.
            self.wal.sync_to(seen_upto)?;
            *publishing = true;
            if let Err(e) = next.write(&self.dir) {
                self.flush_broken.store(true, Ordering::Release);
                return Err(e);
            }
            *manifest = next;
            crash_point("shard.merge.manifest_written", || {});

            let mut state = write(&self.state);
            for e in segment.entries() {
                let Some(row) = e.row else { continue };
                // Still the newest copy unless a newer write landed while the merge built.
                if let Some(Loc::Segment { id, .. }) = state.index.get(&e.id)
                    && ids.contains(id)
                {
                    state.index.insert(e.id, Loc::Segment { id: output, row });
                    slot.set_live(row, true);
                }
            }
            let start = state
                .slots
                .iter()
                .position(|s| s.segment.id() == ids[0])
                .expect("the run is still live");
            state.slots.splice(start..start + ids.len(), [slot]);
        }

        for (i, id) in ids.iter().enumerate() {
            let old = seg_root.join(segment_dir_name(*id));
            fs::remove_dir_all(&old).ctx(|| format!("removing {}", old.display()))?;
            if i == 0 {
                crash_point("shard.merge.first_input_removed", || {});
            }
        }
        sync_dir(&seg_root).ctx(|| format!("syncing {}", seg_root.display()))?;
        Ok(())
    }

    pub(super) fn check_flushable(&self, _manifest: &Manifest) -> Result<()> {
        if self.flush_broken.load(Ordering::Acquire) {
            return Err(Error::Invalid(format!(
                "{}: an earlier manifest write failed, restart to change segments again",
                self.dir.display()
            )));
        }
        Ok(())
    }

    /// Refuses a rebuild unless the disk has twice the inputs' size free.
    fn check_disk(&self, inputs: &[Input]) -> Result<()> {
        let need: u64 = inputs.iter().map(|i| i.segment.disk_bytes()).sum::<u64>() * 2;
        match crate::sync::disk_space(&self.dir).map(|d| d.available) {
            Some(free) if free < need => Err(Error::Invalid(format!(
                "{}: rewriting segments needs {need} bytes free, the disk has {free}",
                self.dir.display()
            ))),
            _ => Ok(()),
        }
    }

    fn set_running(&self, running: Option<Vec<u64>>) {
        lock(&self.optimizer_status).running = running;
    }

    pub fn optimizer_status(&self) -> OptimizerStatus {
        lock(&self.optimizer_status).clone()
    }

    /// Does the most useful piece of upkeep, if any: a flush when the memtable is full, a
    /// vacuum of the segment with the most dead rows past the ratio, or a merge when there are
    /// too many segments.
    pub fn optimize_once(&self, options: &OptimizerOptions) -> Result<Option<Job>> {
        let result = self.choose_and_run(options);
        let mut status = lock(&self.optimizer_status);
        match &result {
            Ok(Some(job)) => {
                status.last = Some(job.clone());
                status.last_error = None;
            }
            Ok(None) => {}
            Err(e) => status.last_error = Some(e.to_string()),
        }
        result
    }

    fn choose_and_run(&self, options: &OptimizerOptions) -> Result<Option<Job>> {
        // A ratio below 0 would rewrite segments with nothing to drop, forever.
        if !(0.0..=1.0).contains(&options.vacuum_ratio) {
            return Err(Error::Invalid(format!(
                "vacuum_ratio must be between 0 and 1, got {}",
                options.vacuum_ratio
            )));
        }
        if self.needs_flush() && self.flush()? {
            return Ok(Some(Job::Flush));
        }
        // (id, rows, droppable share) for every segment, oldest first. Only rows a rewrite can
        // actually drop count, or a segment shadowed by the memtable would be rewritten
        // forever.
        let state = read(&self.state);
        let segments: Vec<(u64, u64, f64)> = state
            .slots
            .iter()
            .map(|s| {
                let rows = s.rows.len() as u64;
                let dead = droppable(&state, s).len();
                let share = if rows == 0 {
                    0.0
                } else {
                    dead as f64 / rows as f64
                };
                (s.segment.id(), rows, share)
            })
            .collect();
        // Released before merging, which takes the lock itself.
        drop(state);

        let worst = segments
            .iter()
            .filter(|(_, _, share)| *share > options.vacuum_ratio)
            .max_by(|a, b| a.2.total_cmp(&b.2));
        if let Some(&(input, _, _)) = worst {
            let output = self.merge(&[input])?;
            return Ok(Some(Job::Vacuum { input, output }));
        }

        if segments.len() > options.max_segments.max(1) {
            let pair = segments
                .windows(2)
                .min_by_key(|w| w[0].1 + w[1].1)
                .expect("more than one segment");
            let inputs = vec![pair[0].0, pair[1].0];
            let output = self.merge(&inputs)?;
            return Ok(Some(Job::Merge { inputs, output }));
        }
        Ok(None)
    }
}

/// Each id's newest entry across the inputs, in id order, leaving out rows that were dead when
/// the merge started and, when the inputs start at the oldest segment, deletions.
fn merged_entries(inputs: &[Input], from_oldest: bool) -> impl Iterator<Item = EntryIn<'_>> {
    // Every input is sorted by id, so walking the union of ids in order and looking each up
    // in the newest input first keeps memory to the id set.
    let ids: BTreeSet<PointId> = inputs
        .iter()
        .flat_map(|i| i.segment.entries().iter().map(|e| e.id))
        .collect();
    let mut cursors = vec![0usize; inputs.len()];
    ids.into_iter().filter_map(move |id| {
        let mut newest = None;
        for (k, input) in inputs.iter().enumerate() {
            let entries = input.segment.entries();
            if entries.get(cursors[k]).is_some_and(|e| e.id == id) {
                newest = Some((k, entries[cursors[k]]));
                cursors[k] += 1;
            }
        }
        let (k, e) = newest.expect("every id comes from some input");
        let input = &inputs[k];
        match e.row {
            // Hidden by a newer durable write outside the run, which also hides any older
            // copy, or by a deletion.
            Some(row) if input.droppable.contains(row) => None,
            Some(row) => Some(EntryIn {
                id,
                version: e.version,
                value: Some((
                    input.segment.vector(row),
                    PayloadIn::Json(input.segment.payload_bytes(row)),
                )),
            }),
            None if from_oldest => None,
            None => Some(EntryIn {
                id,
                version: e.version,
                value: None,
            }),
        }
    })
}
