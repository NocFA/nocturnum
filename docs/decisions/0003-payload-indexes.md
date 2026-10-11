# 0003: payload indexes are derived, in memory, per segment

Decided while building the payload indexes on 2026-10-11, an implementation choice within the v1 scope rather than an owner decision.

## What

- Payload indexes are opt-in per field and per kind (`keyword`, `integer`, `float`, `bool`), and every indexed field also records how many non-null values each row holds and which rows hold a `null`.
- Each immutable segment gets its own indexes, built in memory from the segment's payloads when the segment is written or opened, and never logged, synced or listed in the manifest.
- The memtable has no indexes, filters on it run the reference evaluator, `Filter::matches`, since it holds at most one flush's worth of points.
- A filter on a segment becomes two row sets, the rows it surely matches and the rows it may match, and only rows between the two get their payload read and checked by the reference evaluator.

The code is `crates/nocturnum-index/src/payload.rs` for the indexes and `crates/nocturnum-query/src/plan.rs` for the planner.

## Why

Segments never change once written (decision 0002), so an index built from one stays right for the segment's whole life, with no updates, no deletions inside it and nothing to keep in step with the WAL.

Qdrant persists its payload indexes and has to flush them in the right order against payload storage, and its own code warns that getting that order wrong silently drops points on replay (`docs/research/a3-query.md`, "Lifecycle, locks and durability").
Rebuilding from the segment removes that whole class of bug, because an index can always be thrown away and rebuilt.

Answering with sure and maybe sets, rather than "indexed or not" per filter, lets a filter that is mostly indexed still narrow the rows before any payload is read, and keeps the reference evaluator as the only definition of what a filter means.
Property tests check every indexed answer against it, with indexes on random subsets of fields.

## What it costs

- **Startup time:** opening a shard reads every segment's payloads to build its indexes, which grows with the data, so persisting indexes inside the segment directory is the obvious later step if startup gets slow, and since segments are written once it adds no ordering problem.
- **Memory:** the indexes live in RAM, as roaring bitmaps per value and a sorted list of numbers per numeric field.
- **Unindexed conditions read payloads:** `nested`, conditions on unindexed fields and value types a field has no index for are settled by reading the payload, though a filtered search only reads payloads of rows that score high enough to enter the results.

## Planning

The planner turns the two sets into a minimum, an expected and a maximum count of matching rows, and chooses between scoring just the candidate rows and walking the segment's vector graph with the filter as a check.
When the minimum and maximum fall either side of the threshold, the expected count comes from reading a fixed sample of 64 uncertain rows, one from each stretch of equal size, since an even stride lines up with data that repeats at a fixed period.
Until segments carry a graph both choices run the same exact filtered scan, so the strategy only starts to matter with the HNSW task.
