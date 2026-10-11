# Durability: what is promised and what is tested

nocturnum promises three acknowledgement levels for writes, set out in [`../decisions/0001-v1-scope.md`](../decisions/0001-v1-scope.md), and AGENTS.md rule 4 says every such promise needs a test behind it.

This page says which tests stand behind each promise, and which parts rest on the operating system and the drive rather than on anything a test on one machine can show.

## The write-ahead log's contract

| When the log says | It guarantees | Tested by |
|---|---|---|
| `append` returned | the record is handed to the operating system, so it survives the process crashing | `appended_records_survive_sigkill` |
| `sync_to(seq)` returned | every record up to `seq` was fsynced (`F_FULLFSYNC` on macOS, `fdatasync` on Linux) | `durable_acks_survive_sigkill`, `durable_acks_survive_sigkill_during_truncation`, and power loss as explained below |
| `sync_to` returned an error | the log is poisoned, the write may or may not be on disk and must not be acknowledged | `a_failed_fsync_poisons_the_log`, `a_failed_fsync_while_rotating_poisons_the_log` |
| `open` returned | the log holds an unbroken run of whole records, a torn end was cut off and synced | every crash test, plus the unit tests below |

The collection layer maps these onto the API levels: `accepted` answers after `append`, `applied` after `append` and applying the write, and `durable` after `sync_to` and applying it.

`accepted` and `applied` are documented as losable on a crash, which leaves room to buffer them later, although today a process crash alone loses neither.

## Crash tests

`crates/nocturnum-storage/tests/crash.rs` runs its own test binary again as a child process that writes to a log from two to four threads and prints every acknowledgement, then crashes it, reopens the log and checks:

- recovery succeeds,
- sequence numbers run without gaps from the first record to the last,
- every record is whole, its payload names the thread and counter that wrote it and carries filler derived from them,
- each thread's records appear in the order it wrote them with none missing in between,
- every acknowledged record is present with the right contents, unless truncation was entitled to remove it,
- truncation never removed a record at or after the boundary it was given.

The child is crashed in two ways:

- **SIGKILL at a random moment**, 25 durable runs, 15 durable runs that also truncate, and 15 runs that acknowledge after `append` only, all on one log directory so each run recovers what the last one left. One kill in five lands during startup, which can interrupt recovery itself.
- **At a named crash point**, where the child kills itself at an exact step, built in with the `crash-points` cargo feature, which only the tests enable:

| Crash point | Leaves behind |
|---|---|
| `wal.append.torn` | half a record at the end of the newest segment |
| `wal.segment.created` | a new, empty segment file |
| `wal.segment.half_header` | a new segment with half a header |
| `wal.segment.header_unsynced` | a new segment whose header and directory entry were never synced |
| `wal.truncate.removed_unsynced` | a removed segment whose directory entry was never synced |
| `wal.open.repaired_unsynced` | recovery cut a torn tail but stopped before syncing the cut |

Each crash point runs between a normal run before it and one after it, which checks that writing carries on correctly from whatever the crash left.

Seeds are printed, and `NOCTURNUM_CRASH_SEED=<seed>` replays a run.

A mutation check was done by hand when the harness was written: with recovery's tail truncation removed, two of the crash tests fail.

## Shard crash tests

`crates/nocturnum-storage/tests/shard_crash.rs` covers the whole shard, the memtable, flushes, segments, the manifest and recovery, which [`../decisions/0002-storage-layout.md`](../decisions/0002-storage-layout.md) describes.

A child process applies a deterministic stream of mixed operations (upserts, deletes and every payload change) from one thread with `durable` acknowledgements, flushing every 25 operations, and the parent regenerates the same stream. Because one writer only starts an operation after the previous one was acknowledged, the reopened shard must hold exactly the result of every acknowledged operation, or of those plus the one in flight, and the test checks that point by point.

It runs 20 random kills on one shard directory, which checks 400 to 500 acknowledged operations, and kills the child at every step of a flush:

| Crash point | Leaves behind |
|---|---|
| `segment.built_not_renamed` | a complete segment in a temporary directory |
| `shard.flush.segment_published` | a segment in place that no manifest lists yet |
| `manifest.tmp_written` | a new manifest written beside the old one but not swapped in |
| `shard.flush.manifest_written` | a published flush whose WAL records are not truncated yet |
| `wal.truncate.removed_unsynced` | a WAL truncation whose removal was never synced |

A crash while the shard is first created leaves no shard, since nothing was acknowledged, and creating it again works.

A mutation check was done by hand: truncating the WAL before writing the manifest, the classic ordering bug, fails both shard crash tests with a corruption error naming the missing records.

### Merges and vacuums

The same harness runs a second child that merges and vacuums halfway between flushes, with a limit of two segments so nearly every cycle rewrites something while the memtable holds changes to the points being merged.
It runs 20 random kills, which checks about 400 acknowledged operations, and kills the child at every step of a merge:

| Crash point | Leaves behind |
|---|---|
| `shard.merge.segment_built` | the merged segment in place, unlisted, with its inputs still live |
| `shard.merge.manifest_written` | a manifest listing the merged segment, its inputs not yet deleted |
| `shard.merge.first_input_removed` | one input deleted and the rest still on disk, all unlisted |

A merge only drops a dead row when its point's newest state is already in a segment, or the point is deleted, because the WAL holds operations rather than records: a payload change still in the memtable is replayed against the older copy after a crash.
The shard's model test found this, and the crash tests catch it too, since letting a merge drop every dead row fails two of them.

A dropped row can also be justified by a deletion acknowledged as `applied` and not yet synced, so a merge syncs the WAL up to the point it looked before publishing.
SIGKILL leaves unsynced WAL data in the page cache, so the crash tests cannot catch a merge that skips that sync, and this rule rests on review until the power-loss tests in the LazyFS task cover it.

## Unit tests

`crates/nocturnum-storage/src/wal/tests.rs` covers what a crash test reaches only by chance in the WAL:

- a log cut at every byte offset recovers exactly the whole records before the cut,
- garbage after the last record, and records spliced in from another log with the same history, are both rejected,
- damage in an older segment, a gap between segments, an unknown format version and a damaged header with records after it are errors that leave the files untouched,
- a smaller `max_record_bytes` than the log was written with never cuts existing records,
- injected fsync failures poison the log and are never retried into a false success,
- a second handle on the same directory is refused,
- a property test runs random appends, syncs, reopens and truncations against an in-memory model.

The segment and manifest tests flip every single bit of every file and check that loading refuses each one, and the shard's property test runs random writes, flushes and reopens against a plain in-memory model.

## What these tests do not prove

**Power loss.** Killing a process leaves everything it wrote in the operating system's page cache, which still reaches the disk, so the crash tests show the log's own logic is right but cannot show the data would survive the power going off.

That part rests on the operating system and the drive keeping their word about `F_FULLFSYNC` and `fdatasync`, and on the log calling them at the right moments: records are synced before `sync_to` returns, a full segment is synced before the next is created, a new segment's header and directory entry are synced before any record goes in, and every removal is synced before the next.

Testing that for real needs a filesystem that can drop unsynced writes on demand, such as LazyFS or `dm-log-writes` on Linux, which is planned as its own task.

**Drives that lie.** Some drives acknowledge a cache flush without performing it, and no software can make those durable.

**Other filesystems.** On macOS the log refuses to run where `F_FULLFSYNC` is unsupported rather than settling for less, while on Linux it has only been reasoned about for ext4 and XFS, not tested there yet.
