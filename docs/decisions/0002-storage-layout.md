# 0002: storage layout, a memtable over immutable segments

Decided while building the storage layer on 2026-10-10, an implementation choice within the v1 scope rather than an owner decision.

## What

A shard holds one WAL, one in-memory memtable and any number of immutable on-disk segments, tied together by a manifest.

- Writes are logged to the WAL and applied to the memtable, which lives only in RAM.
- A flush writes the memtable out as a new immutable segment, built in a temporary directory and renamed into place once synced, then publishes a new manifest listing it with the WAL sequence number it covers, then truncates the WAL behind that number.
- Recovery loads the segments the manifest lists, deletes any other segment directory, and replays the WAL from the manifest's watermark into a fresh memtable.
- A point's newest copy wins, and newer layers sit above older ones: the memtable, then a memtable being flushed, then segments from newest to oldest. A deletion is stored as an entry without a row, so it keeps hiding older copies until a merge removes both.

The details live in the module docs of `crates/nocturnum-storage/src/{shard,segment,manifest}.rs`.

## Why not an appendable segment on disk

The research (`docs/research/a1-storage.md`) describes Qdrant keeping mutable, appendable segments on disk next to immutable ones, each with its own files, version tracking and flush order, and its crash safety depends on several repair mechanisms working together.

Keeping the mutable layer in RAM and letting the WAL be its only durable form removes that whole structure along with its crash cases. Every on-disk file is then written exactly once, and the manifest swap is the single moment a flush becomes real, which the crash tests can exercise at every step.

## What it costs

- **Memory:** the memtable holds unflushed points in RAM, bounded by `ShardOptions::flush_bytes` (32 MiB by default), which suits the homelab target.
- **Startup time:** recovery replays at most one memtable's worth of WAL.
- **Segment count:** every flush adds a segment, so the optimizer (`crates/nocturnum-storage/src/shard/merge.rs`) merges adjacent segments past a limit of 8 and rewrites any with more than 20% of their rows droppable, building aside and swapping through the manifest like a flush.
- **Writes block during long reads:** a read view holds the shard's read lock, so a long scan delays writers. That is acceptable for v1, and the optimizer task can move readers onto snapshots of the segment list.

## Invariants the code relies on

1. Every point id appears at most once per segment, and a newer segment only holds newer versions of a point than an older one, because segments are flushed in sequence order. Merges must keep this.
2. The manifest never names a segment that is not fully on disk, because segments are renamed into place and synced before the manifest is written.
3. The WAL is never truncated past the manifest's watermark, and recovery refuses to start, with a corruption error, if the WAL ends before the watermark or starts after the record replay needs.
4. A rewrite keeps every row the WAL may still replay an operation against: a dead row is dropped only when its point's newest state is in a segment or the point is deleted, the WAL is synced up to that moment before the rewrite is published, so an unsynced deletion cannot be lost while the row it hid is gone, and deletion entries are dropped only when the merge starts at the oldest segment.
